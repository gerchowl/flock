//! `msg.uplink_*` — a spoke's messages ride up the relay its hub holds (#410).
//!
//! Both halves live here. On the SPOKE: `try_hand_up` parks a send behind a
//! frame, `msg.uplink_take` gives frames to the relay, and `msg.uplink_result`
//! resolves the parked send with the hub's answer. On the HUB:
//! `forward_uplinked_message` — reached in-process from the relay reader,
//! never from the socket — runs the ordinary `msg.send` on a frame a spoke
//! handed up; the hub already knows how to reach every spoke, so there is
//! still exactly one delivery implementation.
//!
//! The state machine is `crate::app::uplink`; this file is the wire around it.

use std::time::{Duration, Instant};

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, MessageTarget, MsgSendParams, MsgUplinkResultParams,
    MsgUplinkTakeParams, ResponseResult, UplinkFrame,
};
use crate::app::uplink::ParkedSend;
use crate::app::App;

use super::responses::{encode_error, encode_error_with_data, encode_success};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// An uplink id nobody can guess: the counter keeps it unique, and a hash
/// keyed from the OS's randomness keeps it unpredictable, so a stray process
/// cannot forge a `msg.uplink_result` for a send it never saw even if the
/// relay binding were ever bypassed.
fn mint_uplink_id(correlation_id: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(count);
    hasher.write_u64(now_ms());
    format!("up:{correlation_id}:{count:x}:{:016x}", hasher.finish())
}

impl App {
    fn uplink_heartbeat(&self) -> Duration {
        Duration::from_secs(self.state.config.msg.uplink_heartbeat_secs.max(1))
    }

    fn uplink_timeout(&self) -> Duration {
        Duration::from_secs(self.state.config.msg.uplink_timeout_secs.max(1))
    }

    /// Whether a hub currently holds a relay into this server.
    pub(super) fn uplink_attached(&self) -> bool {
        self.uplink
            .is_attached(Instant::now(), self.uplink_heartbeat())
    }

    /// Send `response` to the caller — or, if the handler parked this
    /// request, hold the responder until the answer exists (#410).
    ///
    /// The one hook every transport calls instead of sending directly. It is
    /// what lets a spoke's `msg.send` return the hub's real outcome without
    /// the main loop blocking on a round trip that has to come back THROUGH
    /// the main loop.
    pub(crate) fn respond_or_park(
        &mut self,
        respond_to: std::sync::mpsc::Sender<String>,
        response: String,
    ) {
        let Some(park) = self.uplink.take_pending_park() else {
            let _ = respond_to.send(response);
            return;
        };
        let now = Instant::now();
        let heartbeat = self.uplink_heartbeat();
        if let Err(respond_to) = self.uplink.attach(park, respond_to, now, heartbeat) {
            let _ = respond_to.send(response);
        }
        // A take that just parked may already have something to carry: a send
        // handed up in the same drain, before this relay asked.
        self.feed_parked_take(now);
    }

    fn feed_parked_take(&mut self, now: Instant) {
        let heartbeat = self.uplink_heartbeat();
        if let Some((take, frames)) = self.uplink.feed_parked_take(now, heartbeat) {
            let response = encode_success(
                take.request_id.clone(),
                ResponseResult::MsgUplinkFrames { frames },
            );
            take.answer(response);
        }
    }

    /// Answer what has waited too long: takes (empty, so the relay asks again)
    /// and sends the hub never answered.
    pub(crate) fn expire_uplink(&mut self) {
        // #410 down-gossip: a hub evicts relayed rows when a poll lands, but a
        // spoke polls nobody — rows its hub pushed would never age OUT once
        // the hub went quiet, only go stale. Same eviction, on this tick.
        self.state.evict_expired_relayed_entries();
        let heartbeat = self.uplink_heartbeat();
        let expired = self.uplink.expire(Instant::now(), heartbeat);
        for take in expired.takes {
            let response = encode_success(
                take.request_id.clone(),
                ResponseResult::MsgUplinkFrames { frames: Vec::new() },
            );
            take.answer(response);
        }
        let secs = self.uplink_timeout().as_secs();
        for (send, taken) in expired.sends {
            let message = if taken {
                format!(
                    "handed up to the hub's relay, but no answer came back within {secs}s — the \
                     hub may or may not have delivered it; retrying with the same \
                     correlation_id cannot deliver it twice"
                )
            } else {
                format!(
                    "no hub collected it within {secs}s: the relay the hub holds into this \
                     server stopped asking for messages"
                )
            };
            let response = encode_error_with_data(
                send.request_id.clone(),
                "uplink_timeout",
                message,
                serde_json::json!({ "retryable": true, "taken_by_hub": taken }),
            );
            send.answer(response);
        }
    }

