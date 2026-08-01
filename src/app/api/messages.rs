use crate::api::schema::{
    EventData, EventEnvelope, EventKind, MessageTarget, MsgListParams, MsgReplyParams,
    MsgSendParams, ResponseResult,
};
use crate::app::mailboxes::{EnqueueOutcome, PendingMessage};
use crate::app::App;

use super::responses::{encode_error, encode_success};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn mint_correlation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "msg:{:x}:{:x}",
        now_ms(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// `msg.send` / `msg.reply` / `msg.list` (#175 M1). Messages are queued per
/// recipient pane and delivered at the recipient's next settled turn
/// boundary — never mid-turn (§8.3). Sender identity is stamped from API
/// process ancestry and is routing/audit metadata only (P3): no code path
/// in this module branches on WHO sent a message, only on WHERE it goes.
impl App {
    pub(super) fn handle_msg_send(&mut self, id: String, params: MsgSendParams) -> String {
        let body = crate::app::api_helpers::sanitize_reported_prompt(&params.body);
        if body.trim().is_empty() {
            return encode_error(id, "invalid_request", "message body is empty");
        }

        let (to_ws_idx, to_pane_id) = match self.resolve_message_target(&params.to) {
            Ok(resolved) => resolved,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let Some(to_pane) = self.public_pane_id(to_ws_idx, to_pane_id) else {
            return encode_error(id, "internal_error", "target pane has no public id");
        };
        let to_repo = self.workspace_repo_label(to_ws_idx);

        // Sender stamp: process ancestry of the API peer, an empty claim so
        // ancestry is the only evidence (P3 — routing, never authorization).
        let sender = self.parse_pane_id_or_peer("", self.current_api_peer_pid);
        let from_pane = sender.and_then(|(ws_idx, pane_id)| self.public_pane_id(ws_idx, pane_id));
        let from_repo = sender.and_then(|(ws_idx, _)| self.workspace_repo_label(ws_idx));

        if from_pane.as_deref() == Some(to_pane.as_str()) {
            return encode_error(id, "self_message_forbidden", "a pane cannot message itself");
        }

        let now = now_ms();
        let sender_key = from_pane.clone().unwrap_or_else(|| "unknown".into());
        if let Err(retry_after_ms) = self.mailboxes.admit_rate(&sender_key, now) {
            return encode_error(
                id,
                "msg_rate_limited",
                format!("rate limit exceeded; retry in {retry_after_ms} ms"),
            );
        }

        let correlation_id = params
            .correlation_id
            .filter(|explicit| !explicit.trim().is_empty())
            .unwrap_or_else(mint_correlation_id);
        let mut warnings = Vec::new();
        if from_pane.is_none() {
            warnings.push("sender_unresolved_shared_rate_bucket".to_string());
        }
        let message = PendingMessage {
            correlation_id: correlation_id.clone(),
            body,
            from_pane,
            from_repo,
            to_pane,
            to_repo,
            in_reply_to: params.in_reply_to,
            enqueued_at_ms: now,
            delivery_attempts: 0,
        };

        self.queue_message(id, message, warnings)
    }

    pub(super) fn handle_msg_reply(&mut self, id: String, params: MsgReplyParams) -> String {
        let body = crate::app::api_helpers::sanitize_reported_prompt(&params.body);
        if body.trim().is_empty() {
            return encode_error(id, "invalid_request", "message body is empty");
        }
        // The original sender's pane is the reply target; delivered messages
        // keep it in history, still-queued ones in the queue itself.
        let original = self
            .mailboxes
            .reply_meta(&params.correlation_id)
            .map(|meta| {
                (
                    meta.from_pane.clone(),
                    meta.enqueued_at_ms,
                    meta.root.clone(),
                )
            })
            .or_else(|| {
                self.mailboxes
                    .queued_message(&params.correlation_id)
                    .map(|message| {
                        (
                            message.from_pane.clone(),
                            message.enqueued_at_ms,
                            message.correlation_id.clone(),
                        )
                    })
            });
        let Some((original_sender, original_enqueued_ms, root)) = original else {
            return encode_error(
                id,
                "message_not_found",
                format!("no message with correlation id {}", params.correlation_id),
            );
        };
        let Some(reply_to_pane) = original_sender else {
            return encode_error(
                id,
                "no_reply_address",
                "the original message carried no sender pane to reply to",
            );
        };
        let (to_ws_idx, to_pane_id) = match self.resolve_message_target(&MessageTarget::Pane {
            pane: reply_to_pane.clone(),
        }) {
            Ok(resolved) => resolved,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let Some(to_pane) = self.public_pane_id(to_ws_idx, to_pane_id) else {
            return encode_error(id, "internal_error", "reply pane has no public id");
        };

        let sender = self.parse_pane_id_or_peer("", self.current_api_peer_pid);
        let from_pane = sender.and_then(|(ws_idx, pane_id)| self.public_pane_id(ws_idx, pane_id));
        let from_repo = sender.and_then(|(ws_idx, _)| self.workspace_repo_label(ws_idx));
        let to_repo = self.workspace_repo_label(to_ws_idx);

        let now = now_ms();
        let sender_key = from_pane.clone().unwrap_or_else(|| "unknown".into());
        if let Err(retry_after_ms) = self.mailboxes.admit_rate(&sender_key, now) {
            return encode_error(
                id,
                "msg_rate_limited",
                format!("rate limit exceeded; retry in {retry_after_ms} ms"),
            );
        }

        let reply_correlation_id = params
            .reply_correlation_id
            .filter(|explicit| !explicit.trim().is_empty())
            .unwrap_or_else(mint_correlation_id);
        let mut warnings = Vec::new();
        if from_pane.is_none() {
            warnings.push("sender_unresolved_shared_rate_bucket".to_string());
        }
        let message = PendingMessage {
            correlation_id: reply_correlation_id.clone(),
            body,
            from_pane,
            from_repo,
            to_pane,
            to_repo,
            in_reply_to: Some(params.correlation_id.clone()),
            enqueued_at_ms: now,
            delivery_attempts: 0,
        };
        // Telemetry only records a reply the mailbox actually accepted: a
        // full mailbox or a duplicate must not bump round_trips or leave a
        // MessageReplied with no matching queued/delivered pair.
        let response = self.queue_message(id, message, warnings);
        if response.contains("\"state\":\"queued\"") {
            let round_trips = self.mailboxes.bump_round_trips(&root);
            self.emit_event(EventEnvelope {
                event: EventKind::MessageReplied,
                data: EventData::MessageReplied {
                    correlation_id: params.correlation_id.clone(),
                    reply_correlation_id,
                    reply_latency_ms: now.saturating_sub(original_enqueued_ms),
                    round_trips,
                },
            });
        }
        response
    }

    pub(super) fn handle_msg_list(&mut self, id: String, params: MsgListParams) -> String {
        let pane_filter = match params.pane {
            Some(pane) => match self.resolve_terminal_target(&pane) {
                Ok(resolved) => self.public_pane_id(resolved.ws_idx, resolved.pane_id),
                Err(err) => {
                    return super::responses::encode_error_body(
                        id,
                        self.agent_target_error_body(err),
                    )
                }
            },
            None => None,
        };
        encode_success(
            id,
            ResponseResult::MsgList {
                messages: self.mailboxes.queued_infos(pane_filter.as_deref()),
            },
        )
    }

    /// Shared enqueue tail: dedupe, emit the durable `MessageQueued`, and
    /// answer the caller.
    fn queue_message(
        &mut self,
        id: String,
        message: PendingMessage,
        warnings: Vec<String>,
    ) -> String {
        let correlation_id = message.correlation_id.clone();
        let event = EventData::MessageQueued {
            correlation_id: correlation_id.clone(),
            from_pane: message.from_pane.clone(),
            from_repo: message.from_repo.clone(),
            to_pane: message.to_pane.clone(),
            to_repo: message.to_repo.clone(),
            cross_repo: match (&message.from_repo, &message.to_repo) {
                (Some(from), Some(to)) => from != to,
                _ => false,
            },
            in_reply_to: message.in_reply_to.clone(),
            enqueued_at_ms: message.enqueued_at_ms,
            body: message.body.clone(),
        };
        match self.mailboxes.enqueue(message) {
            EnqueueOutcome::Queued => {
                self.emit_event(EventEnvelope {
                    event: EventKind::MessageQueued,
                    data: event,
                });
                encode_success(
                    id,
                    ResponseResult::MsgQueued {
                        correlation_id,
                        state: "queued".into(),
                        warnings,
                    },
                )
            }
            EnqueueOutcome::Duplicate => encode_success(
                id,
                ResponseResult::MsgQueued {
                    correlation_id,
                    state: "duplicate".into(),
                    warnings,
                },
            ),
            EnqueueOutcome::MailboxFull => encode_error(
                id,
                "mailbox_full",
                "recipient mailbox is at capacity; retry after delivery or cancel",
            ),
        }
    }

    fn workspace_repo_label(&self, ws_idx: usize) -> Option<String> {
        let ws = self.state.workspaces.get(ws_idx)?;
        ws.worktree_space()
            .map(|space| space.label.clone())
            .or_else(|| ws.git_space().map(|space| space.label.clone()))
    }

    fn resolve_message_target(
        &mut self,
        target: &MessageTarget,
    ) -> Result<(usize, crate::layout::PaneId), (&'static str, String)> {
        match target {
            MessageTarget::Pane { pane } => match self.resolve_terminal_target(pane) {
                Ok(resolved) => Ok((resolved.ws_idx, resolved.pane_id)),
                Err(err) => {
                    let body = self.agent_target_error_body(err);
                    let code = if body.code == "agent_target_ambiguous" {
                        "msg_target_ambiguous"
                    } else {
                        "msg_target_not_found"
                    };
                    Err((code, body.message))
                }
            },
            MessageTarget::RepoPane { repo, pane } => {
                let mut matches: Vec<(usize, crate::layout::PaneId)> = Vec::new();
                for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
                    let repo_label = ws
                        .worktree_space()
                        .map(|space| space.label.clone())
                        .or_else(|| ws.git_space().map(|space| space.label.clone()));
                    if repo_label.as_deref() != Some(repo.as_str()) {
                        continue;
                    }
                    for tab in &ws.tabs {
                        for (pane_id, pane_state) in &tab.panes {
                            let public = self.public_pane_id(ws_idx, *pane_id);
                            let public_matches = public
                                .as_deref()
                                .is_some_and(|id| id == pane || id.ends_with(&format!(":{pane}")));
                            let agent_matches = self
                                .state
                                .terminals
                                .get(&pane_state.attached_terminal_id)
                                .is_some_and(|terminal| {
                                    terminal.agent_name.as_deref() == Some(pane.as_str())
                                        || terminal.effective_agent_label() == Some(pane.as_str())
                                });
                            if public_matches || agent_matches {
                                matches.push((ws_idx, *pane_id));
                            }
                        }
                    }
                }
                match matches.len() {
                    0 => Err((
                        "msg_target_not_found",
                        format!("no pane {pane} in a workspace of repo {repo}"),
                    )),
                    1 => Ok(matches[0]),
                    _ => Err((
                        "msg_target_ambiguous",
                        format!("pane {pane} is ambiguous within repo {repo}"),
                    )),
                }
            }
        }
    }

    /// Deliver queued messages to every recipient pane that is Idle and has
    /// dwelled past the attention settle (#175 M1/M2). Polled from the
    /// scheduled tick of BOTH event loops (the #25 dual-loop lesson) rather
    /// than hooked on the Idle→Done completion edge: that edge only commits
    /// while a completion stays UNSEEN, so any read (API poll, operator
    /// glance) swallowed it and stranded the mailbox — live-e2e finding.
    /// Dwell-gating also delivers to a recipient that was already idle when
    /// the message arrived, instead of waiting for its next unrelated turn.
    pub(crate) fn deliver_due_messages(&mut self, now_instant: std::time::Instant) {
        let now = now_ms();
        for expired in self.mailboxes.expire(now) {
            self.emit_event(EventEnvelope {
                event: EventKind::MessageDelivered,
                data: EventData::MessageDelivered {
                    correlation_id: expired.correlation_id.clone(),
                    delivered: false,
                    outcome: "dropped_undeliverable".into(),
                    delivery_attempts: expired.delivery_attempts,
                    latency_ms: now.saturating_sub(expired.enqueued_at_ms),
                },
            });
        }
        for public in self.mailboxes.queued_pane_ids() {
            let Ok(resolved) = self.resolve_terminal_target(&public) else {
                continue;
            };
            let (ws_idx, pane_id) = (resolved.ws_idx, resolved.pane_id);
            // Turn-boundary gate: Idle, and settled long enough that this is
            // a real boundary rather than a between-chunks flicker (§8.3).
            let at_boundary = self
                .state
                .workspaces
                .get(ws_idx)
                .and_then(|ws| ws.pane_state(pane_id))
                .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
                .is_some_and(|terminal| {
                    terminal.state == crate::detect::AgentState::Idle
                        && terminal.state_changed_at.is_some_and(|changed_at| {
                            now_instant.duration_since(changed_at)
                                >= crate::app::actions::ATTENTION_SETTLE
                        })
                });
            if !at_boundary {
                continue;
            }
            while let Some(mut message) = self.mailboxes.pop_next(&public) {
                match self.try_deliver_message(ws_idx, pane_id, &message) {
                    Ok(generic) => {
                        self.mailboxes.record_delivered(&message);
                        self.emit_event(EventEnvelope {
                            event: EventKind::MessageDelivered,
                            data: EventData::MessageDelivered {
                                correlation_id: message.correlation_id.clone(),
                                delivered: true,
                                outcome: if generic {
                                    "delivered_generic".into()
                                } else {
                                    "delivered".into()
                                },
                                delivery_attempts: message.delivery_attempts + 1,
                                latency_ms: now.saturating_sub(message.enqueued_at_ms),
                            },
                        });
                    }
                    Err(()) => {
                        message.delivery_attempts += 1;
                        if message.delivery_attempts >= 8 {
                            self.emit_event(EventEnvelope {
                                event: EventKind::MessageDelivered,
                                data: EventData::MessageDelivered {
                                    correlation_id: message.correlation_id.clone(),
                                    delivered: false,
                                    outcome: "dropped_undeliverable".into(),
                                    delivery_attempts: message.delivery_attempts,
                                    latency_ms: now.saturating_sub(message.enqueued_at_ms),
                                },
                            });
                        } else {
                            self.mailboxes.requeue_front(message);
                        }
                        break;
                    }
                }
            }
        }
    }

    /// Type the message into the settled pane and submit it as a turn (M2).
    /// `Err(())` means "retry at a later boundary" — the queue is the
    /// backpressure, never a mid-turn injection.
    fn try_deliver_message(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        message: &PendingMessage,
    ) -> Result<bool, ()> {
        let agent = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.pane_state(pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
            .and_then(|terminal| terminal.effective_agent_label())
            .map(str::to_string);
        // Unknown agent ⇒ hold: a message must never be typed into a bare
        // shell prompt at Idle.
        let Some(agent) = agent else {
            return Err(());
        };
        let plan = crate::agent_submit::submit_plan(&agent);
        let rendered = render_message(message);
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return Err(());
        };
        // One write: a text-succeeded/enter-failed split would re-queue the
        // whole message and visibly retype the body at the next boundary.
        let mut payload = crate::app::api_helpers::encode_api_text(runtime, &rendered);
        payload.extend_from_slice(&plan.submit_bytes);
        if runtime.try_send_bytes(bytes::Bytes::from(payload)).is_err() {
            return Err(());
        }
        Ok(plan.generic)
    }
}

/// The message as the recipient agent sees it: sender stamp + correlation
/// id (routing/audit — P3) and the reply affordance, then the body.
fn render_message(message: &PendingMessage) -> String {
    let from = message.from_pane.as_deref().unwrap_or("unknown");
    format!(
        "[flk msg {} from {}] {}\n(reply with: flk msg reply {} <text>)",
        message.correlation_id, from, message.body, message.correlation_id
    )
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{
        ErrorResponse, EventData, MessageTarget, Method, MsgListParams, MsgReplyParams,
        MsgSendParams, Request, ResponseResult, SuccessResponse,
    };
    use crate::config::Config;

    fn test_app_with_hub(hub: crate::api::EventHub) -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(&Config::default(), true, None, api_rx, hub);
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("alpha"),
            crate::workspace::Workspace::test_new("beta"),
        ];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app
    }

    fn send(app: &mut crate::app::App, params: MsgSendParams) -> String {
        app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgSend(params),
        })
    }

    fn pane_target(app: &crate::app::App, ws_idx: usize) -> String {
        let ws = &app.state.workspaces[ws_idx];
        let pane_id = ws.focused_pane_id().expect("pane");
        app.public_pane_id(ws_idx, pane_id).expect("public id")
    }

    fn basic_send(app: &mut crate::app::App, correlation: &str, body: &str) -> String {
        let to = pane_target(app, 1);
        send(
            app,
            MsgSendParams {
                to: MessageTarget::Pane { pane: to },
                body: body.into(),
                correlation_id: Some(correlation.into()),
                in_reply_to: None,
            },
        )
    }

    #[tokio::test]
    async fn msg_send_queues_emits_durable_event_and_dedupes() {
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());

        let response = basic_send(&mut app, "c-1", "hello beta");
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgQueued {
            correlation_id,
            state,
            ..
        } = success.result
        else {
            panic!("expected msg_queued: {response}");
        };
        assert_eq!(correlation_id, "c-1");
        assert_eq!(state, "queued");
        let queued_events = hub
            .events_after(0)
            .into_iter()
            .filter(|(_, envelope)| matches!(envelope.data, EventData::MessageQueued { .. }))
            .count();
        assert_eq!(queued_events, 1, "durable MessageQueued emitted");

        // §8.6: same correlation id again — deduped, no second event.
        let response = basic_send(&mut app, "c-1", "hello beta");
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgQueued { state, .. } = success.result else {
            panic!("expected msg_queued");
        };
        assert_eq!(state, "duplicate");
        let queued_events = hub
            .events_after(0)
            .into_iter()
            .filter(|(_, envelope)| matches!(envelope.data, EventData::MessageQueued { .. }))
            .count();
        assert_eq!(queued_events, 1, "no duplicate event");
    }

    #[tokio::test]
    async fn msg_send_refuses_empty_bodies_and_unknown_targets() {
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let response = send(
            &mut app,
            MsgSendParams {
                to: MessageTarget::Pane {
                    pane: "w9:p9".into(),
                },
                body: "   ".into(),
                correlation_id: None,
                in_reply_to: None,
            },
        );
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_request");

        let response = send(
            &mut app,
            MsgSendParams {
                to: MessageTarget::Pane {
                    pane: "no-such".into(),
                },
                body: "hi".into(),
                correlation_id: None,
                in_reply_to: None,
            },
        );
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "msg_target_not_found");

        let response = send(
            &mut app,
            MsgSendParams {
                to: MessageTarget::RepoPane {
                    repo: "ghost-repo".into(),
                    pane: "p1".into(),
                },
                body: "hi".into(),
                correlation_id: None,
                in_reply_to: None,
            },
        );
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "msg_target_not_found");
    }

    #[tokio::test]
    async fn msg_send_rate_limits_the_sender() {
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        for index in 0..crate::app::mailboxes::RATE_LIMIT_PER_MINUTE {
            let response = basic_send(&mut app, &format!("c-{index}"), "hi");
            assert!(
                serde_json::from_str::<SuccessResponse>(&response).is_ok(),
                "send {index} admitted: {response}"
            );
        }
        let response = basic_send(&mut app, "c-over", "hi");
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "msg_rate_limited");
    }

    #[tokio::test]
    async fn msg_reply_routes_errors_without_a_known_original() {
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgReply(MsgReplyParams {
                correlation_id: "nope".into(),
                body: "hi".into(),
                reply_correlation_id: None,
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "message_not_found");

        // A queued message with no sender stamp cannot be replied to.
        basic_send(&mut app, "c-orig", "question");
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgReply(MsgReplyParams {
                correlation_id: "c-orig".into(),
                body: "answer".into(),
                reply_correlation_id: None,
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "no_reply_address");
    }

    #[tokio::test]
    async fn msg_list_shows_queued_messages_and_delivery_retry_holds_them() {
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        basic_send(&mut app, "c-list", "queued body");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgList(MsgListParams::default()),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgList { messages } = success.result else {
            panic!("expected msg_list");
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].correlation_id, "c-list");
        assert_eq!(messages[0].delivery_attempts, 0);

        // Dwell-settled Idle recipient: no agent label on the test
        // terminal, so the message is HELD (never typed into a bare prompt)
        // with a bumped attempt count — the §8.3 unknown-agent gate.
        let ws_idx = 1;
        let pane_id = app.state.workspaces[ws_idx].focused_pane_id().unwrap();
        let terminal_id = app.state.workspaces[ws_idx]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .clone();
        let now = std::time::Instant::now();
        {
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.state = crate::detect::AgentState::Idle;
            terminal.state_changed_at = Some(now - crate::app::actions::ATTENTION_SETTLE * 2);
        }
        // A Working recipient is never drained (§8.3 mid-turn guard).
        {
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.state = crate::detect::AgentState::Working;
        }
        app.deliver_due_messages(now);
        {
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.state = crate::detect::AgentState::Idle;
        }
        app.deliver_due_messages(now);

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgList(MsgListParams::default()),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgList { messages } = success.result else {
            panic!("expected msg_list");
        };
        assert_eq!(messages.len(), 1, "message held for retry");
        assert_eq!(messages[0].delivery_attempts, 1);
    }
}
