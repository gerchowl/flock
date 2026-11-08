//! Exact session-store evidence shared by delegate recovery and submission.
use crate::api::schema::AgentSessionInfo;
use std::path::PathBuf;
pub(crate) mod store;

pub(crate) fn checked_path(session: &AgentSessionInfo) -> std::io::Result<Option<PathBuf>> {
    if session.kind != crate::agent_resume::AgentSessionRefKind::Id {
        return Ok(None);
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(None);
    };
    match (session.agent.as_str(), session.source.as_str()) {
        ("codex", "flock:codex") => store::lookup(
            [std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"))
                .join("sessions")],
            store::Layout::Codex,
            &session.value,
        ),
        ("claude", "flock:claude") => {
            crate::agent_resume::claude_transcript_path_checked(&home, &session.value)
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup(agent: &str) -> (crate::test_support::TempDir, AgentSessionInfo, PathBuf) {
        let root = crate::test_support::TempDir::socket();
        let session = AgentSessionInfo {
            source: format!("flock:{agent}"),
            agent: agent.into(),
            kind: crate::agent_resume::AgentSessionRefKind::Id,
            value: "session-evidence".into(),
        };
        let sessions = root.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("rollout-fixture-session-evidence.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":session.value}})
            ),
        )
        .unwrap();
        (root, session, path)
    }

    #[test]
    fn recovery_rejects_wrong_metadata_and_duplicate_rollouts() {
        let (root, session, rollout) = setup("codex");
        std::env::set_var("CODEX_HOME", root.path());
        assert_eq!(checked_path(&session).unwrap(), Some(rollout.clone()));
        std::fs::write(
            &rollout,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"other\"}}\n",
        )
        .unwrap();
        assert!(checked_path(&session).unwrap().is_none());
        std::fs::write(
            &rollout,
            format!(
                "{}\n",
                json!({"type":"session_meta","payload":{"id":session.value}})
            ),
        )
        .unwrap();
        std::fs::copy(
            &rollout,
            root.path()
                .join("sessions/rollout-duplicate-session-evidence.jsonl"),
        )
        .unwrap();
        assert!(checked_path(&session).is_err());
        std::env::remove_var("CODEX_HOME");
    }
}

#[cfg(test)]
#[path = "session_evidence/store_tests.rs"]
mod store_tests;
