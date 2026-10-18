//! Explicit live/restart policy for every Config field (ADR-0002, #2043).
use super::Config;
use serde::{Deserialize, Serialize};

/// The outcome of reloading one changed or invalid configuration section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionStatus {
    /// Accepted for subsequent operations; existing sessions retain admitted policy.
    Applied,
    /// Startup-owned consumers still require a restart to adopt the whole section.
    RestartRequired,
    /// Validation refused this section and retained its running policy.
    Invalid,
}
/// Section-level outcome; values, credentials and paths are never echoed here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SectionReload {
    /// Top-level configuration field or table name.
    pub section: String,
    /// Typed application/refusal outcome.
    pub status: SectionStatus,
}
const LIVE: &[&str] = &[
    "advanced",
    "channels",
    "events",
    "experimental",
    "fleet",
    "gossip",
    "keys",
    "onboarding",
    "peers",
    "questions",
    "remote",
    "session",
    "slots",
    "structured",
    "terminal",
    "theme",
    "ui",
    "update",
    "worktrees",
];
// These policies retain startup-owned consumers (notably UID, worker storage,
// listener and sampler configuration). Reporting them is part of ADR-0002.
const RESTART: &[&str] = &[
    "checks",
    "client",
    "gpu_command",
    "icon",
    "msg",
    "name",
    "security",
    "sessions",
    "spawn",
    "systemone",
    "thermal_command",
    "title",
    "web",
];
/// Plan reporting before effects, failing closed if the classification drifts.
pub(crate) fn plan(
    current: &Config,
    startup: &Config,
    incoming: &Config,
    invalid: &[String],
) -> Result<Vec<SectionReload>, &'static str> {
    let current = serde_json::to_value(current).map_err(|_| "config reload comparison failed")?;
    let startup = serde_json::to_value(startup).map_err(|_| "config reload comparison failed")?;
    let incoming = serde_json::to_value(incoming).map_err(|_| "config reload comparison failed")?;
    let fields = incoming
        .as_object()
        .ok_or("config reload comparison failed")?;
    let mut sections = Vec::new();
    for (section, value) in fields {
        let live = LIVE.contains(&section.as_str());
        if !live && !RESTART.contains(&section.as_str()) {
            return Err("config section lacks a reload classification");
        }
        let rejected = invalid.contains(section);
        let baseline = if live { &current } else { &startup };
        if baseline.get(section) != Some(value) || rejected {
            sections.push(SectionReload {
                section: section.clone(),
                status: if rejected {
                    SectionStatus::Invalid
                } else if live {
                    SectionStatus::Applied
                } else {
                    SectionStatus::RestartRequired
                },
            });
        }
    }
    Ok(sections)
}

#[cfg(test)]
mod tests;