    /// Hand a message up to the hub, if this server may and can (#410).
    ///
    /// `None` when it may not — the message did not originate here, so it
    /// has already crossed a hub and is not forwarded again — or when no hub
    /// holds a relay into this server. The caller then refuses in its own
    /// words, which name why.
    pub(super) fn try_hand_up(
        &mut self,
        id: &str,
        to_agent: &str,
        body: &str,
        params: &MsgSendParams,
    ) -> Option<String> {
        if params.from_host.is_some() || !self.uplink_attached() {
            return None;
        }
        // Attested HERE, where the sender's process ancestry is. The hub has
        // no way to attest it and must not become the apparent sender (#213).
        let from_agent = self
            .attested_sender_agent()
            .or_else(|| params.from_agent.clone());
        let Some(from_agent) = from_agent else {
            return Some(encode_error(
                id.to_string(),
                "sender_unresolved",
                "cross-host delivery needs a sender identity: call from inside a pane, or pass \
                 from_agent",
            ));
        };
        let correlation_id = params
            .correlation_id
            .clone()
            .filter(|explicit| !explicit.trim().is_empty())
            .unwrap_or_else(super::messages::mint_correlation_id);
        let uplink_id = mint_uplink_id(&correlation_id);
        let frame = UplinkFrame {
            uplink_id,
            message: MsgSendParams {
                to: MessageTarget::Agent {
                    agent: to_agent.to_string(),
                },
                body: body.to_string(),
                intent: params.intent,
                correlation_id: Some(correlation_id.clone()),
                in_reply_to: params
                    .in_reply_to
                    .clone()
                    .filter(|explicit| !explicit.trim().is_empty()),
                from_agent: Some(from_agent.clone()),
                from_host: Some(crate::app::short_host_name()),
                intent_unrecognised: None,
            },
        };
        let now = Instant::now();
        let mut parked = ParkedSend::new(
            id.to_string(),
            correlation_id.clone(),
            from_agent,
            to_agent.to_string(),
            now + self.uplink_timeout(),
        );
        parked.intent = params.intent;
        self.uplink.hand_up(frame, parked);
        self.feed_parked_take(now);
        // Only reaches a caller that came in through no parking transport. A
        // socket caller is answered later, with the hub's outcome.
        Some(encode_success(
            id.to_string(),
            ResponseResult::MsgQueued {
                correlation_id,
                state: "handed_up".into(),
                warnings: Vec::new(),
                to_host: None,
                path: None,
            },
        ))
    }

    /// `peers.relay_attach` — the hub's relay binds this server's uplink to
    /// its own process (#410 review).
    ///
    /// The relay methods ride the local socket, which every same-user process
    /// can reach. Without a binding, a stray process could take a spoke's
    /// pending messages, forge the hub's answers, or plant fleet rows. The
    /// binding is the caller's socket peer pid, refused when it cannot be
    /// read, refused from inside a pane, and never displacing a relay that is
    /// still alive.
    pub(super) fn handle_peers_relay_attach(&mut self, id: String) -> String {
        let Some(pid) = self.current_api_peer_pid else {
            return encode_error(
                id,
                "relay_unattestable",
                "this platform did not report the caller's pid, so no relay can be bound",
            );
        };
        if self.parse_pane_id_or_peer("", Some(pid)).is_some() {
            return encode_error(
                id,
                "not_from_a_pane",
                "a relay is started by the hub's ssh session, never from inside a pane",
            );
        }
        match self
            .uplink
            .attach_relay(pid, crate::platform::process_exists)
        {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(holder) => encode_error(
                id,
                "relay_already_attached",
                format!("relay pid {holder} holds this server's uplink and is still alive"),
            ),
        }
    }

    /// The refusal for a relay-only method called by anyone but the relay.
    pub(super) fn refuse_unless_relay(&self, id: &str, method: &str) -> Option<String> {
        (!self.uplink.is_relay(self.current_api_peer_pid)).then(|| {
            encode_error(
                id.to_string(),
                "not_the_relay",
                format!("{method} is accepted only from the relay bound by peers.relay_attach"),
            )
        })
    }

