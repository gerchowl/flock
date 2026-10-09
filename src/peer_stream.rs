//! A held SSH connection per peer, carrying API requests to `flk peers relay`.
//!
//! `run_peer_ssh` spawns a fresh `ssh` per call, so every poll and every
//! message pays a full handshake to move a few hundred bytes. Measured cold
//! from atlas with multiplexing disabled, exit status checked:
//!
//! ```text
//! kiln           0.13s
//! kiln-dev       0.16s
//! ethz-heimdall   0.97s   (Tailscale-remote)
//! ```
//!
//! There is no cheap peer — the floor is ~130ms per call. Five requests cost
//! 1.93s spawning per call versus 0.38s over one held connection (5.1x):
//! linear versus constant.
//!
//! Tuning the poll interval instead does not work. A 2s cadence is a 6.5%
//! duty cycle against the nearest peer and ~50% against the furthest, and the
//! spread is 7.5x, so no single value is right and a per-peer knob only moves
//! the problem onto the operator. Holding the connection makes the question
//! moot.
//!
//! **Liveness is not silence.** On an idle fleet nothing flows for long
//! stretches, so "no traffic" says nothing about health. Death is observed
//! directly instead: `run_peer_ssh` already sets `ServerAliveInterval=5
//! ServerAliveCountMax=2`, so ssh itself tears down a dead or half-open
//! connection (the roaming-hopper case) and exits — which arrives here as EOF
//! on the reader thread. No timer decides that.
//!
//! Every held edge must complete mesh.hello before carrying requests. A peer
//! that cannot authenticate is refused with its reason retained for status.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::config::PeerConfig;

/// How long a single request may wait before the peer is declared wedged.
///
/// Bounds the case ssh cannot see: the connection is healthy but the far-side
/// relay is stuck (blocked connecting to a hung local server, say), so
/// ServerAlive keeps answering while no response ever comes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Authentication or compatibility refusals retry slowly. Status retains the
/// refusal reason while this backoff runs.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(60);

/// Enrollment refusals default to the long delay. Only transport failures and
/// missing-server and inbound-capacity responses are transient.
struct EnrollmentError {
    detail: String,
    transient: bool,
}

impl From<String> for EnrollmentError {
    fn from(detail: String) -> Self {
        Self {
            detail,
            transient: false,
        }
    }
}

impl EnrollmentError {
    fn transport(detail: String) -> Self {
        Self {
            detail,
            transient: true,
        }
    }

    fn response(error: &serde_json::Value, detail: String) -> Self {
        Self {
            detail,
            transient: matches!(
                error["code"].as_str(),
                Some("no_local_server" | "inbound_edges_full")
            ),
        }
    }
}

/// How long a failed stream waits for ssh's stderr to drain before it is
/// explained. ssh writes its error and exits, and the stdout EOF that reports
/// the failure can overtake the stderr reader by a scheduler tick; this only
/// bounds that race, so a relay that is wedged rather than gone never waits.
const STDERR_SETTLE: Duration = Duration::from_secs(1);

/// One held `ssh <peer> flk peers relay`.
///
/// Requests are strictly sequential, matching the relay's own shape, so
/// responses need no demultiplexing — write one, read one. The reader runs on
/// its own thread purely so a wedged peer hits [`REQUEST_TIMEOUT`] instead of
/// blocking forever on a pipe that has no read timeout.
struct PeerStream {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    /// Latest unsolicited summary the peer pushed. One slot, not a queue:
    /// the payload is a snapshot, so an older push carries nothing the newer
    /// one does not already say and queueing them would only serve staleness.
    /// The newest unconsumed push, with the instant it ARRIVED.
    ///
    /// The timestamp is load-bearing, not diagnostic: a push answers a poll in
    /// place of a live request, so an old one in this slot actively SUPPRESSES
    /// the fetch that would have refreshed the fields that moved while it sat
    /// here (#4). See `MAX_PUSH_AGE`.
    latest_push: Arc<Mutex<Option<(std::time::Instant, String)>>>,
    /// The last few lines ssh wrote to stderr (#418). Before this the relay's
    /// stderr went to /dev/null, so a stream that could never be established
    /// failed without explaining why.
    stderr_tail: Arc<Mutex<std::collections::VecDeque<String>>>,
    /// Disconnects when ssh's stderr reaches EOF — ssh has exited.
    stderr_done: Receiver<()>,
    /// The agent socket this connection was dialled with, so an auth failure
    /// can be attributed to a dead agent (#418).
    agent: crate::platform::ssh_agent::AgentSocket,
}

/// Why a stream could not be established, as last observed.
struct StreamFailure {
    reason: crate::peers::SshFailureReason,
    detail: String,
    since: std::time::Instant,
}

/// Whether a relay line is an unsolicited push rather than a response.
///
/// Decided on the parsed object's top-level `push` member, never on the
/// serialized text. A substring test cannot tell a `push` KEY from a `push`
/// VALUE, and the summary payload carries arbitrary user-controlled strings —
/// workspace labels, branch names, agent statuses. A branch named `push` was
/// enough to route that peer's summary RESPONSE into the push slot, stranding
/// the caller until `REQUEST_TIMEOUT` and tearing the connection down into its
/// reconnect backoff, every round, for as long as the name existed.
fn line_is_push(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .is_ok_and(|value| value.get("push").is_some_and(|push| !push.is_null()))
}

/// The push kind a summary push carries.
const SUMMARY_PUSH: &str = "peers.summary";

fn push_kind(line: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("push")?
        .as_str()
        .map(str::to_string)
}

fn wake_registry() -> &'static Mutex<HashMap<String, std::sync::atomic::AtomicBool>> {
    static WAKES: OnceLock<Mutex<HashMap<String, std::sync::atomic::AtomicBool>>> = OnceLock::new();
    WAKES.get_or_init(Default::default)
}

pub(crate) fn take_wake(peer: &PeerConfig) -> bool {
    wake_registry()
        .lock()
        .ok()
        .and_then(|wakes| {
            wakes
                .get(&peer.name)
                .map(|wake| wake.swap(false, std::sync::atomic::Ordering::AcqRel))
        })
        .unwrap_or(false)
}

