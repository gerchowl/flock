//! Exact session-store evidence shared by delegate recovery and submission.
use crate::api::schema::AgentSessionInfo;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

pub(crate) fn checked_path(session: &AgentSessionInfo) -> std::io::Result<Option<PathBuf>> {
    if session.kind != crate::agent_resume::AgentSessionRefKind::Id {
        return Ok(None);
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(None);
    };
    let path = match (session.agent.as_str(), session.source.as_str()) {
        ("codex", "flock:codex") => crate::codex_transcript::rollout_path(
            &std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            &session.value,
        )
        .ok()
        .flatten(),
        ("claude", "flock:claude") => {
            crate::agent_resume::claude_transcript_path_checked(&home, &session.value)?
        }
        _ => return Ok(None),
    };
    let Some(path) = path else {
        return Ok(None);
    };
    let mut header = Vec::new();
    let read = File::open(&path).and_then(|file| file.take(65536).read_to_end(&mut header));
    read.map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("cannot read session store {}: {error}", path.display()),
        )
    })?;
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
    Ok(matches.then_some(path))
}

/// Evidence since a submission boundary. A start arms exactly one Codex user
/// prompt. A foreign prompt consumes that start and completion disarms it.
/// Claude user records start their own turn. A confirmed pair is conclusive
/// even if that turn finishes before the next poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Evidence {
    Quiet,
    Activity,
    Started,
    Confirmed,
}
impl Evidence {
    pub(crate) fn active(self) -> bool {
        self != Self::Quiet
    }
    pub(crate) fn confirmed(self) -> bool {
        self == Self::Confirmed
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Submission {
    path: PathBuf,
    offset: u64,
    agent: String,
}
impl Submission {
    pub(crate) fn capture(session: &AgentSessionInfo) -> Option<Self> {
        let path = checked_path(session).ok()??;
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

    pub(crate) fn evidence(&self, text: &str) -> Evidence {
        let Some(mut file) = File::open(&self.path).ok() else {
            return Evidence::Quiet;
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Evidence::Quiet;
        }
        let mut evidence = Evidence::Quiet;
        for line in BufReader::new(file.take(1024 * 1024))
            .lines()
            .map_while(Result::ok)
        {
            let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if self.agent == "codex" && row["type"] == "event_msg" {
                evidence = match row["payload"]["type"].as_str() {
                    Some("task_started") => Evidence::Started,
                    Some("task_complete" | "task_failed" | "turn_aborted") => Evidence::Activity,
                    _ => evidence,
                };
                continue;
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
            if (self.agent == "claude" || evidence == Evidence::Started)
                && content.as_deref() == Some(text)
            {
                return Evidence::Confirmed;
            }
            evidence = Evidence::Activity;
        }
        evidence
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
            !proof.evidence("hello").confirmed(),
            "an older identical prompt proves nothing"
        );
        append(&path, prompt("hello"));
        assert!(
            !proof.evidence("hello").confirmed(),
            "an echo without a turn start proves nothing"
        );
        append(
            &path,
            json!({"type":"event_msg","payload":{"type":"task_started"}}),
        );
        append(&path, prompt("different"));
        assert!(!proof.evidence("hello").confirmed());
        append(
            &path,
            json!({"type":"event_msg","payload":{"type":"task_started"}}),
        );
        append(&path, prompt("hello"));
        assert!(proof.evidence("hello").confirmed());
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
        assert!(checked_path(&session).unwrap().is_none());
        std::env::remove_var("CODEX_HOME");
    }
    #[test]
    fn completed_turn_cannot_confirm_later_echo() {
        let (_root, _session, path) = setup("codex");
        let proof = Submission {
            offset: std::fs::metadata(&path).unwrap().len(),
            path: path.clone(),
            agent: "codex".into(),
        };
        append(
            &path,
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
        );
        append(&path, prompt("other prompt"));
        append(
            &path,
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
        );
        append(&path, prompt("hello"));
        assert!(
            !proof.evidence("hello").confirmed(),
            "a completed unrelated turn cannot confirm a later echo"
        );
    }
    #[test]
    fn evidence_transitions_consume_starts_and_preserve_confirmed_pairs() {
        for closing_event in [
            "task_complete",
            "task_failed",
            "turn_aborted",
            "foreign_prompt",
        ] {
            let (_root, _session, path) = setup("codex");
            let proof = Submission {
                offset: std::fs::metadata(&path).unwrap().len(),
                path: path.clone(),
                agent: "codex".into(),
            };
            assert_eq!(proof.evidence("hello"), Evidence::Quiet);
            append(
                &path,
                json!({"type":"event_msg","payload":{"type":"task_started"}}),
            );
            assert_eq!(proof.evidence("hello"), Evidence::Started);
            if closing_event == "foreign_prompt" {
                append(&path, prompt("other prompt"));
            } else {
                append(
                    &path,
                    json!({"type":"event_msg","payload":{"type":closing_event}}),
                );
            }
            append(&path, prompt("hello"));
            assert_eq!(
                proof.evidence("hello"),
                Evidence::Activity,
                "{closing_event}"
            );
            append(
                &path,
                json!({"type":"event_msg","payload":{"type":"task_started"}}),
            );
            append(&path, prompt("hello"));
            append(
                &path,
                json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            );
            assert_eq!(proof.evidence("hello"), Evidence::Confirmed);
        }
    }

    #[test]
    fn claude_user_records_start_their_own_turn() {
        let (_root, _session, path) = setup("claude");
        let proof = Submission {
            offset: std::fs::metadata(&path).unwrap().len(),
            path: path.clone(),
            agent: "claude".into(),
        };
        append(
            &path,
            json!({"type":"user","message":{"role":"user","content":"other prompt"}}),
        );
        assert_eq!(proof.evidence("hello"), Evidence::Activity);
        append(
            &path,
            json!({"type":"user","isMeta":true,"message":{"role":"user","content":"hello"}}),
        );
        assert!(!proof.evidence("hello").confirmed());
        append(
            &path,
            json!({"type":"user","message":{"role":"user","content":"hello"}}),
        );
        assert!(proof.evidence("hello").confirmed());
    }
}