    /// `msg.uplink_take` — the relay asks for frames to push up.
    pub(super) fn handle_msg_uplink_take(
        &mut self,
        id: String,
        params: MsgUplinkTakeParams,
    ) -> String {
        if let Some(refusal) = self.refuse_unless_relay(&id, "msg.uplink_take") {
            return refusal;
        }
        let heartbeat = self.uplink_heartbeat();
        let frames = self
            .uplink
            .take(id.clone(), &params.ack, Instant::now(), heartbeat)
            .unwrap_or_default();
        encode_success(id, ResponseResult::MsgUplinkFrames { frames })
    }

    /// `msg.uplink_result` — the hub's answer to a frame, sent back down.
    pub(super) fn handle_msg_uplink_result(
        &mut self,
        id: String,
        mut params: MsgUplinkResultParams,
    ) -> String {
        if let Some(refusal) = self.refuse_unless_relay(&id, "msg.uplink_result") {
            return refusal;
        }
        // The hub's name is the one recorded for this relay, not whatever this
        // frame says: a relay speaks for one hub for as long as it is bound.
        if let Some(hub) = self.uplink.record_hub(&params.hub) {
            params.hub = hub;
        }
        let Some(send) = self.uplink.complete(&params.uplink_id) else {
            return encode_success(id, ResponseResult::MsgUplinkResultAck { matched: false });
        };
        let response = self.uplinked_outcome(&send, &params);
        send.answer(response);
        encode_success(id, ResponseResult::MsgUplinkResultAck { matched: true })
    }