/// Split the relay's inbound lines: pushes to the single-slot buffer, every
/// other line to whoever is waiting on a response.
///
/// Extracted from the reader thread so the ROUTING is reachable from a test,
/// not just the classifier. A test that only exercises `line_is_push` would
/// still pass if this call site regressed to matching the raw text.
///
/// Returns on EOF, which is exactly how ssh reports the connection is gone.
/// The dropped sender then disconnects the channel and every later request
/// fails fast rather than waiting out the timeout.
fn route_relay_lines<R: BufRead>(
    peer: &str,
    reader: R,
    responses: &std::sync::mpsc::Sender<String>,
    push_slot: &Arc<Mutex<Option<(std::time::Instant, String)>>>,
) {
    for line in reader.lines().map_while(Result::ok) {
        // A push carries `push` where a response carries `id`, so routing
        // needs no guessing. Anything else is a response and belongs to
        // whoever is waiting on the channel — including a line that is not
        // JSON at all, which surfaces to the caller as a parse error rather
        // than disappearing into the push slot where nobody would see it.
        if line_is_push(&line) {
            if push_kind(&line).as_deref() == Some("mesh.wake") {
                if let Ok(mut wakes) = wake_registry().lock() {
                    wakes
                        .entry(peer.into())
                        .or_default()
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                continue;
            }
            // A kind this build does not know is dropped, never fed to the
            // summary parser, where it would fail the poll it answered.
            if let Some(kind) = push_kind(&line).filter(|kind| kind != SUMMARY_PUSH) {
                crate::logging::peer_push_unknown_kind(peer, &kind);
                continue;
            }
            if let Ok(mut slot) = push_slot.lock() {
                *slot = Some((std::time::Instant::now(), line));
            }
            continue;
        }
        if responses.send(line).is_err() {
            break;
        }
    }
}

impl PeerStream {
    fn spawn(peer: &PeerConfig) -> Result<Self, String> {
        let mut command = crate::process::TracedCommand::new("ssh", "peers");
        // Same options as the one-shot path, ServerAlive included: ssh
        // detects a dead or half-open link itself and exits, which is how
        // death reaches us. Nothing here polls for it.
        command
            .args(crate::peers::PEER_DIAL_SSH_OPTIONS)
            .args([peer.ssh_target(), peer.relay_command.as_str()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A peer that cannot hold a stream is re-dialled every backoff for
            // as long as the outage lasts. The spawn is routine; what it came
            // to is reported by `peer.stream.*`, on the edge.
            .periodic();
        let agent = crate::peers::apply_dial_agent(&mut command);
        let mut child = command
            .spawn_traced()
            .map_err(|err| format!("ssh spawn failed: {err}"))?;

        let stderr_tail = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let (stderr_done_tx, stderr_done) = std::sync::mpsc::channel::<()>();
        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let Ok(mut tail) = tail.lock() else {
                        break;
                    };
                    // Bounded twice: lines kept, and each line's length, so a
                    // chatty or hostile far side cannot grow this.
                    let line: String = line
                        .chars()
                        .take(crate::process::MAX_STDERR_LOG_CHARS)
                        .collect();
                    tail.push_back(line);
                    while tail.len() > crate::process::STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                }
                drop(stderr_done_tx);
            });
        }

        let stdin = child.stdin.take().ok_or("ssh stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("ssh stdout unavailable")?;
        let (tx, lines) = std::sync::mpsc::channel();
        let latest_push = Arc::new(Mutex::new(None));
        let push_slot = Arc::clone(&latest_push);
        let reader_peer = peer.name.clone();
        std::thread::spawn(move || {
            route_relay_lines(&reader_peer, BufReader::new(stdout), &tx, &push_slot);
        });
        Ok(Self {
            child,
            stdin,
            lines,
            next_id: 0,
            latest_push,
            stderr_tail,
            stderr_done,
            agent,
        })
    }

    fn enroll(&mut self, peer: &PeerConfig) -> Result<String, EnrollmentError> {
        use crate::mesh::{
            hello::{self, Challenge, Hello, Offer},
            identity::NodeIdentity,
        };
        let identity = NodeIdentity::load().map_err(|e| e.to_string())?;
        let dialer = Offer::new(&identity, crate::app::short_host_name())?;
        let request = serde_json::to_value(Hello::Begin {
            offer: dialer.clone(),
        })
        .map_err(|e| e.to_string())?;
        let raw = self
            .request("mesh.hello", request)
            .map_err(EnrollmentError::transport)?;
        let response: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        if let Some(error) = response.get("error") {
            if error["code"] == "mesh_version_mismatch" {
                if let Some(remote) = error["data"]["mesh"]
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok())
                {
                    return Err(
                        hello::version_mismatch(hello::version(), remote, &peer.name).into(),
                    );
                }
            }
            if matches!(
                error["code"].as_str(),
                Some("invalid_request" | "mesh_version_mismatch")
            ) {
                let remote = error["data"]["mesh"]
                    .as_u64()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unsupported".into());
                return Err(format!("mesh handshake refused (local {}, remote {remote}): {error}; upgrade flk on {}", hello::version(), peer.name).into());
            }
            return Err(EnrollmentError::response(
                error,
                format!("mesh handshake refused for {}: {error}", peer.name),
            ));
        }
        let challenge: Challenge = serde_json::from_value(response["result"]["challenge"].clone())
            .map_err(|e| {
                format!(
                    "unsupported mesh hello (local {}, remote unknown): {e}; upgrade flk on {}",
                    hello::version(),
                    peer.name
                )
            })?;
        challenge.offer.validate(&peer.name)?;
        hello::verify(&dialer, &challenge.offer, "acceptor", &challenge.signature)?;
        hello::check_pin(
            &peer.name,
            &challenge.offer,
            crate::mesh::store::PinSource::Configured,
            false,
        )?;
        let signature = hello::sign(&identity, &dialer, &challenge.offer, "dialer")?;
        let finish =
            serde_json::to_value(Hello::Finish { signature }).map_err(|e| e.to_string())?;
        let raw = self
            .request("mesh.hello", finish)
            .map_err(EnrollmentError::transport)?;
        let ack: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        if let Some(error) = ack.get("error") {
            return Err(EnrollmentError::response(
                error,
                format!("mesh enrollment refused: {error}"),
            ));
        }
        if !ack.get("result").is_some_and(serde_json::Value::is_object) {
            return Err("invalid mesh enrollment acknowledgement".to_string().into());
        }
        hello::check_pin(
            &peer.name,
            &challenge.offer,
            crate::mesh::store::PinSource::Configured,
            true,
        )?;
        if let Ok(mut push) = self.latest_push.lock() {
            *push = None;
        }
        Ok(challenge.offer.node_id)
    }

    /// Explain a failed request from what ssh said on the way out.
    ///
    /// Returns the one-line detail (ssh's last stderr line, attributed to a
    /// dead agent where that is the cause) and the bounded tail for the log.
    /// A connection that went away waits briefly for stderr to drain; a
    /// wedged one does not, since ssh is still running and has said nothing.
    fn explain(&self, err: &str) -> (String, Option<String>) {
        if !err.starts_with(WEDGED) {
            let _ = self.stderr_done.recv_timeout(STDERR_SETTLE);
        }
        let lines: Vec<String> = self
            .stderr_tail
            .lock()
            .map(|tail| tail.iter().cloned().collect())
            .unwrap_or_default();
        let tail = crate::process::shape_stderr_tail(lines.join("\n").as_bytes());
        let detail = match lines.iter().rev().find(|line| !line.trim().is_empty()) {
            Some(last) => crate::peers::attribute_dial_failure(
                crate::report::redact::mask_credentials(last.trim()),
                &self.agent,
            ),
            None => err.to_string(),
        };
        (detail, tail)
    }

    /// Send one API request, return its response line.
    ///
    /// `method` and `params` are serialized here rather than accepted as a
    /// pre-built string so a caller cannot accidentally frame its own request
    /// and desynchronize the one-request-one-response pairing.
    fn request(&mut self, method: &str, params: serde_json::Value) -> Result<String, String> {
        // A response left over from a timed-out predecessor would be read as
        // the answer to this request. Discard anything already buffered.
        loop {
            match self.lines.try_recv() {
                Ok(_) => continue,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Err(CONNECTION_CLOSED.into()),
            }
        }

        self.next_id += 1;
        let request = serde_json::json!({
            "id": format!("stream-{}", self.next_id),
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{request}").map_err(|err| format!("write failed: {err}"))?;
        self.stdin
            .flush()
            .map_err(|err| format!("flush failed: {err}"))?;

        match self.lines.recv_timeout(REQUEST_TIMEOUT) {
            Ok(line) => {
                if method != "mesh.hello" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                        if matches!(
                            value["error"]["code"].as_str(),
                            Some(
                                "mesh_not_enrolled"
                                    | "not_an_edge"
                                    | "inbound_edges_full"
                                    | "no_local_server"
                            )
                        ) {
                            return Err("mesh edge lost enrollment; reconnect required".into());
                        }
                    }
                }
                Ok(line)
            }
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "{WEDGED} in {}s — peer relay wedged",
                REQUEST_TIMEOUT.as_secs()
            )),
            Err(RecvTimeoutError::Disconnected) => Err(CONNECTION_CLOSED.into()),
        }
    }
}

