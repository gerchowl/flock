//! Serialized, bounded keyboard submissions. Outcomes are separate from mail receipts.
use std::time::{Duration, Instant};

use super::api::responses::{encode_error, encode_success};
use crate::api::schema::{PaneSubmitParams, ResponseResult};
use crate::app::App;
use crate::detect::Agent;
use bytes::Bytes;

pub(crate) const CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub(crate) struct Attempt {
    pub(crate) evidence: crate::api::schema::DeliveryAttempt,
    pub(crate) text: String,
    cursor: String,
    child_pid: Option<u32>,
    operator_input: Option<Instant>,
    prompt_generation: u64,
    pub(crate) due: Instant,
    sent: bool,
    retried: bool,
    wake: bool,
    session: Option<String>,
    settle: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Accepted,
    ObservedAccepted,
    Unconfirmed(&'static str),
    Abandoned(&'static str),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Composer {
    Empty,
    Owned,
    Other,
    Unknown,
}

/// Match the complete editable region, allowing a terminal wrap at a row boundary.
fn matches_rows(rows: &[&str], text: &str) -> bool {
    let mut rest = text.trim();
    !rows.is_empty()
        && rows.iter().all(|row| {
            if let Some(tail) = rest.strip_prefix(row.trim()) {
                rest = tail.trim_start();
                true
            } else {
                false
            }
        })
        && rest.is_empty()
}

pub(crate) fn composer(agent: Agent, screen: &str, text: &str) -> Composer {
    let lines: Vec<_> = screen.lines().collect();
    let (rows, body_start, body_end): (Vec<&str>, usize, usize) = match agent {
        Agent::Claude => {
            if crate::detect::claude_composer(screen).is_none() {
                return Composer::Unknown;
            }
            let rule = |line: &str| {
                line.trim()
                    .chars()
                    .filter(|c| *c == '─' || *c == '━')
                    .count()
                    >= 3
            };
            let Some(end) = lines.iter().rposition(|line| rule(line)) else {
                return Composer::Unknown;
            };
            let Some(start) = lines[..end].iter().rposition(|line| rule(line)) else {
                return Composer::Unknown;
            };
            let body = &lines[start + 1..end];
            let Some(prompt) = body
                .iter()
                .position(|line| line.trim_start().starts_with('❯'))
            else {
                return Composer::Unknown;
            };
            let mut rows = vec![body[prompt].trim_start().trim_start_matches('❯').trim()];
            rows.extend(body[prompt + 1..].iter().map(|line| line.trim()));
            (rows, start + 1, end)
        }
        Agent::Codex => {
            let Some((start, end)) = crate::detect::codex_composer_region(screen) else {
                return Composer::Unknown;
            };
            let mut rows = vec![lines[start].trim_start().trim_start_matches('›').trim()];
            rows.extend(lines[start + 1..end].iter().map(|line| line.trim()));
            (rows, start, end)
        }
        Agent::OpenCode => {
            let Some(end) = lines
                .iter()
                .rposition(|line| line.trim_start().starts_with('╹'))
            else {
                return Composer::Unknown;
            };
            if !lines[end + 1..]
                .iter()
                .find(|line| !line.trim().is_empty())
                .is_some_and(|line| line.contains(" commands") && !line.contains("interrupt"))
            {
                return Composer::Unknown;
            }
            let Some(start) = (0..end)
                .rev()
                .take_while(|&i| lines[i].trim_start().starts_with('┃'))
                .last()
            else {
                return Composer::Unknown;
            };
            let rows = lines[start..end]
                .iter()
                .map(|line| line.trim_start().trim_start_matches('┃').trim())
                .skip_while(|line| line.is_empty())
                .take_while(|line| !line.is_empty())
                .collect();
            (rows, start, end)
        }
        _ => return Composer::Unknown,
    };
    // Only the live controls around the editor can authorize input. Menu controls
    // make even a retained empty composer unsafe.
    let mut chrome = lines[..body_start].to_vec();
    chrome.push(match agent {
        Agent::Claude => "❯",
        Agent::Codex => "›",
        _ => "┃",
    });
    chrome.extend_from_slice(&lines[body_end..]);
    let detection = crate::detect::detect_agent(Some(agent), &chrome.join("\n"));
    if detection.state != crate::detect::AgentState::Idle || detection.visible_blocker {
        return Composer::Unknown;
    }
    let tail = lines[body_end..].join("\n").to_lowercase();
    if [
        "enter to select",
        "enter to confirm",
        "enter confirm",
        "enter to submit answer",
        "esc dismiss",
        "esc to cancel",
        "↑↓",
        "hooks need review",
        "ctrl+r",
        "search:",
    ]
    .iter()
    .any(|control| tail.contains(control))
    {
        return Composer::Unknown;
    }
    let nonempty: Vec<_> = rows.into_iter().filter(|row| !row.is_empty()).collect();
    let empty = nonempty.is_empty()
        || (nonempty.len() == 1
            && match agent {
                Agent::Codex => nonempty[0] == "Ask Codex to do anything",
                Agent::OpenCode => {
                    nonempty[0] == "Ask anything…"
                        || (nonempty[0].starts_with("Ask anything… \"")
                            && nonempty[0].ends_with('"'))
                }
                _ => false,
            });
    if empty {
        Composer::Empty
    } else if matches_rows(&nonempty, text) && !text.trim().is_empty() {
        Composer::Owned
    } else {
        Composer::Other
    }
}

/// Same execution plus a new working entry also captures turns between polls.
fn progress(before: &str, after: &str) -> Result<bool, &'static str> {
    let before: Vec<_> = before.split(':').collect();
    let after: Vec<_> = after.split(':').collect();
    if before.len() != 5 || after.len() != 5 || before[..2] != after[..2] {
        return Err("execution_changed");
    }
    let old = before[2].parse::<u64>().map_err(|_| "invalid_cursor")?;
    let new = after[2].parse::<u64>().map_err(|_| "invalid_cursor")?;
    Ok(new > old)
}

impl App {
    pub(crate) fn handle_pane_submit(
        &mut self,
        id: String,
        mut params: PaneSubmitParams,
    ) -> String {
        let Some((ws, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_gone", "pane_gone");
        };
        let Some(pane) = self.public_pane_id(ws, pane_id) else {
            return encode_error(id, "pane_gone", "pane_gone");
        };
        if params.self_submit_confirmed == Some(false) && self.caller_workspace_idx() == Some(ws) {
            return encode_error(
                id,
                "self_submit_unconfirmed",
                "submitting to your own workspace requires self: true",
            );
        }
        params.pane_id = pane;
        match self.begin_guarded_submit(
            &params.pane_id,
            &params.text,
            params.if_session.as_deref(),
            Duration::from_secs(params.min_age_secs),
            Instant::now(),
            false,
        ) {
            Ok(attempt) => {
                self.active_submissions.insert(params.pane_id.clone());
                self.pending_agent_submit = Some((id.clone(), params.pane_id, attempt));
                encode_success(id, ResponseResult::Ok {})
            }
            Err(reason) => encode_error(id, reason, reason),
        }
    }

    pub(crate) fn begin_guarded_submit(
        &self,
        pane: &str,
        text: &str,
        session: Option<&str>,
        settle: Duration,
        now: Instant,
        wake: bool,
    ) -> Result<Attempt, &'static str> {
        if text.trim().is_empty()
            || text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        {
            return Err("unsafe_text");
        }
        let (ws, pane_id) = self.parse_pane_id(pane).ok_or("pane_gone")?;
        let terminal = self.terminal_for_pane(ws, pane_id).ok_or("pane_gone")?;
        if terminal.restart_in_progress {
            return Err("restart_pending");
        }
        if self.active_submissions.contains(pane)
            || (!wake && self.idle_wake.in_flight(pane))
            || terminal.armed_self_compact.as_ref().is_some_and(|armed| {
                matches!(
                    armed.phase,
                    crate::agent_self_compact::SelfCompactPhase::CompactRequested
                        | crate::agent_self_compact::SelfCompactPhase::ContinueTyped
                )
            })
        {
            return Err("injection_pending");
        }
        if session
            .is_some_and(|session| terminal.submission_session_id().as_deref() != Some(session))
        {
            return Err("session_mismatch");
        }
        if let Some(reason) = terminal.guarded_submit_blocker(
            now,
            settle,
            if wake {
                Duration::from_millis(self.state.config.msg.idle_wake_fresh_ms)
            } else {
                Duration::from_secs(5)
            },
        ) {
            return Err(reason);
        }
        let agent = terminal.effective_known_agent().ok_or("unknown_composer")?;
        let runtime = self.lookup_runtime_sender(ws, pane_id).ok_or("pane_gone")?;
        if runtime.last_operator_input_at().is_some_and(|input| {
            now.saturating_duration_since(input) < crate::cli::pane::PANE_RUN_SUBMIT_GAP
        }) {
            return Err("operator_active");
        }
        match composer(agent, &runtime.detection_text(), text) {
            Composer::Empty => (),
            Composer::Other | Composer::Owned => return Err("input_not_empty"),
            Composer::Unknown => return Err("unknown_composer"),
        }
        let bytes = super::api_helpers::encode_api_text(runtime, text);
        if text.contains(['\n', '\t']) && !bytes.starts_with(b"\x1b[200~") {
            return Err("unsafe_text");
        }
        let mut evidence = self.new_delivery_attempt(pane, wake)?;
        self.record_delivery_attempt(&evidence);
        if runtime.try_send_flock_authored(Bytes::from(bytes)).is_err() {
            evidence.state = "abandoned".into();
            evidence.reason = Some("write_failed".into());
            evidence.finished_at_ms = Some(super::api::messages::now_ms());
            self.record_delivery_attempt(&evidence);
            return Err("write_failed");
        }
        evidence.state = "typed".into();
        evidence.typed_at_ms = Some(super::api::messages::now_ms());
        self.record_delivery_attempt(&evidence);
        Ok(Attempt {
            evidence,
            text: text.to_owned(),
            cursor: terminal.turn_cursor(),
            child_pid: runtime.child_pid(),
            operator_input: runtime.last_operator_input_at(),
            prompt_generation: terminal.prompt_report_generation,
            due: now + crate::cli::pane::PANE_RUN_SUBMIT_GAP,
            sent: false,
            retried: false,
            wake,
            session: session.map(str::to_owned),
            settle,
        })
    }

    pub(crate) fn advance_guarded_submit(
        &self,
        pane: &str,
        attempt: &mut Attempt,
        now: Instant,
    ) -> Option<Outcome> {
        let outcome = self.advance_guarded_submit_inner(pane, attempt, now);
        if let Some(ref outcome) = outcome {
            self.finish_delivery_attempt(attempt, outcome);
        } else if attempt.sent
            && (attempt.evidence.state != "submit_sent"
                || attempt.evidence.retried != attempt.retried)
        {
            attempt.evidence.state = "submit_sent".into();
            attempt
                .evidence
                .submit_sent_at_ms
                .get_or_insert_with(super::api::messages::now_ms);
            attempt.evidence.retried = attempt.retried;
            self.record_delivery_attempt(&attempt.evidence);
        }
        outcome
    }

    fn advance_guarded_submit_inner(
        &self,
        pane: &str,
        attempt: &mut Attempt,
        now: Instant,
    ) -> Option<Outcome> {
        let Some((ws, pane_id)) = self.parse_pane_id(pane) else {
            return Some(Outcome::Abandoned("pane_gone"));
        };
        let Some(terminal) = self.terminal_for_pane(ws, pane_id) else {
            return Some(Outcome::Abandoned("pane_gone"));
        };
        let Some(runtime) = self.lookup_runtime_sender(ws, pane_id) else {
            return Some(Outcome::Abandoned("pane_gone"));
        };
        if runtime.child_pid() != attempt.child_pid || terminal.restart_in_progress {
            return Some(Outcome::Abandoned("execution_changed"));
        }
        if attempt
            .session
            .as_deref()
            .is_some_and(|session| terminal.submission_session_id().as_deref() != Some(session))
        {
            return Some(Outcome::Abandoned("session_mismatch"));
        }
        if runtime.last_operator_input_at() != attempt.operator_input {
            return Some(Outcome::Abandoned("operator_active"));
        }
        let started = match progress(&attempt.cursor, &terminal.turn_cursor()) {
            Ok(started) => started,
            Err(reason) => return Some(Outcome::Abandoned(reason)),
        };
        let reported_text = crate::control_bytes::strip(&attempt.text);
        let reported = terminal.prompt_report_generation > attempt.prompt_generation
            && terminal.last_prompt.as_deref() == Some(reported_text.trim());
        if !attempt.sent && (started || reported) {
            return Some(Outcome::Abandoned("turn_started_before_enter"));
        }
        if attempt.sent && reported {
            return Some(Outcome::Accepted);
        }
        if attempt.sent && started {
            return Some(Outcome::ObservedAccepted);
        }
        if now < attempt.due {
            return None;
        }
        if attempt.sent && attempt.retried {
            return Some(Outcome::Unconfirmed("confirm_timeout"));
        }
        if let Some(reason) = terminal.guarded_submit_blocker(
            now,
            attempt.settle,
            if attempt.wake {
                Duration::from_millis(self.state.config.msg.idle_wake_fresh_ms)
            } else {
                Duration::from_secs(5)
            },
        ) {
            return Some(Outcome::Abandoned(reason));
        }
        let Some(agent) = terminal.effective_known_agent() else {
            return Some(Outcome::Abandoned("unknown_composer"));
        };
        let editor = composer(agent, &runtime.detection_text(), &attempt.text);
        // The first Enter may precede the child's paste repaint. An unchanged
        // empty editor remains safe, but only exact owned text permits a retry.
        if editor != Composer::Owned && (attempt.sent || editor != Composer::Empty) {
            return Some(Outcome::Unconfirmed("owned_composer_not_visible"));
        }
        let enter = runtime.encode_terminal_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::empty(),
            )
            .into(),
        );
        if runtime.try_send_flock_authored(Bytes::from(enter)).is_err() {
            return Some(Outcome::Abandoned("write_failed"));
        }
        attempt.retried = attempt.sent;
        attempt.sent = true;
        attempt.due = now + CONFIRM_WINDOW;
        None
    }

    pub(crate) fn schedule_guarded_request(
        &self,
        request_id: String,
        pane_id: String,
        attempt: Attempt,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(POLL.min(attempt.due.saturating_duration_since(Instant::now())))
                .await;
            let _ = tx
                .send(crate::events::AppEvent::AgentSubmit {
                    request_id,
                    pane_id,
                    attempt,
                    respond_to,
                })
                .await;
        });
    }

    pub(crate) fn advance_guarded_request(
        &mut self,
        id: String,
        pane: String,
        mut attempt: Attempt,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        if let Some(outcome) = self.advance_guarded_submit(&pane, &mut attempt, Instant::now()) {
            self.active_submissions.remove(&pane);
            let (outcome, reason) = match outcome {
                Outcome::Accepted => ("accepted", None),
                Outcome::ObservedAccepted => ("observed_accepted", None),
                Outcome::Unconfirmed(reason) => ("unconfirmed", Some(reason.to_owned())),
                Outcome::Abandoned(reason) => ("abandoned", Some(reason.to_owned())),
            };
            let _ = respond_to.send(encode_success(
                id,
                ResponseResult::GuardedSubmit {
                    attempt: Some(attempt.evidence),
                    outcome: outcome.to_owned(),
                    reason,
                    retried: attempt.retried,
                },
            ));
        } else {
            self.schedule_guarded_request(id, pane, attempt, respond_to);
        }
    }
}