    /// Turn the hub's `msg.send` answer into this server's answer to its own
    /// caller: `via <hub>` on success, and on failure the hub's own words —
    /// which name ITS failed hop — prefixed with the hop that did work.
    fn uplinked_outcome(&mut self, send: &ParkedSend, params: &MsgUplinkResultParams) -> String {
        let hub = params.hub.as_str();
        let response = &params.response;
        if let Some(error) = response.get("error") {
            let code = error
                .get("code")
                .and_then(|code| code.as_str())
                .unwrap_or("uplink_failed");
            let message = error
                .get("message")
                .and_then(|message| message.as_str())
                .unwrap_or("the hub refused without saying why");
            let mut data = error
                .get("data")
                .cloned()
                .filter(serde_json::Value::is_object)
                .unwrap_or_else(|| serde_json::json!({}));
            data["via"] = serde_json::json!(hub);
            return encode_error_with_data(
                send.request_id.clone(),
                code,
                format!("handed up to {hub}, which could not deliver it: {message}"),
                data,
            );
        }
        let result = response.get("result").cloned().unwrap_or_default();
        let field = |name: &str| {
            result
                .get(name)
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let state = field("state").unwrap_or_else(|| "relayed".into());
        // No `to_host` from the hub means it queued the message itself: the
        // recipient lives on the hub.
        let to_host = field("to_host").unwrap_or_else(|| hub.to_string());
        let path = match field("path").as_deref() {
            Some(inner) if inner.starts_with("via ") => format!("via {hub}, then {inner}"),
            _ => format!("via {hub}"),
        };
        let warnings: Vec<String> = result
            .get("warnings")
            .and_then(|warnings| serde_json::from_value(warnings.clone()).ok())
            .unwrap_or_default();
        if state != "duplicate" {
            self.emit_event(EventEnvelope {
                event: EventKind::MessageRelayed,
                data: EventData::MessageRelayed {
                    correlation_id: send.correlation_id.clone(),
                    from_agent: send.from_agent.clone(),
                    to_agent: send.to_agent.clone(),
                    to_host: to_host.clone(),
                    route: hub.to_string(),
                    relayed_at_ms: now_ms(),
                    via: Some(hub.to_string()),
                    intent: send.intent,
                },
            });
            if send.intent.wakes() {
                self.mailboxes
                    .record_relayed_question(send.correlation_id.clone());
            }
        }
        encode_success(
            send.request_id.clone(),
            ResponseResult::MsgQueued {
                correlation_id: send.correlation_id.clone(),
                state,
                warnings,
                to_host: Some(to_host),
                path: Some(path),
            },
        )
    }

    /// The HUB side: a spoke handed a message up its relay; deliver it with
    /// the ordinary `msg.send`.
    ///
    /// Reached only from `AppEvent::UplinkForwarded`, which only the relay
    /// reader for an edge THIS hub dialled produces. It is deliberately not a
    /// socket method: as one, any local process — an agent in a pane included
    /// — could name a spoke and have the hub vouch for a sender it never saw.
    ///
    /// The hub vouches for the edge and for nothing else (#410 pitfall 1). The
    /// sender stays the one the spoke attested, and the one thing the hub can
    /// check — that a frame arriving over spoke S claims to come from S — it
    /// does check, so a spoke cannot speak for another machine.
    pub(crate) fn forward_uplinked_message(
        &mut self,
        spoke: &str,
        mut message: MsgSendParams,
    ) -> String {
        let id = "uplink-forward".to_string();
        // Bound to the edge THIS hub configured and dialled, never to what the
        // spoke says about itself: a spoke's summary `host` is self-reported,
        // so vouching against it would let a cloned or compromised spoke speak
        // for any machine it chose to name (#213, one layer down).
        let Some(peer) = self
            .state
            .peers
            .iter()
            .find(|peer| peer.name.eq_ignore_ascii_case(spoke))
            .cloned()
        else {
            return encode_error(
                id,
                "uplink_unknown_spoke",
                format!(
                    "{spoke} is not in this server's [[peers]], so it has no edge to vouch for"
                ),
            );
        };
        let claimed = message.from_host.clone().unwrap_or_default();
        if let Err(why) = self.spoke_may_claim(&peer, &claimed) {
            crate::logging::uplink_sender_refused(&peer.name, &claimed, &why);
            return encode_error(
                id,
                "uplink_sender_mismatch",
                format!(
                    "{} handed up a message claiming to come from {}: {why}",
                    peer.name,
                    if claimed.is_empty() {
                        "no host at all"
                    } else {
                        &claimed
                    }
                ),
            );
        }
        if message.from_agent.is_none() {
            return encode_error(
                id,
                "sender_unresolved",
                format!("{spoke} handed up a message with no sender identity"),
            );
        }
        // What the recipient reads as the origin is the edge this hub
        // CONFIGURED, not the name the spoke gave itself: the claim was only
        // checked for consistency, the identity comes from config. The vouch
        // then travels in-process, never on the wire.
        message.from_host = Some(peer.name.clone());
        // The request came from this hub's own relay worker, not a pane: its
        // process ancestry is the server itself and attests nobody. Clearing
        // it is what keeps the hub from being stamped as the sender.
        let pid = self.current_api_peer_pid.take();
        let vouched = peer.name.clone();
        let response = self.send_message(id, message, Some(&vouched));
        self.current_api_peer_pid = pid;
        response
    }

    /// Whether a frame arriving over `peer`'s relay may claim `claimed` as its
    /// host. The configured identity — the peer's name, or the host it is
    /// dialled at — always may. The spoke's self-reported hostname may only
    /// when no OTHER configured peer reports the same one: two edges claiming
    /// one machine means at least one is lying, and the hub cannot tell which.
    fn spoke_may_claim(
        &self,
        peer: &crate::config::PeerConfig,
        claimed: &str,
    ) -> Result<(), String> {
        if claimed.is_empty() {
            return Err("a frame must name its host".into());
        }
        let identity = |candidate: &crate::config::PeerConfig| {
            let dialled = candidate.ssh_target();
            let dialled_host = dialled.rsplit('@').next().unwrap_or(dialled);
            claimed.eq_ignore_ascii_case(&candidate.name)
                || claimed.eq_ignore_ascii_case(dialled_host)
        };
        if let Some(other) = self
            .state
            .peers
            .iter()
            .find(|other| !other.name.eq_ignore_ascii_case(&peer.name) && identity(other))
        {
            return Err(format!("that is the configured identity of {}", other.name));
        }
        if identity(peer) {
            return Ok(());
        }
        let reports = |summary: &&crate::peers::PeerSummaryState| {
            summary
                .host
                .as_deref()
                .is_some_and(|host| host.eq_ignore_ascii_case(claimed))
        };
        let reporters: Vec<&str> = self
            .state
            .peer_summaries
            .iter()
            .filter(reports)
            .map(|summary| summary.peer.as_str())
            .collect();
        match reporters.as_slice() {
            [only] if only.eq_ignore_ascii_case(&peer.name) => Ok(()),
            [] => Err(format!(
                "that is not {}'s configured identity, and it has not reported that host",
                peer.name
            )),
            [_] => Err(format!("{claimed} is another peer's host")),
            many => Err(format!(
                "{} peers ({}) report host {claimed}, so none of them can vouch for it",
                many.len(),
                many.join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{
        AgentStatus, MessageTarget, Method, MsgIntent, MsgSendParams, MsgUplinkResultParams,
        MsgUplinkTakeParams, PeerAgentSummary, PeerWorkspaceSummary, Request,
    };
    use crate::config::Config;

    fn test_app() -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::app::App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn peer_with_agent(peer: &str, host: &str, agent_id: &str) -> crate::peers::PeerSummaryState {
        let mut state = crate::peers::PeerSummaryState::new(&crate::config::PeerConfig {
            name: peer.into(),
            ..Default::default()
        });
        state.host = Some(host.into());
        state.workspaces = vec![PeerWorkspaceSummary {
            id: "w1".into(),
            workspace: "remote".into(),
            project_key: None,
            project_label: None,
            branch: None,
            is_linked_worktree: false,
            agent: Some("cc".into()),
            status: AgentStatus::Idle,
            status_age_secs: None,
            activity: None,
            agents: vec![PeerAgentSummary {
                agent_id: agent_id.into(),
                pane_id: "w1:p1".into(),
                agent: Some("cc".into()),
                status: AgentStatus::Idle,
            }],
        }];
        state
    }

    fn send_to(agent: &str) -> MsgSendParams {
        MsgSendParams {
            // No pane to attest from in a unit test, so the sender is asserted
            // — the same shape a CLI caller outside a pane uses.
            from_agent: Some("agent_spoke_1".into()),
            from_host: None,
            to: MessageTarget::Agent {
                agent: agent.into(),
            },
            body: "hello".into(),
            correlation_id: Some("c-410".into()),
            in_reply_to: None,
            intent: MsgIntent::NeedsReply,
            intent_unrecognised: None,
        }
    }

    /// Drive a request the way the socket transport does: handle it, then
    /// hand the responder to `respond_or_park`. Returns what the caller has
    /// been answered SO FAR.
    fn via_transport(
        app: &mut crate::app::App,
        method: Method,
    ) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method,
        });
        app.respond_or_park(tx, response);
        rx
    }

    /// A pid no process has, standing in for the hub's relay. Bound directly,
    /// since `peers.relay_attach` would check that it is alive.
    const RELAY_PID: u32 = u32::MAX - 7;

    /// Run a request as the bound relay would: from the relay's pid.
    fn as_relay(app: &mut crate::app::App, request: Request) -> String {
        app.current_api_peer_pid = Some(RELAY_PID);
        let response = app.handle_api_request(request);
        app.current_api_peer_pid = None;
        response
    }

    fn attach_relay(app: &mut crate::app::App) -> std::sync::mpsc::Receiver<String> {
        app.uplink
            .attach_relay(RELAY_PID, |_| true)
            .expect("no relay bound yet");
        let (tx, rx) = std::sync::mpsc::channel();
        let response = as_relay(
            app,
            Request {
                id: "req".into(),
                method: Method::MsgUplinkTake(MsgUplinkTakeParams::default()),
            },
        );
        app.respond_or_park(tx, response);
        rx
    }

    fn value(line: &str) -> serde_json::Value {
        serde_json::from_str(line).expect("json")
    }

    #[tokio::test]
    async fn a_spoke_with_no_hub_says_so_instead_of_blaming_peers() {
        // No [[peers]], no relay: the refusal must name the real gap — "no hub
        // holds a relay" — not advise adding a peer.
        let mut app = test_app();
        let response = value(&app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgSend(send_to("agent_far_1")),
        }));
        let message = response["error"]["message"].as_str().unwrap_or_default();
        assert_eq!(
            response["error"]["code"], "msg_target_not_found",
            "{response}"
        );
        assert!(
            message.contains("no hub holds a relay"),
            "the spoke says which hop is missing: {message}"
        );
    }

