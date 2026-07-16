//! Federated peer servers: poll each configured `[[peers]]` entry over SSH
//! for its `peers.summary`, cache the results for the sidebar's project-
//! folded remote rows, and provide the attach target for switch-on-select.
//!
//! Peers never share PTYs or frames — only this lightweight summary gossip.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::schema::PeerWorkspaceSummary;
use crate::config::PeerConfig;

/// Seconds between summary poll rounds — the shipped default and the ONE
/// source of truth `GossipConfig::default()` reads. Live callers threaded to
/// config (`app::App::gossip_poll_interval_secs` and the round handler) pick
/// up the tunable value; the fleet-snapshot rendering path (see
/// `PeerSummaryState::is_stale` / `reachability`) still reads the const —
/// documented seam for #101 (staleness rework), whose new model will thread
/// config where reachable and retire the const consumers.
pub const PEER_POLL_INTERVAL_SECS: u64 = 15;
/// First poll fires shortly after startup so the sidebar populates fast.
pub const PEER_POLL_INITIAL_DELAY_SECS: u64 = 3;
/// A peer whose last successful poll is older than this renders as stale.
pub const PEER_STALE_AFTER_SECS: u64 = 60;

/// A peer whose latency exceeds this renders as "slow" (yellow dot).
pub const PEER_SLOW_LATENCY_MS: u64 = 150;

/// Overlap-safe per-peer round dispatcher (#96): the round handler consults
/// the tracker to decide whether to spawn a fetch for each peer. Two guards:
///
/// 1. **In-flight guard** — a peer whose previous poll has not completed
///    (still-running SSH `flk peers summary`) is skipped this round. A slow
///    ProxyJump peer polled at a short cadence cannot stack concurrent SSH
///    invocations against itself, no matter what interval is set.
/// 2. **Next-due guard** — a peer with a per-`[[peers]]` `poll_interval_secs`
///    override longer than the global cadence is polled only when its per-peer
///    deadline has arrived.
///
/// The tracker is memory-only. On config reload the round handler retains only
/// the entries for still-configured peer names.
#[derive(Debug, Default)]
pub struct PeerPollTracker {
    entries: HashMap<String, PeerPollEntry>,
}

#[derive(Debug)]
struct PeerPollEntry {
    /// A dispatched fetch is still running (SSH round-trip in-flight).
    in_flight: bool,
    /// Earliest instant a NEW poll may fire — set to `now + effective_interval`
    /// when the previous one was dispatched. `None` = no history yet, so the
    /// first `should_poll_now` call always dispatches.
    next_due: Option<Instant>,
}

impl PeerPollTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether to dispatch a poll for `peer_name` NOW. Returns `true`
    /// when the round should spawn a fetch — and eagerly marks the peer as
    /// in-flight, so back-to-back calls within one round each dispatch at most
    /// once. Callers MUST invoke `mark_finished` on completion (both success
    /// and error), else this peer is silently frozen out until config reload.
    pub fn should_poll_now(
        &mut self,
        peer_name: &str,
        now: Instant,
        effective_interval: Duration,
    ) -> bool {
        let entry = self
            .entries
            .entry(peer_name.to_string())
            .or_insert(PeerPollEntry {
                in_flight: false,
                next_due: None,
            });
        if entry.in_flight {
            return false;
        }
        if let Some(due) = entry.next_due {
            if now < due {
                return false;
            }
        }
        entry.in_flight = true;
        entry.next_due = Some(now + effective_interval);
        true
    }

    /// Release the in-flight lock for `peer_name`. Called from the
    /// `PeerSummaryFetched` handler regardless of `Ok`/`Err` — the next
    /// round's `should_poll_now` will then decide from the next-due gate.
    pub fn mark_finished(&mut self, peer_name: &str) {
        if let Some(entry) = self.entries.get_mut(peer_name) {
            entry.in_flight = false;
        }
    }

    /// Prune entries for peers no longer in config. Preserves in-flight state
    /// for surviving peers so a reload during a slow poll doesn't accidentally
    /// permit a concurrent dispatch.
    pub fn retain_only<I>(&mut self, names: I)
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let keep: std::collections::HashSet<String> =
            names.into_iter().map(|s| s.as_ref().to_string()).collect();
        self.entries.retain(|k, _| keep.contains(k));
    }

    #[cfg(test)]
    fn in_flight(&self, peer_name: &str) -> bool {
        self.entries
            .get(peer_name)
            .is_some_and(|entry| entry.in_flight)
    }
}

/// Cached state of one configured peer, updated by the poll loop.
#[derive(Debug, Clone)]
pub struct PeerSummaryState {
    /// Peer name from config (sidebar host badge).
    pub peer: String,
    /// SSH destination used for polling and switch-on-select attach.
    pub ssh_target: String,
    /// Hostname the peer reported about itself (display fallback: peer name).
    pub host: Option<String>,
    /// flock version the peer reported (spot un-deployed peers).
    pub version: Option<String>,
    /// Wire protocol the peer reported (#58) — drives the sidebar skew badge.
    pub protocol: Option<u32>,
    /// Machine health snapshot from the last successful poll.
    pub system: Option<crate::api::schema::PeerSystemSummary>,
    /// Round-trip latency of the last successful summary poll.
    pub latency_ms: Option<u64>,
    pub workspaces: Vec<PeerWorkspaceSummary>,
    pub last_ok: Option<Instant>,
    /// Last poll error, cleared on success.
    pub error: Option<String>,
    /// Gossip v3 (#101 part 2): the ORIGIN's report age at CAPTURE time, in
    /// seconds. FROZEN — receiver dwell does NOT tick it up. Set from a wire
    /// [`crate::protocol::FleetPeer::origin_last_ok_secs`] on snapshot ingest
    /// and from a relayed entry's field on cache merge. `None` for locally
    /// polled config peers, where `last_ok` (a real Instant) carries the
    /// freshness and staleness falls back to the local-dwell path.
    pub origin_last_ok_secs: Option<u64>,
    /// Gossip v3 (#101 part 3): SSH ProxyJump identity for reaching this
    /// peer. Set by the hub on relay so a receiver dialing a snapshot row
    /// routes through the hub instead of trying the target directly. `None`
    /// for entries the receiver can dial straight (its own config peers).
    pub proxy_jump: Option<String>,
    /// The peer's SELF-DECLARED fleet icon NAME (#164): a semantic name the
    /// RECEIVER maps to a flat Nerd Font glyph for the servers band, so a
    /// server's icon renders identically fleet-wide. Set from the peer's own
    /// `peers.summary`, carried through relay + snapshot. `None` = no icon.
    pub icon: Option<String>,
}