fn new_evidence(
    attempt_id: String,
    pane: &str,
    wake: bool,
    correlation_ids: Vec<String>,
) -> crate::api::schema::DeliveryAttempt {
    crate::api::schema::DeliveryAttempt {
        attempt_id,
        pane: pane.into(),
        correlation_ids,
        wake,
        state: "queued".into(),
        reason: None,
        queued_at_ms: super::api::messages::now_ms(),
        typed_at_ms: None,
        submit_sent_at_ms: None,
        finished_at_ms: None,
        retried: false,
    }
}

impl App {
    fn new_delivery_attempt(
        &self,
        pane: &str,
        wake: bool,
    ) -> Result<crate::api::schema::DeliveryAttempt, &'static str> {
        let queued_ids = self.mailboxes.queued_correlation_ids();
        let id = self
            .delivery_attempt_registry
            .borrow_mut()
            .reserve_id(|id| queued_ids.contains(id))?;
        let ids = if wake {
            self.mailboxes
                .queued_infos(Some(pane))
                .into_iter()
                .map(|m| m.correlation_id)
                .collect()
        } else {
            Vec::new()
        };
        let evidence = new_evidence(id, pane, wake, ids);
        if self.node_id.is_some() {
            match crate::mesh::hello::with_store(|store| {
                store.record_attempt(&evidence).map_err(|e| e.to_string())
            }) {
                Ok(_) => {}
                // Only a full durable registry refuses admission. Any other
                // store failure leaves local typing to the in-memory registry.
                Err(reason) if reason.starts_with("mail_store_full") => {
                    return Err("delivery_attempt_capacity")
                }
                Err(_) => {
                    crate::logging::mesh_custody_failed("record_attempt", "mail_store_unavailable")
                }
            }
        }
        Ok(evidence)
    }

    pub(crate) fn record_delivery_attempt(&self, attempt: &crate::api::schema::DeliveryAttempt) {
        if self.node_id.is_some()
            && crate::mesh::hello::with_store(|store| {
                store.record_attempt(attempt).map_err(|e| e.to_string())
            })
            .is_err()
        {
            crate::logging::mesh_custody_failed("record_attempt", "mail_store_unavailable");
        }
        self.delivery_attempt_registry
            .borrow_mut()
            .record(attempt.clone());
        self.emit_event(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::DeliveryAttemptUpdated,
            data: crate::api::schema::EventData::DeliveryAttemptUpdated {
                attempt: attempt.clone(),
            },
        });
    }

    pub(crate) fn finish_delivery_attempt(&self, attempt: &mut Attempt, outcome: &Outcome) {
        if attempt.evidence.finished_at_ms.is_some() {
            return;
        }
        let (state, reason) = match outcome {
            Outcome::Accepted => ("accepted", None),
            Outcome::ObservedAccepted => ("observed_accepted", None),
            Outcome::Unconfirmed(reason) => ("unconfirmed", Some(*reason)),
            Outcome::Abandoned(reason) => ("abandoned", Some(*reason)),
        };
        attempt.evidence.state = state.into();
        attempt.evidence.reason = reason.map(str::to_owned);
        attempt.evidence.retried = attempt.retried;
        attempt.evidence.finished_at_ms = Some(super::api::messages::now_ms());
        self.record_delivery_attempt(&attempt.evidence);
    }

    pub(crate) fn delivery_attempts(&self) -> Vec<crate::api::schema::DeliveryAttempt> {
        let mut attempts = self
            .delivery_attempt_registry
            .borrow()
            .snapshot()
            .into_iter()
            .map(|a| (a.attempt_id.clone(), a))
            .collect::<std::collections::BTreeMap<_, _>>();
        if self.node_id.is_some() {
            match crate::mesh::runtime_store::read(|store| {
                store.delivery_attempts().map_err(|e| e.to_string())
            }) {
                Ok(Some(stored)) => {
                    attempts.extend(stored.into_iter().map(|a| (a.attempt_id.clone(), a)))
                }
                Err(_) => {
                    crate::logging::mesh_custody_failed("read_attempts", "mail_store_unavailable")
                }
                Ok(None) => {}
            }
        }
        attempts.into_values().collect()
    }
}

