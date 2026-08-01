//! POSIX 5-field cron predicate (#175 phase 5, S1).
//!
//! Hand-rolled, closed-grammar parser: `<min> <hour> <dom> <mon> <dow>` where
//! each field accepts `*`, a bare number, `*/N`, comma lists, and ranges
//! (`lo-hi`). Anything else — day-of-week names, `@daily` aliases, `L` / `W`
//! extensions — is refused loudly at parse (P7: closed enum, never "loose").
//!
//! `CronExpr::next_after(prev_fire_wall_ms, tz)` is pure integer/table math
//! over `libc::localtime_r` / `gmtime_r`, no crate deps. DST semantics:
//! * **Spring forward** — because we advance one epoch minute at a time and
//!   check the *local* broken-down time, the skipped local hour never
//!   satisfies the fields, so a `30 2 * * *` on the day 2am doesn't exist
//!   locally just fires the following day. Verified in
//!   `spring_forward_skips_missing_local_time`.
//! * **Fall back** — the local wall-clock `1:30` happens twice; the
//!   predicate fires once. `next_after(prev, tz)` requires the returned
//!   epoch's local broken-down time to differ from `prev`'s (same-slot
//!   guard), so the second `1:30` is skipped. Verified in
//!   `fall_back_fires_once`.
//!
//! Machine-asleep collapse (S1): when the runner wakes and `now` is far past
//! `next_fire`, it fires ONCE with `missed_fires = <count of skipped
//! slots>`, then advances `next_fire = next_after(now, tz)`. That's a
//! `CheckRunner::tick_crons` concern; this module only supplies the pure
//! `next_after` iterator.

use std::fmt::Write;

/// The five fields of a POSIX cron expression, each as an explicit bitmask of
/// allowed values. Bitmask is cheap to intersect against the wall-clock
/// broken-down time inside `next_after`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    /// Minutes 0..=59.
    minute: u64,
    /// Hours 0..=23.
    hour: u32,
    /// Day-of-month 1..=31.
    dom: u32,
    /// Month 1..=12.
    month: u16,
    /// Day-of-week 0..=6 (Sunday = 0), POSIX form.
    dow: u8,
    /// Original text — echoed back in error contexts / config round-trip.
    text: String,
}

/// Timezone the cron's local time is interpreted in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CronTz {
    /// Follow the host's `TZ` / `/etc/localtime`. Used via `libc::localtime_r`.
    #[default]
    Local,
    /// Interpret every field in UTC. Used via `libc::gmtime_r`.
    Utc,
}

impl CronExpr {
    /// Parse a 5-field cron expression. Whitespace between fields is any
    /// number of ASCII spaces or tabs. Trailing/leading whitespace tolerated.
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron expression must have exactly 5 fields (min hour dom mon dow); got {} in {:?}",
                fields.len(),
                trimmed
            ));
        }
        let minute = parse_field(fields[0], 0, 59, "minute")?;
        let hour = parse_field(fields[1], 0, 23, "hour")? as u32;
        let dom = parse_field(fields[2], 1, 31, "day-of-month")? as u32;
        let month = parse_field(fields[3], 1, 12, "month")? as u16;
        let dow = parse_field(fields[4], 0, 6, "day-of-week")? as u8;
        Ok(Self {
            minute,
            hour,
            dom,
            month,
            dow,
            text: trimmed.to_string(),
        })
    }

    /// The first epoch millisecond strictly greater than `prev_fire_wall_ms`
    /// whose broken-down time in `tz` matches every field AND whose local
    /// broken-down time differs from `prev_fire_wall_ms`'s (same-slot guard
    /// — solves DST fall-back). `None` if no match exists within the next
    /// four years (guards against unreachable patterns like `0 0 30 2 *`
    /// on a non-leap calendar).
    pub fn next_after(&self, prev_fire_wall_ms: u64, tz: CronTz) -> Option<u64> {
        // Round `prev` up to the next whole minute; cron granularity is one
        // minute so scanning at sub-minute resolution is wasted work.
        let prev_min_epoch = prev_fire_wall_ms / 60_000;
        // The bd of `prev` — used for the same-slot guard.
        let prev_bd = broken_down(i64::try_from(prev_min_epoch * 60).ok()?, tz)?;

        // Hard cap: four calendar years (2 * ~525k minutes) is enough to
        // reach the next Feb 29 for `0 0 29 2 *` even from the day after.
        let max_iters: u64 = 4 * 366 * 24 * 60;
        let start = prev_min_epoch + 1;
        for minute_epoch in start..start + max_iters {
            let seconds = i64::try_from(minute_epoch * 60).ok()?;
            let bd = broken_down(seconds, tz)?;
            if self.matches(&bd) && !bd.is_same_local_minute(&prev_bd) {
                return Some(minute_epoch * 60_000);
            }
        }
        None
    }

    fn matches(&self, bd: &BrokenDown) -> bool {
        (self.minute & (1u64 << bd.minute)) != 0
            && (self.hour & (1u32 << bd.hour)) != 0
            && (self.dom & (1u32 << bd.dom)) != 0
            && (self.month & (1u16 << bd.month)) != 0
            && (self.dow & (1u8 << bd.dow)) != 0
    }
}