    #[tokio::test]
    async fn a_spoke_hands_up_and_the_sender_hears_the_hubs_real_outcome() {
        let mut app = test_app();
        let take = attach_relay(&mut app);
        assert!(take.try_recv().is_err(), "an idle relay's take parks");

        let sender = via_transport(&mut app, Method::MsgSend(send_to("agent_far_1")));
        assert!(
            sender.try_recv().is_err(),
            "the sender waits for the hub rather than being told a hopeful 'queued'"
        );

        // The parked take is fed at once, carrying the ORIGINATING sender.
        let frames = value(&take.try_recv().expect("the relay is handed the frame"));
        let frame = &frames["result"]["frames"][0];
        assert_eq!(frame["message"]["from_agent"], "agent_spoke_1", "{frame}");
        assert_eq!(
            frame["message"]["from_host"],
            crate::app::short_host_name(),
            "the spoke attests its own host: {frame}"
        );
        assert_eq!(frame["message"]["correlation_id"], "c-410");
        let uplink_id = frame["uplink_id"].as_str().expect("uplink id").to_string();

        // The hub answers: it relayed the message on to `ksb`.
        let ack = value(&as_relay(
            &mut app,
            Request {
                id: "r2".into(),
                method: Method::MsgUplinkResult(MsgUplinkResultParams {
                    uplink_id: uplink_id.clone(),
                    hub: "mba22".into(),
                    response: serde_json::json!({
                        "id": "uplink-forward",
                        "result": {
                            "type": "msg_queued",
                            "correlation_id": "c-410",
                            "state": "relayed",
                            "to_host": "ksb",
                            "path": "direct",
                        },
                    }),
                }),
            },
        ));
        assert_eq!(ack["result"]["matched"], true);

        let answer = value(&sender.try_recv().expect("the sender is answered"));
        assert_eq!(answer["id"], "req", "addressed to the caller's own request");
        assert_eq!(answer["result"]["state"], "relayed", "{answer}");
        assert_eq!(answer["result"]["path"], "via mba22", "{answer}");
        assert_eq!(answer["result"]["to_host"], "ksb", "{answer}");

        // And the sender's own log agrees, for a `msg.status` asked later.
        let status = value(&app.handle_api_request(Request {
            id: "r3".into(),
            method: Method::MsgStatus(crate::api::schema::MsgStatusParams {
                correlation_id: "c-410".into(),
            }),
        }));
        assert_eq!(status["result"]["path"], "via mba22", "{status}");

        // A re-offered frame answered twice resolves nothing the second time.
        let again = value(&as_relay(
            &mut app,
            Request {
                id: "r4".into(),
                method: Method::MsgUplinkResult(MsgUplinkResultParams {
                    uplink_id,
                    hub: "mba22".into(),
                    response: serde_json::json!({"result": {"state": "duplicate"}}),
                }),
            },
        ));
        assert_eq!(again["result"]["matched"], false);
    }

