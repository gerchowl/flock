//! Observe plain paste delivery without injecting any confirmation keystrokes.
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::api::responses::{encode_error, encode_success};
use super::guarded_submit::{composer, Composer, CONFIRM_WINDOW};
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
    deadline: Instant,
}

fn digest(screen: &str) -> String {
    Sha256::digest(screen.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn observed(agent: Option<Agent>, before: &str, after: &str, text: &str) -> bool {
    if text.trim().is_empty() || before == after {
        return false;
    }
    if let Some(agent) = agent {
        // A recognized harness needs evidence in its editor, not its transcript.
        return composer(agent, before, text) != Composer::Owned
            && composer(agent, after, text) == Composer::Owned;
    }
    // Ordinary terminals may echo input without a known composer. Require new
    // visible text, rather than treating unrelated screen activity as delivery.
    let flatten = |s: &str| s.lines().map(str::trim).collect::<String>();
    let text = flatten(text);
    !text.is_empty() && !flatten(before).contains(&text) && flatten(after).contains(&text)
}

impl App {
    pub(crate) fn begin_paste(
        &mut self,
        id: String,
        ws: usize,
        pane_id: PaneId,
        text: String,
    ) -> String {
        let Some(pane) = self.public_pane_id(ws, pane_id) else {
            return encode_error(id, "pane_gone", "pane_gone");
        };
        let Some(runtime) = self.lookup_runtime_sender(ws, pane_id) else {
            return encode_error(id, "pane_gone", "pane_gone");
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
            return encode_error(id, "pane_send_failed", err.to_string());
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
            Some(_)
                if observed(
                    paste.agent,
                    &paste.before,
                    after.as_deref().unwrap_or_default(),
                    &paste.text,
                ) =>
            {
                "text_observed"
            }
            Some(_) if Instant::now() >= paste.deadline => "confirm_timeout",
            Some(_) => {
                self.schedule_paste(paste, respond_to);
                return;
            }
        };
        let _ = respond_to.send(encode_success(
            paste.id,
            ResponseResult::Paste {
                outcome: if reason == "text_observed" {
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
                    "probe": if paste.agent.is_some() { "composer" } else { "screen_text" },
                }),
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_confirmation_requires_new_text_not_any_screen_change() {
        assert!(observed(None, "$ ", "$ hello", "hello"));
        assert!(!observed(None, "$ ", "spinner", "hello"));
        assert!(!observed(None, "hello\n$ ", "hello\n$ other", "hello"));
        assert!(!observed(None, "$ ", "$ ", ""));
        assert!(observed(None, "$ ", "$ hello\r\nworld", "hello\nworld"));
    }

    #[test]
    fn paste_confirmation_requires_known_agent_composer() {
        let before = "OpenAI Codex\n› \n  ? for shortcuts";
        let after = "OpenAI Codex\n› hello\n  ? for shortcuts";
        assert!(observed(Some(Agent::Codex), before, after, "hello"));
        assert!(!observed(
            Some(Agent::Codex),
            before,
            "hello\n› \n  ? for shortcuts",
            "hello"
        ));
        assert!(!observed(Some(Agent::Codex), after, after, "hello"));
    }
}