/// Broken-down wall-clock time in a specific timezone. Fields are 0-based
/// where cron is 0-based (minute/hour/dow) and 1-based where cron is 1-based
/// (dom/month). Only the bits the predicate needs are captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BrokenDown {
    year: i32,
    month: u32,  // 1..=12
    dom: u32,    // 1..=31
    hour: u32,   // 0..=23
    minute: u32, // 0..=59
    dow: u32,    // 0..=6, Sunday = 0
}

impl BrokenDown {
    fn is_same_local_minute(&self, other: &BrokenDown) -> bool {
        self.year == other.year
            && self.month == other.month
            && self.dom == other.dom
            && self.hour == other.hour
            && self.minute == other.minute
    }
}

/// Convert a unix epoch second to a broken-down time in `tz` via libc.
/// `libc::localtime_r` / `gmtime_r` are reentrant and thread-safe.
fn broken_down(epoch_seconds: i64, tz: CronTz) -> Option<BrokenDown> {
    // `libc::time_t` is `i64` on the platforms flock targets; keep the
    // explicit cast so the type is legible.
    let time_t: libc::time_t = epoch_seconds;
    // SAFETY: we pass a valid pointer to a stack `tm`. libc writes to it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = match tz {
        CronTz::Local => unsafe { libc::localtime_r(&time_t, &mut tm) },
        CronTz::Utc => unsafe { libc::gmtime_r(&time_t, &mut tm) },
    };
    if ok.is_null() {
        return None;
    }
    Some(BrokenDown {
        year: 1900 + tm.tm_year,
        month: (tm.tm_mon + 1) as u32,
        dom: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        dow: tm.tm_wday as u32,
    })
}

