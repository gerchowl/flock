//! Report composition (#233).
//!
//! One seam for "turn what the reporter knows plus what the binary knows into
//! a filable report". [`compose::compose`] is pure; the impure edges are
//! [`schema::ReportProvenance::collect`] (reads status), [`collect_diagnostics`]
//! (reads log files) and the CLI adapter (clipboard, browser, stdout). Keeping
//! them apart is what makes the redaction rules testable, and what stops a
//! second entry point from resolving provenance its own way.

pub mod compose;
pub mod redact;
pub mod schema;
pub mod template;
pub mod url;

/// Read the session's log files and return redacted diagnostic records.
///
/// Reads the same fixed allowlist of files as `crate::logging`'s tail, from
/// `crate::session::data_dir()` — both the client and the server write there,
/// so this works with the server dead, which is exactly when someone files a
/// bug.
pub fn collect_diagnostics(max_records: usize) -> Vec<redact::DiagnosticRecord> {
    const LOG_FILES: [&str; 4] = [
        "flock.log",
        "flock-server.log",
        "flock-client.log",
        "flock-relay.log",
    ];

    let scrubber = redact::Scrubber::from_env();
    let dir = crate::session::data_dir();
    let mut records = Vec::new();
    for file in LOG_FILES {
        if let Ok(content) = std::fs::read_to_string(dir.join(file)) {
            records.extend(redact::extract(&content, file, &scrubber));
        }
    }
    records.sort_by_key(|record| timestamp_key(&record.ts));
    let start = records.len().saturating_sub(max_records);
    records.drain(..start);
    records
}

/// Sort key for an RFC3339 UTC timestamp, tolerant of differing sub-second
/// precision.
///
/// A plain byte compare is what `crate::logging::merge_log_records` uses, and it
/// is only chronological while every record carries the SAME precision: `.`
/// (0x2E) sorts before `Z` (0x5A), so `…:12Z` compares as LATER than
/// `…:12.7Z` despite being earlier. One subscriber config makes that safe
/// today, but reports merge four files including rotated generations written by
/// older builds, so the whole-second form does turn up. Splitting the fraction
/// out and padding it to a fixed width removes the dependency on precision
/// being uniform.
fn timestamp_key(ts: &str) -> (String, String) {
    let trimmed = ts.trim_end_matches('Z');
    match trimmed.split_once('.') {
        Some((whole, fraction)) => {
            let mut padded = fraction.to_string();
            padded.truncate(9);
            while padded.len() < 9 {
                padded.push('0');
            }
            (whole.to_string(), padded)
        }
        None => (trimmed.to_string(), "0".repeat(9)),
    }
}

/// Keep only records at WARN or ERROR — the default for a bug report, where an
/// INFO-level poll loop is noise that crowds out the failure.
pub fn only_problems(records: Vec<redact::DiagnosticRecord>) -> Vec<redact::DiagnosticRecord> {
    records
        .into_iter()
        .filter(|record| matches!(record.level.as_str(), "WARN" | "ERROR"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_second_and_fractional_timestamps_order_chronologically() {
        // The byte compare this replaced put the fractional stamp FIRST,
        // because '.' sorts before 'Z'.
        assert!(timestamp_key("2026-08-06T13:05:12Z") < timestamp_key("2026-08-06T13:05:12.7Z"));
        assert!(timestamp_key("2026-08-06T13:05:12.7Z") < timestamp_key("2026-08-06T13:05:13Z"));
        assert!(
            timestamp_key("2026-08-06T13:05:12.000001Z") < timestamp_key("2026-08-06T13:05:12.1Z")
        );
    }

    #[test]
    fn only_problems_keeps_warnings_and_errors() {
        let record = |level: &str| redact::DiagnosticRecord {
            ts: "t".into(),
            level: level.into(),
            source: None,
            fields: vec![("event".into(), "e".into())],
        };
        let kept = only_problems(vec![
            record("INFO"),
            record("WARN"),
            record("ERROR"),
            record("DEBUG"),
        ]);
        assert_eq!(kept.len(), 2);
    }
}
