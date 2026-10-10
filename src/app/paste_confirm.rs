//! Observe plain paste delivery without injecting any confirmation keystrokes.
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::api::responses::{encode_error, encode_success};
use super::guarded_submit::{composer_contents, CONFIRM_WINDOW};
use super::App;
use crate::api::schema::ResponseResult;
use crate::detect::Agent;
use crate::layout::PaneId;

#[derive(Debug)]
pub(crate) struct Paste {
    id: String,
    pane: String,
    text: String,
    before: String,
    agent: Option<Agent>,
    child_pid: Option<u32>,
    operator_input: Option<Instant>,
    pub(super) deadline: Instant,
}

fn digest(screen: &str) -> String {
    Sha256::digest(screen.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Isolate the inserted/changed span instead of searching old transcript text.
fn inserted_text(before: &str, after: &str, text: &str) -> bool {
    let before = before.lines().map(normalize).collect::<Vec<_>>().join("\n");
    let after = after.lines().map(normalize).collect::<Vec<_>>().join("\n");
    let text = normalize(text);
    if text.is_empty() || before == after {
        return false;
    }
    let prefix = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let before_tail: Vec<_> = before.chars().skip(prefix).collect();
    let after_tail: Vec<_> = after.chars().skip(prefix).collect();
    let suffix = before_tail
        .iter()
        .rev()
        .zip(after_tail.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let changed: String = after_tail[..after_tail.len() - suffix].iter().collect();
    contains_wrapped(&changed, &text)
}

/// Terminal row boundaries may split a word or replace whitespace.
fn contains_wrapped(changed: &str, text: &str) -> bool {
    let wanted: Vec<_> = text.chars().collect();
    let changed: Vec<_> = changed.chars().collect();
    (0..changed.len()).any(|start| {
        let mut matched = 0;
        for &ch in &changed[start..] {
            if ch == '\n' {
                if wanted.get(matched) == Some(&' ') {
                    matched += 1;
                }
            } else if wanted.get(matched) == Some(&ch) {
                matched += 1;
            } else {
                return false;
            }
            if matched == wanted.len() {
                return true;
            }
        }
        false
    })
}

/// Align retained rows before looking for inserted text. A scroll shifts old
/// rows without making their contents new input, including repeated rows.
fn gained_text(before: &str, after: &str, text: &str) -> bool {
    let before: Vec<_> = before.lines().map(normalize).collect();
    let after: Vec<_> = after.lines().map(normalize).collect();
    let mut common = vec![vec![0; after.len() + 1]; before.len() + 1];
    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            common[i][j] = if before[i] == after[j] {
                1 + common[i + 1][j + 1]
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut old_start, mut new_start) = (0, 0, 0, 0);
    while i < before.len() && j < after.len() {
        if before[i] == after[j] {
            if inserted_text(
                &before[old_start..i].join("\n"),
                &after[new_start..j].join("\n"),
                text,
            ) {
                return true;
            }
            i += 1;
            j += 1;
            old_start = i;
            new_start = j;
        } else if common[i + 1][j] >= common[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    inserted_text(
        &before[old_start..].join("\n"),
        &after[new_start..].join("\n"),
        text,
    )
}

fn observed(agent: Option<Agent>, before: &str, after: &str, text: &str) -> Option<&'static str> {
    if before == after {
        return None;
    }
    if let Some((before, after)) = agent.and_then(|agent| {
        Some((
            composer_contents(agent, before, before)?,
            composer_contents(agent, after, after)?,
        ))
    }) {
        if before == after {
            return None;
        }
        return Some(if gained_text(&before, &after, text) {
            "text_matched"
        } else {
            "composer_changed"
        });
    }

    // Unsupported harnesses and ordinary terminals share an observational
    // fallback. A screen change is deliberately weaker evidence than text.
    Some(if gained_text(before, after, text) {
        "text_matched"
    } else {
        "screen_changed"
    })
}

impl App {
    pub(crate) fn begin_paste(
        &mut self,
        id: String,
        ws: usize,
        pane_id: PaneId,
        text: String,
        not_found: (&str, String),
        write_failed: &str,
    ) -> String {
        let Some(pane) = self.public_pane_id(ws, pane_id) else {
            return encode_error(id, not_found.0, not_found.1);
        };
        let Some(runtime) = self.lookup_runtime_sender(ws, pane_id) else {
            return encode_error(id, not_found.0, not_found.1);
        };
        let mut paste = Paste {
            id: id.clone(),
            pane,
            text: text.clone(),
            before: runtime.detection_text(),
            agent: self
                .terminal_for_pane(ws, pane_id)
                .and_then(|t| t.effective_known_agent()),
            child_pid: runtime.child_pid(),
            operator_input: runtime.last_operator_input_at(),
            deadline: Instant::now() + CONFIRM_WINDOW,
        };
        let encoded = super::api_helpers::encode_api_text(runtime, &text);
        if let Err(err) = runtime.try_send_bytes(Bytes::from(encoded)) {
            return encode_error(id, write_failed, err.to_string());
        }
        paste.operator_input = runtime.last_operator_input_at();
        self.pending_paste = Some(paste);
        encode_success(id, ResponseResult::Ok {})
    }

    pub(crate) fn schedule_paste(&self, paste: Paste, respond_to: Sender<String>) {
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx
                .send(crate::events::AppEvent::PasteConfirm { paste, respond_to })
                .await;
        });
    }

    pub(crate) fn advance_paste(&self, paste: Paste, respond_to: Sender<String>) {
        let current = self.parse_pane_id(&paste.pane).and_then(|(ws, pane)| {
            let runtime = self.lookup_runtime_sender(ws, pane)?;
            let terminal = self.terminal_for_pane(ws, pane)?;
            Some((runtime, terminal))
        });
        let after = current.as_ref().map(|(r, _)| r.detection_text());
        let level = after
            .as_deref()
            .and_then(|after| observed(paste.agent, &paste.before, after, &paste.text));
        let expired = Instant::now() >= paste.deadline;
        let reason = match current {
            None => "pane_gone",
            Some((runtime, terminal))
                if runtime.child_pid() != paste.child_pid || terminal.restart_in_progress =>
            {
                "execution_changed"
            }
            Some((runtime, _)) if runtime.last_operator_input_at() != paste.operator_input => {
                "operator_active"
            }
            Some(_) if level.is_some() && (level != Some("screen_changed") || expired) => {
                "change_observed"
            }
            Some(_) if expired => "confirm_timeout",
            Some(_) => {
                self.schedule_paste(paste, respond_to);
                return;
            }
        };
        let _ = respond_to.send(encode_success(
            paste.id,
            ResponseResult::Paste {
                outcome: if reason == "change_observed" {
                    "delivered"
                } else {
                    "unconfirmed"
                }
                .into(),
                reason: reason.into(),
                evidence: json!({
                    "pane_id": paste.pane,
                    "before_digest": digest(&paste.before),
                    "after_digest": after.as_deref().map(digest),
                    "level": if reason == "change_observed" { level } else { None },
                }),
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paste_scrolling_old_rows_is_not_a_text_match() {
        for (before, after) in [
            ("old\nhello\nretained", "hello\nretained\nnew output"),
            ("hello\nretained", "older output\nhello\nretained"),
            (
                "old\nhello\nhello\nretained",
                "hello\nhello\nretained\nnew output",
            ),
            ("hello\nclock 1\n$", "hello\nclock 2\n$"),
        ] {
            assert_eq!(
                observed(None, before, after, "hello"),
                Some("screen_changed")
            );
        }
        assert!(gained_text("$ hello\n$", "$ hello\n$ hello", "hello"));
        assert!(gained_text("$", "$ hel\nlo world", "hello world"));
    }

    #[test]
    fn plain_paste_confirms_short_repeated_and_wrapped_input() {
        for text in ["y", "q", "1", "make test"] {
            assert_eq!(
                observed(
                    None,
                    &format!("{text}\n$ "),
                    &format!("{text}\n$ {text}"),
                    text
                ),
                Some("text_matched")
            );
        }
        assert_eq!(
            observed(None, "$ ", "$ hello\nworld", "hello world"),
            Some("text_matched")
        );
        assert_eq!(
            observed(None, "$ ", "$ hello\nworld", "hello\nworld"),
            Some("text_matched")
        );
        assert_eq!(observed(None, "$ ", "$ ", "hello"), None);
    }

    #[test]
    fn plain_paste_confirms_unsupported_harness_with_weaker_evidence() {
        for agent in [Agent::Pi, Agent::Gemini, Agent::Cursor, Agent::Kimi] {
            assert_eq!(
                observed(Some(agent), "prompt", "changed", "hello"),
                Some("screen_changed")
            );
        }
        assert_eq!(
            observed(Some(Agent::Pi), "prompt", "prompt hello", "hello"),
            Some("text_matched")
        );
        assert_eq!(observed(Some(Agent::Pi), "prompt", "prompt", "hello"), None);
    }

    #[test]
    fn plain_paste_confirms_claude_and_codex_chips() {
        for (agent, before, after) in [
            (
                Agent::Claude,
                "────\n❯ \n────",
                "────\n❯ [Pasted text #1 +20 lines]\n────",
            ),
            (
                Agent::Codex,
                "› \n? for shortcuts",
                "› [Pasted Content 4000 chars]\n? for shortcuts",
            ),
        ] {
            assert_eq!(
                observed(Some(agent), before, after, &"long brief\n".repeat(400)),
                Some("composer_changed")
            );
        }
    }

    #[test]
    fn plain_paste_confirms_busy_composer_and_draft_append() {
        for (agent, before, after) in [
            (
                Agent::Claude,
                "Working (esc to interrupt)\n────\n❯ draft \n────",
                "Working (esc to interrupt)\n────\n❯ draft hello world\n────",
            ),
            (
                Agent::Codex,
                "• Working (esc to interrupt)\n› draft \n? for shortcuts",
                "• Working (esc to interrupt)\n› draft hello\nworld\n? for shortcuts",
            ),
        ] {
            assert_eq!(
                observed(Some(agent), before, after, "hello world"),
                Some("text_matched")
            );
            assert_eq!(observed(Some(agent), after, after, "hello world"), None);
        }
    }
}