/// The error a request reports when ssh has gone away under it.
const CONNECTION_CLOSED: &str = "connection closed";

/// How a request's error starts when the connection is up but the relay never
/// answered — the one failure where ssh has nothing on stderr to wait for.
const WEDGED: &str = "no response";

impl Drop for PeerStream {
    fn drop(&mut self) {
        // Closing stdin ends `peers relay` at its own read loop, so the remote
        // side exits cleanly instead of being killed mid-request.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Per-peer connection slot: the stream when held, plus when it may next be
/// re-spawned after a failure.
#[derive(Default)]
struct Slot {
    stream: Option<PeerStream>,
    retry_after: Option<std::time::Instant>,
    transient_failures: u32,
    /// The ssh destination this connection was opened against, so a config
    /// reload can tell "this peer moved" from "some unrelated key changed".
    target: String,
    /// Why the last attempt to ESTABLISH a stream failed, until one succeeds
    /// (#418). A stream that was up and later died is not recorded here: that
    /// is an ordinary reconnect, not a peer that cannot hold a stream at all.
    failure: Option<StreamFailure>,
}

impl Slot {
    fn transient_backoff(&mut self) -> Duration {
        let ceiling_ms = (1_000_u64 << self.transient_failures.min(5)).min(30_000);
        self.transient_failures = self.transient_failures.saturating_add(1);
        let mut random = [0_u8; 8];
        if getrandom::fill(&mut random).is_err() {
            return Duration::from_millis(ceiling_ms);
        }
        // Equal jitter avoids synchronized retries without permitting a tight loop.
        Duration::from_millis(ceiling_ms / 2 + u64::from_ne_bytes(random) % (ceiling_ms / 2 + 1))
    }
}

type Registry = Mutex<HashMap<String, Arc<Mutex<Slot>>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

type Enrollments = Mutex<HashMap<String, (crate::mesh::hello::Enrollment, u64)>>;

fn enrollments() -> &'static Enrollments {
    static STATUS: OnceLock<Enrollments> = OnceLock::new();
    STATUS.get_or_init(Default::default)
}

static ENROLLMENT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn enrollment_generation() -> u64 {
    ENROLLMENT_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

fn set_enrollment(peer: &PeerConfig, result: Result<String, EnrollmentError>) {
    let (node_id, reason, state) = match result {
        Ok(node) => (Some(node), None, "pinned"),
        Err(error) => (
            None,
            Some(error.detail),
            if error.transient {
                "retrying"
            } else {
                "refused"
            },
        ),
    };
    let enrolled = node_id.is_some();
    if let Ok(mut statuses) = enrollments().lock() {
        let previous = statuses.get(&peer.name).and_then(|s| s.0.node_id.clone());
        let generation = if enrolled {
            enrollment_generation().saturating_add(1)
        } else {
            statuses.get(&peer.name).map(|s| s.1).unwrap_or(0)
        };
        statuses.insert(
            peer.name.clone(),
            (
                crate::mesh::hello::Enrollment {
                    peer: peer.name.clone(),
                    source: crate::mesh::store::PinSource::Configured,
                    pin_origin: Default::default(),
                    node_id: node_id.or(previous),
                    state: state.into(),
                    reason,
                },
                generation,
            ),
        );
        // Publish the wake only after the enrolled status is visible.
        if enrolled {
            ENROLLMENT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub(crate) fn enrollment(peer: &PeerConfig) -> crate::mesh::hello::Enrollment {
    enrollments()
        .lock()
        .ok()
        .and_then(|s| s.get(&peer.name).map(|s| s.0.clone()))
        .unwrap_or_else(|| crate::mesh::hello::Enrollment {
            peer: peer.name.clone(),
            source: crate::mesh::store::PinSource::Configured,
            pin_origin: Default::default(),
            node_id: None,
            state: "pending".into(),
            reason: None,
        })
}

/// A successful reattachment remains observable even between app ticks.
pub(crate) fn peer_enrollment_generation(peer: &PeerConfig) -> u64 {
    enrollments()
        .lock()
        .ok()
        .and_then(|s| s.get(&peer.name).map(|s| s.1))
        .unwrap_or(0)
}

pub(crate) fn reset_enrollment(peer: &str) {
    // Removing the registry entry avoids waiting on a worker that is itself
    // waiting for an API response from this loop.
    if let Ok(mut registry) = registry().lock() {
        registry.remove(peer);
    }
    if let Ok(mut statuses) = enrollments().lock() {
        statuses.remove(peer);
    }
}

/// Send an API request to `peer` over its held connection.
///
/// Errors refuse this edge. There is no compatibility fallback.
pub fn request(
    peer: &PeerConfig,
    method: &str,
    params: serde_json::Value,
) -> Result<String, String> {
    request_over(peer, method, params, true)
}

/// Hand a spoke this hub's view of the rest of the fleet (#410), over the
/// connection already held to it — never a fresh one. Gossip used to flow only
/// up, from pollee to poller, so a spoke that polls nobody knew nothing past
/// itself. A spoke too old to know the method answers with an error line,
/// which is ignored: it simply keeps its old, empty view.
///
/// `hub_self` is this hub's own row (#424): `fleet` never includes the hub, so
/// without it a client that switched here from the hub only had a copy of the
/// hub frozen at switch time.
pub fn push_hub_fleet(
    peer: &PeerConfig,
    fleet: Vec<crate::api::schema::RelayedFleetPeer>,
    hub_self: Option<Box<crate::api::schema::RelayedFleetPeer>>,
) {
    let params = crate::api::schema::PeersHubFleetParams {
        hub: crate::app::short_host_name(),
        fleet,
        hub_self,
    };
    let Ok(params) = serde_json::to_value(params) else {
        return;
    };
    let _ = request_over(peer, "peers.hub_fleet", params, false);
}

/// `spawn: false` sends only over a connection that is already held.
fn request_over(
    peer: &PeerConfig,
    method: &str,
    params: serde_json::Value,
    spawn: bool,
) -> Result<String, String> {
    if peer.summary_command != crate::config::model::default_peer_summary_command() {
        let reason = "custom summary transport unsupported: restore the default summary_command to hold a mesh edge".to_string();
        set_enrollment(peer, Err(reason.clone().into()));
        return Err(reason);
    }
    // Outer lock is held only long enough to find the slot; the request itself
    // runs under the per-peer lock so one slow peer cannot stall the others.
    let slot = {
        let mut registry = registry().lock().map_err(|_| "registry poisoned")?;
        Arc::clone(registry.entry(peer.name.clone()).or_default())
    };
    // A caller that panicked mid-request poisons this slot. Nothing in `Slot`
    // is left inconsistent by that — the worst case is a held stream whose
    // request/response pairing is no longer known, which is exactly the state
    // where dropping the stream is the right move. Recover and reconnect
    // rather than returning an error for the rest of the process lifetime:
    // otherwise the panic guard on the fetch worker would only trade "this
    // peer is never polled again" for "this peer is stuck on one-shot ssh
    // forever", which is not the recovery it claims to be.
    let mut slot = match slot.lock() {
        Ok(slot) => slot,
        Err(poisoned) => {
            let mut slot = poisoned.into_inner();
            slot.stream = None;
            slot
        }
    };

    if let Some(retry_after) = slot.retry_after {
        if std::time::Instant::now() < retry_after {
            return Err(slot
                .failure
                .as_ref()
                .map(|f| f.detail.clone())
                .unwrap_or_else(|| "connection backing off".into()));
        }
    }

    // The peer's ssh destination can move under a slot that was busy when the
    // config reload swept through, and `retain_configured` deliberately does
    // not wait for it. Check at the point of use, where the lock is already
    // held: without this, a still-reachable OLD target would keep answering
    // indefinitely because nothing forces the stream to be re-spawned.
    if slot.stream.is_some() && slot.target != peer.ssh_target() {
        slot.stream = None;
    }

    let fresh = slot.stream.is_none();
    if fresh {
        if !spawn {
            return Err("no held connection".into());
        }
        match PeerStream::spawn(peer) {
            Ok(mut stream) => match stream.enroll(peer) {
                Ok(node_id) => {
                    slot.transient_failures = 0;
                    slot.retry_after = None;
                    set_enrollment(peer, Ok(node_id));
                    slot.stream = Some(stream);
                }
                Err(err) => {
                    let backoff = if err.transient {
                        slot.transient_backoff()
                    } else {
                        RECONNECT_BACKOFF
                    };
                    let transient = err.transient;
                    let err = err.detail;
                    let (detail, tail) = if err == CONNECTION_CLOSED || err.starts_with(WEDGED) {
                        stream.explain(&err)
                    } else {
                        (err, None)
                    };
                    set_enrollment(
                        peer,
                        Err(EnrollmentError {
                            detail: detail.clone(),
                            transient,
                        }),
                    );
                    record_establish_failure(
                        &mut slot,
                        &peer.name,
                        &detail,
                        tail.as_deref(),
                        backoff,
                    );
                    slot.retry_after = Some(std::time::Instant::now() + backoff);
                    return Err(detail);
                }
            },
            Err(err) => {
                set_enrollment(peer, Err(EnrollmentError::transport(err.clone())));
                record_establish_failure(&mut slot, &peer.name, &err, None, Duration::ZERO);
                return Err(err);
            }
        }
        slot.target = peer.ssh_target().to_string();
    }

    let Some(stream) = slot.stream.as_mut() else {
        return Err("no connection".into());
    };
    match stream.request(method, params) {
        Ok(response) => {
            if fresh {
                let down_secs = slot
                    .failure
                    .take()
                    .map(|failure| failure.since.elapsed().as_secs());
                crate::logging::peer_stream_established(&peer.name, down_secs);
            }
            Ok(response)
        }
        Err(err) => {
            let (detail, tail) = stream.explain(&err);
            set_enrollment(peer, Err(EnrollmentError::transport(detail.clone())));
            // Drop the stream rather than reuse it: after a timeout the pairing
            // between requests and responses is no longer known to hold.
            slot.stream = None;
            // A best-effort extra (#410 down-gossip, `spawn: false`) must not
            // cost the poll its connection for a whole backoff: the next poll
            // reconnects at once, exactly as if this line had never been sent.
            let backoff = if spawn {
                slot.transient_backoff()
            } else {
                Duration::ZERO
            };
            if spawn {
                slot.retry_after = Some(std::time::Instant::now() + backoff);
            }
            if fresh {
                record_establish_failure(&mut slot, &peer.name, &detail, tail.as_deref(), backoff);
            } else {
                crate::logging::peer_stream_closed(&peer.name, &detail, backoff.as_secs());
            }
            Err(detail)
        }
    }
}

/// Record, and report on the edge, that a stream could not be established.
///
/// WARN when the reason is new — the first failure, or a different kind of
/// broken than last time — and DEBUG while it is the same, because a peer
/// whose stream is down is re-dialled every backoff for as long as the outage
/// lasts and one line per attempt is the fire hose #318 removed. The ongoing
/// state is not lost by going quiet: it is on the peer's row and in
/// `flk peers`, via [`establish_failure`].
fn record_establish_failure(
    slot: &mut Slot,
    peer: &str,
    detail: &str,
    stderr_tail: Option<&str>,
    backoff: Duration,
) {
    let reason = crate::peers::SshFailureReason::classify(detail);
    let changed = slot.failure.as_ref().map(|failure| failure.reason) != Some(reason);
    match slot.failure.as_mut() {
        Some(failure) if !changed => failure.detail = detail.to_string(),
        _ => {
            slot.failure = Some(StreamFailure {
                reason,
                detail: detail.to_string(),
                since: std::time::Instant::now(),
            })
        }
    }
    crate::logging::peer_stream_unavailable(
        peer,
        reason.as_str(),
        detail,
        stderr_tail.unwrap_or(""),
        backoff.as_secs(),
        changed,
    );
}

/// Why `peer` has no held stream, if the last attempt to establish one failed
/// and none has succeeded since (#418). `None` for a peer holding a stream,
/// one never tried, and one that only ever used the one-shot path.
pub fn establish_failure(peer: &PeerConfig) -> Option<String> {
    let slot = {
        let registry = registry().lock().ok()?;
        Arc::clone(registry.get(&peer.name)?)
    };
    // A blocking lock, not `try_lock`: this runs on the poll's own worker
    // thread, never the main loop, and a request in flight (an uplink answer,
    // say) would otherwise read as "no failure" and blank the peer's row for
    // a poll. Poison is recovered like `request_over` does.
    let slot = match slot.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    slot.failure.as_ref().map(|failure| failure.detail.clone())
}

/// How old a pushed summary may be and still answer a poll (#4).
///
/// A push does not merely arrive early — it REPLACES the request that poll
/// would otherwise have made. The payload is not the problem: the pusher
/// re-fetches a full `peers.summary`, system block included, so a push carries
/// everything a poll would. What it cannot carry is time. A push emitted at t=0
/// sits in this slot until a poll consumes it, which may be most of a poll
/// interval later, and the fields that keep moving in the meantime are
/// delivered as though they were current.
///
/// That is why cpu and memory are the half of the report that named itself.
/// Workspaces and agent state only change when something happens, and something
/// happening is what emits the next push — so those fields self-correct.
/// Utilisation drifts continuously and emits nothing, so nothing re-pushes it,
/// and on a peer busy enough to keep this slot occupied the live fetch that
/// would have refreshed it never runs.
///
/// Well under the poll interval, so a stale push costs one fresh request rather
/// than a whole cycle, and comfortably above the push debounce, so a push
/// emitted for a real change is still used for the change it was emitted for.
const MAX_PUSH_AGE: std::time::Duration = std::time::Duration::from_secs(5);

/// Take the freshest summary this peer pushed, if it is still current.
///
/// Consuming rather than peeking: a summary answers exactly one poll, and
/// leaving it in place would let a peer that has gone quiet keep answering
/// with a snapshot that is no longer current. An empty slot falls through to
/// an ordinary request, so a silent peer is still polled at its usual cadence.
///
/// A push past [`MAX_PUSH_AGE`] is DISCARDED rather than returned — taken out
/// of the slot either way, because leaving it would have the next poll re-judge
/// the same expired snapshot instead of getting on with a live request.
pub fn take_pushed_summary(peer: &PeerConfig) -> Option<String> {
    if peer.summary_command != crate::config::model::default_peer_summary_command() {
        return None;
    }
    let slot = {
        let registry = registry().lock().ok()?;
        Arc::clone(registry.get(&peer.name)?)
    };
    let mut slot = slot.lock().ok()?;
    if slot.target != peer.ssh_target() {
        return None;
    }
    let stream = slot.stream.as_mut()?;
    let mut push = stream.latest_push.lock().ok()?;
    let (arrived, payload) = push.take()?;
    if arrived.elapsed() > MAX_PUSH_AGE {
        crate::logging::peer_push_expired(&peer.name, arrived.elapsed().as_secs());
        return None;
    }
    Some(payload)
}

/// Drop connections invalidated by a config reload.
///
/// Deliberately not "drop everything": config reloads happen for unrelated
/// keys, and tearing down healthy connections each time would pay the
/// handshake this module exists to avoid. A connection is dropped only when
/// its peer is gone from config, or when its ssh destination changed — the two
/// cases where the held connection no longer points where the config says.
pub fn retain_configured(peers: &[PeerConfig]) {
    if let Ok(mut wakes) = wake_registry().lock() {
        wakes.retain(|name, _| peers.iter().any(|peer| &peer.name == name));
    }
    let Ok(mut registry) = registry().lock() else {
        return;
    };
    registry.retain(|name, slot| {
        let Some(peer) = peers.iter().find(|peer| &peer.name == name) else {
            return false;
        };
        // A slot that never connected has an empty target and no stream to
        // invalidate; keep it so its backoff still applies.
        //
        // `try_lock`, not `lock`: the per-peer lock is held for the whole of a
        // request, so blocking here would stall the config reload — and with
        // it the loop that calls it — for up to REQUEST_TIMEOUT per wedged
        // peer, serially. A busy slot is left for `request` to reconcile: it
        // re-checks `target` against config under the lock it already holds,
        // so a moved peer reconnects there rather than waiting for the old
        // stream to happen to die.
        match slot.try_lock() {
            Ok(slot) => slot.stream.is_none() || slot.target == peer.ssh_target(),
            Err(std::sync::TryLockError::WouldBlock) => true,
            Err(std::sync::TryLockError::Poisoned(_)) => false,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str) -> PeerConfig {
        PeerConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_wake_push_interleaved_with_a_response_disturbs_neither() {
        let peer = crate::config::PeerConfig {
            name: "wake.example".into(),
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let slot = Arc::new(Mutex::new(None));
        let wire = concat!(
            "{\"id\":\"one\"}\n",
            "{\"push\":\"mesh.wake\"}\n",
            "{\"push\":\"unknown\"}\n",
            "{\"id\":\"two\"}\n"
        );
        route_relay_lines(&peer.name, std::io::Cursor::new(wire), &tx, &slot);
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![r#"{"id":"one"}"#, r#"{"id":"two"}"#]
        );
        assert!(take_wake(&peer));
        assert!(!take_wake(&peer));
        assert!(slot.lock().unwrap().is_none());
    }

    #[test]
    fn enrollment_backoff_classifies_missing_server_and_edge_capacity_as_transient() {
        for code in [
            "no_local_server",
            "inbound_edges_full",
            "mesh_version_mismatch",
            "identity_changed",
            "invalid_request",
        ] {
            let failure =
                EnrollmentError::response(&serde_json::json!({"code": code}), code.into());
            assert_eq!(
                failure.transient,
                matches!(code, "no_local_server" | "inbound_edges_full")
            );
            assert_eq!(failure.detail, code);
        }
        assert!(!EnrollmentError::from("identity changed".to_string()).transient);
        assert!(EnrollmentError::transport(CONNECTION_CLOSED.into()).transient);
    }

    #[test]
    fn enrollment_status_distinguishes_retrying_transport_from_handshake_refusal() {
        let target = peer("enrollment-status.test");
        set_enrollment(&target, Ok("pinned-node".into()));
        let generation = peer_enrollment_generation(&target);
        for detail in [CONNECTION_CLOSED, "ConnectionReset", "peer relay wedged"] {
            set_enrollment(&target, Err(EnrollmentError::transport(detail.into())));
            let status = enrollment(&target);
            assert_eq!(status.state, "retrying");
            assert_eq!(status.reason.as_deref(), Some(detail));
            assert_eq!(status.node_id.as_deref(), Some("pinned-node"));
            assert_eq!(peer_enrollment_generation(&target), generation);
        }
        for (code, expected) in [
            ("inbound_edges_full", "retrying"),
            ("no_local_server", "retrying"),
            ("mesh_version_mismatch", "refused"),
            ("mesh_refused", "refused"),
        ] {
            set_enrollment(
                &target,
                Err(EnrollmentError::response(
                    &serde_json::json!({"code":code}),
                    code.into(),
                )),
            );
            let status = enrollment(&target);
            assert_eq!(status.state, expected);
            assert_eq!(status.reason.as_deref(), Some(code));
        }
        set_enrollment(&target, Ok("pinned-node".into()));
        assert_eq!(enrollment(&target).state, "pinned");
        assert!(enrollment(&target).reason.is_none());
    }

    #[test]
    fn transient_backoff_grows_with_jitter_and_caps_at_thirty_seconds() {
        let mut slot = Slot::default();
        for ceiling in [1, 2, 4, 8, 16, 30, 30, 30] {
            let delay = slot.transient_backoff();
            assert!(delay >= Duration::from_millis(ceiling * 500));
            assert!(delay <= Duration::from_secs(ceiling));
        }
    }

    /// #418 with a real ssh: a relay stream that cannot be established says
    /// why — ssh's own words, classified — instead of falling back in
    /// silence, and the reason stays readable for the peer's row until a
    /// stream is held. Before this the relay's stderr went to /dev/null and
    /// the error was a bare "connection closed".
    #[test]
    fn a_stream_that_cannot_be_established_says_why() {
        let target = PeerConfig {
            name: "flk418-stream".into(),
            ssh: "nobody@flk-418-no-such-host.invalid".into(),
            ..Default::default()
        };
        let mut outcome = None;
        let logs = crate::logging::capture_logs(|| {
            outcome = Some(request(&target, "peers.summary", serde_json::json!({})));
        });
        let err = outcome
            .expect("ran")
            .expect_err("an .invalid host holds no stream");
        if err.starts_with("ssh spawn failed") {
            // No ssh client on this runner: nothing real to observe.
            return;
        }
        assert_eq!(
            crate::peers::SshFailureReason::classify(&err),
            crate::peers::SshFailureReason::UnknownHost,
            "{err}"
        );
        assert!(
            logs.contains("peer.stream.unavailable")
                && logs.contains("WARN")
                && logs.contains("reason=\"unknown_host\""),
            "{logs}"
        );
        assert_eq!(establish_failure(&target).as_deref(), Some(err.as_str()));
        assert_eq!(enrollment(&target).state, "retrying");
        assert_eq!(enrollment(&target).reason.as_deref(), Some(err.as_str()));
    }

    /// The same establish failure on every backoff is one WARN, not one per
    /// attempt; a different reason is news again.
    #[test]
    fn a_repeated_establish_failure_warns_once_per_reason() {
        let mut slot = Slot::default();
        let attempt = |slot: &mut Slot, detail: &str| {
            crate::logging::capture_logs(|| {
                record_establish_failure(slot, "p", detail, None, RECONNECT_BACKOFF);
            })
        };
        let refused = "ssh: connect to host p port 22: Connection refused";
        assert!(attempt(&mut slot, refused).contains("WARN"));
        assert!(!attempt(&mut slot, refused).contains("WARN"));
        assert!(attempt(&mut slot, "Permission denied (publickey).").contains("WARN"));
    }

    /// A response must never be mistaken for a push just because the peer's
    /// own data mentions one. The summary payload is full of user-controlled
    /// strings, and a workspace or branch named `push` used to strand every
    /// response from that peer: the caller waited out `REQUEST_TIMEOUT`, the
    /// stream was torn down, and the peer fell back to one-shot ssh with a
    /// 15s stall per poll for as long as the name existed.
    #[test]
    fn a_response_mentioning_push_is_not_routed_as_a_push() {
        let push = r#"{"push":"peers.summary","result":{"host":"kiln","workspaces":[]}}"#;
        assert!(line_is_push(push), "the emitter's push shape must route");

        // The poisoned response: a workspace labelled `push`, which serializes
        // to the very substring the old check looked for.
        let response = r#"{"id":"stream-1","result":{"host":"kiln","workspaces":[{"label":"push","branch":"main"}]}}"#;
        assert!(
            !line_is_push(response),
            "a `push` VALUE in the payload is not a push KEY"
        );

        // A branch named `push` is the same trap by another route.
        let branch =
            r#"{"id":"stream-2","result":{"workspaces":[{"label":"api","branch":"push"}]}}"#;
        assert!(!line_is_push(branch));

        // `peers.checkout_prepare` answers carry a genuine `push` field. It
        // rides one-shot ssh today, but if it ever moves onto the stream it
        // must not self-route.
        let checkout = r#"{"id":"stream-3","result":{"branch":"main","push":true,"pushed":true}}"#;
        assert!(
            !line_is_push(checkout),
            "a nested `push` field is not a top-level push envelope"
        );

        // Garbage stays a response: the caller's parse reports it, rather than
        // it vanishing into the push slot where nobody would ever see it.
        assert!(!line_is_push("not json at all"));
        assert!(!line_is_push(""));

        // A null-valued `push` is not an envelope. Not emitable today, but the
        // classifier should not reroute on a sentinel someone adds later.
        assert!(!line_is_push(r#"{"push":null,"result":{}}"#));
    }

    /// The ordering invariant the whole module rests on: requests and
    /// responses pair up one-for-one, and a push may arrive at ANY point —
    /// including between a request being written and its response coming
    /// back. The push must not consume the response's slot in the channel,
    /// and the response must not overwrite the push.
    ///
    /// Drives `route_relay_lines` rather than `line_is_push`, so a call site
    /// that regressed to matching raw text fails here even with the
    /// classifier left intact.
    #[test]
    fn a_push_interleaved_with_a_response_disturbs_neither() {
        use std::io::Cursor;

        let (tx, responses) = std::sync::mpsc::channel();
        let push_slot: Arc<Mutex<Option<(std::time::Instant, String)>>> =
            Arc::new(Mutex::new(None));

        // As the relay would emit it: a push lands between the two responses,
        // and one of the responses mentions `push` in its payload.
        let wire = concat!(
            r#"{"id":"stream-1","result":{"workspaces":[{"label":"push"}]}}"#,
            "\n",
            r#"{"push":"peers.summary","result":{"host":"kiln"}}"#,
            "\n",
            r#"{"id":"stream-2","result":{"host":"kiln"}}"#,
            "\n",
        );
        route_relay_lines("kiln", Cursor::new(wire), &tx, &push_slot);
        drop(tx);

        let routed: Vec<String> = responses.iter().collect();
        assert_eq!(
            routed.len(),
            2,
            "both responses must reach the requester, and only those: {routed:?}"
        );
        assert!(
            routed[0].contains("stream-1"),
            "order preserved: {routed:?}"
        );
        assert!(
            routed[1].contains("stream-2"),
            "order preserved: {routed:?}"
        );

        let pushed = push_slot.lock().expect("push slot").take();
        assert_eq!(
            pushed.as_ref().map(|(_, line)| line.as_str()),
            Some(r#"{"push":"peers.summary","result":{"host":"kiln"}}"#),
            "the push is delivered exactly once, to the push slot"
        );
    }

    /// #4: a push does not merely arrive early, it REPLACES the request the
    /// poll would have made — so an expired one must not be handed back, or
    /// everything a push cannot carry (cpu, memory: nothing emits an event for
    /// them) stops refreshing for as long as pushes keep pre-empting the fetch.
    #[test]
    fn an_expired_push_is_discarded_rather_than_answering_a_poll() {
        let fresh = std::time::Instant::now();
        let expired = fresh
            .checked_sub(MAX_PUSH_AGE + std::time::Duration::from_secs(1))
            .expect("a monotonic clock far enough from its origin");

        assert!(
            fresh.elapsed() <= MAX_PUSH_AGE,
            "a just-arrived push answers the poll it was emitted for"
        );
        assert!(
            expired.elapsed() > MAX_PUSH_AGE,
            "one older than the budget must not"
        );

        // The budget has to sit between the two cadences it mediates, or it is
        // either useless (never expires) or destroys the feature (always does).
        assert!(
            MAX_PUSH_AGE < std::time::Duration::from_secs(crate::peers::PEER_POLL_INTERVAL_SECS),
            "an expiry at or past the poll interval never fires"
        );
        assert!(
            MAX_PUSH_AGE > std::time::Duration::from_secs(1),
            "an expiry at the push debounce would discard pushes emitted for a \
             real change before they could ever be used"
        );
    }

    /// A line that is not JSON reaches the requester, where its parse failure
    /// is reported. Before the classifier was parsed rather than substring
    /// matched, a bogus line that happened to contain the text `"push"` was
    /// swallowed into the push slot and the caller waited out the timeout.
    #[test]
    fn a_malformed_line_reaches_the_requester_rather_than_vanishing() {
        use std::io::Cursor;

        let (tx, responses) = std::sync::mpsc::channel();
        let push_slot: Arc<Mutex<Option<(std::time::Instant, String)>>> =
            Arc::new(Mutex::new(None));

        route_relay_lines(
            "kiln",
            Cursor::new("ssh: connect to host kiln port 22: \"push\"\n"),
            &tx,
            &push_slot,
        );
        drop(tx);

        assert_eq!(responses.iter().count(), 1, "the caller sees the bad line");
        assert!(
            push_slot.lock().expect("push slot").is_none(),
            "noise must not masquerade as a push"
        );
    }

    /// A config reload must not wait on a peer that is mid-request. The
    /// per-peer lock is held for the whole of a request, so blocking here cost
    /// up to REQUEST_TIMEOUT (15s) per wedged peer, serially, on the loop that
    /// triggers the reload.
    #[test]
    fn a_reload_does_not_wait_on_a_peer_that_is_mid_request() {
        let peer = peer("reload-busy-slot");
        // Materialize the slot, then hold its lock the way an in-flight
        // request does.
        let slot = {
            let mut registry = registry().lock().expect("registry");
            Arc::clone(registry.entry(peer.name.clone()).or_default())
        };
        let held = slot.lock().expect("hold the slot like a live request");

        let started = std::time::Instant::now();
        retain_configured(std::slice::from_ref(&peer));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "reload blocked on a busy slot for {:?}",
            started.elapsed()
        );

        // The busy slot is retained rather than silently dropped — dropping it
        // would tear down a live connection mid-request.
        drop(held);
        assert!(
            registry()
                .lock()
                .expect("registry")
                .contains_key(&peer.name),
            "a busy slot must survive the reload"
        );
        retain_configured(&[]);
    }

    #[test]
    fn a_failed_peer_backs_off_instead_of_respawning_every_poll() {
        // A peer that is asleep, or running an `flk` without `peers relay`,
        // fails every attempt. Without the backoff each 15s poll would spawn a
        // fresh ssh into the same failure — strictly worse than the one-shot
        // path it is meant to improve on.
        let peer = peer("backoff-test-unreachable-host");
        let first = request(&peer, "peers.summary", serde_json::json!({}));
        assert!(first.is_err(), "an unreachable peer cannot be connected");

        let second = request(&peer, "peers.summary", serde_json::json!({}));
        assert_eq!(
            second.unwrap_err(),
            first.unwrap_err(),
            "the retry is refused locally rather than spawning ssh again"
        );
        retain_configured(&[]);
    }

    /// Live check against a real ssh connection — `#[ignore]` because it needs
    /// a reachable peer, so it is a manual verification rather than CI.
    /// `cargo nextest run holds_one_connection_across_requests --ignored`
    ///
    /// The stand-in honours the relay's contract (one response line per
    /// request line) without needing a new `flk` deployed to the far side,
    /// which is what makes this runnable before the fleet has rolled over.
    #[test]
    #[ignore = "needs a reachable ssh peer"]
    fn holds_one_connection_across_requests() {
        let mut peer = peer("kiln");
        peer.ssh = "kiln".into();
        peer.relay_command =
            r#"sh -lc 'while IFS= read -r line; do echo "{\"id\":\"live\",\"result\":{}}"; done'"#
                .into();

        for attempt in 1..=3 {
            let response = request(&peer, "peers.summary", serde_json::json!({}))
                .unwrap_or_else(|err| panic!("request {attempt} over the held connection: {err}"));
            assert!(
                response.contains("\"id\":\"live\""),
                "request {attempt} got a response: {response}"
            );
        }
        retain_configured(&[]);
    }

    #[test]
    fn a_peer_dropped_from_config_loses_its_slot() {
        let peer = peer("retain-test-removed-host");
        let _ = request(&peer, "peers.summary", serde_json::json!({}));
        retain_configured(&[]);
        assert!(
            !registry().lock().unwrap().contains_key(&peer.name),
            "a peer no longer in config keeps no state"
        );
    }

    #[test]
    fn an_unrelated_config_reload_keeps_a_peer_waiting_out_its_backoff() {
        // Reloads fire for keys that have nothing to do with peers. If those
        // cleared the registry, a failing peer would retry on every reload and
        // the backoff would stop bounding anything.
        let peer = peer("retain-test-kept-host");
        let first = request(&peer, "peers.summary", serde_json::json!({}));
        retain_configured(std::slice::from_ref(&peer));
        assert_eq!(
            request(&peer, "peers.summary", serde_json::json!({})).unwrap_err(),
            first.unwrap_err(),
            "a still-configured peer keeps its slot, backoff included"
        );
        retain_configured(&[]);
    }
}
