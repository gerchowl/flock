//! Codex rollout records used by agent.result, checked against Codex 0.160.1.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::agent_transcript::{
    Block, Role, TranscriptError, TranscriptEvent, HISTORY_WINDOW_BYTES,
};

pub(crate) fn rollout_path(home: &Path, session: &str) -> Result<Option<PathBuf>, TranscriptError> {
    if session.is_empty()
        || !session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Ok(None);
    }
    find_rollout(&home.join("sessions"), &format!("-{session}.jsonl"), 4)
}

fn find_rollout(
    dir: &Path,
    suffix: &str,
    depth: usize,
) -> Result<Option<PathBuf>, TranscriptError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(TranscriptError::Unreadable),
    };
    for entry in entries {
        let entry = entry.map_err(|_| TranscriptError::Unreadable)?;
        let kind = entry.file_type().map_err(|_| TranscriptError::Unreadable)?;
        if kind.is_file() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(suffix) {
                return Ok(Some(entry.path()));
            }
        } else if kind.is_dir() && depth > 0 {
            if let Some(path) = find_rollout(&entry.path(), suffix, depth - 1)? {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

pub(crate) fn read_tail(path: &Path) -> Result<(Vec<TranscriptEvent>, bool), TranscriptError> {
    let mut file = File::open(path).map_err(|_| TranscriptError::Unreadable)?;
    let len = file
        .metadata()
        .map_err(|_| TranscriptError::Unreadable)?
        .len();
    let start = len.saturating_sub(HISTORY_WINDOW_BYTES).saturating_sub(1);
    file.seek(SeekFrom::Start(start))
        .map_err(|_| TranscriptError::Unreadable)?;
    let mut bytes = Vec::new();
    file.take(HISTORY_WINDOW_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| TranscriptError::Unreadable)?;
    let first = if start > 0 {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1)
    } else {
        0
    };
    let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    parse_lines(bytes.get(first..end).unwrap_or_default())
}

fn timestamp(record: &Value) -> Option<SystemTime> {
    let raw = record["timestamp"].as_str()?;
    raw.get(..4)?.parse::<u16>().ok()?;
    if raw.as_bytes().get(4) != Some(&b'-') {
        return None;
    }
    let seconds = crate::agent_transcript::parse_rfc3339_utc(raw)?;
    let millis = raw
        .split_once('.')
        .and_then(|(_, fraction)| fraction.strip_suffix('Z'))
        .map(|fraction| {
            fraction
                .chars()
                .take(3)
                .chain(std::iter::repeat('0'))
                .take(3)
                .collect::<String>()
        })
        .and_then(|fraction| fraction.parse::<u64>().ok())
        .unwrap_or(0);
    seconds.checked_add(Duration::from_millis(millis))
}

fn text_message(role: Role, text: String, at: Option<SystemTime>) -> TranscriptEvent {
    TranscriptEvent::Message {
        role,
        blocks: vec![Block::Text(text)],
        at,
    }
}

fn parse_lines(bytes: &[u8]) -> Result<(Vec<TranscriptEvent>, bool), TranscriptError> {
    let mut events = Vec::new();
    let mut finished = false;
    let mut pending: Option<TranscriptEvent> = None;
    let mut parsed = 0;
    let mut failed = 0;
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let record: Value = match serde_json::from_slice(line) {
            Ok(record) => {
                parsed += 1;
                record
            }
            Err(_) => {
                failed += 1;
                continue;
            }
        };
        let payload = &record["payload"];
        let at = timestamp(&record);
        match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("task_started")) => {
                // A turn boundary excludes unfinished output even if its prompt is outside the tail.
                events.push(text_message(Role::User, "turn started".into(), at));
                pending = None;
                finished = false;
            }
            (Some("response_item"), Some("message")) if payload["role"] == "user" => {
                events.push(text_message(Role::User, "user input".into(), at));
                pending = None;
                finished = false;
            }
            (Some("response_item"), Some("message")) if payload["role"] == "assistant" => {
                if payload["phase"] == "commentary" {
                    continue;
                }
                let text = payload["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|block| block["type"] == "output_text")
                            .filter_map(|block| block["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    pending = Some(text_message(Role::Assistant, text, at));
                    finished = false;
                }
            }
            (Some("event_msg"), Some("task_complete")) => {
                if let Some(text) = payload["last_agent_message"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                {
                    let matches = matches!(&pending, Some(TranscriptEvent::Message { blocks, .. })
                        if matches!(blocks.as_slice(), [Block::Text(candidate)] if candidate == text));
                    if !matches {
                        pending = Some(text_message(Role::Assistant, text.to_string(), at));
                    }
                }
                if let Some(reply) = pending.take() {
                    events.push(reply);
                }
                finished = true;
            }
            (Some("event_msg"), Some("turn_aborted")) => {
                finished = false;
            }
            _ => {}
        }
    }
    if failed > 0 && failed * 20 > parsed + failed {
        return Err(TranscriptError::FormatMoved { parsed, failed });
    }
    Ok((events, finished))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_transcript::turn_result;

    const FINISHED: &str = include_str!("../tests/fixtures/codex/finished.jsonl");
    const IN_PROGRESS: &str = include_str!("../tests/fixtures/codex/in-progress.jsonl");

    fn fixture(name: &str, text: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("flock-codex-reader-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn completed_turn_has_final_reply_and_recorded_millisecond_time() {
        let path = fixture("finished", FINISHED);
        let (events, finished) = read_tail(&path).unwrap();
        assert!(finished);
        let reply = turn_result(&events, finished).unwrap();
        assert_eq!(reply.text, "The fixture is sound.\nDONE: checked");
        assert_eq!(
            reply
                .at
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            1_767_225_603_125
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn running_turn_returns_previous_reply_without_narration() {
        let (events, finished) =
            parse_lines(format!("{FINISHED}{IN_PROGRESS}").as_bytes()).unwrap();
        assert!(!finished);
        assert_eq!(
            turn_result(&events, finished).unwrap().text,
            "The fixture is sound.\nDONE: checked"
        );
        let (events, finished) = parse_lines(IN_PROGRESS.as_bytes()).unwrap();
        assert!(turn_result(&events, finished).is_none());
    }

    #[test]
    fn final_message_is_not_finished_without_completion_event() {
        let incomplete = FINISHED.lines().take(5).collect::<Vec<_>>().join("\n") + "\n";
        let (events, finished) = parse_lines(incomplete.as_bytes()).unwrap();
        assert!(!finished);
        assert!(turn_result(&events, finished).is_none());
    }

    #[test]
    fn completion_snapshot_can_supply_a_reply_missing_from_the_tail() {
        let record = FINISHED.lines().last().unwrap();
        let (events, finished) = parse_lines(record.as_bytes()).unwrap();
        assert!(finished);
        assert_eq!(
            turn_result(&events, finished).unwrap().text,
            "The fixture is sound.\nDONE: checked"
        );
    }

    #[test]
    fn partial_trailing_record_is_ignored_and_reads_are_bounded() {
        let text = format!(
            "{}\n{FINISHED}{{\"type\":",
            "x".repeat(HISTORY_WINDOW_BYTES as usize)
        );
        let path = fixture("bounded", &text);
        let (events, finished) = read_tail(&path).unwrap();
        assert!(finished);
        assert_eq!(
            turn_result(&events, finished).unwrap().text,
            "The fixture is sound.\nDONE: checked"
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn missing_and_malformed_files_are_unreadable() {
        let path = fixture("unreadable", "invalid JSON\n");
        assert!(matches!(
            read_tail(&path),
            Err(TranscriptError::FormatMoved { .. })
        ));
        fs::remove_file(&path).unwrap();
        assert!(matches!(read_tail(&path), Err(TranscriptError::Unreadable)));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn session_lookup_is_exact_and_stays_inside_codex_home() {
        let path = fixture("lookup", "");
        let home = path.parent().unwrap();
        let dir = home.join("sessions/2026/01/01");
        fs::create_dir_all(&dir).unwrap();
        let rollout = dir.join("rollout-2026-01-01T00-00-00-sess-codex.jsonl");
        fs::write(&rollout, FINISHED).unwrap();
        assert_eq!(rollout_path(home, "sess-codex").unwrap(), Some(rollout));
        assert_eq!(rollout_path(home, "missing").unwrap(), None);
        assert_eq!(rollout_path(home, "../rollout").unwrap(), None);
        assert_eq!(rollout_path(home, "").unwrap(), None);
        fs::remove_dir_all(home).unwrap();
    }
}