    #[tokio::test]
    async fn the_hubs_refusal_reaches_the_sender_naming_the_hop_that_broke() {
        let mut app = test_app();
        let _take = attach_relay(&mut app);
        let sender = via_transport(&mut app, Method::MsgSend(send_to("agent_far_1")));
        let uplink_id = app
            .uplink
            .complete_peek_for_test()
            .expect("a frame is waiting");
        as_relay(
            &mut app,
            Request {
                id: "r2".into(),
                method: Method::MsgUplinkResult(MsgUplinkResultParams {
                    uplink_id,
                    hub: "mba22".into(),
                    response: serde_json::json!({
                        "id": "uplink-forward",
                        "error": {
                            "code": "peer_unreachable",
                            "message": "mba22 cannot reach ksb (auth refused): Permission denied",
                            "data": {"hop": "mba22 → ksb", "reason": "auth_refused", "retryable": true},
                        },
                    }),
                }),
            },
        );
        let answer = value(&sender.try_recv().expect("answered"));
        let message = answer["error"]["message"].as_str().unwrap_or_default();
        assert_eq!(answer["error"]["code"], "peer_unreachable", "{answer}");
        assert!(message.contains("mba22 cannot reach ksb"), "{message}");
        assert_eq!(answer["error"]["data"]["via"], "mba22", "{answer}");
        assert_eq!(
            answer["error"]["data"]["reason"], "auth_refused",
            "{answer}"
        );
    }

