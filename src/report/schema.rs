//! The report payload's own versioned schema (#233).
//!
//! Deliberately NOT `src/cli/status.rs`'s `FullStatusJson`. That struct is a
//! private CLI output format and is free to change; a report block gets pasted
//! into a public tracker and read by triagers months later, which makes it a
//! de-facto public contract. So it gets the same treatment the crate already
//! gives its other cross-version payloads: an explicit version integer
//! (`crate::protocol::wire::PROTOCOL_VERSION`'s posture) and `#[serde(default)]`
//! on everything additive (`crate::logging::LogLine`'s posture, which doubles
//! as the `peers logs` SSH wire type).
//!
//! Bump [`REPORT_SCHEMA_VERSION`] when a field changes meaning or is removed.
//! Adding an optional field is backward-compatible and does not need a bump.

use serde::{Deserialize, Serialize};

use crate::cli::status::ServerRuntimeStatus;

/// Version of the provenance block's shape. See the module doc for the bump
/// rule.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Everything about the reporter's environment that the binary can determine
/// on its own. Nothing here is ever asked of the human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportProvenance {
    pub report_schema_version: u32,
    pub client: ClientProvenance,
    pub server: ServerProvenance,
    pub host: HostProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientProvenance {
    pub version: String,
    pub channel: String,
    /// The commit this binary was built from, when the build set it. On the
    /// preview/`latest` channels many revisions share one base version, so
    /// this is the field that actually answers "which code ran".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub protocol: u32,
    pub binary: String,
    /// Target triple and `debug`/`release`. Neither is derivable at runtime.
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerProvenance {
    /// `running` | `not_running`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatible: Option<bool>,
    /// The running server is a different build than the binary that composed
    /// this report — i.e. someone updated and never handed off. A top triage
    /// explanation on its own, so it rides in the provenance block rather
    /// than being re-derived by a reader comparing two version strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_needed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProvenance {
    pub os: String,
    pub arch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term_program: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

impl ReportProvenance {
    /// Build the block from live sources. The only impure function in this
    /// module — everything downstream takes the built value.
    pub fn collect(server: &ServerRuntimeStatus) -> Self {
        Self {
            report_schema_version: REPORT_SCHEMA_VERSION,
            client: ClientProvenance {
                version: crate::build_info::version(),
                channel: crate::build_info::channel().to_string(),
                commit: crate::build_info::commit().map(str::to_string),
                protocol: crate::protocol::PROTOCOL_VERSION,
                // Scrubbed AT COLLECTION, not at render: `current_exe_label`
                // returns an absolute path carrying the username, and this
                // struct exists to be published. Masking here means no future
                // caller can serialize the raw value by accident.
                binary: super::redact::Scrubber::from_env()
                    .scrub(&crate::cli::status::current_exe_label()),
                target: crate::build_info::target().to_string(),
                profile: crate::build_info::profile().to_string(),
            },
            server: ServerProvenance::from_runtime(server),
            host: HostProvenance::collect(),
        }
    }

    /// Render as the markdown the `environment` template field expects.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("- Flock version: {}\n", self.client.version));
        if let Some(commit) = &self.client.commit {
            out.push_str(&format!("- Build commit: {commit}\n"));
        }
        out.push_str(&format!("- Update channel: {}\n", self.client.channel));
        out.push_str(&format!(
            "- Operating system: {} ({})\n",
            self.host.os, self.host.arch
        ));
        // Absent values are STATED, not omitted.
        //
        // Borrowed from the `bugreport` crate, whose collectors turn a failure
        // into a report entry rather than dropping the section
        // (`collect(..).unwrap_or_else(|e| e.to_entry())`). A missing "Shell:"
        // line leaves a triager unable to tell "the reporter has no SHELL set"
        // — itself diagnostic, and the shape of a stripped systemd
        // environment — from "flock never looked".
        out.push_str(&format!(
            "- Terminal: {}\n",
            self.host
                .term_program
                .as_deref()
                .or(self.host.term.as_deref())
                .unwrap_or("not set")
        ));
        out.push_str(&format!(
            "- Shell: {}\n",
            self.host.shell.as_deref().unwrap_or("not set")
        ));
        out.push_str(&format!(
            "- Build: {} ({})\n",
            self.client.target, self.client.profile
        ));
        out.push_str(&format!("- Client protocol: {}\n", self.client.protocol));
        // #232 showed the resolved binary path is genuinely diagnostic — it is
        // how a Nix store path or an unexpected install location gets noticed.
        out.push_str(&format!("- Binary: {}\n", self.client.binary));
        out.push_str(&format!("- Server: {}", self.server.status));
        if let Some(version) = &self.server.version {
            out.push_str(&format!(" (version {version}"));
            match self.server.compatible {
                Some(true) => out.push_str(", protocol compatible"),
                Some(false) => out.push_str(", protocol INCOMPATIBLE"),
                None => {}
            }
            out.push(')');
        }
        out.push('\n');
        if self.server.restart_needed == Some(true) {
            out.push_str(
                "- NOTE: the running server is an older build than this binary \
                 (`flk update --handoff` not yet run) — worth re-checking after a handoff\n",
            );
        }
        out
    }
}

impl ServerProvenance {
    fn from_runtime(server: &ServerRuntimeStatus) -> Self {
        match server {
            ServerRuntimeStatus::Running {
                version, protocol, ..
            } => Self {
                status: "running".into(),
                version: version.clone(),
                protocol: *protocol,
                compatible: protocol.map(|value| value == crate::protocol::PROTOCOL_VERSION),
                restart_needed: crate::cli::status::restart_needed_bool(server),
            },
            // The degrade path, and the one that matters most: a dead server is
            // exactly when someone files a bug. Report composition must not
            // depend on the socket being answerable.
            ServerRuntimeStatus::NotRunning => Self {
                status: "not_running".into(),
                version: None,
                protocol: None,
                compatible: None,
                restart_needed: Some(false),
            },
        }
    }
}

impl HostProvenance {
    fn collect() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            term: std::env::var("TERM").ok().filter(|v| !v.is_empty()),
            term_program: std::env::var("TERM_PROGRAM").ok().filter(|v| !v.is_empty()),
            // Basename only: the full path is `/nix/store/<hash>-zsh-5.9/bin/zsh`
            // on this project's own dev machines, which is noise and leaks the
            // store layout.
            shell: std::env::var("SHELL").ok().and_then(|shell| {
                shell
                    .rsplit('/')
                    .next()
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> ReportProvenance {
        ReportProvenance {
            report_schema_version: REPORT_SCHEMA_VERSION,
            client: ClientProvenance {
                version: "0.6.8".into(),
                channel: "stable".into(),
                commit: Some("abc1234".into()),
                protocol: 24,
                binary: "/usr/local/bin/flk".into(),
                target: "aarch64-apple-darwin".into(),
                profile: "release".into(),
            },
            server: ServerProvenance {
                status: "running".into(),
                version: Some("0.6.7".into()),
                protocol: Some(24),
                compatible: Some(true),
                restart_needed: Some(true),
            },
            host: HostProvenance {
                os: "macos".into(),
                arch: "aarch64".into(),
                term: Some("xterm-256color".into()),
                term_program: Some("Alacritty".into()),
                shell: Some("zsh".into()),
            },
        }
    }

    #[test]
    fn binary_path_is_masked_before_it_can_be_published() {
        let collected = ReportProvenance::collect(&ServerRuntimeStatus::NotRunning);
        let home = std::env::var("HOME").unwrap_or_default();
        if home.len() > 1 {
            assert!(
                !collected.client.binary.contains(&home),
                "raw home path in provenance: {}",
                collected.client.binary
            );
        }
    }

    #[test]
    fn absent_values_are_stated_rather_than_omitted() {
        let mut provenance = provenance();
        provenance.host.shell = None;
        provenance.host.term = None;
        provenance.host.term_program = None;
        let out = provenance.to_markdown();
        // A missing line cannot be distinguished from a failed lookup.
        assert!(out.contains("- Shell: not set"), "{out}");
        assert!(out.contains("- Terminal: not set"), "{out}");
    }

    #[test]
    fn build_shape_is_reported() {
        let out = provenance().to_markdown();
        assert!(out.contains("aarch64-apple-darwin (release)"), "{out}");
    }

    #[test]
    fn markdown_carries_the_triage_fields() {
        let out = provenance().to_markdown();
        assert!(out.contains("0.6.8"), "{out}");
        assert!(out.contains("abc1234"), "{out}");
        assert!(out.contains("Alacritty"), "{out}");
        assert!(out.contains("macos (aarch64)"), "{out}");
    }

    #[test]
    fn stale_server_is_called_out_without_claiming_a_fix() {
        let out = provenance().to_markdown();
        assert!(out.contains("older build"), "{out}");
        // The tool cannot know whether a given bug is fixed; saying so would
        // train people to skip filing real regressions.
        assert!(!out.to_lowercase().contains("fixed"), "{out}");
    }

    #[test]
    fn dead_server_still_produces_a_block() {
        let provenance = ReportProvenance {
            server: ServerProvenance::from_runtime(&ServerRuntimeStatus::NotRunning),
            ..provenance()
        };
        let out = provenance.to_markdown();
        assert!(out.contains("not_running"), "{out}");
    }

    #[test]
    fn schema_version_is_serialized() {
        let json = serde_json::to_string(&provenance()).expect("serialize");
        assert!(json.contains("\"report_schema_version\":1"), "{json}");
    }
}
