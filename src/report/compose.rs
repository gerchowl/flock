//! The pure composer (#233).
//!
//! Everything here is a function of its inputs: no socket calls, no file
//! reads, no subprocesses, no clock. The CLI adapter gathers the inputs and
//! decides what to do with the result; a future MCP or TUI entry point does the
//! same, so provenance and redaction are resolved in exactly one place rather
//! than once per surface.

use std::collections::BTreeMap;

use super::redact::{render_block, DiagnosticRecord};
use super::schema::ReportProvenance;
use super::template::{form, prose_fields, ReportKind};
use super::url::{self, PrefilledUrl};

/// Something the reporter should know before this becomes an issue. Advisory,
/// not fatal — GitHub's own form validation is the gate that actually blocks a
/// submit, and a CLI that refuses outright just teaches people to type "n/a".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Advisory {
    /// The reproduction reads as a placeholder. CONTRIBUTING.md asks for a
    /// discussion rather than an issue when there isn't one yet.
    ThinReproduction,
    /// The running server is an older build than this binary — `flk update
    /// --handoff` was never run. Worth re-checking before filing.
    StaleServer,
    /// Client and server disagree on the wire protocol.
    ProtocolMismatch,
}

impl Advisory {
    pub fn message(&self) -> &'static str {
        match self {
            Self::ThinReproduction => {
                "the reproduction looks like a placeholder — an issue without real \
                 steps is likely to be closed; CONTRIBUTING.md asks for a discussion \
                 instead when you don't have one yet"
            }
            Self::StaleServer => {
                "the running server is an older build than this binary — run \
                 `flk update --handoff` and re-check before filing"
            }
            Self::ProtocolMismatch => {
                "client and server disagree on the wire protocol — restart the \
                 server and re-check before filing"
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReportInputs {
    pub kind: ReportKind,
    /// `owner/name`, already resolved and validated.
    pub repo: String,
    /// Field id -> prose, as written by the human.
    pub prose: BTreeMap<String, String>,
    pub provenance: ReportProvenance,
    /// Already redacted. `compose` does not scrub — that happens at extraction
    /// so unredacted values never reach this struct in the first place.
    pub diagnostics: Vec<DiagnosticRecord>,
}

#[derive(Debug, Clone)]
pub struct Composed {
    pub destination: String,
    pub url: PrefilledUrl,
    /// The block the reporter pastes into the issue body. Deliberately not in
    /// the URL — see `url::build`.
    pub diagnostics_block: Option<String>,
    /// Byte-for-byte what leaves the machine, for display before it does.
    pub preview: String,
    pub advisories: Vec<Advisory>,
}

pub fn compose(inputs: &ReportInputs) -> Composed {
    let form = form(inputs.kind);

    let mut values: Vec<(&str, String)> = url::prose_pairs(prose_fields(inputs.kind), |id| {
        inputs.prose.get(id).cloned()
    });

    // The environment field is machine-filled — that is the whole point of the
    // tool, and it is the field reporters most often leave thin.
    values.push(("environment", inputs.provenance.to_markdown()));

    let built = url::build(&inputs.repo, form, &values);

    let diagnostics_block = (!inputs.diagnostics.is_empty()).then(|| {
        format!(
            "<details><summary>flk diagnostics ({} records, redacted)</summary>\n\n{}\n</details>\n",
            inputs.diagnostics.len(),
            render_block(&inputs.diagnostics)
        )
    });

    Composed {
        destination: inputs.repo.clone(),
        preview: render_preview(inputs, &values, diagnostics_block.as_deref()),
        url: built,
        diagnostics_block,
        advisories: advisories(inputs),
    }
}

fn advisories(inputs: &ReportInputs) -> Vec<Advisory> {
    let mut out = Vec::new();
    if let Some(reproduction) = inputs.prose.get("reproduction") {
        if is_placeholder(reproduction) {
            out.push(Advisory::ThinReproduction);
        }
    }
    if inputs.provenance.server.restart_needed == Some(true) {
        out.push(Advisory::StaleServer);
    }
    if inputs.provenance.server.compatible == Some(false) {
        out.push(Advisory::ProtocolMismatch);
    }
    out
}

fn is_placeholder(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    let stripped = normalized.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    // Counted in chars, not bytes: a byte length would flag "aaa\u{1f600}aaa"
    // while letting four CJK characters through, and neither has anything to do
    // with whether a reproduction is real.
    //
    // The threshold only catches near-empty text. "git push" and "make test" are
    // complete reproductions, and an advisory that cries wolf on them stops
    // being read at all — which costs more than the placeholders it would catch.
    stripped.is_empty()
        || matches!(
            stripped,
            "na" | "n a" | "none" | "todo" | "tbd" | "unknown" | "see above" | "not sure"
        )
        || stripped.chars().count() < 4
}

fn render_preview(
    inputs: &ReportInputs,
    values: &[(&str, String)],
    diagnostics_block: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("destination: {}\n", inputs.repo));
    out.push_str(&format!("kind:        {}\n\n", inputs.kind.as_str()));
    for (id, value) in values {
        out.push_str(&format!("## {id}\n{}\n\n", value.trim_end()));
    }
    match diagnostics_block {
        Some(block) => {
            out.push_str("## diagnostics (clipboard, not in the URL)\n");
            out.push_str(block);
        }
        None => out.push_str(
            "## diagnostics\n(none collected — drop --no-diagnostics to attach a log tail)\n",
        ),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::schema::{
        ClientProvenance, HostProvenance, ServerProvenance, REPORT_SCHEMA_VERSION,
    };

    fn provenance(restart_needed: Option<bool>, compatible: Option<bool>) -> ReportProvenance {
        ReportProvenance {
            report_schema_version: REPORT_SCHEMA_VERSION,
            client: ClientProvenance {
                version: "0.6.8".into(),
                channel: "stable".into(),
                commit: Some("abc1234".into()),
                protocol: 24,
                binary: "/usr/local/bin/flk".into(),
                target: "x86_64-unknown-linux-gnu".into(),
                profile: "release".into(),
            },
            server: ServerProvenance {
                status: "running".into(),
                version: Some("0.6.7".into()),
                protocol: Some(24),
                compatible,
                restart_needed,
            },
            host: HostProvenance {
                os: "linux".into(),
                arch: "x86_64".into(),
                term: Some("xterm-256color".into()),
                term_program: None,
                shell: Some("zsh".into()),
            },
        }
    }

    fn inputs(reproduction: &str) -> ReportInputs {
        let mut prose = BTreeMap::new();
        prose.insert("current-behavior".into(), "worktree create fails".into());
        prose.insert("expected-behavior".into(), "it creates a worktree".into());
        prose.insert("reproduction".into(), reproduction.into());
        prose.insert("impact".into(), "cannot use worktrees at all".into());
        ReportInputs {
            kind: ReportKind::Bug,
            repo: "gerchowl/flock".into(),
            prose,
            provenance: provenance(None, None),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn environment_is_machine_filled_without_being_asked() {
        let composed = compose(&inputs("run flk worktree create --branch x"));
        assert!(composed.url.url.contains("&environment="));
        assert!(composed.preview.contains("0.6.8"));
        assert!(composed.preview.contains("abc1234"));
    }

    #[test]
    fn diagnostics_never_enter_the_url() {
        let mut inputs = inputs("run flk worktree create --branch x");
        inputs.diagnostics = vec![DiagnosticRecord {
            ts: "t".into(),
            level: "ERROR".into(),
            source: Some("flock-server.log".into()),
            // A value that cannot occur incidentally in the URL — "git" would
            // match `github.com` in the host and prove nothing.
            fields: vec![("program".into(), "zzdiagnosticzz".into())],
        }];
        let composed = compose(&inputs);
        assert!(
            !composed.url.url.contains("zzdiagnosticzz"),
            "{}",
            composed.url.url
        );
        assert!(composed
            .diagnostics_block
            .as_deref()
            .expect("block")
            .contains("program=zzdiagnosticzz"));
    }

    #[test]
    fn flags_a_placeholder_reproduction_without_blocking() {
        for placeholder in ["n/a", "  ", "TODO", "none", "-"] {
            let composed = compose(&inputs(placeholder));
            assert!(
                composed.advisories.contains(&Advisory::ThinReproduction),
                "{placeholder:?}"
            );
            // Still composed — the CLI warns, GitHub's required-field
            // validation is what actually gates the submit.
            assert!(!composed.url.url.is_empty());
        }
    }

    #[test]
    fn accepts_a_real_reproduction() {
        let composed = compose(&inputs("run `flk worktree create --branch x` in any repo"));
        assert!(!composed.advisories.contains(&Advisory::ThinReproduction));
    }

    #[test]
    fn short_but_genuine_reproductions_are_not_nagged() {
        for real in ["git push", "make test", "cargo run", "flk web"] {
            let composed = compose(&inputs(real));
            assert!(
                !composed.advisories.contains(&Advisory::ThinReproduction),
                "{real:?} was wrongly flagged"
            );
        }
    }

    #[test]
    fn surfaces_stale_server_and_protocol_skew() {
        let mut inputs = inputs("run `flk worktree create --branch x` in any repo");
        inputs.provenance = provenance(Some(true), Some(false));
        let composed = compose(&inputs);
        assert!(composed.advisories.contains(&Advisory::StaleServer));
        assert!(composed.advisories.contains(&Advisory::ProtocolMismatch));
    }

    #[test]
    fn preview_shows_the_destination() {
        let composed = compose(&inputs("run `flk worktree create --branch x` in any repo"));
        assert!(composed.preview.contains("destination: gerchowl/flock"));
    }
}