impl PeerSummaryState {
    pub fn new(config: &PeerConfig) -> Self {
        Self {
            peer: config.name.clone(),
            ssh_target: config.ssh_target().to_string(),
            host: None,
            version: None,
            protocol: None,
            system: None,
            latency_ms: None,
            workspaces: Vec::new(),
            last_ok: None,
            error: None,
            origin_last_ok_secs: None,
            proxy_jump: None,
            icon: None,
        }
    }

    pub fn is_stale(&self) -> bool {
        self.is_stale_with(PEER_STALE_AFTER_SECS)
    }

    /// Config-aware staleness (#96): uses the caller-supplied threshold.
    ///
    /// Gossip v3 (#101 part 2): a carried / relayed entry judges freshness
    /// against the ORIGIN's report age at capture — FROZEN, not the receiver's
    /// dwell — so a snapshot entry that the origin polled 5s before capture
    /// stays fresh even after 90s of local dwell. Locally-polled entries
    /// (`origin_last_ok_secs = None`) keep the pre-v3 last_ok.elapsed() path.
    pub fn is_stale_with(&self, stale_after_secs: u64) -> bool {
        if let Some(origin_secs) = self.origin_last_ok_secs {
            return origin_secs > stale_after_secs;
        }
        match self.last_ok {
            Some(at) => at.elapsed().as_secs() > stale_after_secs,
            None => true,
        }
    }

    /// The name to DISPLAY for this node (#42): the configured `[[peers]]`
    /// name (validated non-empty), chosen over the peer's self-reported
    /// gethostname (`host`) so a node always shows the name you gave it —
    /// `anvil`, not a raw OS hostname like `mac-studio-12345.local`.
    pub fn display_name(&self) -> &str {
        &self.peer
    }

    /// Reachability for the sidebar dot: live / slow / stale-or-error.
    pub fn reachability(&self) -> PeerReachability {
        self.reachability_with(PEER_STALE_AFTER_SECS, PEER_SLOW_LATENCY_MS)
    }