#[cfg(test)]
impl Attempt {
    pub(crate) fn test_new() -> Self {
        Self {
            evidence: new_evidence("test-attempt".into(), "test-pane", false, Vec::new()),
            text: String::new(),
            cursor: "1:0:0:0:i".into(),
            child_pid: None,
            operator_input: None,
            prompt_generation: 0,
            due: Instant::now(),
            sent: false,
            retried: false,
            wake: false,
            session: None,
            settle: Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_startup_passive_banners_leave_composer_ready() {
        let screen = include_str!("../../tests/fixtures/codex/startup-passive-banners.txt");
        let captured = include_str!("../../tests/fixtures/codex-submit-721/inline-idle.txt");
        let prompt = "› Ask Codex to do anything";
        // Preserve the real prompt, blank row and model/effort/directory footer.
        assert_eq!(
            screen.split_once(prompt).unwrap().1,
            captured.split_once(prompt).unwrap().1
        );
        let (_, footer) = crate::detect::codex_composer_region(screen).expect("live composer");
        assert_eq!(screen.lines().nth(footer), captured.lines().last());
        let detection = crate::detect::detect_agent(Some(Agent::Codex), screen);
        assert_eq!(detection.state, crate::detect::AgentState::Idle);
        assert!(detection.visible_idle);
        assert!(!detection.visible_blocker);
        assert!(detection.provider_limit.is_none());
        assert_eq!(composer(Agent::Codex, screen, "brief"), Composer::Empty);
        assert_eq!(
            composer(
                Agent::Codex,
                &screen.replace("Ask Codex to do anything", "brief"),
                "brief"
            ),
            Composer::Owned
        );
    }

    #[test]
    fn codex_startup_update_dialog_is_not_a_composer() {
        let screen = include_str!("../../tests/fixtures/codex/startup-update-dialog.txt");
        let captured = include_str!("../../tests/fixtures/codex-submit-721/alt-idle.txt");
        let (history, _) = captured.split_once("› Ask Codex to do anything").unwrap();
        assert!(screen.starts_with(history));
        // Like the captured /model picker, an active menu replaces the editor
        // and its footer rather than retaining an editable prompt underneath.
        let picker = include_str!("../../tests/fixtures/codex-submit-721/alt-model.txt");
        assert!(crate::detect::codex_composer_region(picker).is_none());
        assert!(crate::detect::codex_composer_region(screen).is_none());
        assert_eq!(composer(Agent::Codex, screen, "brief"), Composer::Unknown);
    }

    #[test]
    fn guarded_composer_requires_complete_exact_editor() {
        let screen = "──────\n❯ hello world\n──────\n? for shortcuts";
        assert_eq!(
            composer(Agent::Claude, screen, "hello world"),
            Composer::Owned
        );
        assert_eq!(
            composer(
                Agent::Claude,
                &screen.replace("hello world", "enter to confirm"),
                "enter to confirm"
            ),
            Composer::Owned
        );
        assert_eq!(composer(Agent::Claude, screen, "hello"), Composer::Other);
        assert_eq!(
            composer(
                Agent::Claude,
                &screen.replace("hello world", "\u{a0}"),
                "hello"
            ),
            Composer::Empty
        );
        assert_eq!(
            composer(
                Agent::Claude,
                &screen.replace("hello world", "hello\n  world"),
                "hello world"
            ),
            Composer::Owned
        );
        assert_eq!(
            composer(
                Agent::Claude,
                &format!("{screen}\nEnter to select · Esc to cancel"),
                "hello world"
            ),
            Composer::Unknown
        );
        assert_eq!(
            composer(Agent::Claude, "❯ hello world", "hello world"),
            Composer::Unknown
        );
        assert_eq!(
            composer(
                Agent::Claude,
                &screen.replace("hello world", "[Pasted text #1]"),
                "hello world"
            ),
            Composer::Other
        );
    }
    #[test]
    fn guarded_cursor_confirms_short_turn_and_rejects_restart() {
        assert_eq!(progress("1:0:2:4:i", "1:0:3:6:i"), Ok(true));
        assert_eq!(progress("1:0:2:4:i", "1:0:2:5:i"), Ok(false));
        assert_eq!(progress("1:0:2:4:i", "1:1:3:6:i"), Err("execution_changed"));
    }
}

#[cfg(test)]
#[path = "guarded_submit/codex_721_tests.rs"]
mod codex_721_tests;
