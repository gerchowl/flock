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
    let output = run(cwd, args, deadline)?;
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

/// One bounded subprocess, shared by output probes and quiet database validation.
fn run(cwd: &Path, args: &[&str], deadline: Instant) -> Result<std::process::Output, Error> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Error::Timeout);
    }
    crate::process::TracedCommand::new("git", "git")
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
        })
}

/// Quiet verification exits 1 for absence, while fatal exit codes remain errors.
/// On absence, validate the ref database too: quiet verification also hides a
/// malformed loose ref. Both commands share the original deadline.
pub(super) fn revision(
    cwd: &Path,
    revision: &str,
    deadline: Instant,
) -> Result<Option<String>, Error> {
    let oid = text(
        cwd,
        &["rev-parse", "--verify", "--quiet", revision],
        Some(1),
        deadline,
    )?;
    if oid.is_none() {
        // show-ref exits 1 for an empty database, but fails on corrupt refs.
        let refs = run(cwd, &["show-ref", "--quiet"], deadline)?;
        if !refs.status.success() && refs.status.code() != Some(1) {
            return Err(Error::GitExit);
        }
        // Reftable readers can present an unreadable table stack as empty.
        // Even an unborn repository must retain a symbolic HEAD. A detached
        // repository instead needs a resolvable HEAD identity.
        if text(cwd, &["symbolic-ref", "--quiet", "HEAD"], Some(1), deadline)?.is_none()
            && text(
                cwd,
                &["rev-parse", "--verify", "--quiet", "HEAD"],
                Some(1),
                deadline,
            )?
            .is_none()
        {
            return Err(Error::GitExit);
        }
    }
    if oid
        .as_ref()
        .is_some_and(|s| !matches!(s.len(), 40 | 64) || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Error::GitOutput);
    }
    Ok(oid)
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
