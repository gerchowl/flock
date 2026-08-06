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
    // RFC3339 UTC at one fixed precision sorts correctly as bytes — the same
    // property `crate::logging::merge_log_records` relies on.
    records.sort_by(|a, b| a.ts.cmp(&b.ts));
    let start = records.len().saturating_sub(max_records);
    records.drain(..start);
    records
}

/// Keep only records at WARN or ERROR — the default for a bug report, where an
/// INFO-level poll loop is noise that crowds out the failure.
pub fn only_problems(records: Vec<redact::DiagnosticRecord>) -> Vec<redact::DiagnosticRecord> {
    records
        .into_iter()
        .filter(|record| matches!(record.level.as_str(), "WARN" | "ERROR"))
        .collect()
}
