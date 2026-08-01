use std::path::Path;

use serde::{Deserialize, Serialize};

const MAX_SESSION_ID_LEN: usize = 512;
const MAX_SESSION_PATH_LEN: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionRef {
    pub kind: AgentSessionRefKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionRefKind {
    Id,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumePlan {
    pub agent: String,
    pub argv: Vec<String>,
    pub dedupe_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAgentSession {
    pub source: String,
    pub agent: String,
    pub session_ref: AgentSessionRef,
}

impl AgentSessionRef {
    pub fn id(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_session_id(&value).then_some(Self {
            kind: AgentSessionRefKind::Id,
            value,
        })
    }

    pub fn path(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_session_path(&value).then_some(Self {
            kind: AgentSessionRefKind::Path,
            value,
        })
    }
}

pub fn session_ref_from_report(
    source: &str,
    agent: &str,
    agent_session_id: Option<String>,
    _agent_session_path: Option<String>,
) -> Option<AgentSessionRef> {
    if !is_official_agent_source(source, agent) {
        return None;
    }

    if agent == "pi" {
        return _agent_session_path
            .and_then(AgentSessionRef::path)
            .or_else(|| agent_session_id.and_then(AgentSessionRef::id));
    }

    agent_session_id.and_then(AgentSessionRef::id)
}

/// Normalize the optional `session_start_source` field reported by the Claude
/// Code hook on `SessionStart`. Claude reports `startup`, `resume`, `clear`,
/// or `compact` — anything else is treated as absent so we don't trust an
/// unrecognized value.
pub fn normalize_claude_session_start_source(value: Option<String>) -> Option<String> {
    match value.as_deref().map(str::trim) {
        Some(source @ ("startup" | "resume" | "clear" | "compact")) => Some(source.to_string()),
        _ => None,
    }
}

pub fn is_reserved_native_state_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("flock:claude", "claude") | ("flock:codex", "codex") | ("flock:opencode", "opencode")
    )
}

pub fn session_ref_from_snapshot(
    source: &str,
    agent: &str,
    kind: AgentSessionRefKind,
    value: &str,
) -> Option<PersistedAgentSession> {
    if !is_official_agent_source(source, agent) {
        return None;
    }
    let session_ref = match (agent, kind) {
        ("pi", AgentSessionRefKind::Path) => AgentSessionRef::path(value)?,
        (_, AgentSessionRefKind::Id) => AgentSessionRef::id(value)?,
        _ => return None,
    };
    Some(PersistedAgentSession {
        source: source.to_string(),
        agent: agent.to_string(),
        session_ref,
    })
}

pub fn plan(source: &str, agent: &str, session_ref: &AgentSessionRef) -> Option<AgentResumePlan> {
    if !is_official_agent_source(source, agent) {
        return None;
    }

    let argv = match (source, agent, session_ref.kind) {
        ("flock:claude", "claude", AgentSessionRefKind::Id) => {
            vec![
                "claude".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("flock:codex", "codex", AgentSessionRefKind::Id) => {
            vec!["codex".into(), "resume".into(), session_ref.value.clone()]
        }
        ("flock:copilot", "copilot", AgentSessionRefKind::Id) => {
            vec!["copilot".into(), format!("--resume={}", session_ref.value)]
        }
        ("flock:pi", "pi", AgentSessionRefKind::Path | AgentSessionRefKind::Id) => {
            vec!["pi".into(), "--session".into(), session_ref.value.clone()]
        }
        ("flock:hermes", "hermes", AgentSessionRefKind::Id) => {
            vec![
                "hermes".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("flock:opencode", "opencode", AgentSessionRefKind::Id) => {
            vec![
                "opencode".into(),
                "--session".into(),
                session_ref.value.clone(),
            ]
        }
        _ => return None,
    };

    Some(AgentResumePlan {
        agent: agent.to_string(),
        argv,
        dedupe_key: dedupe_key(source, agent, session_ref),
    })
}

/// Why a session cannot be branched into a fork (#175 F2). Distinguishes
/// "there is nothing to resume" from "resuming would be dangerous": a plain
/// resume of a fork target puts two live processes on one session id, so the
/// old silent plain-resume fallback is now a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchUnsupported {
    /// The (source, agent) pair is not an official resumable integration, or
    /// the session ref shape is wrong for the agent — no resume plan exists.
    NotResumable { source: String, agent: String },
    /// The agent resumes sessions but its CLI has no conversation-fork
    /// affordance; forking would double-attach the same session id.
    ForkUnsupported { agent: String },
}

/// Like [`plan`], but for branching: the new pane should fork the
/// conversation instead of taking over the original session. Only Claude has
/// a fork affordance (`--fork-session`); every other agent is refused with a
/// typed reason instead of silently degrading to a plain resume (#175 F2).
pub fn branch_plan(
    source: &str,
    agent: &str,
    session_ref: &AgentSessionRef,
) -> Result<AgentResumePlan, BranchUnsupported> {
    let Some(mut plan) = plan(source, agent, session_ref) else {
        return Err(BranchUnsupported::NotResumable {
            source: source.to_string(),
            agent: agent.to_string(),
        });
    };
    if source == "flock:claude" {
        plan.argv.push("--fork-session".into());
        Ok(plan)
    } else {
        Err(BranchUnsupported::ForkUnsupported {
            agent: agent.to_string(),
        })
    }
}

/// Locate the on-disk transcript for a Claude session id (#178). Claude Code
/// stores transcripts per project under
/// `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`; session ids are
/// UUIDs, unique across projects, so a scan of the project directories keyed
/// on the file name is the robust lookup — the cwd encoding is
/// claude-internal and not worth replicating. A missing or empty transcript
/// means `claude --resume` would print "No conversation found" and exit,
/// leaving a dead pane, so fork callers refuse up front.
pub fn claude_transcript_path(home: &Path, session_id: &str) -> Option<std::path::PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || !valid_session_id(session_id) {
        return None;
    }
    let projects = home.join(".claude").join("projects");
    let entries = std::fs::read_dir(projects).ok()?;
    let file_name = format!("{session_id}.jsonl");
    for entry in entries.flatten() {
        let candidate = entry.path().join(&file_name);
        if candidate
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
        {
            return Some(candidate);
        }
    }
    None
}

/// The session id a Claude fork plan resumes, if `plan` is one (#178).
pub fn claude_fork_session_id(plan: &AgentResumePlan) -> Option<&str> {
    let is_claude_fork = plan.argv.first().map(String::as_str) == Some("claude")
        && plan.argv.iter().any(|arg| arg == "--fork-session");
    if !is_claude_fork {
        return None;
    }
    let resume = plan.argv.iter().position(|arg| arg == "--resume")?;
    plan.argv.get(resume + 1).map(String::as_str)
}

/// Append a one-shot pivot prompt as the forked agent's first turn (#106).
/// Only applies to a CLAUDE fork (argv starts with `claude` and carries
/// `--fork-session`); Claude takes a positional prompt as the opening user
/// turn in interactive mode. A no-op for an empty message or any other agent
/// (codex/copilot resume take no positional prompt). The argv is built once
/// per branch and never persisted, so later resumes re-inject nothing.
pub fn append_pivot_message(plan: &mut AgentResumePlan, message: &str) {
    if message.is_empty() {
        return;
    }
    let is_claude_fork = plan.argv.first().map(String::as_str) == Some("claude")
        && plan.argv.iter().any(|a| a == "--fork-session");
    if is_claude_fork {
        plan.argv.push(message.to_string());
    }
}

pub fn dedupe_key(source: &str, agent: &str, session_ref: &AgentSessionRef) -> String {
    format!(
        "{source}\u{0}{agent}\u{0}{:?}\u{0}{}",
        session_ref.kind, session_ref.value
    )
}

fn is_official_agent_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("flock:claude", "claude")
            | ("flock:codex", "codex")
            | ("flock:copilot", "copilot")
            | ("flock:pi", "pi")
            | ("flock:hermes", "hermes")
            | ("flock:opencode", "opencode")
    )
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_SESSION_ID_LEN && !value.chars().any(char::is_control)
}

fn valid_session_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_PATH_LEN
        && !value.chars().any(char::is_control)
        && Path::new(value).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_allows_supported_agents() {
        assert_eq!(
            plan(
                "flock:claude",
                "claude",
                &AgentSessionRef::id("claude-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["claude", "--resume", "claude-session"]
        );
        assert_eq!(
            plan(
                "flock:codex",
                "codex",
                &AgentSessionRef::id("codex-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["codex", "resume", "codex-session"]
        );
        assert_eq!(
            plan(
                "flock:copilot",
                "copilot",
                &AgentSessionRef::id("copilot-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["copilot", "--resume=copilot-session"]
        );
        assert_eq!(
            plan(
                "flock:pi",
                "pi",
                &AgentSessionRef::path("/tmp/pi-session.jsonl").unwrap()
            )
            .unwrap()
            .argv,
            vec!["pi", "--session", "/tmp/pi-session.jsonl"]
        );
        assert_eq!(
            plan(
                "flock:hermes",
                "hermes",
                &AgentSessionRef::id("hermes-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["hermes", "--resume", "hermes-session"]
        );
        assert_eq!(
            plan(
                "flock:opencode",
                "opencode",
                &AgentSessionRef::id("opencode-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["opencode", "--session", "opencode-session"]
        );
    }

    #[test]
    fn planner_rejects_custom_and_unsupported_path_refs() {
        assert!(plan(
            "custom:claude",
            "claude",
            &AgentSessionRef::id("session").unwrap()
        )
        .is_none());
        assert!(plan(
            "flock:claude",
            "claude",
            &AgentSessionRef::path("/tmp/claude-session").unwrap()
        )
        .is_none());
    }

    #[test]
    fn report_ref_prefers_pi_path_and_validates_values() {
        let session_ref = session_ref_from_report(
            "flock:pi",
            "pi",
            Some("pi-id".into()),
            Some("/tmp/pi-session.jsonl".into()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Path);
        assert_eq!(session_ref.value, "/tmp/pi-session.jsonl");

        assert!(session_ref_from_report("flock:pi", "pi", Some("bad\nid".into()), None).is_none());
        assert!(
            session_ref_from_report("flock:pi", "pi", None, Some("relative.jsonl".into()))
                .is_none()
        );
        assert!(session_ref_from_report("custom:pi", "pi", Some("pi-id".into()), None).is_none());
        assert!(session_ref_from_report(
            "flock:claude",
            "claude",
            None,
            Some("/tmp/claude-session".into())
        )
        .is_none());

        let session_ref =
            session_ref_from_report("flock:copilot", "copilot", Some("copilot-id".into()), None)
                .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "copilot-id");
        assert!(session_ref_from_report(
            "flock:copilot",
            "copilot",
            None,
            Some("/tmp/copilot-session".into())
        )
        .is_none());
    }

    #[test]
    fn ids_are_data_not_shell_text() {
        let id = "abc; rm -rf /";
        let codex_plan = plan("flock:codex", "codex", &AgentSessionRef::id(id).unwrap()).unwrap();
        assert_eq!(codex_plan.argv, vec!["codex", "resume", id]);

        let copilot_plan = plan(
            "flock:copilot",
            "copilot",
            &AgentSessionRef::id(id).unwrap(),
        )
        .unwrap();
        assert_eq!(copilot_plan.argv, vec!["copilot", "--resume=abc; rm -rf /"]);
    }

    #[test]
    fn planner_rejects_path_refs_for_id_only_agents() {
        assert!(plan(
            "flock:hermes",
            "hermes",
            &AgentSessionRef::path("/tmp/hermes-session").unwrap()
        )
        .is_none());
        assert!(plan(
            "flock:opencode",
            "opencode",
            &AgentSessionRef::path("/tmp/opencode-session").unwrap()
        )
        .is_none());
        assert!(plan(
            "flock:copilot",
            "copilot",
            &AgentSessionRef::path("/tmp/copilot-session").unwrap()
        )
        .is_none());
        assert!(session_ref_from_snapshot(
            "flock:hermes",
            "hermes",
            AgentSessionRefKind::Id,
            "hermes-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "flock:opencode",
            "opencode",
            AgentSessionRefKind::Id,
            "opencode-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "flock:copilot",
            "copilot",
            AgentSessionRefKind::Id,
            "copilot-session"
        )
        .is_some());
    }
    #[test]
    fn branch_plan_claude_appends_fork_session_flag() {
        let session = AgentSessionRef::id("claude-session").unwrap();
        let plan = branch_plan("flock:claude", "claude", &session).unwrap();
        assert_eq!(
            plan.argv,
            vec!["claude", "--resume", "claude-session", "--fork-session"]
        );
    }

    #[test]
    fn append_pivot_message_pushes_only_for_claude_forks() {
        let session = AgentSessionRef::id("sid").unwrap();
        let mut claude = branch_plan("flock:claude", "claude", &session).unwrap();
        append_pivot_message(&mut claude, "PIVOT now");
        assert_eq!(claude.argv.last().unwrap(), "PIVOT now");

        // Empty message: no-op.
        let mut claude2 = branch_plan("flock:claude", "claude", &session).unwrap();
        append_pivot_message(&mut claude2, "");
        assert_eq!(claude2.argv.last().unwrap(), "--fork-session");

        // Non-claude (codex): a plain-resume plan gets no positional prompt
        // appended even if asked.
        let mut codex = plan("flock:codex", "codex", &session).unwrap();
        let before = codex.argv.clone();
        append_pivot_message(&mut codex, "PIVOT now");
        assert_eq!(codex.argv, before);
    }

    #[test]
    fn branch_plan_refuses_non_claude_agents_instead_of_plain_resume() {
        // #175 F2: the old behavior silently returned a plain resume, which
        // races two processes on one session id. Every resumable non-Claude
        // agent must now be a typed refusal.
        for (source, agent, session_ref) in [
            ("flock:codex", "codex", AgentSessionRef::id("s").unwrap()),
            (
                "flock:copilot",
                "copilot",
                AgentSessionRef::id("s").unwrap(),
            ),
            (
                "flock:pi",
                "pi",
                AgentSessionRef::path("/tmp/s.jsonl").unwrap(),
            ),
            ("flock:hermes", "hermes", AgentSessionRef::id("s").unwrap()),
            (
                "flock:opencode",
                "opencode",
                AgentSessionRef::id("s").unwrap(),
            ),
        ] {
            assert_eq!(
                branch_plan(source, agent, &session_ref),
                Err(BranchUnsupported::ForkUnsupported {
                    agent: agent.to_string()
                }),
                "{agent} must refuse to fork"
            );
        }
    }

    #[test]
    fn branch_plan_classifies_unofficial_sources_as_not_resumable() {
        let session = AgentSessionRef::id("claude-session").unwrap();
        assert_eq!(
            branch_plan("tmux:claude", "claude", &session),
            Err(BranchUnsupported::NotResumable {
                source: "tmux:claude".into(),
                agent: "claude".into()
            })
        );
        // Agents flock detects but has no resume integration for (#175: the
        // "not 9" gap — omp/kimi/qodercli have no official resume source).
        for agent in ["omp", "kimi", "qodercli"] {
            assert_eq!(
                branch_plan(&format!("flock:{agent}"), agent, &session),
                Err(BranchUnsupported::NotResumable {
                    source: format!("flock:{agent}"),
                    agent: agent.into()
                }),
                "{agent} has no resume integration and must be not_resumable"
            );
        }
    }

    #[test]
    fn normalize_claude_session_start_source_keeps_known_values() {
        for source in ["startup", "resume", "clear", "compact"] {
            assert_eq!(
                normalize_claude_session_start_source(Some(source.into())),
                Some(source.into())
            );
        }
    }

    #[test]
    fn normalize_claude_session_start_source_trims_whitespace() {
        assert_eq!(
            normalize_claude_session_start_source(Some(" resume ".into())),
            Some("resume".into())
        );
    }

    #[test]
    fn claude_transcript_path_scans_project_dirs_and_rejects_bad_ids() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let home = std::env::temp_dir().join(format!("flock-home-{}-{nanos}", std::process::id()));
        let project = home.join(".claude/projects/-tmp-repo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("sess-1.jsonl"), "{}\n").unwrap();
        std::fs::write(project.join("empty.jsonl"), "").unwrap();

        assert_eq!(
            claude_transcript_path(&home, "sess-1"),
            Some(project.join("sess-1.jsonl"))
        );
        assert!(
            claude_transcript_path(&home, "empty").is_none(),
            "empty transcript is as dead as a missing one"
        );
        assert!(claude_transcript_path(&home, "missing").is_none());
        assert!(
            claude_transcript_path(&home, "../escape").is_none(),
            "path traversal in a session id must never resolve"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn claude_fork_session_id_extracts_only_from_fork_plans() {
        let session = AgentSessionRef::id("sid-9").unwrap();
        let fork = branch_plan("flock:claude", "claude", &session).unwrap();
        assert_eq!(claude_fork_session_id(&fork), Some("sid-9"));
        let plain = plan("flock:claude", "claude", &session).unwrap();
        assert_eq!(
            claude_fork_session_id(&plain),
            None,
            "plain resume is not a fork"
        );
        let codex = plan("flock:codex", "codex", &session).unwrap();
        assert_eq!(claude_fork_session_id(&codex), None);
    }

    #[test]
    fn normalize_claude_session_start_source_rejects_unknown_or_missing() {
        assert_eq!(
            normalize_claude_session_start_source(Some("bogus".into())),
            None
        );
        assert_eq!(
            normalize_claude_session_start_source(Some(String::new())),
            None
        );
        assert_eq!(normalize_claude_session_start_source(None), None);
    }
}
