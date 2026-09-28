use crate::api::schema::{
    EventData, EventEnvelope, EventKind, MessageTarget, MsgIntent, MsgListParams, MsgReadParams,
    MsgReplyParams, MsgSendParams, MsgStatusParams, ResponseResult,
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

use super::responses::{encode_error, encode_error_with_data, encode_success};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// The durable record of a queued message — the mailbox's restart source.
fn queued_event(message: &PendingMessage) -> EventData {
    EventData::MessageQueued {
        correlation_id: message.correlation_id.clone(),
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
        intent: message.intent,
        body: message.body.clone(),
    }
}

/// A mute's `reason`, made fit to quote to other agents: control sequences
/// stripped by the same sanitiser message bodies go through, folded onto one
/// line, and cut to `max_chars`. `None` when nothing is left to say.
fn bound_mute_reason(reason: &str, max_chars: usize) -> Option<String> {
    let cleaned = crate::app::api_helpers::sanitize_reported_prompt(reason);
    let folded = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.is_empty() || max_chars == 0 {
        return None;
    }
    if folded.chars().count() <= max_chars {
        return Some(folded);
    }
    let mut cut: String = folded.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    Some(cut)
}

/// What a deferred sender reads (ADR-0018 §3): that it was deferred, why if
/// the recipient said, and the absolute time the mute lifts — a deadline the
/// sender can plan around, in UTC so it means the same on every host.
fn deferral_body(muted_until_ms: u64, reason: Option<&str>) -> String {
    let until = crate::digest::utc_timestamp(muted_until_ms);
    let reason = reason
        .map(|reason| format!(" Reason given: {reason}."))
        .unwrap_or_default();
    format!(
        "Automatic reply: the recipient has muted its inbox and deferred your message until \
         {until} (muted_until_ms={muted_until_ms}).{reason} Your message is still queued and \
         nothing was lost; expect an answer after the mute lifts. This notice needs no reply."
    )
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
        let from_agent = attested_agent.clone().or_else(|| params.from_agent.clone());

        // Allow policy (ADR-0008), enforced HERE because the receiver is the
        // only party that cannot be bypassed — a sender-side check is advice,
        // and once the hub forwards for spokes, reachability is no longer
        // gated by which SSH keys happen to exist. A locally-attested sender
        // has no remote party to gate, so it is always accepted.
        let origin_host = if attested_agent.is_some() {
            None
        } else {
            params.from_host.as_deref()
        };
        if !self.state.config.msg.accepts_from(origin_host) {
            return encode_error(
                id,
                "msg_not_allowed",
                match origin_host {
                    Some(host) => format!(
                        "this node does not accept agent messages from {host} \
                         (see [msg] allow_from)"
                    ),
                    None => "this node does not accept agent messages ([msg] enabled = false)"
                        .to_string(),
                },
            );
        }

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
            intent: params.intent,
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
                        intent: params.intent,
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
            intent: params.intent,
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

    /// `msg.status` — what became of one message, asked by whoever sent it.
    ///
    /// Read from the durable log rather than the mailbox: a message that was
    /// already consumed, aged out, or relayed away is gone from the queue,
    /// and those are precisely the outcomes a sender wants to ask about.
    ///
    /// The last event for a correlation id wins. Ordering is by sequence, and
    /// #175 O1 makes that monotonic across restarts, so "last" is a real
    /// ordering rather than whatever the file happened to yield.
    pub(super) fn handle_msg_status(&mut self, id: String, params: MsgStatusParams) -> String {
        let mut found: Option<ResponseResult> = None;
        for (_, event) in self.event_hub.events_after(0) {
            match &event.data {
                EventData::MessageQueued { correlation_id, .. }
                    if *correlation_id == params.correlation_id =>
                {
                    found = Some(ResponseResult::MsgStatus {
                        correlation_id: params.correlation_id.clone(),
                        state: "queued".into(),
                        outcome_known: true,
                        to_host: None,
                        route: None,
                        detail: Some("waiting in a local inbox, not yet read".into()),
                    });
                }
                EventData::MessageRelayed {
                    correlation_id,
                    to_host,
                    route,
                    ..
                } if *correlation_id == params.correlation_id => {
                    found = Some(ResponseResult::MsgStatus {
                        correlation_id: params.correlation_id.clone(),
                        state: "relayed".into(),
                        // The receiving node owns the outcome. Saying so beats
                        // implying delivery we cannot see.
                        outcome_known: false,
                        to_host: Some(to_host.clone()),
                        route: Some(route.clone()),
                        detail: Some(format!(
                            "handed to {to_host} via [[peers]] {route}; whether it was read is \
                             recorded there, not here"
                        )),
                    });
                }
                EventData::MessageDelivered {
                    correlation_id,
                    delivered,
                    outcome,
                    ..
                } if *correlation_id == params.correlation_id => {
                    found = Some(ResponseResult::MsgStatus {
                        correlation_id: params.correlation_id.clone(),
                        state: if *delivered { "read" } else { "dropped" }.into(),
                        outcome_known: true,
                        to_host: None,
                        route: None,
                        detail: Some(outcome.clone()),
                    });
                }
                _ => {}
            }
        }
        match found {
            Some(result) => encode_success(id, result),
            None => encode_error(
                id,
                "message_not_found",
                format!("no message with correlation id {}", params.correlation_id),
            ),
        }
    }

    /// Which inbox a receiver-side verb is about: an explicit `pane` target,
    /// or — omitted — the caller's own pane, resolved the way `msg.send`
    /// resolves its sender. `Err` is the encoded refusal, ready to return.
    ///
    /// Shared by `msg.read`, `msg.wake` and `msg.mute` so the three cannot
    /// drift on what "my inbox" means. They have to agree: a wake counted
    /// against one pane and a read taken from another is an agent told it has
    /// mail that it then cannot find.
    fn resolve_inbox_pane(
        &mut self,
        id: &str,
        pane: Option<String>,
        verb: &str,
    ) -> Result<String, String> {
        let resolved = match pane {
            Some(pane) => match self.resolve_terminal_target(&pane) {
                Ok(resolved) => self.public_pane_id(resolved.ws_idx, resolved.pane_id),
                Err(err) => {
                    return Err(super::responses::encode_error_body(
                        id.to_string(),
                        self.agent_target_error_body(err),
                    ))
                }
            },
            None => self
                .parse_pane_id_or_peer("", self.current_api_peer_pid)
                .and_then(|(ws_idx, pane_id)| self.public_pane_id(ws_idx, pane_id)),
        };
        resolved.ok_or_else(|| {
            encode_error(
                id.to_string(),
                "msg_target_not_found",
                format!(
                    "no pane to {verb}: pass `pane`, or call from inside the pane whose inbox \
                     you want"
                ),
            )
        })
    }

    /// `msg.wake` — the count the wake path is allowed to act on (#316).
    ///
    /// Suppression, not filtering: nothing leaves the mailbox and nothing is
    /// marked delivered. A suppressed wake reports zero and names why; the
    /// messages are still there for `msg.list`, for `msg.read`, and for the
    /// next wake that is allowed to fire.
    pub(super) fn handle_msg_wake(
        &mut self,
        id: String,
        params: crate::api::schema::MsgWakeParams,
    ) -> String {
        let pane = match self.resolve_inbox_pane(&id, params.pane, "wake") {
            Ok(pane) => pane,
            Err(refusal) => return refusal,
        };

        // US-9 (#175 S3 commit 3). Pause halts what FLOCK initiates and
        // exempts human agency on purpose — an operator can still type into a
        // paused pane. A wake is neither: it is flock interrupting an agent
        // on its own initiative, so it sits on the halted side of that line.
        //
        // This gate is a restoration, not a new rule. Pause used to hold the
        // mailbox by gating `deliver_due_messages`; ADR-0008 (#216) deleted
        // that drain along with the keystrokes, and the stop-hook wake that
        // replaced it inherited no gate. A paused fleet went on returning
        // `decision: block` to every agent at every turn boundary.
        if self.fleet_pause.paused {
            return encode_success(
                id,
                ResponseResult::MsgWake {
                    count: 0,
                    suppressed: Some("fleet_paused".into()),
                    muted_until_ms: None,
                },
            );
        }

        let now = now_ms();
        if let Some(until) = self.mailboxes.muted_until(&pane, now) {
            return encode_success(
                id,
                ResponseResult::MsgWake {
                    count: 0,
                    suppressed: Some("muted".into()),
                    muted_until_ms: Some(until),
                },
            );
        }

        encode_success(
            id,
            ResponseResult::MsgWake {
                count: self.mailboxes.queued_len(&pane),
                suppressed: None,
                muted_until_ms: None,
            },
        )
    }

    /// `msg.mute` — a recipient declining to be woken for a bounded window.
    ///
    /// Receiver-side and self-scoped: the pane defaults to the caller's own.
    /// It is not gated on the pause, because it only ever removes wakes — a
    /// receiver quieting itself inside a paused fleet is asking for less, and
    /// refusing that would be pause working against its own purpose.
    ///
    /// A mute must answer (ADR-0018 §3): every waking message ALREADY waiting
    /// is deferred here, and every one that arrives while the mute holds is
    /// deferred as it is queued ([`Self::queue_message`]). A clear
    /// (`seconds: 0`) owes nobody anything and sends nothing.
    pub(super) fn handle_msg_mute(
        &mut self,
        id: String,
        params: crate::api::schema::MsgMuteParams,
    ) -> String {
        let pane = match self.resolve_inbox_pane(&id, params.pane, "mute") {
            Ok(pane) => pane,
            Err(refusal) => return refusal,
        };
        let reason = params.reason.as_deref().and_then(|reason| {
            bound_mute_reason(reason, self.state.config.msg.mute_reason_max_chars)
        });
        let muted_until_ms =
            self.mailboxes
                .set_mute(&pane, params.seconds, now_ms(), reason.clone());
        let mut deferred = 0;
        if muted_until_ms != 0 {
            for message in self.mailboxes.owed_deferrals(&pane) {
                if self.defer_message(&message, muted_until_ms, reason.clone()) {
                    deferred += 1;
                }
            }
        }
        encode_success(
            id,
            ResponseResult::MsgMute {
                muted_until_ms,
                deferred,
            },
        )
    }

    /// Answer one message on behalf of its muted recipient (ADR-0018 §3).
    /// Returns whether an answer was sent (or, cross-host, dispatched).
    ///
    /// The answer is an ordinary reply in every respect that matters to the
    /// sender: `in_reply_to` the message, addressed by the sender's
    /// fleet-global identity first, and routed through the same
    /// resolve-then-relay path `msg.reply` uses, so a sender on another host
    /// hears it the way it would hear a real answer. What it is NOT is
    /// sender-shaped: it speaks for the muted pane, never for whoever's API
    /// call happened to trigger it — at arrival time that caller is the
    /// sender itself.
    ///
    /// Intent is `fyi` by construction, never copied, so two agents muted at
    /// each other cannot ping-pong deferrals (#316 pitfall 7): a deferral is
    /// never owed a deferral.
    fn defer_message(
        &mut self,
        message: &PendingMessage,
        muted_until_ms: u64,
        reason: Option<String>,
    ) -> bool {
        if !self.mailboxes.owes_deferral(message) {
            return false;
        }
        let target = match (&message.from_agent, &message.from_pane) {
            (Some(agent), _) => MessageTarget::Agent {
                agent: agent.clone(),
            },
            (None, Some(pane)) => MessageTarget::Pane { pane: pane.clone() },
            // An anonymous sender has no address. Nothing can reach it, so
            // there is nothing to claim.
            (None, None) => return false,
        };
        // The muted pane is the deferral's sender. Resolved from the
        // message's own recipient, not from the API caller.
        let muter_agent = self
            .resolve_terminal_target(&message.to_pane)
            .ok()
            .and_then(|resolved| {
                let ws = self.state.workspaces.get(resolved.ws_idx)?;
                let terminal = self
                    .state
                    .terminals
                    .get(&ws.pane_state(resolved.pane_id)?.attached_terminal_id)?;
                Some(terminal.agent_id.to_string())
            });
        let deferral_correlation_id = format!("{}:deferred", message.correlation_id);
        let body = deferral_body(muted_until_ms, reason.as_deref());

        let resolved = match self.resolve_message_target(&target) {
            Ok(resolved) => resolved,
            Err((code, detail)) => {
                tracing::warn!(
                    correlation_id = message.correlation_id.as_str(),
                    code,
                    detail = detail.as_str(),
                    "mute deferral: the sender is no longer reachable"
                );
                return false;
            }
        };
        match resolved {
            ResolvedTarget::Local(ws_idx, pane_id) => {
                let Some(to_pane) = self.public_pane_id(ws_idx, pane_id) else {
                    return false;
                };
                let deferral = PendingMessage {
                    correlation_id: deferral_correlation_id.clone(),
                    body,
                    from_pane: Some(message.to_pane.clone()),
                    from_agent: muter_agent,
                    from_host: Some(crate::app::short_host_name()),
                    from_repo: message.to_repo.clone(),
                    to_pane,
                    to_repo: self.workspace_repo_label(ws_idx),
                    in_reply_to: Some(message.correlation_id.clone()),
                    enqueued_at_ms: now_ms(),
                    delivery_attempts: 0,
                    intent: MsgIntent::Fyi,
                };
                // Straight to the mailbox rather than through `queue_message`:
                // the deferral is `fyi`, so it could not trigger another, but
                // not having the recursion at all is cheaper than proving it
                // terminates.
                let event = queued_event(&deferral);
                match self.mailboxes.enqueue(deferral) {
                    EnqueueOutcome::Queued => self.emit_event(EventEnvelope {
                        event: EventKind::MessageQueued,
                        data: event,
                    }),
                    // Already answered, by a run whose `MessageDeferred` never
                    // made it to disk. The sender has its answer.
                    EnqueueOutcome::Duplicate => {}
                    // No room in the sender's own inbox. Leave it unclaimed so
                    // the next mute retries once the sender has read.
                    EnqueueOutcome::MailboxFull => return false,
                }
                self.mailboxes.mark_deferred(&message.correlation_id);
                self.emit_message_deferred(
                    &message.correlation_id,
                    deferral_correlation_id,
                    &message.to_pane,
                    muted_until_ms,
                    reason,
                    None,
                );
                true
            }
            ResolvedTarget::Remote(location) => {
                let Some(peer) = self.peer_for_location(&location) else {
                    return false;
                };
                let Some(from_agent) = muter_agent else {
                    return false;
                };
                // Claim before the hop, so a mute renewed while it is in
                // flight does not dispatch a second one. A failed hop
                // withdraws the claim ([`Self::handle_msg_deferral_relayed`]).
                self.mailboxes.mark_deferred(&message.correlation_id);
                let relay = crate::events::MsgDeferralRelay {
                    correlation_id: message.correlation_id.clone(),
                    deferral_correlation_id,
                    pane: message.to_pane.clone(),
                    muted_until_ms,
                    reason,
                    from_agent,
                    to_agent: location.agent_id.clone(),
                    to_host: location.host.clone(),
                    route: peer.name.clone(),
                    result: Ok(()),
                };
                let event_tx = self.event_tx.clone();
                let from_host = crate::app::short_host_name();
                std::thread::spawn(move || {
                    let result = crate::peers::send_peer_message(
                        &peer,
                        &relay.to_agent,
                        &relay.from_agent,
                        &from_host,
                        &body,
                        &relay.deferral_correlation_id,
                        Some(&relay.correlation_id),
                        MsgIntent::Fyi,
                    )
                    .map_err(|failure| failure.message(&relay.to_host));
                    let _ = event_tx.blocking_send(crate::events::AppEvent::MsgDeferralRelayed(
                        crate::events::MsgDeferralRelay { result, ..relay },
                    ));
                });
                true
            }
        }
    }

    /// Record the outcome of a cross-host deferral's hop.
    pub(crate) fn handle_msg_deferral_relayed(&mut self, relay: crate::events::MsgDeferralRelay) {
        if let Err(detail) = &relay.result {
            tracing::warn!(
                correlation_id = relay.correlation_id.as_str(),
                route = relay.route.as_str(),
                detail = detail.as_str(),
                "mute deferral could not reach the sender's host; the next mute retries it"
            );
            self.mailboxes.unmark_deferred(&relay.correlation_id);
            return;
        }
        // The same audit fact every relayed message leaves behind, so the
        // deferral is not the one cross-host send missing from this log.
        self.emit_event(EventEnvelope {
            event: EventKind::MessageRelayed,
            data: EventData::MessageRelayed {
                correlation_id: relay.deferral_correlation_id.clone(),
                from_agent: relay.from_agent,
                to_agent: relay.to_agent,
                to_host: relay.to_host,
                route: relay.route.clone(),
                relayed_at_ms: now_ms(),
            },
        });
        self.emit_message_deferred(
            &relay.correlation_id,
            relay.deferral_correlation_id,
            &relay.pane,
            relay.muted_until_ms,
            relay.reason,
            Some(relay.route),
        );
    }

    fn emit_message_deferred(
        &mut self,
        correlation_id: &str,
        deferral_correlation_id: String,
        pane: &str,
        muted_until_ms: u64,
        reason: Option<String>,
        route: Option<String>,
    ) {
        self.emit_event(EventEnvelope {
            event: EventKind::MessageDeferred,
            data: EventData::MessageDeferred {
                correlation_id: correlation_id.to_string(),
                deferral_correlation_id,
                pane: pane.to_string(),
                muted_until_ms,
                reason,
                route,
                deferred_at_ms: now_ms(),
            },
        });
    }

    /// `msg.read` — the recipient consumes its inbox (ADR-0008).
    ///
    /// This is where "delivered" now happens. Under pane injection, delivery
    /// meant flock typed the body into a TTY and hoped the agent read it;
    /// here it means the agent actually took the message. The event is
    /// emitted on the same edge, so the audit trail gets truer rather than
    /// noisier.
    pub(super) fn handle_msg_read(&mut self, id: String, params: MsgReadParams) -> String {
        let pane = match self.resolve_inbox_pane(&id, params.pane, "read") {
            Ok(pane) => pane,
            Err(refusal) => return refusal,
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
                intent: message.intent,
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
        let host = location.host.as_str();
        let to_agent = location.agent_id.as_str();
        let Some(peer) = self.peer_for_location(location) else {
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
        let in_reply_to = params
            .in_reply_to
            .as_deref()
            .filter(|explicit| !explicit.trim().is_empty());

        match crate::peers::send_peer_message(
            &peer,
            to_agent,
            &from_agent,
            &crate::app::short_host_name(),
            body,
            &correlation_id,
            in_reply_to,
            params.intent,
        ) {
            Ok(()) => {
                // Without this the sender's durable log has NO record that a
                // cross-host message was ever sent — the relay path returned
                // straight to the caller and never reached `queue_message`,
                // which is what emits the local audit event. A message that
                // left the machine was the one kind that vanished from the
                // audit substrate it is supposed to be recorded in (#175).
                self.emit_event(EventEnvelope {
                    event: EventKind::MessageRelayed,
                    data: EventData::MessageRelayed {
                        correlation_id: correlation_id.clone(),
                        from_agent: from_agent.clone(),
                        to_agent: to_agent.to_string(),
                        to_host: host.to_string(),
                        route: peer.name.clone(),
                        relayed_at_ms: now_ms(),
                    },
                });
                encode_success(
                    id,
                    ResponseResult::MsgQueued {
                        correlation_id,
                        state: "relayed".into(),
                        warnings: Vec::new(),
                    },
                )
            }
            // #380: a peer that READ the command and rejected it is not an
            // unreachable peer. The relay is `flk msg send` on the far side,
            // so a flag that build does not understand now comes back here as
            // a refusal — the same failure that used to arrive as flag text
            // welded to the front of somebody's message body, with a success
            // reported to the sender.
            Err(failure) => encode_error_with_data(
                id,
                failure.code(),
                failure.message(host),
                serde_json::json!({
                    "retryable": failure.retryable(),
                    "peer": peer.name,
                    "detail": failure.detail(),
                }),
            ),
        }
    }

    /// The `[[peers]]` entry that reaches `location`.
    ///
    /// Route by the peer entry the DIRECTORY answered from, not by the name
    /// the far machine calls itself. Those differ in any normal fleet — a
    /// peer configured as `anvil` reports its hostname as `vm-dev` — and
    /// matching on the reported host is exactly why the first live
    /// cross-host send came back "not in this server's [[peers]]". Falls
    /// back to the host for a directory answer that carried no route.
    fn peer_for_location(
        &self,
        location: &crate::app::directory::AgentLocation,
    ) -> Option<crate::config::PeerConfig> {
        let route = location.route.as_deref().unwrap_or_default();
        let host = location.host.as_str();
        self.state
            .peers
            .iter()
            .find(|peer| {
                (!route.is_empty() && peer.name.eq_ignore_ascii_case(route))
                    || peer.name.eq_ignore_ascii_case(host)
                    || peer.ssh_target().eq_ignore_ascii_case(host)
            })
            .cloned()
    }

    /// Shared enqueue tail: dedupe, emit the durable `MessageQueued`, and
    /// answer the caller.
    ///
    /// Also the arrival half of ADR-0018 §3: a waking message queued for a
    /// muted recipient is answered with a deferral as it lands, so the
    /// sender learns the deadline now rather than when the mute lifts.
    fn queue_message(
        &mut self,
        id: String,
        message: PendingMessage,
        warnings: Vec<String>,
    ) -> String {
        let correlation_id = message.correlation_id.clone();
        let event = queued_event(&message);
        let owed = self
            .mailboxes
            .owes_deferral(&message)
            .then(|| message.clone());
        match self.mailboxes.enqueue(message) {
            EnqueueOutcome::Queued => {
                self.emit_event(EventEnvelope {
                    event: EventKind::MessageQueued,
                    data: event,
                });
                if let Some(message) = owed {
                    let now = now_ms();
                    if let Some(until) = self.mailboxes.muted_until(&message.to_pane, now) {
                        let reason = self.mailboxes.mute_reason(&message.to_pane, now);
                        self.defer_message(&message, until, reason);
                    }
                }
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
        ErrorResponse, EventData, EventEnvelope, EventKind, MessageTarget, Method, MsgIntent,
        MsgListParams, MsgReadParams, MsgReplyParams, MsgSendParams, MsgStatusParams, Request,
        ResponseResult, SuccessResponse,
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
                intent: MsgIntent::Fyi,
            },
        )
    }

    /// Drive `msg.wake` the way the stop hook does and return what it
    /// reports: `(count, suppressed_reason)`.
    fn wake(app: &mut crate::app::App, pane: &str) -> (usize, Option<String>) {
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgWake(crate::api::schema::MsgWakeParams {
                pane: Some(pane.to_string()),
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgWake {
            count, suppressed, ..
        } = success.result
        else {
            panic!("expected msg_wake: {response}");
        };
        (count, suppressed)
    }

    fn queued_count(app: &mut crate::app::App) -> usize {
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgList(MsgListParams::default()),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgList { messages } = success.result else {
            panic!("expected msg_list: {response}");
        };
        messages.len()
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
                intent: MsgIntent::Fyi,
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
                intent: MsgIntent::Fyi,
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
                intent: MsgIntent::Fyi,
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
                intent: MsgIntent::Fyi,
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
                intent: MsgIntent::Fyi,
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
                intent: MsgIntent::Fyi,
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
                intent: MsgIntent::Fyi,
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "msg_target_not_found");
    }

    #[tokio::test]
    async fn a_disallowed_origin_host_is_refused_at_the_receiver() {
        // The policy has to bite on the RECEIVING side: a sender-side check is
        // advice, and once the hub forwards for spokes, "can reach" stops
        // being decided by which SSH keys exist.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        app.state.config.msg.allow_from = vec!["mba22".into()];
        let to_pane = app
            .state
            .workspaces
            .get(1)
            .and_then(|ws| ws.focused_pane_id())
            .and_then(|pane_id| app.public_pane_id(1, pane_id))
            .expect("recipient pane");

        let send = |app: &mut crate::app::App, host: &str, cid: &str| {
            app.handle_api_request(Request {
                id: "req".into(),
                method: Method::MsgSend(MsgSendParams {
                    from_agent: Some("agent_elsewhere_1".into()),
                    from_host: Some(host.into()),
                    to: MessageTarget::Pane {
                        pane: to_pane.clone(),
                    },
                    body: "knock knock".into(),
                    correlation_id: Some(cid.into()),
                    in_reply_to: None,
                    intent: MsgIntent::Fyi,
                }),
            })
        };

        let refused = send(&mut app, "anvil", "c-blocked");
        let error: ErrorResponse = serde_json::from_str(&refused).unwrap();
        assert_eq!(error.error.code, "msg_not_allowed");
        assert!(
            error.error.message.contains("anvil"),
            "{}",
            error.error.message
        );

        let allowed = send(&mut app, "mba22", "c-allowed");
        assert!(!allowed.contains("\"error\""), "{allowed}");
    }

    #[tokio::test]
    async fn msg_status_answers_the_senders_question_through_the_lifecycle() {
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        let status = |app: &mut crate::app::App, correlation: &str| -> serde_json::Value {
            let response = app.handle_api_request(Request {
                id: "req".into(),
                method: Method::MsgStatus(MsgStatusParams {
                    correlation_id: correlation.into(),
                }),
            });
            serde_json::from_str(&response).unwrap()
        };

        // Unknown ids are an error, not a fabricated "pending".
        assert_eq!(
            status(&mut app, "c-never-sent")["error"]["code"],
            "message_not_found"
        );

        basic_send(&mut app, "c-live", "hello");
        let queued = status(&mut app, "c-live");
        assert_eq!(queued["result"]["state"], "queued");
        assert_eq!(queued["result"]["outcome_known"], true);

        // The LAST event wins, so reading it moves the answer on rather than
        // leaving the first verdict standing.
        let recipient = pane_target(&app, 1);
        app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgRead(MsgReadParams {
                pane: Some(recipient),
            }),
        });
        let read = status(&mut app, "c-live");
        assert_eq!(read["result"]["state"], "read");
        assert_eq!(read["result"]["outcome_known"], true);
    }

    #[tokio::test]
    async fn msg_status_never_claims_to_know_a_relayed_messages_fate() {
        // The receiving node resolves a relayed message, so its outcome is in
        // that node's log. Reporting "relayed" as if it were delivery would
        // turn the one honest answer into a misleading one.
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        app.emit_event(EventEnvelope {
            event: EventKind::MessageRelayed,
            data: EventData::MessageRelayed {
                correlation_id: "c-gone".into(),
                from_agent: "agent_sage_cafe".into(),
                to_agent: "agent_anvil-dev_beef".into(),
                to_host: "anvil-dev".into(),
                route: "anvil".into(),
                relayed_at_ms: 1,
            },
        });
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgStatus(MsgStatusParams {
                correlation_id: "c-gone".into(),
            }),
        });
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["result"]["state"], "relayed");
        assert_eq!(
            value["result"]["outcome_known"], false,
            "this node cannot see whether the far side read it"
        );
        assert_eq!(value["result"]["to_host"], "anvil-dev");
        assert_eq!(
            value["result"]["route"], "anvil",
            "the route is what a reader needs to reproduce the hop"
        );
    }

    #[tokio::test]
    async fn a_relay_that_never_left_the_machine_writes_no_audit_record() {
        // `MessageRelayed` has to mean "this message left, and went there".
        // Emitting it on the attempt rather than the success would make the
        // audit trail claim delivery for messages that never crossed the
        // wire — worse than the missing record it replaces, because a wrong
        // record reads as a true one.
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        app.state.peers = vec![crate::config::PeerConfig {
            name: "anvil".into(),
            // Unresolvable, so the ssh attempt fails fast without a network.
            ssh: "relay-audit-test-nonexistent-host.invalid".into(),
            ..Default::default()
        }];
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
                from_agent: Some("agent_sage_cafe".into()),
                from_host: None,
                to: MessageTarget::Agent {
                    agent: "agent_anvil-dev_beef".into(),
                },
                body: "never arrives".into(),
                correlation_id: Some("c-unreachable".into()),
                in_reply_to: None,
                intent: MsgIntent::Fyi,
            }),
        });
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            error.error.code, "peer_unreachable",
            "the send fails synchronously, so the caller learns immediately"
        );
        // #380 split this from a refusal, so the code alone is now a claim
        // about which of the two happened — and it comes with the advice that
        // follows from it. A peer that was never reached is worth retrying.
        let data: serde_json::Value = serde_json::from_str(&response).unwrap();
        let data = &data["error"]["data"];
        assert_eq!(
            data["retryable"], true,
            "an unreachable peer is a transient failure: {data}"
        );
        assert!(
            data["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "the far side's own words reach the caller unedited: {data}"
        );
        let relayed = hub
            .events_after(0)
            .iter()
            .filter(|(_, event)| matches!(event.data, EventData::MessageRelayed { .. }))
            .count();
        assert_eq!(relayed, 0, "a failed hop leaves no record claiming it left");
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
                intent: MsgIntent::Fyi,
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

    /// #316: pause must hold the WAKE, and the wake is what ADR-0008 left
    /// ungated.
    ///
    /// US-9 gated `deliver_due_messages`; #216 deleted that drain along with
    /// the keystrokes, and the stop-hook wake that replaced it inherited
    /// nothing — so a paused fleet went on answering every turn boundary with
    /// `decision: block`. The reader's view is deliberately NOT gated: pause
    /// halts the interruption, not the information.
    #[tokio::test]
    async fn a_paused_fleet_suppresses_the_wake_but_still_lists() {
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let pane = pane_target(&app, 1);
        basic_send(&mut app, "c-wake-pause", "hi");

        assert_eq!(
            wake(&mut app, &pane),
            (1, None),
            "an unpaused fleet names the waiting message",
        );

        app.fleet_pause.paused = true;
        assert_eq!(
            wake(&mut app, &pane),
            (0, Some("fleet_paused".into())),
            "a paused fleet must not interrupt an agent about its mail",
        );
        assert_eq!(
            queued_count(&mut app),
            1,
            "suppression is not deletion: the message is still queued and still listable",
        );

        app.fleet_pause.paused = false;
        assert_eq!(
            wake(&mut app, &pane),
            (1, None),
            "resume restores the wake, with the message still there to name",
        );
    }

    /// #316 C: a receiver may decline to be woken for a bounded window, and
    /// that must cost it latency rather than a message.
    #[tokio::test]
    async fn a_muted_pane_is_not_woken_and_keeps_its_mail() {
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let pane = pane_target(&app, 1);
        basic_send(&mut app, "c-mute-1", "before the mute");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgMute(crate::api::schema::MsgMuteParams {
                pane: Some(pane.clone()),
                seconds: 600,
                reason: None,
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgMute { muted_until_ms, .. } = success.result else {
            panic!("expected msg_mute: {response}");
        };
        assert!(muted_until_ms > 0, "a mute must name when it lifts");

        assert_eq!(
            wake(&mut app, &pane),
            (0, Some("muted".into())),
            "a muted pane is not woken",
        );

        // Mail keeps arriving while muted — the mute is on the wake, not the
        // delivery.
        basic_send(&mut app, "c-mute-2", "during the mute");
        assert_eq!(
            queued_count(&mut app),
            2,
            "delivery is unaffected by a mute"
        );
        assert_eq!(wake(&mut app, &pane), (0, Some("muted".into())));

        // Clearing it names everything that queued meanwhile, so the window
        // cost latency and nothing else.
        app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgMute(crate::api::schema::MsgMuteParams {
                pane: Some(pane.clone()),
                seconds: 0,
                reason: None,
            }),
        });
        assert_eq!(
            wake(&mut app, &pane),
            (2, None),
            "the first allowed wake names everything that arrived during the mute",
        );
    }

    /// ADR-0008: the wake channel names a count and a tool, never a body.
    /// `msg.list` carries a 120-character preview of every queued message, so
    /// peeking with it put sender-written text in the hook process at every
    /// turn boundary. Assert on the raw wire, because that is what the hook
    /// actually receives.
    #[tokio::test]
    async fn the_wake_response_carries_no_sender_text() {
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let pane = pane_target(&app, 1);
        basic_send(&mut app, "c-no-body", "SENDER WROTE THIS");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgWake(crate::api::schema::MsgWakeParams { pane: Some(pane) }),
        });
        assert!(
            !response.contains("SENDER WROTE THIS"),
            "the wake must not carry the body: {response}",
        );
        assert!(
            !response.contains("preview"),
            "the wake must not carry a preview either: {response}",
        );
    }
    /// Build a request the way a client does — off the wire, as JSON — so the
    /// serde contract is under test alongside the handler. A struct literal
    /// would prove the field exists in Rust and nothing about whether a
    /// caller can set it.
    fn wire_request(json: serde_json::Value) -> Request {
        serde_json::from_value(json).expect("a request a client could send")
    }

    fn read_inbox(app: &mut crate::app::App, pane: &str) -> Vec<crate::api::schema::InboxMessage> {
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgRead(MsgReadParams {
                pane: Some(pane.to_string()),
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgRead { messages } = success.result else {
            panic!("expected msg_read: {response}");
        };
        messages
    }

    #[tokio::test]
    async fn a_needs_reply_stamp_reaches_the_reader_on_the_envelope() {
        // #280, R4 of #213. The founding case: an hour-deep redirect whose
        // "answer me" sat in the last line of a ~2.5k-character body, where a
        // recipient reading the envelope could not see it. The stamp has to
        // arrive as a FIELD, before the body is read, or it has changed
        // nothing.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let to = pane_target(&app, 1);
        let response = app.handle_api_request(wire_request(serde_json::json!({
            "id": "req",
            "method": "msg.send",
            "params": {
                "to": { "type": "pane", "pane": to },
                "body": "re-derive both disputed parameters and report back",
                "correlation_id": "c-question",
                "intent": "needs_reply",
            },
        })));
        assert!(!response.contains("\"error\""), "{response}");

        // Asserted on the WIRE rather than the typed field, deliberately: the
        // MCP bridge hands this JSON to the model verbatim, so the string is
        // what a recipient actually sees. It also makes the test portable to
        // the commit before this one, where it fails rather than failing to
        // compile.
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgRead(MsgReadParams {
                pane: Some(to.clone()),
            }),
        });
        assert!(
            response.contains("\"intent\":\"needs_reply\""),
            "the sender's stamp must survive to the reader: {response}"
        );
    }

    #[tokio::test]
    async fn an_unstamped_send_is_fyi_rather_than_refused() {
        // The wire default, which is what keeps every pre-#280 caller working:
        // an older peer relaying a message, `flk msg send` without the flag, a
        // reply. `fyi` is the conservative reading of an unstamped envelope and
        // is what all of them meant before the field existed. The forcing
        // function lives on the MCP tool schema instead — see
        // `build_msg_send_requires_an_intent`.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let to = pane_target(&app, 1);
        let response = app.handle_api_request(wire_request(serde_json::json!({
            "id": "req",
            "method": "msg.send",
            "params": {
                "to": { "type": "pane", "pane": to },
                "body": "landed the fix",
                "correlation_id": "c-quiet",
            },
        })));
        assert!(!response.contains("\"error\""), "{response}");

        let messages = read_inbox(&mut app, &to);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].intent, MsgIntent::Fyi);
    }

    #[tokio::test]
    async fn an_intent_survives_the_restart_that_rebuilds_the_mailbox() {
        // The durable log is what reconstructs an undelivered queue at boot
        // (§8.4). An intent that lived only in memory would be silently
        // downgraded to `fyi` by a restart — turning the one signal this field
        // carries into exactly the mislabel it exists to prevent. So the test
        // starts where the value really starts: the event.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let to = pane_target(&app, 1);
        app.mailboxes = crate::app::mailboxes::MailboxRegistry::default();
        let queued: EventEnvelope = serde_json::from_value(serde_json::json!({
            "event": "message_queued",
            "data": {
                "type": "message_queued",
                "correlation_id": "c-restarted",
                "from_pane": "w1:p1",
                "from_agent": "agent_sage_1",
                "from_host": "sage",
                "to_pane": to,
                "cross_repo": false,
                "enqueued_at_ms": 1,
                "intent": "needs_reply",
                "body": "still waiting on an answer",
            },
        }))
        .expect("an event the durable log could hold");
        app.mailboxes.seed_from_events([queued].iter());

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgRead(MsgReadParams {
                pane: Some(to.clone()),
            }),
        });
        assert!(
            response.contains("\"intent\":\"needs_reply\""),
            "a restart must not downgrade a question to a notice: {response}"
        );
    }

    #[tokio::test]
    async fn a_reply_can_itself_ask_for_an_answer() {
        // Without this, a two-turn exchange strands its intent in prose again:
        // the answer to a question is `fyi` by default, correctly, but a reply
        // that asks something back has no way to say so. Same field, opposite
        // default — see `MsgReplyParams::intent`.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let asker = pane_target(&app, 0);
        let answerer = pane_target(&app, 1);
        // A message the reply can route home to: `msg.reply` addresses by the
        // ORIGINAL sender, so the exchange has to start from the other pane.
        app.mailboxes
            .enqueue(crate::app::mailboxes::PendingMessage {
                correlation_id: "c-orig".into(),
                body: "which sigma governs the fits?".into(),
                from_pane: Some(answerer.clone()),
                from_agent: None,
                from_host: None,
                from_repo: None,
                to_pane: asker.clone(),
                to_repo: None,
                in_reply_to: None,
                enqueued_at_ms: 1,
                delivery_attempts: 0,
                intent: MsgIntent::NeedsReply,
            });
        assert_eq!(read_inbox(&mut app, &asker).len(), 1);

        let response = app.handle_api_request(wire_request(serde_json::json!({
            "id": "req",
            "method": "msg.reply",
            "params": {
                "correlation_id": "c-orig",
                "body": "0.165 ns — but which of the two fits do you mean?",
                "intent": "needs_reply",
            },
        })));
        assert!(!response.contains("\"error\""), "{response}");

        let messages = read_inbox(&mut app, &answerer);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].intent, MsgIntent::NeedsReply);
    }

    #[tokio::test]
    async fn the_wake_still_reports_a_count_and_never_an_intent() {
        // ADR-0008: the wake channel names a COUNT and a tool, never anything
        // a sender chose. #316 made that structural by giving `msg.wake` a
        // number to return; an intent leaking into it would reopen the hole
        // for the sake of a louder knock, which is escalation machinery this
        // issue deliberately does not build.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let to = pane_target(&app, 1);
        for (cid, intent) in [("c-a", "needs_reply"), ("c-b", "fyi")] {
            let response = app.handle_api_request(wire_request(serde_json::json!({
                "id": "req",
                "method": "msg.send",
                "params": {
                    "to": { "type": "pane", "pane": to },
                    "body": "hi",
                    "correlation_id": cid,
                    "intent": intent,
                },
            })));
            assert!(!response.contains("\"error\""), "{response}");
        }

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgWake(crate::api::schema::MsgWakeParams {
                pane: Some(to.clone()),
            }),
        });
        assert!(
            !response.contains("intent") && !response.contains("needs_reply"),
            "the wake must carry no intent: {response}"
        );
        assert_eq!(wake(&mut app, &to), (2, None), "the count is unchanged");
    }

    #[tokio::test]
    async fn the_operators_peek_shows_which_queued_message_is_a_question() {
        // `msg.list` is the operator's view of an inbox nobody has read yet.
        // Without the stamp it cannot distinguish a fleet waiting on answers
        // from a fleet with a backlog of notices.
        let mut app = test_app_with_hub(crate::api::EventHub::default());
        let to = pane_target(&app, 1);
        let response = app.handle_api_request(wire_request(serde_json::json!({
            "id": "req",
            "method": "msg.send",
            "params": {
                "to": { "type": "pane", "pane": to },
                "body": "are you still on the bound?",
                "correlation_id": "c-peek",
                "intent": "needs_reply",
            },
        })));
        assert!(!response.contains("\"error\""), "{response}");

        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgList(MsgListParams::default()),
        });
        assert!(
            response.contains("\"intent\":\"needs_reply\""),
            "the peek must say which queued message is a question: {response}"
        );
    }

    // ---- ADR-0018 §3: a mute must answer ---------------------------------

    /// The fleet-global id of the agent in workspace `ws_idx`'s focused pane.
    fn agent_id_of(app: &crate::app::App, ws_idx: usize) -> String {
        let ws = &app.state.workspaces[ws_idx];
        let pane_id = ws.focused_pane_id().expect("pane");
        let terminal_id = &ws
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id;
        app.state.terminals[terminal_id].agent_id.to_string()
    }

    /// A send FROM workspace `from_ws`'s agent. The test process has no pane
    /// ancestry, so the sender is asserted the way a relay asserts it —
    /// which is also what makes the message answerable.
    fn send_from(
        app: &mut crate::app::App,
        from_ws: usize,
        to_ws: usize,
        correlation: &str,
        intent: MsgIntent,
    ) -> String {
        let from_agent = agent_id_of(app, from_ws);
        let to = pane_target(app, to_ws);
        send(
            app,
            MsgSendParams {
                from_agent: Some(from_agent),
                from_host: None,
                to: MessageTarget::Pane { pane: to },
                body: format!("question {correlation}"),
                correlation_id: Some(correlation.into()),
                in_reply_to: None,
                intent,
            },
        )
    }

    /// `msg.mute` over the socket; returns `(muted_until_ms, deferred)`.
    fn mute(
        app: &mut crate::app::App,
        pane: &str,
        seconds: u64,
        reason: Option<&str>,
    ) -> (u64, usize) {
        let response = app.handle_api_request(wire_request(serde_json::json!({
            "id": "req",
            "method": "msg.mute",
            "params": {"pane": pane, "seconds": seconds, "reason": reason},
        })));
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::MsgMute {
            muted_until_ms,
            deferred,
        } = success.result
        else {
            panic!("expected msg_mute: {response}");
        };
        (muted_until_ms, deferred)
    }

    fn deferred_events(hub: &crate::api::EventHub) -> Vec<EventData> {
        hub.events_after(0)
            .into_iter()
            .map(|(_, envelope)| envelope.data)
            .filter(|data| matches!(data, EventData::MessageDeferred { .. }))
            .collect()
    }

    /// The whole contract through the socket: a question already waiting
    /// when the mute is set is answered, once, with the reason and the lift
    /// time; a notice is not; renewing the mute answers nobody twice; a
    /// question arriving into the mute is answered as it lands.
    #[tokio::test]
    async fn a_mute_answers_each_waiting_question_exactly_once() {
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        let sender_pane = pane_target(&app, 0);
        let muted_pane = pane_target(&app, 1);

        send_from(&mut app, 0, 1, "c-q1", MsgIntent::NeedsReply);
        send_from(&mut app, 0, 1, "c-n1", MsgIntent::Fyi);

        let (until, deferred) = mute(
            &mut app,
            &muted_pane,
            600,
            Some("deep in a refactor\u{1b}[31m\nback soon"),
        );
        assert_eq!(
            deferred, 1,
            "one question waiting, one answer; a notice is owed nothing"
        );

        let inbox = read_inbox(&mut app, &sender_pane);
        assert_eq!(inbox.len(), 1, "exactly one deferral: {inbox:?}");
        let deferral = &inbox[0];
        assert_eq!(deferral.in_reply_to.as_deref(), Some("c-q1"));
        assert_eq!(
            deferral.intent,
            MsgIntent::Fyi,
            "a deferral must never itself ask for an answer (#316 pitfall 7)"
        );
        assert_eq!(
            deferral.from_agent.as_deref(),
            Some(agent_id_of(&app, 1).as_str()),
            "the deferral speaks for the muted agent, not for whoever triggered it"
        );
        assert!(
            deferral.body.contains(&crate::digest::utc_timestamp(until)),
            "the body names the absolute time the mute lifts: {}",
            deferral.body
        );
        assert!(
            deferral.body.contains("deep in a refactor back soon"),
            "the reason is quoted, folded onto one line: {}",
            deferral.body
        );
        assert!(
            !deferral.body.contains('\u{1b}'),
            "the reason goes through the body sanitiser: {:?}",
            deferral.body
        );
        assert_eq!(
            deferred_events(&hub).len(),
            1,
            "one durable record per deferral"
        );

        // Renewing (and extending) the mute owes nobody anything new.
        let (_, deferred) = mute(&mut app, &muted_pane, 1200, Some("still at it"));
        assert_eq!(deferred, 0, "a renewed mute must not re-answer");
        assert!(read_inbox(&mut app, &sender_pane).is_empty());

        // A question arriving INTO the mute is answered as it lands, with the
        // renewed mute's reason.
        send_from(&mut app, 0, 1, "c-q2", MsgIntent::NeedsReply);
        let inbox = read_inbox(&mut app, &sender_pane);
        assert_eq!(inbox.len(), 1, "{inbox:?}");
        assert_eq!(inbox[0].in_reply_to.as_deref(), Some("c-q2"));
        assert!(inbox[0].body.contains("still at it"), "{}", inbox[0].body);

        // A notice arriving into the mute is not.
        send_from(&mut app, 0, 1, "c-n2", MsgIntent::Fyi);
        assert!(read_inbox(&mut app, &sender_pane).is_empty());

        // Every original is still waiting for the muted agent: deferral costs
        // the sender latency, never the message.
        let waiting = read_inbox(&mut app, &muted_pane);
        let ids: Vec<&str> = waiting.iter().map(|m| m.correlation_id.as_str()).collect();
        assert_eq!(ids, ["c-q1", "c-n1", "c-q2", "c-n2"]);
        assert_eq!(deferred_events(&hub).len(), 2);
    }

    /// `seconds: 0` clears. Nothing was deferred, so nothing is said.
    #[tokio::test]
    async fn clearing_a_mute_tells_nobody_anything() {
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        let sender_pane = pane_target(&app, 0);
        let muted_pane = pane_target(&app, 1);
        send_from(&mut app, 0, 1, "c-q", MsgIntent::NeedsReply);

        assert_eq!(mute(&mut app, &muted_pane, 0, Some("never mind")), (0, 0));
        assert!(read_inbox(&mut app, &sender_pane).is_empty());
        assert!(deferred_events(&hub).is_empty());
    }

    /// Two agents muted at each other: A's question is deferred by B, and
    /// B's deferral — `fyi` by construction — is owed nothing by A. Without
    /// that, each deferral would answer the last until a rate limit stopped
    /// them.
    #[tokio::test]
    async fn two_muted_agents_cannot_ping_pong_deferrals() {
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        let pane_a = pane_target(&app, 0);
        let pane_b = pane_target(&app, 1);
        mute(&mut app, &pane_a, 600, None);
        mute(&mut app, &pane_b, 600, None);

        send_from(&mut app, 0, 1, "c-ab", MsgIntent::NeedsReply);

        assert_eq!(
            deferred_events(&hub).len(),
            1,
            "one deferral, and it ends there"
        );
        let a_inbox = read_inbox(&mut app, &pane_a);
        assert_eq!(a_inbox.len(), 1);
        assert_eq!(a_inbox[0].intent, MsgIntent::Fyi);
        assert!(
            !a_inbox[0].body.contains("Reason given"),
            "no reason given, none invented: {}",
            a_inbox[0].body
        );
        assert_eq!(read_inbox(&mut app, &pane_b).len(), 1, "only the original");
    }

    /// The "already deferred" set survives a restart through the durable
    /// log. Mutes do not (they fail open), so the realistic sequence is:
    /// deferred, restart, the agent mutes again — and must not re-answer a
    /// question it already answered.
    #[tokio::test]
    async fn a_restart_does_not_repeat_a_deferral() {
        let hub = crate::api::EventHub::default();
        let mut app = test_app_with_hub(hub.clone());
        let sender_pane = pane_target(&app, 0);
        let muted_pane = pane_target(&app, 1);
        send_from(&mut app, 0, 1, "c-q", MsgIntent::NeedsReply);
        assert_eq!(mute(&mut app, &muted_pane, 600, None).1, 1);
        assert_eq!(read_inbox(&mut app, &sender_pane).len(), 1);

        // Restart: the registry is rebuilt from nothing but the log.
        let events: Vec<EventEnvelope> = hub
            .events_after(0)
            .into_iter()
            .map(|(_, envelope)| envelope)
            .collect();
        app.mailboxes = crate::app::mailboxes::MailboxRegistry::default();
        app.mailboxes.seed_from_events(events.iter());
        assert!(
            app.mailboxes
                .muted_until(&muted_pane, super::now_ms())
                .is_none(),
            "precondition: a mute does not survive a restart"
        );

        assert_eq!(
            mute(&mut app, &muted_pane, 600, None).1,
            0,
            "the question is still queued, and still already answered"
        );
        assert!(read_inbox(&mut app, &sender_pane).is_empty());
        assert_eq!(deferred_events(&hub).len(), 1);
        assert_eq!(
            read_inbox(&mut app, &muted_pane).len(),
            1,
            "the original itself survived the restart"
        );
    }

    #[test]
    fn a_mute_reason_is_bounded_and_blank_means_none() {
        assert_eq!(super::bound_mute_reason("  \n\t ", 200), None);
        assert_eq!(super::bound_mute_reason("fine", 0), None);
        assert_eq!(
            super::bound_mute_reason("abcdef", 4).as_deref(),
            Some("abc\u{2026}"),
            "truncated to the cap, ellipsis included"
        );
        assert_eq!(
            super::bound_mute_reason("äöü", 3).as_deref(),
            Some("äöü"),
            "the cap counts characters, not bytes"
        );
    }
}
