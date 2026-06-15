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

/// Like [`plan`], but for branching: the new pane should fork the
/// conversation instead of taking over the original session. Claude has a
/// dedicated `--fork-session` flag; other agents fall back to a plain
/// resume of the same session.
pub fn branch_plan(
    source: &str,
    agent: &str,
    session_ref: &AgentSessionRef,
) -> Option<AgentResumePlan> {
    let mut plan = plan(source, agent, session_ref)?;
    if source == "flock:claude" {
        plan.argv.push("--fork-session".into());
    }
    Some(plan)
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

        // Non-claude (codex): no positional prompt appended even if asked.
        let mut codex = branch_plan("flock:codex", "codex", &session).unwrap();
        let before = codex.argv.clone();
        append_pivot_message(&mut codex, "PIVOT now");
        assert_eq!(codex.argv, before);
    }

    #[test]
    fn branch_plan_non_claude_agents_fall_back_to_plain_resume() {
        let session = AgentSessionRef::id("codex-session").unwrap();
        let plan = branch_plan("flock:codex", "codex", &session).unwrap();
        assert_eq!(plan.argv, vec!["codex", "resume", "codex-session"]);
    }

    #[test]
    fn branch_plan_rejects_unofficial_sources() {
        let session = AgentSessionRef::id("claude-session").unwrap();
        assert!(branch_plan("tmux:claude", "claude", &session).is_none());
    }
}
