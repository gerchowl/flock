//! Delegate screen evidence and silence timing, independent of sockets and S1.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub(super) const DEFAULT_SILENCE_MS: u64 = 180_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Verdict {
    pub verdict: String,
    pub reason: String,
    pub retry_after_ms: Option<u64>,
    pub last_line: String,
}

pub(super) fn last_line(screen: &str) -> String {
    screen
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub(super) fn provider_limit(screen: &str) -> Option<Verdict> {
    // The provider status row needs BOTH the error and retry controls. A quoted
    // error in the agent's transcript is not evidence of a provider wait.
    let line = screen.lines().rev().take(40).find(|line| {
        let lower = line.to_lowercase();
        (lower.contains("usage exceeded") || lower.contains("rate limit"))
            && lower.contains("retrying in ")
            && lower.contains("attempt #")
            && lower.contains("esc interrupt")
    })?;
    let lower = line.to_lowercase();
    let eta = lower
        .split_once("retrying in ")?
        .1
        .split_once(" attempt #")?
        .0;
    Some(Verdict {
        verdict: "provider_limit".into(),
        reason: "provider_limit".into(),
        retry_after_ms: parse_duration(eta).ok(),
        last_line: last_line(screen),
    })
}

pub(super) fn parse_duration(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value == "0" {
        return Ok(0);
    }
    let mut total = 0_u64;
    let mut rest = value;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let count = rest.bytes().take_while(u8::is_ascii_digit).count();
        if count == 0 {
            return Err(format!("invalid duration: {value}"));
        }
        let number = rest[..count]
            .parse::<u64>()
            .map_err(|_| format!("invalid duration: {value}"))?;
        rest = &rest[count..];
        let (unit, factor) = if rest.starts_with("ms") {
            (2, 1)
        } else if rest.starts_with('s') {
            (1, 1_000)
        } else if rest.starts_with('m') {
            (1, 60_000)
        } else if rest.starts_with('h') {
            (1, 3_600_000)
        } else {
            return Err(format!("duration needs ms, s, m or h: {value}"));
        };
        total = number
            .checked_mul(factor)
            .and_then(|n| total.checked_add(n))
            .ok_or_else(|| format!("duration too large: {value}"))?;
        rest = &rest[unit..];
    }
    if value.is_empty() {
        return Err("duration is empty".into());
    }
    Ok(total)
}

pub(super) fn readiness(screen: &str, status: &str) -> Verdict {
    if let Some(mut verdict) = provider_limit(screen) {
        verdict.verdict = "stalled".into();
        return verdict;
    }
    let line = last_line(screen);
    let prompt = crate::detect::has_confirmation_prompt(&line.to_lowercase())
        || line.to_lowercase().contains("press enter")
        || line.to_lowercase().contains("enter confirm")
        || line.to_lowercase().contains("esc dismiss");
    let verdict = if status == "blocked" || prompt {
        "waiting_on_input"
    } else if status == "working" {
        "stalled"
    } else {
        "unknown"
    };
    Verdict {
        verdict: verdict.into(),
        reason: "ready_timeout".into(),
        retry_after_ms: None,
        last_line: line,
    }
}

pub(super) struct Monitor {
    silence: Option<Duration>,
    hash: Option<u64>,
    working: bool,
    changed: Instant,
    pub screen: String,
    pub latest: Option<Verdict>,
}

impl Monitor {
    pub fn new(silence_ms: u64, now: Instant) -> Self {
        Self {
            silence: (silence_ms != 0).then(|| Duration::from_millis(silence_ms)),
            hash: None,
            working: false,
            changed: now,
            screen: String::new(),
            latest: None,
        }
    }

    pub fn interrupted(&mut self, now: Instant) {
        self.hash = None;
        self.working = false;
        self.changed = now;
    }

    pub fn observe(&mut self, status: &str, screen: &str, now: Instant) -> bool {
        self.screen = screen.to_string();
        if let Some(verdict) = provider_limit(screen) {
            self.latest = Some(verdict);
            return true;
        }
        let hash = screen
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        if status != "working" || !self.working || self.hash != Some(hash) {
            self.changed = now;
        }
        self.hash = Some(hash);
        self.working = status == "working";
        if status == "working"
            && self
                .silence
                .is_some_and(|silence| now.saturating_duration_since(self.changed) >= silence)
        {
            self.latest = Some(Verdict {
                verdict: "stalled".into(),
                reason: "silence".into(),
                retry_after_ms: None,
                last_line: last_line(screen),
            });
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_silence_requires_unchanged_working_screen() {
        let now = Instant::now();
        let mut monitor = Monitor::new(100, now);
        assert!(!monitor.observe("working", "tool running", now));
        assert!(!monitor.observe("working", "tool running", now + Duration::from_millis(99)));
        assert!(monitor.observe("working", "tool running", now + Duration::from_millis(100)));
        assert_eq!(monitor.latest.unwrap().reason, "silence");
        let mut disabled = Monitor::new(0, now);
        assert!(!disabled.observe("working", "tool", now));
        assert!(!disabled.observe("working", "tool", now + Duration::from_secs(1000)));
    }

    #[test]
    fn delegate_provider_limit_has_retry_eta_despite_spinner_progress() {
        let screen = "■■⬝⬝⬝⬝⬝⬝ Free usage exceeded, subscribe to Go [retrying in 46m 15s attempt #1] esc interrupt";
        let verdict = provider_limit(screen).unwrap();
        assert_eq!(verdict.retry_after_ms, Some(2_775_000));
        assert_eq!(verdict.verdict, "provider_limit");
        assert!(provider_limit(
            "The transcript mentions usage exceeded and retrying in 46m attempt #1"
        )
        .is_none());
    }

    #[test]
    fn delegate_readiness_names_prompt_and_last_non_blank_line() {
        let verdict = readiness("details\nDo you want to proceed? yes\n\n", "unknown");
        assert_eq!(verdict.verdict, "waiting_on_input");
        assert_eq!(verdict.last_line, "Do you want to proceed? yes");
        assert_eq!(readiness("still running", "working").verdict, "stalled");
        assert_eq!(readiness("booting", "unknown").verdict, "unknown");
    }

    #[test]
    fn delegate_progress_and_transport_gap_reset_silence() {
        let now = Instant::now();
        let mut monitor = Monitor::new(100, now);
        assert!(!monitor.observe("working", "one", now));
        assert!(!monitor.observe("working", "two", now + Duration::from_millis(99)));
        monitor.interrupted(now + Duration::from_secs(1));
        assert!(!monitor.observe("working", "two", now + Duration::from_secs(2)));
        assert!(!monitor.observe("idle", "two", now + Duration::from_secs(3)));
    }
}