    /// Config-aware reachability (#96) — the live-config path. The zero-arg
    /// twin above stays for the fleet-snapshot rendering seam (#101).
    pub fn reachability_with(
        &self,
        stale_after_secs: u64,
        slow_threshold_ms: u64,
    ) -> PeerReachability {
        if self.is_stale_with(stale_after_secs) || self.error.is_some() {
            PeerReachability::Down
        } else if self.latency_ms.is_some_and(|ms| ms > slow_threshold_ms) {
            PeerReachability::Slow
        } else {
            PeerReachability::Live
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerReachability {
    Live,
    Slow,
    Down,
}

/// Fleet snapshot received at attach (hub-and-spoke down-gossip, issue #36):
/// the origin (home) host label plus render-only peer rows carried from the
/// server the client switched away from. These entries are NEVER polled —
/// their freshness only decays, which the existing staleness rendering shows.
#[derive(Debug, Clone)]
pub struct FleetSnapshotState {
    /// Short host name of the original origin (the client's home).
    pub origin: String,
    /// Carried peer summaries, converted into the poller's cache shape so
    /// the sidebar reuses the existing peer-row machinery.
    pub peers: Vec<PeerSummaryState>,
    /// The origin (hub) server's OWN summary (#66): its workspaces fold into
    /// the spaces list and its health populates the home row. The hub is not
    /// its own peer, so without this the hub's spaces are invisible on a
    /// spoke. Its `ssh_target` is the reserved home sentinel — origin rows
    /// switch home, never ssh.
    pub origin_summary: Option<PeerSummaryState>,
    /// When this snapshot arrived (home-row staleness display).
    pub received_at: Instant,
}

impl FleetSnapshotState {
    pub fn from_wire(snapshot: crate::protocol::FleetSnapshot) -> Self {
        Self {
            origin: snapshot.origin,
            peers: snapshot.peers.into_iter().map(peer_from_wire).collect(),
            origin_summary: snapshot.origin_summary.map(|p| peer_from_wire(*p)),
            received_at: Instant::now(),
        }
    }

    /// Re-encode for the next leap, excluding the hop target itself (it
    /// becomes the self row on the receiving end) and any entry matching the
    /// origin — the home row owns that slot, so a hub that lists itself in
    /// [[peers]] must not render twice. Ages are recomputed so time spent on
    /// this server keeps counting against freshness. Peer count is bounded:
    /// the snapshot rides an env var between attach legs, and an unbounded
    /// fleet could brush ARG_MAX and kill the leg spawn.
    pub fn to_wire(&self, exclude_ssh_target: &str) -> crate::protocol::FleetSnapshot {
        crate::protocol::FleetSnapshot {
            origin: self.origin.clone(),
            peers: self
                .peers
                .iter()
                .filter(|peer| peer.ssh_target != exclude_ssh_target && peer.peer != self.origin)
                .take(FLEET_SNAPSHOT_MAX_PEERS)
                .map(peer_to_wire)
                .collect(),
            // Pass-through: a nested leap keeps the ORIGINAL hub's own
            // summary so the way-home spaces stay visible the whole chain.
            origin_summary: self
                .origin_summary
                .as_ref()
                .map(|p| Box::new(peer_to_wire(p))),
        }
    }
}

/// Carried-snapshot peer cap (env-var transport between attach legs — see
/// `to_wire`). Far above any realistic personal fleet.
pub const FLEET_SNAPSHOT_MAX_PEERS: usize = 16;

/// Wire shape of one cached peer summary (`Instant` freshness → age in
/// seconds at capture time).
pub fn peer_to_wire(peer: &PeerSummaryState) -> crate::protocol::FleetPeer {
    crate::protocol::FleetPeer {
        name: peer.peer.clone(),
        ssh_target: peer.ssh_target.clone(),
        host: peer.host.clone(),
        version: peer.version.clone(),
        protocol: peer.protocol,
        system: peer.system.clone().map(Into::into),
        latency_ms: peer.latency_ms,
        workspaces: peer.workspaces.iter().cloned().map(Into::into).collect(),
        age_secs: peer.last_ok.map(|at| at.elapsed().as_secs()),
        error: peer.error.clone(),
        // Gossip v3 (#101 part 2): forward the frozen origin assertion when
        // the source was a snapshot / relay entry that already carried it.
        // Otherwise the local-poll last_ok IS the origin and doubles as the
        // frozen assertion at capture time (age_secs).
        origin_last_ok_secs: peer
            .origin_last_ok_secs
            .or_else(|| peer.last_ok.map(|at| at.elapsed().as_secs())),
        proxy_jump: peer.proxy_jump.clone(),
        icon: peer.icon.clone(),
    }
}

/// Rehydrate a carried peer entry into the poller's cache shape. `last_ok`
/// is mapped back onto a synthetic `Instant` so the local-dwell display and
/// pre-v3 fallback keep working; `origin_last_ok_secs` carries the FROZEN
/// origin assertion (#101 part 2) that staleness now judges against, so a
/// receiver's dwell no longer cliffs a snapshot entry at `stale_after`.
pub fn peer_from_wire(peer: crate::protocol::FleetPeer) -> PeerSummaryState {
    PeerSummaryState {
        peer: peer.name,
        ssh_target: peer.ssh_target,
        host: peer.host,
        version: peer.version,
        protocol: peer.protocol,
        system: peer.system.map(Into::into),
        latency_ms: peer.latency_ms,
        workspaces: peer.workspaces.into_iter().map(Into::into).collect(),
        last_ok: peer
            .age_secs
            .and_then(|secs| Instant::now().checked_sub(std::time::Duration::from_secs(secs))),
        error: peer.error,
        // Prefer the explicit origin field; fall back to `age_secs` for
        // pre-v22 wires so an entry from an older peer still gets the
        // origin-honest staleness path instead of decaying against dwell.
        origin_last_ok_secs: peer.origin_last_ok_secs.or(peer.age_secs),
        proxy_jump: peer.proxy_jump,
        icon: peer.icon,
    }
}

/// Parsed summary payload from one peer (everything its `peers.summary` carries).
#[derive(Debug, Clone, PartialEq)]
pub struct PeerSummaryPayload {
    pub host: String,
    pub version: Option<String>,
    pub protocol: Option<u32>,
    /// The peer's self-declared fleet icon name (#164), if any.
    pub icon: Option<String>,
    pub system: Option<crate::api::schema::PeerSystemSummary>,
    pub workspaces: Vec<PeerWorkspaceSummary>,
    /// Round-trip wall time of the summary SSH call (free latency probe).
    pub latency_ms: u64,
    /// Gossip v3 relay: the peer's own polled peers, so the hub can render
    /// two-hop fleet visibility. Empty when the peer is v(N-1) — additive
    /// with a serde default keeps mixed-version fleets safe.
    pub relayed_fleet: Vec<crate::api::schema::RelayedFleetPeer>,
}

/// Result of one poll of one peer, sent back as an AppEvent.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerSummaryFetch {
    pub peer: String,
    pub result: Result<PeerSummaryPayload, String>,
}

/// Fetch a peer's summary over SSH (blocking; run off the UI thread). The
/// round-trip wall time doubles as a free latency probe — no separate ping.
pub fn fetch_peer_summary(peer: &PeerConfig) -> PeerSummaryFetch {
    let started = Instant::now();
    let result = run_summary_command(peer).and_then(|stdout| {
        let latency_ms = started.elapsed().as_millis() as u64;
        parse_summary_response(&stdout, latency_ms)
    });
    PeerSummaryFetch {
        peer: peer.name.clone(),
        result,
    }
}

/// What a peer reported (and did) for a cross-machine checkout-prepare (#125):
/// the resolved branch plus the working-tree / push state, parsed from the
/// `peers.checkout_prepare` response envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCheckoutOutcome {
    pub branch: String,
    pub was_dirty: bool,
    pub was_unpushed: bool,
    pub pushed: bool,
}

/// Ask a peer to prepare one of its OWN workspaces' branches for a cross-machine
/// checkout (#125, "defer to the client"): the spoke resolves the repo + branch
/// from the workspace id and acts on its own git; with `push` it pushes to
/// origin so the hub can `git fetch origin <branch>` afterwards. `push == false`
/// is a read-only probe feeding the hub's pre-action confirmation. Runs over the
/// SAME SSH-invoked verb surface as `run_summary_command` — the hub never
/// touches the peer's `.git`, keeping the model hub-spoke. Blocking; run off the
/// UI thread.
pub fn run_checkout_prepare_command(
    peer: &PeerConfig,
    workspace_id: &str,
    push: bool,
) -> Result<PeerCheckoutOutcome, String> {
    // Workspace ids are server-assigned ("ws_3"); refuse anything that could
    // escape the remote shell command (mirrors prepare_peer_switch's guard).
    if workspace_id.is_empty()
        || !workspace_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!("invalid workspace id: {workspace_id:?}"));
    }
    let push_flag = if push { " --push" } else { "" };
    // The `flk` invocation is wrapped in a login shell so profile-managed PATHs
    // (nix, brew) apply — same shape as the default summary_command and the
    // prepare_peer_switch pre-focus call.
    let remote =
        format!("sh -lc 'flk peers checkout-prepare --workspace {workspace_id}{push_flag} --json'");
    let stdout = run_peer_ssh(peer, &remote)?;
    parse_checkout_prepare_response(&stdout)
}

