//! Bind the relay process used by mesh and fleet requests.
use super::responses::{encode_error, encode_success};
use crate::api::schema::ResponseResult;
use crate::app::App;

const SSHD_ANCESTRY_DEPTH: usize = 16;

/// The ancestor a relay must descend from: `sshd` (which also matches macOS's
/// `sshd-session` and Linux's `sshd:` privsep child).
///
/// Debug builds let the multi-node test harness name a different one: its
/// fake ssh runs the relay under the polling node's own server, and macOS
/// refuses to run a copied system shell renamed to stand in for sshd. A
/// release build never reads the variable.
fn relay_ancestor() -> String {
    #[cfg(debug_assertions)]
    if let Ok(name) = std::env::var("FLOCK_TEST_RELAY_ANCESTOR") {
        return name;
    }
    "sshd".to_string()
}

/// Whether `pid` was started by an ssh session: some ancestor is sshd.
fn descends_from_sshd(pid: u32) -> bool {
    let ancestor = relay_ancestor();
    let mut current = pid;
    for _ in 0..SSHD_ANCESTRY_DEPTH {
        let Some(parent) = crate::platform::process_parent_id(current) else {
            return false;
        };
        if parent <= 1 || parent == current {
            return false;
        }
        if crate::platform::process_name(parent).is_some_and(|name| name.starts_with(&ancestor)) {
            return true;
        }
        current = parent;
    }
    false
}

impl App {
    pub(crate) fn respond_or_park(
        &mut self,
        respond_to: std::sync::mpsc::Sender<String>,
        response: String,
    ) {
        if let Some(mut relay) = self.message_relays.pending.take() {
            relay.respond_to = Some(respond_to);
            self.enqueue_message_relay(relay.into_work());
            return;
        }
        if let Some((request_id, pane_id, attempt)) = self.pending_agent_submit.take() {
            self.schedule_guarded_request(request_id, pane_id, attempt, respond_to);
            return;
        }
        if let Some(paste) = self.pending_paste.take() {
            self.schedule_paste(paste, respond_to);
            return;
        }
        let _ = respond_to.send(response);
    }
    pub(crate) fn expire_uplink(&mut self) {
        self.state.evict_expired_relayed_entries();
    }
    /// `peers.relay_attach` — the hub's relay binds this server's uplink to
    /// its own process (#410 review).
    ///
    /// The relay methods ride the local socket, which every same-user process
    /// can reach. Without a binding, a stray process could take a spoke's
    /// pending messages, kiln the hub's answers, or plant fleet rows. The
    /// binding is the caller's socket peer pid AND that process's start time
    /// (so a reused pid is not the relay), refused when either cannot be read,
    /// refused from inside a pane, refused unless the caller descends from
    /// sshd — the real relay is started by the hub's ssh session — and never
    /// displacing a relay that is still alive.
    ///
    /// Same-user is still the boundary: `ssh localhost flk peers relay` has
    /// sshd for a parent too. What this closes is the casual squatter — a
    /// detached shell, a launchd job, a script — and the pane agent.
    pub(super) fn handle_peers_relay_attach(&mut self, id: String) -> String {
        if self.uplink.is_relay(
            self.current_api_peer_pid,
            crate::platform::process_start_time,
        ) {
            return encode_success(id, ResponseResult::Ok {});
        }
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
        let Some(started) = crate::platform::process_start_time(pid) else {
            return encode_error(
                id,
                "relay_unattestable",
                "the caller's start time could not be read, so it cannot be told from a pid reuser",
            );
        };
        if !descends_from_sshd(pid) {
            return encode_error(
                id,
                "relay_not_from_ssh",
                "a relay is started by the hub's ssh session; this caller has no sshd ancestor",
            );
        }
        match self
            .uplink
            .attach_relay(pid, started, crate::platform::process_start_time)
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
    pub(super) fn refuse_unless_relay(&mut self, id: &str, method: &str) -> Option<String> {
        let caller = self.current_api_peer_pid;
        (!self
            .uplink
            .is_relay(caller, crate::platform::process_start_time))
        .then(|| {
            encode_error(
                id.to_string(),
                "not_the_relay",
                format!("{method} is accepted only from the relay bound by peers.relay_attach"),
            )
        })
    }
}