/// Parse one cron field into a bitmask covering `[min_val, max_val]`.
///
/// Grammar (closed; anything else errors):
/// * `*` — every value in `[min, max]`
/// * bare integer — one value
/// * `lo-hi` — inclusive range
/// * `*/N` or `lo-hi/N` — step; N must be `>= 1`
/// * comma list of any of the above: `1,5,10-20/2`
fn parse_field(text: &str, min_val: u32, max_val: u32, field_name: &str) -> Result<u64, String> {
    if text.is_empty() {
        return Err(format!("cron {field_name} field is empty"));
    }
    let mut mask: u64 = 0;
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!(
                "cron {field_name} field has an empty comma-separated entry in {text:?}"
            ));
        }
        // Split off `/step` if present.
        let (base, step) = match part.split_once('/') {
            Some((base, step_str)) => {
                let step: u32 = step_str
                    .parse()
                    .map_err(|_| step_err(field_name, part, step_str))?;
                if step == 0 {
                    return Err(step_err(field_name, part, step_str));
                }
                (base, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if base == "*" {
            (min_val, max_val)
        } else if let Some((a, b)) = base.split_once('-') {
            let a: u32 = a.parse().map_err(|_| range_err(field_name, part))?;
            let b: u32 = b.parse().map_err(|_| range_err(field_name, part))?;
            if a > b {
                return Err(range_err(field_name, part));
            }
            (a, b)
        } else {
            let v: u32 = base.parse().map_err(|_| bare_err(field_name, part))?;
            (v, v)
        };
        if lo < min_val || hi > max_val {
            return Err(format!(
                "cron {field_name} field entry {part:?} out of bounds [{min_val},{max_val}]"
            ));
        }
        let mut v = lo;
        while v <= hi {
            mask |= 1u64 << v;
            v += step;
        }
    }
    Ok(mask)
}

fn step_err(field_name: &str, part: &str, step_str: &str) -> String {
    format!(
        "cron {field_name} field entry {part:?} has an invalid step {step_str:?} (must be integer >= 1)"
    )
}

fn range_err(field_name: &str, part: &str) -> String {
    format!(
        "cron {field_name} field entry {part:?} is not a valid range (expected `lo-hi` with lo <= hi)"
    )
}

fn bare_err(field_name: &str, part: &str) -> String {
    format!("cron {field_name} field entry {part:?} is not a recognized token (expected `*`, a number, `lo-hi`, `*/N`, or a comma list)")
}

/// Serde helper: parse a cron expression from a config string.
impl<'de> serde::Deserialize<'de> for CronExpr {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(de)?;
        CronExpr::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Serde emits the original text form so round-trips are stable.
impl serde::Serialize for CronExpr {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ser.serialize_str(&self.text)
    }
}

impl std::fmt::Display for CronExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Small helper for callers that need the run-id shape defined in the
/// design comment: `run:<name>:<scheduled_ms hex>`. Kept here so the
/// runner (which owns the schedule) and the event emitter agree.
pub fn cron_run_id(name: &str, scheduled_ms: u64) -> String {
    let mut out = String::with_capacity(name.len() + 24);
    let _ = write!(out, "run:{name}:{scheduled_ms:x}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------- parser -------

    #[test]
    fn parse_star_every_field() {
        let e = CronExpr::parse("* * * * *").unwrap();
        // Every minute matches, every hour, etc.
        assert_eq!(e.minute.count_ones(), 60);
        assert_eq!(e.hour.count_ones(), 24);
        assert_eq!(e.dom.count_ones(), 31);
        assert_eq!(e.month.count_ones(), 12);
        assert_eq!(e.dow.count_ones(), 7);
    }

    #[test]
    fn parse_step_and_range_and_list() {
        // Every 15 minutes past 9-17 on weekdays.
        let e = CronExpr::parse("*/15 9-17 * * 1-5").unwrap();
        assert_eq!(e.minute.count_ones(), 4); // 0,15,30,45
        assert!((e.minute & (1u64 << 0)) != 0);
        assert!((e.minute & (1u64 << 15)) != 0);
        assert!((e.minute & (1u64 << 45)) != 0);
        assert_eq!(e.hour.count_ones(), 9); // 9..=17
        assert_eq!(e.dow.count_ones(), 5);
    }

    #[test]
    fn parse_comma_list() {
        let e = CronExpr::parse("0 0,6,12,18 * * *").unwrap();
        assert_eq!(e.hour.count_ones(), 4);
        assert!(e.hour & (1u32 << 0) != 0);
        assert!(e.hour & (1u32 << 6) != 0);
        assert!(e.hour & (1u32 << 12) != 0);
        assert!(e.hour & (1u32 << 18) != 0);
    }

    #[test]
    fn parse_rejects_unknown_grammar() {
        // Named day-of-week, aliases, `L`, `W`, ranges out of bounds, bad
        // steps — closed grammar, every one an explicit error.
        for bad in [
            "@daily",
            "@hourly",
            "* * * * MON",
            "60 * * * *",   // minute > 59
            "* 24 * * *",   // hour > 23
            "* * 0 * *",    // dom < 1
            "* * 32 * *",   // dom > 31
            "* * * 0 *",    // month < 1
            "* * * 13 *",   // month > 12
            "* * * * 7",    // dow > 6
            "*/0 * * * *",  // step zero
            "5-3 * * * *",  // reversed range
            "* * * * L",    // extension
            "1,,3 * * * *", // empty list entry
            "* * *",        // too few fields
            "1 2 3 4 5 6",  // too many fields
            "abc * * * *",  // gibberish
        ] {
            assert!(
                CronExpr::parse(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    // ------- next_after helpers -------

    /// Convert a `YYYY-MM-DD HH:MM:SS` UTC string to unix ms via a fixed
    /// table. Only the specific instants the DST/asleep tests need are
    /// covered; keeps this test suite hermetic (no chrono).
    fn utc_ms(year: i32, month: u32, day: u32, h: u32, m: u32, s: u32) -> u64 {
        // Days from unix epoch to the given date. Simple leap-year math.
        let mut days: i64 = 0;
        for y in 1970..year {
            days += if is_leap(y) { 366 } else { 365 };
        }
        let mdays = if is_leap(year) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };
        for m in 1..month {
            days += mdays[(m - 1) as usize] as i64;
        }
        days += day as i64 - 1;
        let secs = days * 86_400 + h as i64 * 3600 + m as i64 * 60 + s as i64;
        (secs as u64) * 1000
    }

    fn is_leap(year: i32) -> bool {
        (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
    }

    // ------- next_after — UTC -------

    #[test]
    fn next_after_every_minute_utc() {
        let e = CronExpr::parse("* * * * *").unwrap();
        let start = utc_ms(2025, 6, 15, 12, 30, 15);
        // Round-up to the next whole minute is 12:31:00 UTC.
        let want = utc_ms(2025, 6, 15, 12, 31, 0);
        assert_eq!(e.next_after(start, CronTz::Utc), Some(want));
    }

    #[test]
    fn next_after_daily_midnight_utc() {
        let e = CronExpr::parse("0 0 * * *").unwrap();
        let start = utc_ms(2025, 6, 15, 12, 0, 0);
        let want = utc_ms(2025, 6, 16, 0, 0, 0);
        assert_eq!(e.next_after(start, CronTz::Utc), Some(want));
    }

    #[test]
    fn next_after_weekday_at_9am_utc() {
        // Mondays 9am UTC.
        let e = CronExpr::parse("0 9 * * 1").unwrap();
        // 2025-06-13 is a Friday; the next Monday 9am UTC is 2025-06-16.
        let start = utc_ms(2025, 6, 13, 10, 0, 0);
        let want = utc_ms(2025, 6, 16, 9, 0, 0);
        assert_eq!(e.next_after(start, CronTz::Utc), Some(want));
    }

    #[test]
    fn next_after_leap_day_utc() {
        // Yearly on Feb 29 at 00:00.
        let e = CronExpr::parse("0 0 29 2 *").unwrap();
        // From 2025-06-15 the next Feb 29 is 2028-02-29.
        let start = utc_ms(2025, 6, 15, 0, 0, 0);
        let want = utc_ms(2028, 2, 29, 0, 0, 0);
        assert_eq!(e.next_after(start, CronTz::Utc), Some(want));
    }

    #[test]
    fn next_after_same_local_minute_guard() {
        // Ask twice in a row starting at the same prev — the second call
        // must return the *next* matching minute, not repeat the same one.
        let e = CronExpr::parse("*/5 * * * *").unwrap();
        let start = utc_ms(2025, 6, 15, 12, 5, 0);
        let a = e.next_after(start, CronTz::Utc).unwrap();
        let b = e.next_after(a, CronTz::Utc).unwrap();
        assert!(b > a);
        assert_eq!(b - a, 5 * 60_000);
    }

    // ------- DST — Local, requires the process TZ.
    // These use libc's TZ env override so the tests are hermetic no matter
    // what the host's real /etc/localtime says.

    // `libc` on unix targets doesn't expose tzset(), but every unix libc
    // implements it. Declare the extern ourselves for the DST tests.
    unsafe extern "C" {
        fn tzset();
    }

    fn with_tz<F: FnOnce()>(tz: &str, run: F) {
        let prev = std::env::var("TZ").ok();
        std::env::set_var("TZ", tz);
        // libc caches TZ; tzset() reloads from env.
        unsafe { tzset() };
        run();
        match prev {
            Some(v) => std::env::set_var("TZ", v),
            None => std::env::remove_var("TZ"),
        }
        unsafe { tzset() };
    }

    #[test]
    fn spring_forward_skips_missing_local_time() {
        // 2025-03-09 in America/New_York: clocks jump from 01:59 EST directly
        // to 03:00 EDT. A cron of "30 2 * * *" wants 02:30 every day. On the
        // 9th, local 02:30 does not exist, so the predicate must fire on the
        // 10th at 02:30 EDT (== 06:30 UTC).
        with_tz("America/New_York", || {
            let e = CronExpr::parse("30 2 * * *").unwrap();
            // Start just before the DST boundary in UTC (03:00 UTC = 22:00 EST
            // on the 8th, well before the transition).
            let start = utc_ms(2025, 3, 9, 3, 0, 0);
            let got = e.next_after(start, CronTz::Local).unwrap();
            // 2025-03-10 02:30 EDT = 06:30 UTC.
            let want = utc_ms(2025, 3, 10, 6, 30, 0);
            assert_eq!(got, want, "cron must skip the non-existent local 02:30");
        });
    }

    #[test]
    fn fall_back_fires_once() {
        // 2025-11-02 in America/New_York: clocks fall back from 01:59 EDT to
        // 01:00 EST. A cron of "30 1 * * *" fires ONCE. Two consecutive
        // `next_after` calls with the same prev must produce two different
        // wall-clock instants at least 24h apart (i.e. we do NOT re-hit the
        // duplicated 01:30 hour).
        with_tz("America/New_York", || {
            let e = CronExpr::parse("30 1 * * *").unwrap();
            // Start well before the transition: Nov 1 00:00 UTC.
            let start = utc_ms(2025, 11, 1, 0, 0, 0);
            let first = e.next_after(start, CronTz::Local).unwrap();
            // First hit: Nov 1 01:30 EDT = 05:30 UTC.
            assert_eq!(first, utc_ms(2025, 11, 1, 5, 30, 0));
            // Feed `first` as prev and ask again — the same-local-minute
            // guard is only for the *prev* fire; a fresh call will land at
            // the next matching local 01:30, which post-fall-back is Nov 2
            // 01:30 EST = 06:30 UTC. The natural iteration on Nov 2 hits
            // EDT-01:30 (05:30 UTC) first, and *that* is the second fire —
            // the duplicated 01:30 EST is skipped by the same-local guard
            // when we feed EDT-01:30 forward again below.
            let second = e.next_after(first, CronTz::Local).unwrap();
            assert_eq!(second, utc_ms(2025, 11, 2, 5, 30, 0));
            let third = e.next_after(second, CronTz::Local).unwrap();
            // Third fire must NOT be 06:30 UTC (the duplicated 01:30 EST)
            // — it must skip past to Nov 3 01:30 EST = 06:30 UTC.
            assert_eq!(third, utc_ms(2025, 11, 3, 6, 30, 0));
        });
    }

    // ------- run_id shape -------

    #[test]
    fn cron_run_id_shape() {
        assert_eq!(
            cron_run_id("nightly", 0x1_2345_6789),
            "run:nightly:123456789"
        );
    }
}
