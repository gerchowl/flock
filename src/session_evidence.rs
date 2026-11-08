//! Exact session-store evidence shared by delegate recovery and submission.
use crate::api::schema::AgentSessionInfo;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
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

#[derive(Debug, Clone)]
pub(crate) struct Submission {
    path: PathBuf,
    offset: u64,
    agent: String,
}
impl Submission {
    pub(crate) fn capture(session: &AgentSessionInfo) -> Option<Self> {
        let path = path(session)?;
        let offset = std::fs::metadata(&path).ok()?.len();
        Some(Self {
            path,
            offset,
            agent: session.agent.clone(),
        })
    }
    /// Record the transcript boundary immediately before the first Enter.
    pub(crate) fn at_current_end(&self) -> Option<Self> {
        Some(Self {
            offset: std::fs::metadata(&self.path).ok()?.len(),
            ..self.clone()
        })
    }

    pub(crate) fn started(&self, text: &str) -> bool {
        let Some(mut file) = File::open(&self.path).ok() else {
            return false;
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut started = self.agent == "claude";
        for line in BufReader::new(file.take(1024 * 1024))
            .lines()
            .map_while(Result::ok)
        {
            let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if self.agent == "codex"
                && row["type"] == "event_msg"
                && row["payload"]["type"] == "task_started"
            {
                started = true;
            }
            let message = if self.agent == "codex" && row["type"] == "response_item" {
                &row["payload"]
            } else if self.agent == "claude" && row["type"] == "user" && row["isMeta"] != true {
                &row["message"]
            } else {
                continue;
            };
            if message["role"] != "user" {
                continue;
            }
            let content = message["content"].as_str().map(str::to_string).or_else(|| {
                Some(
                    message["content"]
                        .as_array()?
                        .iter()
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            });
            if started && content.as_deref() == Some(text) {
                return true;
            }
        }
        false
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

    fn append(path: &std::path::Path, row: serde_json::Value) {
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{row}"
        )
        .unwrap();
    }

    fn prompt(text: &str) -> serde_json::Value {
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}})
    }

    #[test]
    fn codex_confirmation_requires_new_exact_prompt_and_task_start() {
        let (_root, _session, path) = setup("codex");
        append(
            &path,
            json!({"type":"event_msg","payload":{"type":"task_started"}}),
        );
        append(&path, prompt("hello"));
        let proof = Submission {
            offset: std::fs::metadata(&path).unwrap().len(),
            path: path.clone(),
            agent: "codex".into(),
        };
        assert!(
            !proof.started("hello"),
            "an older identical prompt proves nothing"
        );
        append(&path, prompt("hello"));
        assert!(
            !proof.started("hello"),
            "an echo without a turn start proves nothing"
        );
        append(
            &path,
            json!({"type":"event_msg","payload":{"type":"task_started"}}),
        );
        append(&path, prompt("different"));
        assert!(!proof.started("hello"));
        append(
            &path,
            json!({"type":"event_msg","payload":{"type":"task_started"}}),
        );
        append(&path, prompt("hello"));
        assert!(proof.started("hello"));
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
