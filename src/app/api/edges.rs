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
    pub(crate) fn expire_relayed_entries(&mut self) {
        self.state.evict_expired_relayed_entries();
    }
    /// Bind only a hello Begin from an attestable SSH relay outside panes.
    pub(super) fn attach_inbound_edge(&mut self, id: String) -> String {
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
            .inbound
            .attach(pid, started, crate::platform::process_start_time)
        {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(code) => encode_error(
                id,
                code,
                "inbound edge attachment unavailable; retry mesh.hello",
            ),
        }
    }

    /// The refusal for a relay-only method called by anyone but the relay.
    pub(super) fn refuse_unless_edge(&mut self, id: &str, method: &str) -> Option<String> {
        let caller = self.current_api_peer_pid;
        (self
            .inbound
            .edge(caller, crate::platform::process_start_time)
            .is_none())
        .then(|| {
            encode_error(
                id.to_string(),
                "not_an_edge",
                format!("{method} is accepted only from an edge bound by mesh.hello"),
            )
        })
    }
}
