//! A held SSH connection per peer, carrying API requests to `flk peers relay`.
//!
//! `run_peer_ssh` spawns a fresh `ssh` per call, so every poll and every
//! message pays a full handshake to move a few hundred bytes. Measured cold
//! from sage with multiplexing disabled, exit status checked:
//!
//! ```text
//! anvil           0.13s
//! anvil-dev       0.16s
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
//! connection (the roaming-laptop case) and exits — which arrives here as EOF
//! on the reader thread. No timer decides that.
//!
//! Every failure falls back to the one-shot spawn. That is what keeps this
//! never-worse-than-today, and it is not only an error path: a peer whose
//! `flk` predates `peers relay` can never hold a stream at all, so the
//! fallback is also the compatibility path during a fleet rollout.

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

/// Refuse to re-spawn a connection that just died, so a peer that is asleep or
/// running an old `flk` cannot turn into a reconnect storm. Polls continue via
/// the one-shot fallback throughout, so data keeps flowing at today's cadence
/// while this backoff runs.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(60);

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
    latest_push: Arc<Mutex<Option<String>>>,
}

impl PeerStream {
    fn spawn(peer: &PeerConfig) -> Result<Self, String> {
        let mut child = crate::process::TracedCommand::new("ssh", "peers")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                // Same probe cadence as the one-shot path: ssh detects a dead
                // or half-open link itself and exits, which is how death
                // reaches us. Nothing here polls for it.
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=2",
                peer.ssh_target(),
                &peer.relay_command,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn_traced()
            .map_err(|err| format!("ssh spawn failed: {err}"))?;

        let stdin = child.stdin.take().ok_or("ssh stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("ssh stdout unavailable")?;
        let (tx, lines) = std::sync::mpsc::channel();
        let latest_push = Arc::new(Mutex::new(None));
        let push_slot = Arc::clone(&latest_push);
        std::thread::spawn(move || {
            // Ends on EOF, which is exactly how ssh reports the connection is
            // gone. The dropped sender then disconnects the channel and every
            // later request fails fast rather than waiting out the timeout.
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // A push carries `push` where a response carries `id`, so
                // routing needs no guessing. Anything else is a response and
                // belongs to whoever is waiting on the channel.
                if line.contains("\"push\"") {
                    if let Ok(mut slot) = push_slot.lock() {
                        *slot = Some(line);
                    }
                    continue;
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            lines,
            next_id: 0,
            latest_push,
        })
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
                Err(TryRecvError::Disconnected) => return Err("connection closed".into()),
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
            Ok(line) => Ok(line),
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "no response in {}s — peer relay wedged",
                REQUEST_TIMEOUT.as_secs()
            )),
            Err(RecvTimeoutError::Disconnected) => Err("connection closed".into()),
        }
    }
}

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
    /// The ssh destination this connection was opened against, so a config
    /// reload can tell "this peer moved" from "some unrelated key changed".
    target: String,
}

type Registry = Mutex<HashMap<String, Arc<Mutex<Slot>>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Send an API request to `peer` over its held connection.
///
/// `Err` means the caller should fall back to the one-shot spawn — it covers
/// a peer too old to have `peers relay`, one that is asleep, and one whose
/// relay has wedged, deliberately without distinguishing them: the answer is
/// the same in every case, and the fallback is a working path rather than a
/// degraded one.
pub fn request(
    peer: &PeerConfig,
    method: &str,
    params: serde_json::Value,
) -> Result<String, String> {
    // Outer lock is held only long enough to find the slot; the request itself
    // runs under the per-peer lock so one slow peer cannot stall the others.
    let slot = {
        let mut registry = registry().lock().map_err(|_| "registry poisoned")?;
        Arc::clone(registry.entry(peer.name.clone()).or_default())
    };
    let mut slot = slot.lock().map_err(|_| "peer slot poisoned")?;

    if let Some(retry_after) = slot.retry_after {
        if std::time::Instant::now() < retry_after {
            return Err("connection backing off".into());
        }
    }

    if slot.stream.is_none() {
        slot.stream = Some(PeerStream::spawn(peer)?);
        slot.target = peer.ssh_target().to_string();
    }

    let Some(stream) = slot.stream.as_mut() else {
        return Err("no connection".into());
    };
    match stream.request(method, params) {
        Ok(response) => Ok(response),
        Err(err) => {
            // Drop the stream rather than reuse it: after a timeout the pairing
            // between requests and responses is no longer known to hold.
            slot.stream = None;
            slot.retry_after = Some(std::time::Instant::now() + RECONNECT_BACKOFF);
            crate::logging::peer_stream_closed(&peer.name, &err, RECONNECT_BACKOFF.as_secs());
            Err(err)
        }
    }
}

/// Take the freshest summary this peer pushed, if any.
///
/// Consuming rather than peeking: a summary answers exactly one poll, and
/// leaving it in place would let a peer that has gone quiet keep answering
/// with a snapshot that is no longer current. An empty slot falls through to
/// an ordinary request, so a silent peer is still polled at its usual cadence.
pub fn take_pushed_summary(peer: &PeerConfig) -> Option<String> {
    let slot = {
        let registry = registry().lock().ok()?;
        Arc::clone(registry.get(&peer.name)?)
    };
    let mut slot = slot.lock().ok()?;
    let stream = slot.stream.as_mut()?;
    let mut push = stream.latest_push.lock().ok()?;
    push.take()
}

/// Drop connections invalidated by a config reload.
///
/// Deliberately not "drop everything": config reloads happen for unrelated
/// keys, and tearing down healthy connections each time would pay the
/// handshake this module exists to avoid. A connection is dropped only when
/// its peer is gone from config, or when its ssh destination changed — the two
/// cases where the held connection no longer points where the config says.
pub fn retain_configured(peers: &[PeerConfig]) {
    let Ok(mut registry) = registry().lock() else {
        return;
    };
    registry.retain(|name, slot| {
        let Some(peer) = peers.iter().find(|peer| &peer.name == name) else {
            return false;
        };
        // A slot that never connected has an empty target and no stream to
        // invalidate; keep it so its backoff still applies.
        let Ok(slot) = slot.lock() else {
            return false;
        };
        slot.stream.is_none() || slot.target == peer.ssh_target()
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
            "connection backing off",
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
        let mut peer = peer("anvil");
        peer.ssh = "anvil".into();
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
        let _ = request(&peer, "peers.summary", serde_json::json!({}));
        retain_configured(std::slice::from_ref(&peer));
        assert_eq!(
            request(&peer, "peers.summary", serde_json::json!({})).unwrap_err(),
            "connection backing off",
            "a still-configured peer keeps its slot, backoff included"
        );
        retain_configured(&[]);
    }
}
