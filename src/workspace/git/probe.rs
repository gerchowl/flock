//! Bounded git probes. Optional-ref absence is explicit, never a catch-all error.
use crate::health::GitRefreshErrorKind as Error;
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// Existing per-command ceiling, also clamped to the owned round's deadline.
pub(super) const GIT_EXEC_TIMEOUT: Duration = Duration::from_secs(10);

/// Capture nonempty UTF-8 within both deadlines. Only the explicitly supplied
/// optional-reference exit code is absence, rather than a failed command.
pub(super) fn text(
    cwd: &Path,
    args: &[&str],
    absent: Option<i32>,
    deadline: Instant,
) -> Result<Option<String>, Error> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Error::Timeout);
    }
    let output = crate::process::TracedCommand::new("git", "git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output_traced_with_timeout(remaining.min(GIT_EXEC_TIMEOUT))
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::TimedOut {
                Error::Timeout
            } else {
                Error::GitIo
            }
        })?;
    if !output.status.success() {
        return if absent.is_some() && output.status.code() == absent {
            Ok(None)
        } else {
            Err(Error::GitExit)
        };
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| Error::GitOutput)?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(Error::GitOutput);
    }
    Ok(Some(trimmed.to_owned()))
}

/// `symbolic-ref --quiet` exits 1 for a detached HEAD. `rev-parse --verify`
/// exits 128 for an absent optional ref (including an unborn branch).
pub(super) fn revision(
    cwd: &Path,
    revision: &str,
    deadline: Instant,
) -> Result<Option<String>, Error> {
    let oid = text(
        cwd,
        &["rev-parse", "--verify", revision],
        Some(128),
        deadline,
    )?;
    if oid
        .as_ref()
        .is_some_and(|s| !matches!(s.len(), 40 | 64) || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Error::GitOutput);
    }
    Ok(oid)
}
