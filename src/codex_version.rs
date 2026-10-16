//! Compatibility eligibility for the rollout binding in ADR-0028 §1a.
//!
//! Call the probe on a worker. It establishes version eligibility only; writer
//! attribution and lifecycle authority belong to #1662 and #1664.
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::process::TracedCommand;

pub(crate) const TESTED_VERSION: &str = "0.160.1";
const PROBE_DEADLINE: Duration = Duration::from_millis(5000);
const MAX_VERSION_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeFailure {
    UnresolvedExecutable,
    SpawnOrRead,
    Timeout,
    NonzeroExit,
    InvalidOutput,
}

/// Eligibility facts, never a turn-state decision or a successful binding.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Compatibility {
    pub metadata_version: Option<String>,
    pub writer_version: Option<String>,
    pub probe_failure: Option<ProbeFailure>,
}

impl Compatibility {
    pub fn supports_lifecycle(&self) -> bool {
        self.probe_failure.is_none()
            && self.metadata_version.as_deref() == Some(TESTED_VERSION)
            && self.writer_version.as_deref() == Some(TESTED_VERSION)
    }

    /// Later binding code selects existing screen evidence on this reason;
    /// unavailable version evidence must never manufacture an idle state.
    pub fn screen_fallback_reason(&self) -> Option<&'static str> {
        (!self.supports_lifecycle()).then_some("unsupported_codex_version")
    }
}

/// Probe exactly the launch-resolved executable once; never search PATH or retry.
/// The traced funnel shares the deadline across exit/stdout/stderr and kills
/// its dedicated process group if the child or an inherited pipe stays open.
pub(crate) fn probe(executable: &Path, metadata_version: Option<&str>) -> Compatibility {
    let result = if executable.is_absolute() {
        TracedCommand::new(executable, "codex-version")
            .arg("--version")
            .stdin(Stdio::null())
            .output_traced_with_timeout(PROBE_DEADLINE)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::TimedOut => ProbeFailure::Timeout,
                _ => ProbeFailure::SpawnOrRead,
            })
            .and_then(|output| {
                if !output.status.success() {
                    return Err(ProbeFailure::NonzeroExit);
                }
                parse_version(&output.stdout).ok_or(ProbeFailure::InvalidOutput)
            })
    } else {
        Err(ProbeFailure::UnresolvedExecutable)
    };
    let (writer_version, probe_failure) = match result {
        Ok(version) => (Some(version), None),
        Err(failure) => (None, Some(failure)),
    };
    Compatibility {
        metadata_version: metadata_version.map(str::to_owned),
        writer_version,
        probe_failure,
    }
}

/// Accept exactly `codex-cli MAJOR.MINOR.PATCH` and an optional LF or CRLF.
/// Components are canonical decimal (no leading zero except zero itself).
/// Prereleases, build suffixes, banners, extra lines and non-UTF-8 are missing
/// version evidence, rather than an inferred compatible release.
fn parse_version(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_VERSION_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let line = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    let version = line.strip_prefix("codex-cli ")?;
    let parts: Vec<_> = version.split('.').collect();
    (parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'))
        }))
    .then(|| version.to_owned())
}

#[cfg(test)]
mod tests;