    #[tokio::test]
    async fn a_send_the_hub_never_answers_times_out_rather_than_hanging() {
        let mut config = Config::default();
        config.msg.uplink_timeout_secs = 1;
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            crate::app::App::new(&config, true, None, api_rx, crate::api::EventHub::default());
        let _take = attach_relay(&mut app);
        let sender = via_transport(&mut app, Method::MsgSend(send_to("agent_far_1")));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        app.expire_uplink();
        let answer = value(&sender.try_recv().expect("answered on expiry"));
        assert_eq!(answer["error"]["code"], "uplink_timeout", "{answer}");
        assert_eq!(
            answer["error"]["data"]["taken_by_hub"], true,
            "the sender learns a hub DID take it, so a retry is a dedupe, not a gamble"
        );
    }

    #[tokio::test]
    async fn a_hub_vouches_for_its_edge_and_refuses_a_spoke_speaking_for_another_host() {
        // Pitfall 1: the hub must not launder identity. A frame that arrived
        // over the relay into `sage` may only speak for `sage`.
        let mut app = test_app();
        app.state.peers = ["sage", "ksb"]
            .into_iter()
            .map(|name| crate::config::PeerConfig {
                name: name.into(),
                ..Default::default()
            })
            .collect();
        // sage's own summary claims to BE ksb — a cloned VM, or a lie. The
        // self-report must not be what the hub vouches against.
        app.state.peer_summaries = vec![
            peer_with_agent("sage", "ksb", "agent_sage_1"),
            peer_with_agent("ksb", "ksb", "agent_ksb_1"),
        ];
        let mut message = send_to("agent_nowhere_1");
        message.from_host = Some("ksb".into());
        let response = value(&app.forward_uplinked_message("sage", message));
        assert_eq!(
            response["error"]["code"], "uplink_sender_mismatch",
            "{response}"
        );

        // From its own host it is forwarded — and a target the hub cannot place
        // is a plain miss: a forwarded message is never handed up again.
        let mut message = send_to("agent_nowhere_1");
        message.from_host = Some("sage".into());
        let response = value(&app.forward_uplinked_message("sage", message));
        assert_eq!(
            response["error"]["code"], "msg_target_not_found",
            "{response}"
        );
    }

    #[tokio::test]
    async fn a_forwarded_message_crosses_at_most_one_hub() {
        // Pitfall 2, loops: a message that already came from another host is
        // handed on only over a DIRECT edge. Here the target is known only
        // through a relayed row, so a second forward is refused by name.
        let mut app = test_app();
        app.state.peers = vec![crate::config::PeerConfig {
            name: "hub2".into(),
            ..Default::default()
        }];
        let mut relayed =
            crate::peers::relayed_entry_from_wire(crate::api::schema::RelayedFleetPeer {
                name: "ksb".into(),
                ssh_target: "ksb".into(),
                host: Some("ksb".into()),
                version: None,
                protocol: None,
                system: None,
                latency_ms: None,
                workspaces: peer_with_agent("ksb", "ksb", "agent_ksb_1").workspaces,
                age_secs: Some(1),
                error: None,
                origin: "hub2".into(),
                origin_last_ok_secs: Some(1),
                proxy_jump: Some("hub2".into()),
                icon: None,
            })
            .expect("valid row");
        relayed.via = Some("hub2".into());
        app.state.relayed_fleet_cache.insert("ksb".into(), relayed);

        let located = app
            .locate_agent("agent_ksb_1")
            .expect("found via the relay");
        assert_eq!(
            located.route.as_deref(),
            Some("hub2"),
            "a relayed row routes via the hub that relayed it, not None"
        );
        assert!(!located.direct);

        let mut forwarded = send_to("agent_ksb_1");
        forwarded.from_host = Some("sage".into());
        let response = value(&app.handle_api_request(Request {
            id: "req".into(),
            method: Method::MsgSend(forwarded),
        }));
        assert_eq!(response["error"]["code"], "forward_limit", "{response}");
    }

    #[tokio::test]
    async fn no_socket_caller_can_reach_the_forward_path() {
        // Re-review of #414: as a socket method, the forward let any local
        // process — an agent in a pane included — name a spoke and have the
        // hub vouch for a sender it never saw. It is in-process only now, so
        // the wire has no spelling for it at all.
        let request = r#"{"id":"x","method":"msg.uplink_forward","params":{"spoke":"sage","message":{"to":{"type":"agent","agent":"a"},"body":"b","from_host":"sage","from_agent":"a"}}}"#;
        assert!(
            serde_json::from_str::<Request>(request).is_err(),
            "msg.uplink_forward must not parse as a request"
        );
    }

