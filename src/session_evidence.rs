//! Exact session-store evidence shared by delegate recovery and submission.
use crate::api::schema::AgentSessionInfo;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

pub(crate) fn path(session: &AgentSessionInfo) -> Option<PathBuf> {
    if session.kind != crate::agent_resume::AgentSessionRefKind::Id {
        return None;
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let path = match (session.agent.as_str(), session.source.as_str()) {
        ("codex", "flock:codex") => crate::codex_transcript::rollout_path(
            &std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            &session.value,
        )
        .ok()??,
        ("claude", "flock:claude") => {
            crate::agent_resume::claude_transcript_path(&home, &session.value)?
        }
        _ => return None,
    };
    let mut header = Vec::new();
    File::open(&path)
        .ok()?
        .take(65536)
        .read_to_end(&mut header)
        .ok()?;
    let matches = String::from_utf8_lossy(&header)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|row| {
            if session.agent == "codex" {
                row["type"] == "session_meta" && row["payload"]["id"] == session.value
            } else {
                row["sessionId"] == session.value
            }
        });
    matches.then_some(path)
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
        assert_eq!(path(&session), Some(rollout.clone()));
        std::fs::write(
            &rollout,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"other\"}}\n",
        )
        .unwrap();
        assert!(path(&session).is_none());
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
        assert!(path(&session).is_none());
        std::env::remove_var("CODEX_HOME");
    }
}