/// Parse the `peers.checkout_prepare` response envelope:
/// `{"id":..,"result":{"branch":..,"was_dirty":..,"was_unpushed":..,"pushed":..}}`.
fn parse_checkout_prepare_response(stdout: &str) -> Result<PeerCheckoutOutcome, String> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .ok_or_else(|| "no JSON in checkout-prepare output".to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|err| format!("checkout-prepare parse error: {err}"))?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(format!("peer error: {message}"));
    }
    let result = value
        .get("result")
        .ok_or_else(|| "checkout-prepare response has no result".to_string())?;
    let branch = result
        .get("branch")
        .and_then(|b| b.as_str())
        .filter(|b| !b.is_empty())
        .ok_or_else(|| "checkout-prepare response has no branch".to_string())?
        .to_string();
    let flag = |key: &str| result.get(key).and_then(serde_json::Value::as_bool);
    Ok(PeerCheckoutOutcome {
        branch,
        was_dirty: flag("was_dirty").unwrap_or(false),
        was_unpushed: flag("was_unpushed").unwrap_or(false),
        pushed: flag("pushed").unwrap_or(false),
    })
}

fn run_summary_command(peer: &PeerConfig) -> Result<String, String> {
    run_peer_ssh(peer, &peer.summary_command)
}

/// Fetch the tail of a peer's session logs over SSH for the cross-host log view
/// (#67). Mirrors `run_checkout_prepare_command`: a login-shell `flk peers
/// logs --json` whose envelope we parse. `lines` is a bounded integer we format
/// ourselves, so nothing user-controlled reaches the remote shell. Blocking; run
/// off the UI thread.
pub fn run_logs_command(
    peer: &PeerConfig,
    lines: u32,
) -> Result<Vec<crate::logging::LogLine>, String> {
    let remote = format!("sh -lc 'flk peers logs --json --lines {lines}'");
    let stdout = run_peer_ssh(peer, &remote)?;
    parse_logs_response(&stdout)
}

/// Parse the `peers logs` response envelope:
/// `{"id":..,"result":{"type":"peers_logs","host":..,"lines":[..]}}`.
fn parse_logs_response(stdout: &str) -> Result<Vec<crate::logging::LogLine>, String> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .ok_or_else(|| "no JSON in logs output".to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|err| format!("logs parse error: {err}"))?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(format!("peer error: {message}"));
    }
    let result = value
        .get("result")
        .ok_or_else(|| "logs response has no result".to_string())?;
    let lines: Vec<crate::logging::LogLine> = result
        .get("lines")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| format!("logs parse error: {err}"))?
        .unwrap_or_default();
    Ok(lines)
}

/// Run one command on a peer over SSH (batch mode, short timeouts), returning
/// stdout. Shared by the summary poll and the checkout-prepare invocation.
fn run_peer_ssh(peer: &PeerConfig, remote_command: &str) -> Result<String, String> {
    let output = crate::process::TracedCommand::new("ssh", "peers")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            peer.ssh_target(),
            remote_command,
        ])
        .stdin(std::process::Stdio::null())
        .output_traced()
        .map_err(|err| format!("ssh spawn failed: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            output.status.to_string()
        } else {
            // Keep the tail: ssh banners/motd come first, the error last.
            stderr.lines().next_back().unwrap_or(stderr).to_string()
        };
        return Err(detail);
    }
    String::from_utf8(output.stdout).map_err(|_| "non-utf8 ssh output".to_string())
}