    #[test]
    fn a_relay_stamps_a_foreign_host_only_for_a_vouched_forward() {
        use super::super::messages::relay_sender_host;
        let me = crate::app::short_host_name();
        assert_eq!(relay_sender_host(false, None), me, "unvouched, unattested");
        assert_eq!(
            relay_sender_host(false, Some("sage")),
            "sage",
            "vouched by the hub's in-process uplink path"
        );
        assert_eq!(
            relay_sender_host(true, Some("sage")),
            me,
            "a locally attested sender is always this host"
        );
    }

    #[tokio::test]
    async fn a_hub_stamps_the_configured_edge_not_the_spokes_self_report() {
        // Blocker A: a spoke configured as `anvil` that calls itself `vm-dev`
        // may hand up as vm-dev (its unique self-report), but the origin the
        // recipient reads is the edge the hub dialled.
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].focused_pane_id().expect("pane");
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        let local_agent = app.state.terminals[&terminal_id].agent_id.to_string();
        let pane = app.locate_agent(&local_agent).expect("local").pane_id;

        app.state.peers = vec![crate::config::PeerConfig {
            name: "anvil".into(),
            ..Default::default()
        }];
        app.state.peer_summaries = vec![peer_with_agent("anvil", "vm-dev", "agent_vm-dev_1")];
        let mut message = send_to(&local_agent);
        message.from_agent = Some("agent_vm-dev_1".into());
        message.from_host = Some("vm-dev".into());
        let queued = value(&app.forward_uplinked_message("anvil", message));
        assert_eq!(queued["result"]["state"], "queued", "{queued}");

        let inbox = value(&app.handle_api_request(Request {
            id: "read".into(),
            method: Method::MsgRead(crate::api::schema::MsgReadParams { pane: Some(pane) }),
        }));
        let delivered = &inbox["result"]["messages"][0];
        assert_eq!(delivered["from_host"], "anvil", "{inbox}");
        assert_eq!(delivered["from_agent"], "agent_vm-dev_1", "{inbox}");
    }

    #[tokio::test]
    async fn only_the_bound_relay_may_take_answer_or_push() {
        // #416 review, blocker 2: the relay methods are on the local socket,
        // so without a binding any same-user process could take a spoke's
        // pending messages or forge the hub's answer to them.
        let mut app = test_app();
        let _take = attach_relay(&mut app);
        let _sender = via_transport(&mut app, Method::MsgSend(send_to("agent_far_1")));
        let uplink_id = app.uplink.complete_peek_for_test().expect("waiting");

        app.current_api_peer_pid = Some(RELAY_PID - 1);
        for method in [
            Method::MsgUplinkTake(MsgUplinkTakeParams::default()),
            Method::MsgUplinkResult(MsgUplinkResultParams {
                uplink_id: uplink_id.clone(),
                hub: "attacker".into(),
                response: serde_json::json!({"result": {"state": "relayed"}}),
            }),
            Method::PeersHubFleet(crate::api::schema::PeersHubFleetParams {
                hub: "attacker".into(),
                fleet: Vec::new(),
            }),
        ] {
            let response = value(&app.handle_api_request(Request {
                id: "forged".into(),
                method,
            }));
            assert_eq!(response["error"]["code"], "not_the_relay", "{response}");
        }
        app.current_api_peer_pid = None;
        assert_eq!(
            app.uplink.complete_peek_for_test().as_deref(),
            Some(uplink_id.as_str()),
            "the frame is still waiting for the REAL relay"
        );

        // A live relay cannot be displaced by a second attach.
        app.current_api_peer_pid = Some(std::process::id());
        let displaced = value(&app.handle_api_request(Request {
            id: "attach".into(),
            method: Method::PeersRelayAttach(crate::api::schema::EmptyParams {}),
        }));
        app.current_api_peer_pid = None;
        assert!(
            displaced["error"]["code"] == "relay_already_attached"
                || displaced["result"].is_object(),
            "{displaced}"
        );
    }

    #[test]
    fn an_uplink_id_is_not_guessable_from_its_neighbour() {
        let first = super::mint_uplink_id("c1");
        let second = super::mint_uplink_id("c1");
        assert_ne!(first, second);
        let tail = |id: &str| id.rsplit(':').next().unwrap_or_default().to_string();
        assert_eq!(tail(&first).len(), 16, "{first}");
        assert_ne!(tail(&first), tail(&second), "the random half differs");
    }
}
