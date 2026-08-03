use crate::api::schema::{
    EventData, EventEnvelope, EventKind, MessageTarget, MsgListParams, MsgReadParams,
    MsgReplyParams, MsgSendParams, ResponseResult,
};
use crate::app::mailboxes::{EnqueueOutcome, PendingMessage};
use crate::app::App;

/// Where a message target resolved to.
///
/// A typed outcome rather than an error code carrying a delimiter-packed
/// string. The packed form existed briefly and immediately produced the bug it
/// invites: the writer emitted two fields, the reader expected three, and a
/// cross-host send reported "agent lives on agent_vm-dev_…" with the id in the
/// host slot. Two functions in one module do not need a wire format between
/// them.
enum ResolvedTarget {
    Local(usize, crate::layout::PaneId),
    Remote(Box<crate::app::directory::AgentLocation>),
}

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

        // Resolved to another host: hand the message to the server that owns
        // the recipient and let ITS mailbox do the rest. One delivery
        // implementation, wherever the sender was.
        let resolved = match self.resolve_message_target(&params.to) {
            Ok(resolved) => resolved,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let (to_ws_idx, to_pane_id) = match resolved {
            ResolvedTarget::Local(ws_idx, pane_id) => (ws_idx, pane_id),
            ResolvedTarget::Remote(location) => {
                return self.relay_message_to_host(id, &location, &body, params)
            }
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
        // Provenance is a claim PLUS whatever this server could attest, kept
        // apart. Locally, ancestry attests an identity. A message relayed from
        // another host has no local ancestry — that is precisely why it used
        // to arrive anonymous — so the relay's asserted `from_agent` carries
        // it instead. Absence of proof must not mean absence of a name.
        let attested_agent = sender.and_then(|(ws_idx, pane_id)| {
            let ws = self.state.workspaces.get(ws_idx)?;
            let terminal = self
                .state
                .terminals
                .get(&ws.pane_state(pane_id)?.attached_terminal_id)?;
            Some(terminal.agent_id.to_string())
        });
        // The sender's host: asserted by a relay, else this host when WE
        // attested the sender locally. Never guessed — a relayed message whose
        // sender the local directory cannot see used to fall back to
        // `short_host_name()` and claim the recipient's own host as the
        // origin, which read as "sage sent this" on the machine that received
        // it from sage.
        let from_host = params.from_host.clone().or_else(|| {
            attested_agent
                .as_ref()
                .map(|_| crate::app::short_host_name())
        });
        let from_agent = attested_agent.or_else(|| params.from_agent.clone());

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
            from_agent,
            from_host,
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
                    meta.from_agent.clone(),
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
                            message.from_agent.clone(),
                            message.enqueued_at_ms,
                            message.correlation_id.clone(),
                        )
                    })
            });
        let Some((original_sender, original_agent, original_enqueued_ms, root)) = original else {
            return encode_error(
                id,
                "message_not_found",
                format!("no message with correlation id {}", params.correlation_id),
            );
        };
        // Reply address: the fleet-global identity FIRST. A pane id only means
        // something on the server that minted it, so replying to a message
        // that crossed a machine boundary has to go through the identity —
        // that is the whole reason a cross-host reply used to be impossible.
        let reply_target = match &original_agent {
            Some(agent) => MessageTarget::Agent {
                agent: agent.clone(),
            },
            None => match &original_sender {
                Some(pane) => MessageTarget::Pane { pane: pane.clone() },
                None => {
                    return encode_error(
                        id,
                        "no_reply_address",
                        "the original message carried no sender to reply to",
                    )
                }
            },
        };
        let reply_resolved = match self.resolve_message_target(&reply_target) {
            Ok(resolved) => resolved,
            Err((code, message)) => return encode_error(id, code, message),
        };
        let (to_ws_idx, to_pane_id) = match reply_resolved {
            ResolvedTarget::Local(ws_idx, pane_id) => (ws_idx, pane_id),
            // The original sender is on another host: relay the reply the same
            // way the message came.
            ResolvedTarget::Remote(location) => {
                return self.relay_message_to_host(
                    id,
                    &location,
                    &body,
                    MsgSendParams {
                        from_agent: None,
                        from_host: None,
                        to: reply_target.clone(),
                        body: body.clone(),
                        correlation_id: params.reply_correlation_id.clone(),
                        in_reply_to: Some(params.correlation_id.clone()),
                    },
                );
            }
        };
        let Some(to_pane) = self.public_pane_id(to_ws_idx, to_pane_id) else {
            return encode_error(id, "internal_error", "reply pane has no public id");
        };

        let sender = self.parse_pane_id_or_peer("", self.current_api_peer_pid);
        let from_pane = sender.and_then(|(ws_idx, pane_id)| self.public_pane_id(ws_idx, pane_id));
        let from_repo = sender.and_then(|(ws_idx, _)| self.workspace_repo_label(ws_idx));
        let attested_agent = sender.and_then(|(ws_idx, pane_id)| {
            let ws = self.state.workspaces.get(ws_idx)?;
            let terminal = self
                .state
                .terminals
                .get(&ws.pane_state(pane_id)?.attached_terminal_id)?;
            Some(terminal.agent_id.to_string())
        });
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
            from_agent: attested_agent,
            from_host: Some(crate::app::short_host_name()),
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

    /// `msg.read` — the recipient consumes its inbox (ADR-0008).
    ///
    /// This is where "delivered" now happens. Under pane injection, delivery
    /// meant flock typed the body into a TTY and hoped the agent read it;
    /// here it means the agent actually took the message. The event is
    /// emitted on the same edge, so the audit trail gets truer rather than
    /// noisier.
    pub(super) fn handle_msg_read(&mut self, id: String, params: MsgReadParams) -> String {
        let pane = match params.pane {
            Some(pane) => match self.resolve_terminal_target(&pane) {
                Ok(resolved) => self.public_pane_id(resolved.ws_idx, resolved.pane_id),
                Err(err) => {
                    return super::responses::encode_error_body(
                        id,
                        self.agent_target_error_body(err),
                    )
                }
            },
            // Omitted: the caller's own pane, resolved the way `msg.send`
            // resolves its sender.
            None => self
                .parse_pane_id_or_peer("", self.current_api_peer_pid)
                .and_then(|(ws_idx, pane_id)| self.public_pane_id(ws_idx, pane_id)),
        };
        let Some(pane) = pane else {
            return encode_error(
                id,
                "msg_target_not_found",
                "no pane to read: pass `pane`, or call from inside the pane whose inbox you want",
            );
        };

        let now = now_ms();
        let mut messages = Vec::new();
        while let Some(message) = self.mailboxes.pop_next(&pane) {
            self.mailboxes.record_delivered(&message);
            self.emit_event(EventEnvelope {
                event: EventKind::MessageDelivered,
                data: EventData::MessageDelivered {
                    correlation_id: message.correlation_id.clone(),
                    delivered: true,
                    outcome: "read".into(),
                    delivery_attempts: message.delivery_attempts + 1,
                    latency_ms: now.saturating_sub(message.enqueued_at_ms),
                },
            });
            messages.push(crate::api::schema::InboxMessage {
                correlation_id: message.correlation_id.clone(),
                from_agent: message.from_agent.clone(),
                from_host: message.from_host.clone(),
                // `replyable` is computed from the same field `msg.reply`
                // routes on, so the recipient is never told it can reply when
                // it cannot — the failure #213 was filed for.
                // Replyable when there is ANY routable sender: a local pane, or
                // a fleet-global identity the directory can still resolve.
                replyable: message.from_pane.is_some() || message.from_agent.is_some(),
                from_pane: message.from_pane.clone(),
                from_repo: message.from_repo.clone(),
                to_pane: message.to_pane.clone(),
                in_reply_to: message.in_reply_to.clone(),
                enqueued_at_ms: message.enqueued_at_ms,
                body: message.body.clone(),
            });
        }
        messages.sort_by_key(|message| message.enqueued_at_ms);
        encode_success(id, ResponseResult::MsgRead { messages })
    }

    /// Hand a message to the peer that owns the recipient (ADR-0008).
    ///
    /// The sender's own identity travels with it: the receiving server has no
    /// local process ancestry to attest from, so without an asserted
    /// `from_agent` the message would arrive anonymous and unreplyable —
    /// exactly the failure that started this.
    fn relay_message_to_host(
        &mut self,
        id: String,
        location: &crate::app::directory::AgentLocation,
        body: &str,
        params: MsgSendParams,
    ) -> String {
        let route = location.route.as_deref().unwrap_or_default();
        let host = location.host.as_str();
        let to_agent = location.agent_id.as_str();
        // Route by the peer entry the DIRECTORY answered from, not by the name
        // the far machine calls itself. Those differ in any normal fleet — a
        // peer configured as `anvil` reports its hostname as `vm-dev` — and
        // matching on the reported host is exactly why the first live
        // cross-host send came back "not in this server's [[peers]]". Falls
        // back to the host for a directory answer that carried no route.
        let Some(peer) = self
            .state
            .peers
            .iter()
            .find(|peer| {
                (!route.is_empty() && peer.name.eq_ignore_ascii_case(route))
                    || peer.name.eq_ignore_ascii_case(host)
                    || peer.ssh_target().eq_ignore_ascii_case(host)
            })
            .cloned()
        else {
            return encode_error(
                id,
                "peer_not_configured",
                format!(
                    "agent lives on {host}, which is not in this server's [[peers]] — add it to \
                     reach that agent"
                ),
            );
        };

        // The sender is whoever asked, attested locally where possible.
        let sender = self.parse_pane_id_or_peer("", self.current_api_peer_pid);
        let from_agent = sender
            .and_then(|(ws_idx, pane_id)| {
                let ws = self.state.workspaces.get(ws_idx)?;
                let terminal = self
                    .state
                    .terminals
                    .get(&ws.pane_state(pane_id)?.attached_terminal_id)?;
                Some(terminal.agent_id.to_string())
            })
            .or_else(|| params.from_agent.clone());
        let Some(from_agent) = from_agent else {
            return encode_error(
                id,
                "sender_unresolved",
                "cross-host delivery needs a sender identity: call from inside a pane, or pass \
                 from_agent",
            );
        };

        let correlation_id = params
            .correlation_id
            .filter(|explicit| !explicit.trim().is_empty())
            .unwrap_or_else(mint_correlation_id);

        match crate::peers::send_peer_message(
            &peer,
            to_agent,
            &from_agent,
            &crate::app::short_host_name(),
            body,
            &correlation_id,
        ) {
            Ok(()) => encode_success(
                id,
                ResponseResult::MsgQueued {
                    correlation_id,
                    state: "relayed".into(),
                    warnings: Vec::new(),
                },
            ),
            Err(err) => encode_error(
                id,
                "peer_unreachable",
                format!("could not reach {host}: {err}"),
            ),
        }
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
            from_agent: message.from_agent.clone(),
            from_host: message.from_host.clone(),
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
        // #197: the action view — repo addressing must name the repo the pane
        // is in, not one its grouping still remembers.
        ws.worktree_space_here()
            .map(|space| space.label.clone())
            .or_else(|| ws.git_space().map(|space| space.label.clone()))
    }

    fn resolve_message_target(
        &mut self,
        target: &MessageTarget,
    ) -> Result<ResolvedTarget, (&'static str, String)> {
        match target {
            MessageTarget::Pane { pane } => match self.resolve_terminal_target(pane) {
                Ok(resolved) => Ok(ResolvedTarget::Local(resolved.ws_idx, resolved.pane_id)),
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
            // ADR-0008: address by identity. Resolution goes through the ONE
            // fleet directory, so messaging, targeting and lineage cannot
            // drift into separate answers for "where is this agent".
            MessageTarget::Agent { agent } => {
                let Some(location) = self.locate_agent(agent) else {
                    return Err((
                        "msg_target_not_found",
                        format!("no agent with id {agent} anywhere in the fleet"),
                    ));
                };
                if !location.local {
                    // Not ours to deliver. Handing back the whole location
                    // keeps resolution and delivery separate concerns without
                    // inventing a format between them.
                    return Ok(ResolvedTarget::Remote(Box::new(location)));
                }
                match self.resolve_terminal_target(&location.pane_id) {
                    Ok(resolved) => Ok(ResolvedTarget::Local(resolved.ws_idx, resolved.pane_id)),
                    Err(err) => {
                        let body = self.agent_target_error_body(err);
                        Err(("msg_target_not_found", body.message))
                    }
                }
            }
            MessageTarget::RepoPane { repo, pane } => {
                let mut matches: Vec<(usize, crate::layout::PaneId)> = Vec::new();
                for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
                    let repo_label = ws
                        .worktree_space_here()
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
                    1 => Ok(ResolvedTarget::Local(matches[0].0, matches[0].1)),
                    _ => Err((
                        "msg_target_ambiguous",
                        format!("pane {pane} is ambiguous within repo {repo}"),
                    )),
                }
            }
        }
    }

    /// Expire messages nobody read in time.
    ///
    /// ADR-0008: this used to also DELIVER, by typing each message into its
    /// recipient's pane. That is gone — agents read their own inbox with
    /// `msg.read`, woken at a turn boundary by the stop hook. With the
    /// keystrokes went the machinery they required: the `Idle` +
    /// `ATTENTION_SETTLE` dwell that made typing safe, the retry/backoff for a
    /// pane that was mid-turn, and the refusal that kept a message out of a
    /// bare shell prompt. A pull inbox has no such hazards, so what remains is
    /// the one thing still time-based — the TTL sweep.
    pub(crate) fn expire_undeliverable_messages(&mut self) {
        // US-9 (#175 S3 commit 3): fleet pause halts the mailbox clock, so a
        // paused fleet does not quietly age messages out.
        if self.fleet_pause.paused {
            return;
        }
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
    }
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
                from_agent: None,
                from_host: None,
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
                from_agent: None,
                from_host: None,
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
                from_agent: None,
                from_host: None,
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
                from_agent: None,
                from_host: None,
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
    async fn a_remote_agent_is_relayed_not_refused() {
        // ADR-0008 end state: addressing an agent that lives on another host
        // is a ROUTING decision, not a failure. Without a peer entry for that
        // host we cannot reach it — but the refusal names the host, which is
        // already more than the "from unknown" dead end this started as.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        app.state.peer_summaries = vec![{
            let mut peer = crate::peers::PeerSummaryState::new(&crate::config::PeerConfig {
                name: "anvil".into(),
                ..Default::default()
            });
            peer.host = Some("anvil-dev".into());
            peer.workspaces = vec![crate::api::schema::PeerWorkspaceSummary {
                id: "w1".into(),
                workspace: "remote".into(),
                project_key: None,
                project_label: None,
                branch: None,
                is_linked_worktree: false,
                agent: Some("cc".into()),
                status: crate::api::schema::AgentStatus::Idle,
                status_age_secs: None,
                activity: None,
                agents: vec![crate::api::schema::PeerAgentSummary {
                    agent_id: "agent_anvil-dev_beef".into(),
                    pane_id: "w1:p1".into(),
                    agent: Some("cc".into()),
                    status: crate::api::schema::AgentStatus::Idle,
                }],
            }];
            peer
        }];

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgSend(MsgSendParams {
                from_agent: None,
                from_host: None,
                to: MessageTarget::Agent {
                    agent: "agent_anvil-dev_beef".into(),
                },
                body: "cross-host".into(),
                correlation_id: Some("c-remote".into()),
                in_reply_to: None,
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        // Routed, then blocked on reachability — never "unknown agent".
        assert_eq!(error.error.code, "peer_not_configured");
        assert!(
            error.error.message.contains("anvil-dev"),
            "the refusal must name where the agent is: {}",
            error.error.message
        );
    }

    #[tokio::test]
    async fn an_agent_that_exists_nowhere_is_a_clean_miss() {
        // Routing to the wrong agent is worse than refusing, so an unknown
        // identity must not fall back to any pane.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgSend(MsgSendParams {
                from_agent: None,
                from_host: None,
                to: MessageTarget::Agent {
                    agent: "agent_nowhere_0".into(),
                },
                body: "hello".into(),
                correlation_id: None,
                in_reply_to: None,
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "msg_target_not_found");
    }

    #[tokio::test]
    async fn a_relayed_message_keeps_its_senders_identity_and_stays_replyable() {
        // The failure that started all of this: a message from another host
        // arrived with no sender and a reply command that could not work.
        // A relay asserts `from_agent`, so the recipient reads a named sender
        // and `replyable` is true.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let to_pane = app
            .state
            .workspaces
            .get(1)
            .and_then(|ws| ws.focused_pane_id())
            .and_then(|pane_id| app.public_pane_id(1, pane_id))
            .expect("recipient pane");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgSend(MsgSendParams {
                // No local ancestry attests this — it came off the wire.
                from_agent: Some("agent_mba22_cafe".into()),
                from_host: Some("mba22".into()),
                to: MessageTarget::Pane {
                    pane: to_pane.clone(),
                },
                body: "from another machine".into(),
                correlation_id: Some("c-relayed".into()),
                in_reply_to: None,
            }),
        });
        assert!(!response.contains("\"error\""), "{response}");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgRead(crate::api::schema::MsgReadParams {
                pane: Some(to_pane),
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgRead { messages } = success.result else {
            panic!("expected msg_read: {response}");
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from_agent.as_deref(), Some("agent_mba22_cafe"));
        // The relay's asserted host, not the receiver's own — a receiver that
        // guesses reports itself as the origin.
        assert_eq!(messages[0].from_host.as_deref(), Some("mba22"));
        assert!(
            messages[0].replyable,
            "a named sender must be replyable even with no local pane"
        );
    }

    #[tokio::test]
    async fn msg_list_peeks_and_msg_read_consumes() {
        // ADR-0008: two verbs, two side effects. `msg.list` is the operator's
        // view and must never consume; `msg.read` is the recipient taking its
        // mail, and a second read finds an empty inbox.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        basic_send(&mut app, "c-list", "queued body");

        let peek = |app: &mut crate::app::App| -> usize {
            let response = app.handle_api_request(Request {
                id: "req".into(),
                method: Method::MsgList(MsgListParams::default()),
            });
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            let ResponseResult::MsgList { messages } = success.result else {
                panic!("expected msg_list");
            };
            messages.len()
        };

        assert_eq!(peek(&mut app), 1);
        assert_eq!(peek(&mut app), 1, "listing must not consume");

        let to_pane = app
            .state
            .workspaces
            .get(1)
            .and_then(|ws| ws.focused_pane_id())
            .and_then(|pane_id| app.public_pane_id(1, pane_id))
            .expect("recipient pane");
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgRead(crate::api::schema::MsgReadParams {
                pane: Some(to_pane),
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgRead { messages } = success.result else {
            panic!("expected msg_read: {response}");
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].correlation_id, "c-list");
        // The whole body, not a preview — the recipient acts on this.
        assert_eq!(messages[0].body, "queued body");

        assert_eq!(peek(&mut app), 0, "read consumed the inbox");
    }

    #[tokio::test]
    async fn msg_read_marks_delivered_on_the_read_edge() {
        // "Delivered" now means the agent took it, not that flock typed it
        // into a TTY and hoped.
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        basic_send(&mut app, "c-read", "body");

        let delivered = |hub: &crate::api::EventHub| {
            hub.events_after(0)
                .iter()
                .filter(|(_, event)| {
                    matches!(
                        &event.data,
                        EventData::MessageDelivered { correlation_id, outcome, .. }
                            if correlation_id == "c-read" && outcome == "read"
                    )
                })
                .count()
        };
        assert_eq!(delivered(&hub), 0, "queued is not delivered");

        let to_pane = app
            .state
            .workspaces
            .get(1)
            .and_then(|ws| ws.focused_pane_id())
            .and_then(|pane_id| app.public_pane_id(1, pane_id))
            .expect("recipient pane");
        let _ = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgRead(crate::api::schema::MsgReadParams {
                pane: Some(to_pane),
            }),
        });
        assert_eq!(delivered(&hub), 1);
    }

    /// #175 S3 commit 3 (US-9): fleet pause halts the mailbox clock. Under
    /// ADR-0008 there is no scheduled delivery left to gate — what pause must
    /// still hold is the TTL sweep, so a paused fleet does not quietly age
    /// messages out from under a recipient that never got the chance to read.
    #[tokio::test]
    async fn fleet_pause_holds_the_ttl_sweep() {
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        let _ = basic_send(&mut app, "c-pause", "hi during pause");

        let dropped = |hub: &crate::api::EventHub| {
            hub.events_after(0)
                .iter()
                .filter(|(_, event)| {
                    matches!(
                        &event.data,
                        EventData::MessageDelivered { correlation_id, outcome, .. }
                            if correlation_id == "c-pause" && outcome == "dropped_undeliverable"
                    )
                })
                .count()
        };

        // Age the message past its TTL, then sweep while paused.
        app.mailboxes
            .test_age_all(crate::app::mailboxes::UNDELIVERED_TTL_MS + 1);
        app.fleet_pause.paused = true;
        app.expire_undeliverable_messages();
        assert_eq!(dropped(&hub), 0, "paused fleet must not age messages out");

        app.fleet_pause.paused = false;
        app.expire_undeliverable_messages();
        assert_eq!(dropped(&hub), 1, "resume lets the sweep run");
    }
}