/// Parse the CLI's response envelope:
/// `{"id":..,"result":{"host":..,"version":..,"system":..,"workspaces":[..]}}`.
fn parse_summary_response(stdout: &str, latency_ms: u64) -> Result<PeerSummaryPayload, String> {
    // Login shells can print banners before the JSON; find the envelope line.
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .ok_or_else(|| "no JSON in summary output".to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|err| format!("summary parse error: {err}"))?;
    if let Some(error) = value.get("error") {
        return Err(format!("peer error: {error}"));
    }
    let result = value
        .get("result")
        .ok_or_else(|| "summary response has no result".to_string())?;
    let host = result
        .get("host")
        .and_then(|host| host.as_str())
        .unwrap_or_default()
        .to_string();
    let version = result
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let protocol = result
        .get("protocol")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u32::try_from(p).ok());
    // #164: the peer's self-declared icon name. Additive/optional — a v(N-1)
    // peer never emits it, parsing as None (no icon).
    let icon = result
        .get("icon")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let system = result
        .get("system")
        .filter(|system| !system.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| format!("summary system parse error: {err}"))?;
    let workspaces: Vec<PeerWorkspaceSummary> = result
        .get("workspaces")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| format!("summary workspaces parse error: {err}"))?
        .unwrap_or_default();
    // Gossip v3 (#101): relayed_fleet is additive with a serde default so a
    // v(N-1) peer that never emits the field parses cleanly.
    let relayed_fleet: Vec<crate::api::schema::RelayedFleetPeer> = result
        .get("relayed_fleet")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| format!("summary relayed_fleet parse error: {err}"))?
        .unwrap_or_default();
    Ok(PeerSummaryPayload {
        host,
        version,
        protocol,
        icon,
        system,
        workspaces,
        latency_ms,
        relayed_fleet,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn to_wire_dedups_origin_and_caps_peer_count() {
        let mk = |name: &str| PeerSummaryState {
            peer: name.to_string(),
            ssh_target: name.to_string(),
            host: None,
            version: None,
            protocol: None,
            system: None,
            latency_ms: None,
            workspaces: Vec::new(),
            last_ok: None,
            error: None,
            origin_last_ok_secs: None,
            proxy_jump: None,
            icon: None,
        };
        let mut peers: Vec<PeerSummaryState> = (0..FLEET_SNAPSHOT_MAX_PEERS + 3)
            .map(|i| mk(&format!("p{i}")))
            .collect();
        peers.push(mk("mba22")); // a hub that lists itself in [[peers]]
        let snapshot = FleetSnapshotState {
            origin: "mba22".into(),
            peers,
            origin_summary: None,
            received_at: Instant::now(),
        };

        let wire = snapshot.to_wire("p0");

        assert!(
            wire.peers.iter().all(|p| p.name != "mba22"),
            "origin owns the home row"
        );
        assert!(
            wire.peers.iter().all(|p| p.name != "p0"),
            "hop target excluded"
        );
        assert!(
            wire.peers.len() <= FLEET_SNAPSHOT_MAX_PEERS,
            "env-var transport cap"
        );
    }

    use super::*;
    use crate::api::schema::AgentStatus;

    fn summary_state(name: &str, ssh_target: &str, age_secs: Option<u64>) -> PeerSummaryState {
        PeerSummaryState {
            peer: name.to_string(),
            ssh_target: ssh_target.to_string(),
            host: Some(format!("{name}-host")),
            version: Some("0.9.0".to_string()),
            protocol: None,
            system: Some(crate::api::schema::PeerSystemSummary {
                cpu_percent: Some(42),
                mem_used: Some(13 << 30),
                mem_total: Some(16 << 30),
                disk_free: None,
            }),
            latency_ms: Some(34),
            workspaces: vec![crate::api::schema::PeerWorkspaceSummary {
                id: "ws_3".to_string(),
                workspace: "proj".to_string(),
                project_key: Some("github.com/x/proj".to_string()),
                project_label: Some("proj".to_string()),
                branch: Some("main".to_string()),
                is_linked_worktree: false,
                agent: Some("cc".to_string()),
                status: AgentStatus::Working,
                status_age_secs: Some(12),
                activity: None,
            }],
            last_ok: age_secs
                .and_then(|secs| Instant::now().checked_sub(std::time::Duration::from_secs(secs))),
            error: None,
            origin_last_ok_secs: None,
            proxy_jump: None,
            icon: None,
        }
    }

    #[test]
    fn fleet_peer_wire_roundtrip_preserves_summary_and_freshness() {
        let state = summary_state("anvil", "lars@anvil", Some(5));
        let wire = peer_to_wire(&state);
        assert_eq!(wire.age_secs, Some(5));

        let back = peer_from_wire(wire);
        assert_eq!(back.peer, state.peer);
        assert_eq!(back.ssh_target, state.ssh_target);
        assert_eq!(back.host, state.host);
        assert_eq!(back.version, state.version);
        assert_eq!(back.system, state.system);
        assert_eq!(back.latency_ms, state.latency_ms);
        assert_eq!(back.workspaces, state.workspaces);
        assert_eq!(back.error, state.error);
        // The age maps back onto a synthetic last_ok so reachability keeps
        // working — a 5s-old summary is still Live...
        let age = back.last_ok.expect("freshness carried").elapsed().as_secs();
        assert!((5..8).contains(&age), "age {age} should stay ~5s");
        assert_eq!(back.reachability(), PeerReachability::Live);

        // ...while an old one decays to Down with no polling involved.
        let stale = peer_from_wire(peer_to_wire(&summary_state(
            "sage",
            "lars@sage",
            Some(PEER_STALE_AFTER_SECS + 30),
        )));
        assert_eq!(stale.reachability(), PeerReachability::Down);

        // Never-reached peers stay never-reached.
        let never = peer_from_wire(peer_to_wire(&summary_state("ksb", "lars@ksb", None)));
        assert!(never.last_ok.is_none());
    }

    #[test]
    fn fleet_peer_wire_carries_icon_both_directions() {
        // #164: the self-declared icon survives the bincode roundtrip present...
        let mut state = summary_state("anvil", "lars@anvil", Some(5));
        state.icon = Some("anvil".to_string());
        assert_eq!(
            peer_from_wire(peer_to_wire(&state)).icon.as_deref(),
            Some("anvil")
        );

        // ...and absent (a v(N-1) peer never sets it) decodes to None.
        let mut none = summary_state("sage", "lars@sage", Some(5));
        none.icon = None;
        assert_eq!(peer_from_wire(peer_to_wire(&none)).icon, None);
    }

    #[test]
    fn fleet_snapshot_to_wire_keeps_origin_and_excludes_hop_target() {
        let snapshot = FleetSnapshotState {
            origin: "mba22".to_string(),
            peers: vec![
                summary_state("anvil", "lars@anvil", Some(3)),
                summary_state("sage", "lars@sage", Some(9)),
            ],
            origin_summary: None,
            received_at: Instant::now(),
        };

        let wire = snapshot.to_wire("lars@sage");
        // Pass-through: the ORIGINAL origin survives nested leaps.
        assert_eq!(wire.origin, "mba22");
        // The hop target becomes the self row on the receiving end.
        assert_eq!(wire.peers.len(), 1);
        assert_eq!(wire.peers[0].ssh_target, "lars@anvil");
    }

    #[test]
    fn origin_summary_survives_wire_roundtrip_and_passthrough() {
        let mut origin = summary_state("mba22", crate::protocol::HOME_SWITCH_TARGET, Some(0));
        origin.workspaces[0].workspace = "flock".to_string();
        let snapshot = FleetSnapshotState {
            origin: "mba22".to_string(),
            peers: vec![summary_state("anvil", "lars@anvil", Some(3))],
            origin_summary: Some(origin),
            received_at: Instant::now(),
        };

        // Round-trip carries the hub's own workspaces home-targeted.
        let back = FleetSnapshotState::from_wire(snapshot.to_wire("lars@anvil"));
        let carried = back
            .origin_summary
            .clone()
            .expect("origin summary survives");
        assert_eq!(carried.ssh_target, crate::protocol::HOME_SWITCH_TARGET);
        assert_eq!(carried.workspaces[0].workspace, "flock");
        // A nested leap (pass-through) keeps the hub's own summary too.
        let nested = FleetSnapshotState::from_wire(back.to_wire("lars@anvil"));
        assert!(nested.origin_summary.is_some());
    }

    #[test]
    fn parse_summary_response_reads_envelope() {
        let stdout = r#"
Last login: whatever banner
{"id":"cli:peers:summary","result":{"host":"anvil","version":"0.6.8","system":{"cpu_percent":71,"mem_used":48000000000,"mem_total":64000000000,"disk_free":200000000000},"workspaces":[{"workspace":"flock","project_key":"github.com/gerchowl/flock","project_label":"flock","branch":"fix/pty","is_linked_worktree":true,"agent":"cc","status":"blocked","status_age_secs":840}]}}
"#;
        let payload = parse_summary_response(stdout, 34).unwrap();
        assert_eq!(payload.host, "anvil");
        assert_eq!(payload.version.as_deref(), Some("0.6.8"));
        assert_eq!(payload.latency_ms, 34);
        let system = payload.system.expect("system stats present");
        assert_eq!(system.cpu_percent, Some(71));
        assert_eq!(system.mem_total, Some(64000000000));
        assert_eq!(payload.workspaces.len(), 1);
        assert_eq!(payload.workspaces[0].workspace, "flock");
        assert_eq!(payload.workspaces[0].status, AgentStatus::Blocked);
        assert_eq!(payload.workspaces[0].status_age_secs, Some(840));
        assert!(payload.workspaces[0].is_linked_worktree);
    }

    #[test]
    fn parse_summary_response_reads_relayed_fleet() {
        // Gossip v3 (#101): peers.summary carries relayed_fleet — one hop of
        // the polling hub's own peers, so a spoke attaching to this hub sees
        // the FULL fleet, not just this hub's direct rows.
        let stdout = r#"{"id":"x","result":{"host":"hub","workspaces":[],"relayed_fleet":[{"name":"spoke2","ssh_target":"lars@spoke2","host":"spoke2","workspaces":[],"origin":"hub"}]}}"#;
        let payload = parse_summary_response(stdout, 4).unwrap();
        assert_eq!(payload.relayed_fleet.len(), 1);
        assert_eq!(payload.relayed_fleet[0].name, "spoke2");
        assert_eq!(payload.relayed_fleet[0].origin, "hub");
    }

    #[test]
    fn parse_summary_response_treats_missing_relayed_fleet_as_empty() {
        // Additive-with-default: a v(N-1) peer that never emits relayed_fleet
        // parses cleanly and the merged cache stays empty.
        let stdout = r#"{"id":"x","result":{"host":"sage","workspaces":[]}}"#;
        let payload = parse_summary_response(stdout, 5).unwrap();
        assert!(payload.relayed_fleet.is_empty());
    }

    #[test]
    fn parse_summary_response_reads_icon_and_tolerates_absence() {
        // #164: the self-declared icon name parses from the JSON envelope...
        let with = r#"{"id":"x","result":{"host":"mba22","icon":"laptop","workspaces":[]}}"#;
        assert_eq!(
            parse_summary_response(with, 5).unwrap().icon.as_deref(),
            Some("laptop")
        );
        // ...and a v(N-1) peer that never emits it parses as None.
        let without = r#"{"id":"x","result":{"host":"sage","workspaces":[]}}"#;
        assert_eq!(parse_summary_response(without, 5).unwrap().icon, None);
    }

    #[test]
    fn parse_summary_response_tolerates_missing_system_block() {
        let stdout = r#"{"id":"x","result":{"host":"sage","workspaces":[]}}"#;
        let payload = parse_summary_response(stdout, 5).unwrap();
        assert_eq!(payload.host, "sage");
        assert!(payload.system.is_none());
        assert!(payload.version.is_none());
        assert!(payload.workspaces.is_empty());
    }

    #[test]
    fn parse_summary_response_surfaces_peer_errors() {
        let err = parse_summary_response(r#"{"id":"x","error":{"code":"nope"}}"#, 1).unwrap_err();
        assert!(err.contains("peer error"));
        assert!(parse_summary_response("no json here", 1).is_err());
    }

    #[test]
    fn parse_checkout_prepare_reads_report_and_surfaces_errors() {
        let stdout = r#"
Last login: banner noise
{"id":"cli:peers:checkout_prepare","result":{"type":"peers_checkout_prepared","branch":"feature-x","was_dirty":true,"was_unpushed":true,"pushed":true}}
"#;
        let outcome = parse_checkout_prepare_response(stdout).unwrap();
        assert_eq!(
            outcome,
            PeerCheckoutOutcome {
                branch: "feature-x".into(),
                was_dirty: true,
                was_unpushed: true,
                pushed: true,
            }
        );

        // A pure probe (push=false) carries pushed=false.
        let probe = parse_checkout_prepare_response(
            r#"{"id":"x","result":{"branch":"main","was_dirty":false,"was_unpushed":false,"pushed":false}}"#,
        )
        .unwrap();
        assert!(!probe.pushed);
        assert!(!probe.was_dirty);

        // Peer-side errors and malformed output surface as Err.
        let err = parse_checkout_prepare_response(
            r#"{"id":"x","error":{"code":"no_branch","message":"workspace has no git branch"}}"#,
        )
        .unwrap_err();
        assert!(err.contains("peer error"));
        assert!(err.contains("no git branch"));
        assert!(parse_checkout_prepare_response("no json here").is_err());
        // A result with no branch is rejected (the hub needs it to fetch).
        assert!(parse_checkout_prepare_response(r#"{"id":"x","result":{"pushed":true}}"#).is_err());
    }

    #[test]
    fn checkout_prepare_command_rejects_unsafe_workspace_ids() {
        let peer = PeerConfig {
            name: "anvil".into(),
            ..Default::default()
        };
        // Never spawns ssh: the guard rejects shell-unsafe ids before dialing.
        assert!(run_checkout_prepare_command(&peer, "ws_3; rm -rf /", false).is_err());
        assert!(run_checkout_prepare_command(&peer, "", false).is_err());
    }

    #[test]
    fn parse_logs_response_reads_lines_and_surfaces_errors() {
        // Login-shell banner before the envelope, as a real peer would emit.
        let stdout = r#"
Last login: banner noise
{"id":"cli:peers:logs","result":{"type":"peers_logs","host":"anvil","lines":[{"ts":"2026-06-29T00:00:01Z","level":"INFO","target":"flock::app","message":"up","source":"flock-server.log"}]}}
"#;
        let lines = parse_logs_response(stdout).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].target, "flock::app");
        assert_eq!(lines[0].source.as_deref(), Some("flock-server.log"));

        let err = parse_logs_response(r#"{"id":"x","error":{"message":"nope"}}"#).unwrap_err();
        assert!(err.contains("nope"), "{err}");
        assert!(parse_logs_response("no json here").is_err());
    }

    #[test]
    fn parse_logs_response_round_trips_serialized_log_lines() {
        // Build the SAME envelope the CLI's print_logs_json emits (a serialized
        // LogLine inside result.lines) and parse it back — catches any drift
        // between the producer's serde field names and the consumer.
        let original = crate::logging::LogLine {
            ts: "2026-06-29T00:00:01Z".into(),
            level: "INFO".into(),
            target: "flock::app::api".into(),
            message: "ok".into(),
            source: Some("flock-server.log".into()),
            host: None,
        };
        let envelope = serde_json::json!({
            "id": "cli:peers:logs",
            "result": { "type": "peers_logs", "host": "anvil", "lines": [original.clone()] },
        });
        let parsed = parse_logs_response(&envelope.to_string()).unwrap();
        assert_eq!(parsed, vec![original]);
    }

    #[test]
    fn reachability_reflects_latency_and_staleness() {
        let mut peer = PeerSummaryState::new(&PeerConfig {
            name: "anvil".into(),
            ..Default::default()
        });
        assert_eq!(peer.reachability(), PeerReachability::Down); // never polled
        peer.last_ok = Some(Instant::now());
        peer.latency_ms = Some(20);
        assert_eq!(peer.reachability(), PeerReachability::Live);
        peer.latency_ms = Some(PEER_SLOW_LATENCY_MS + 1);
        assert_eq!(peer.reachability(), PeerReachability::Slow);
        peer.error = Some("timeout".into());
        assert_eq!(peer.reachability(), PeerReachability::Down);
    }

    #[test]
    fn reachability_with_uses_configured_thresholds() {
        // The config-threaded variants (#96) must gate on the caller-supplied
        // thresholds, not the const default. A 30s-stale peer is Live when
        // stale_after=60, Down when stale_after=15.
        let mut peer = PeerSummaryState::new(&PeerConfig {
            name: "anvil".into(),
            ..Default::default()
        });
        peer.last_ok = Instant::now().checked_sub(std::time::Duration::from_secs(30));
        peer.latency_ms = Some(50);
        assert!(!peer.is_stale_with(60));
        assert_eq!(
            peer.reachability_with(60, 200),
            PeerReachability::Live,
            "50ms < slow_threshold=200 keeps peer live"
        );
        assert!(peer.is_stale_with(15));
        assert_eq!(
            peer.reachability_with(15, 200),
            PeerReachability::Down,
            "shorter stale threshold flips to Down"
        );
        // A tighter slow threshold flips the color.
        peer.last_ok = Some(Instant::now());
        assert_eq!(
            peer.reachability_with(60, 20),
            PeerReachability::Slow,
            "50ms > slow_threshold=20 renders Slow"
        );
    }

    #[test]
    fn carried_entry_fresh_by_origin_survives_dwell_past_stale_after() {
        // Gossip v3 (#101) part 2 (RED): the 60s-dwell ghost cliff dies.
        // A carried snapshot entry whose local `last_ok` is far past the
        // configured `stale_after` still renders Live when the ORIGIN's
        // frozen assertion (`origin_last_ok_secs`) says fresh. Without
        // origin_last_ok_secs, this test fails because is_stale_with falls
        // through to last_ok.elapsed() and cliffs the row.
        let mut peer = PeerSummaryState::new(&PeerConfig {
            name: "sage".into(),
            ..Default::default()
        });
        // Dwell = 90s (past stale_after = 60), but origin polled sage 5s ago.
        peer.last_ok = Instant::now().checked_sub(std::time::Duration::from_secs(90));
        peer.origin_last_ok_secs = Some(5);
        peer.latency_ms = Some(20);

        assert!(
            !peer.is_stale_with(60),
            "origin-fresh entry must not cliff at local dwell = 90s"
        );
        assert_eq!(
            peer.reachability_with(60, 200),
            PeerReachability::Live,
            "dwell past stale_after cannot ghost an origin-fresh row"
        );

        // The FROZEN origin assertion also blocks Down: if the origin's
        // assertion says stale (origin_last_ok_secs > stale_after), we ARE
        // stale regardless of what a fresh local last_ok would say.
        peer.last_ok = Some(Instant::now());
        peer.origin_last_ok_secs = Some(120);
        assert!(
            peer.is_stale_with(60),
            "origin-stale entry must render Down"
        );
        assert_eq!(
            peer.reachability_with(60, 200),
            PeerReachability::Down,
            "origin's stale assertion wins over fresh local last_ok"
        );
    }

    #[test]
    fn fleet_peer_wire_missing_origin_last_ok_falls_back_to_age_secs() {
        // Mixed-version safety (#101 part 2): a pre-v22 wire has
        // origin_last_ok_secs=None on decode. peer_from_wire falls back to
        // age_secs, so the origin-honest staleness path applies even for
        // entries from an older peer — the 60s cliff dies for those too.
        let wire = crate::protocol::FleetPeer {
            name: "old".into(),
            ssh_target: "lars@old".into(),
            host: Some("old".into()),
            version: None,
            protocol: None,
            system: None,
            latency_ms: None,
            workspaces: Vec::new(),
            age_secs: Some(5),
            error: None,
            origin_last_ok_secs: None,
            proxy_jump: None,
            icon: None,
        };
        let state = peer_from_wire(wire);
        assert_eq!(state.origin_last_ok_secs, Some(5));
        assert!(!state.is_stale_with(60));
    }

    #[test]
    fn relayed_fleet_peer_json_round_trips_both_ways_missing_field() {
        // Mixed-version JSON safety (#101 part 2): a v(N-1) peer that never
        // emits origin_last_ok_secs decodes to None (round-trip forward), and
        // a v(N) peer that emits it decodes intact (round-trip backward).
        use crate::api::schema::RelayedFleetPeer;

        // v(N-1) JSON → v(N) struct: origin_last_ok_secs missing → None.
        let json_old =
            r#"{"name":"sage","ssh_target":"lars@sage","workspaces":[],"origin":"anvil"}"#;
        let decoded: RelayedFleetPeer = serde_json::from_str(json_old).expect("parse old wire");
        assert_eq!(decoded.origin_last_ok_secs, None);

        // v(N) struct → JSON → v(N) struct: value preserved.
        let full = RelayedFleetPeer {
            name: "sage".into(),
            ssh_target: "lars@sage".into(),
            host: None,
            version: None,
            protocol: None,
            system: None,
            latency_ms: None,
            workspaces: Vec::new(),
            age_secs: Some(3),
            error: None,
            origin: "anvil".into(),
            origin_last_ok_secs: Some(3),
            proxy_jump: Some("anvil".into()),
            icon: None,
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: RelayedFleetPeer = serde_json::from_str(&json).unwrap();
        assert_eq!(back, full);

        // v(N) JSON → hypothetical v(N-1) struct: unknown fields ignored is
        // serde_json's default; simulate by decoding into a value and checking
        // known fields, which is the only cross-version compat guarantee.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["name"], "sage");
        assert_eq!(value["origin_last_ok_secs"], 3);
    }

    #[test]
    fn peer_poll_tracker_dispatches_first_call_and_arms_next_due() {
        // First call on a fresh peer: always dispatch, mark in-flight.
        // Callers must invoke mark_finished before the next round.
        let mut tracker = PeerPollTracker::new();
        let now = Instant::now();
        assert!(
            tracker.should_poll_now("anvil", now, Duration::from_secs(15)),
            "first call must dispatch"
        );
        assert!(tracker.in_flight("anvil"));
        assert!(
            !tracker.should_poll_now("anvil", now, Duration::from_secs(15)),
            "second call while in-flight must skip (overlap guard)"
        );
        tracker.mark_finished("anvil");
        assert!(
            !tracker.should_poll_now(
                "anvil",
                now + Duration::from_secs(1),
                Duration::from_secs(15)
            ),
            "not-yet-due skips even after the previous call finished"
        );
        assert!(
            tracker.should_poll_now(
                "anvil",
                now + Duration::from_secs(15),
                Duration::from_secs(15)
            ),
            "at-or-past next_due dispatches"
        );
    }

    #[test]
    fn peer_poll_tracker_overlap_guard_holds_across_config_reload() {
        // A slow ProxyJump peer polling at 2s must not stack: if the previous
        // fetch is still in flight, the next round MUST skip that peer even
        // though `now` is far past `next_due`. Then `retain_only` on a config
        // reload (peer still present) preserves the in-flight lock.
        let mut tracker = PeerPollTracker::new();
        let t0 = Instant::now();
        assert!(tracker.should_poll_now("sage", t0, Duration::from_secs(2)));

        // Two rounds later, the slow SSH is still running.
        assert!(
            !tracker.should_poll_now("sage", t0 + Duration::from_secs(4), Duration::from_secs(2)),
            "in-flight guard MUST hold even past next_due — a hung SSH cannot pile"
        );
        // Config reload (peer still present): the in-flight lock survives.
        tracker.retain_only(vec!["sage"]);
        assert!(
            !tracker.should_poll_now("sage", t0 + Duration::from_secs(8), Duration::from_secs(2)),
            "reload must NOT drop the in-flight lock for a surviving peer"
        );
        // Retain that drops the peer clears its state.
        tracker.retain_only::<Vec<&str>>(vec![]);
        assert!(
            tracker.should_poll_now("sage", t0 + Duration::from_secs(9), Duration::from_secs(2)),
            "peer dropped from config, then re-added, starts fresh"
        );
    }
}
