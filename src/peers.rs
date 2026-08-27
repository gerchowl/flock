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

/// Wall-clock bound on one peer SSH round.
///
/// `ConnectTimeout` bounds the CONNECT and `ServerAlive` detects a dead
/// network, but neither covers the case that matters: a peer that is reachable
/// and answering TCP while the remote `flk` is wedged. The channel stays
/// healthy, the command never returns, and `PeerPollTracker` holds that peer's
/// in-flight slot forever — so the peer silently stops being polled for the
/// life of the process.
const PEER_SSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
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
    /// Instant this peer's current fetch was dispatched at. `Some` iff
    /// `in_flight` — the timestamp the aggregate poller-health snapshot
    /// projects to answer "how long has the oldest fetch been out?", the
    /// signal that a peer is wedging rather than merely slow (#295).
    in_flight_since: Option<Instant>,
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
                in_flight_since: None,
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
        entry.in_flight_since = Some(now);
        entry.next_due = Some(now + effective_interval);
        true
    }

    /// Release the in-flight lock for `peer_name`. Called from the
    /// `PeerSummaryFetched` handler regardless of `Ok`/`Err` — the next
    /// round's `should_poll_now` will then decide from the next-due gate.
    pub fn mark_finished(&mut self, peer_name: &str) {
        if let Some(entry) = self.entries.get_mut(peer_name) {
            entry.in_flight = false;
            entry.in_flight_since = None;
        }
    }

    /// The OLDEST `in_flight_since` across all peers, or `None` when nothing
    /// is out. Projected into `PollerHealth.in_flight_age_secs` so the
    /// snapshot reports how long the most-behind fetch has been running —
    /// the rate-of-change signal an operator alerts on. The `in_flight` bit
    /// on the wire is derived from this being `Some`.
    pub fn oldest_in_flight_since(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter_map(|entry| entry.in_flight_since)
            .min()
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

    /// Test seam: pin a peer's `in_flight_since` to a known instant so
    /// aggregate-age projections can be asserted without racing the wall
    /// clock through `should_poll_now`'s next-due arming.
    #[cfg(test)]
    pub(crate) fn set_in_flight_since_for_test(&mut self, peer_name: &str, at: Instant) {
        let entry = self
            .entries
            .entry(peer_name.to_string())
            .or_insert(PeerPollEntry {
                in_flight: false,
                next_due: None,
                in_flight_since: None,
            });
        entry.in_flight = true;
        entry.in_flight_since = Some(at);
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
    /// seconds. Set from a wire
    /// [`crate::protocol::FleetPeer::origin_last_ok_secs`] on snapshot ingest
    /// and from a relayed entry's field on cache merge. `None` for locally
    /// polled config peers, where `last_ok` (a real Instant) carries the
    /// freshness and staleness falls back to the local-dwell path.
    ///
    /// This is the age AT CAPTURE and does not move on its own; freshness is
    /// this plus [`Self::ingested_at`]'s dwell. See `is_stale_with`.
    pub origin_last_ok_secs: Option<u64>,
    /// When a carried/relayed entry landed here — the clock that turns
    /// [`Self::origin_last_ok_secs`] from a fixed capture-time reading into a
    /// live age. `None` for locally polled peers, which have a real `last_ok`.
    pub ingested_at: Option<Instant>,
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
            ingested_at: None,
            proxy_jump: None,
            icon: None,
        }
    }

    pub fn is_stale(&self) -> bool {
        self.is_stale_with(PEER_STALE_AFTER_SECS)
    }

    /// Config-aware staleness (#96): uses the caller-supplied threshold.
    ///
    /// A carried / relayed entry is judged on the ORIGIN's report age at
    /// capture PLUS the time it has since sat here. Both halves matter:
    ///
    /// * without the origin's age, a snapshot entry decays against the
    ///   receiver's own clock as though the receiver had polled it, which is
    ///   the 60s-dwell ghost cliff #101 part 2 set out to kill;
    /// * without dwell, the reading is frozen at capture and never moves —
    ///   so when the relaying hub itself goes away and nothing refreshes
    ///   these rows again, every node it relayed renders Live forever.
    ///
    /// The second is the worse failure. flock exists so the fleet view can be
    /// trusted; a node that stopped answering must stop looking alive, and an
    /// unbounded confident lie is strictly worse than showing it as gone.
    ///
    /// Locally-polled entries (`origin_last_ok_secs = None`) keep the
    /// `last_ok.elapsed()` path — there `last_ok` is a real local Instant and
    /// already carries both halves.
    /// Reads the clock. See [`Self::is_stale_at`] for the decision itself.
    pub fn is_stale_with(&self, stale_after_secs: u64) -> bool {
        self.is_stale_at(Instant::now(), stale_after_secs)
    }

    /// Staleness as of `now` — the whole decision, with no ambient clock.
    ///
    /// Time enters this module here and in [`Self::carried_age_secs_at`], and
    /// nowhere else. That is what lets a test advance the clock by ninety
    /// seconds and assert on the result, rather than back-dating a field and
    /// hoping the arithmetic underneath matches. It also matches how the
    /// headless loop already works, where `now` is sampled once per pass and
    /// threaded into `can_render_now`, `unattended_render_due` and the
    /// scheduled-task handlers.
    pub fn is_stale_at(&self, now: Instant, stale_after_secs: u64) -> bool {
        if let Some(age) = self.carried_age_secs_at(now) {
            return age > stale_after_secs;
        }
        match self.last_ok {
            Some(at) => now.saturating_duration_since(at).as_secs() > stale_after_secs,
            None => true,
        }
    }

    /// Reads the clock. See [`Self::carried_age_secs_at`].
    pub fn carried_age_secs(&self) -> Option<u64> {
        self.carried_age_secs_at(Instant::now())
    }

    /// Live age of a carried/relayed entry as of `now`: the origin's age at
    /// capture plus local dwell. `None` for a locally polled peer.
    pub fn carried_age_secs_at(&self, now: Instant) -> Option<u64> {
        let origin_secs = self.origin_last_ok_secs?;
        let dwell = self
            .ingested_at
            .map(|at| now.saturating_duration_since(at).as_secs())
            .unwrap_or(0);
        Some(origin_secs.saturating_add(dwell))
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
    /// Reads the clock. See [`Self::reachability_at`].
    pub fn reachability_with(
        &self,
        stale_after_secs: u64,
        slow_threshold_ms: u64,
    ) -> PeerReachability {
        self.reachability_at(Instant::now(), stale_after_secs, slow_threshold_ms)
    }

    /// Reachability as of `now`.
    pub fn reachability_at(
        &self,
        now: Instant,
        stale_after_secs: u64,
        slow_threshold_ms: u64,
    ) -> PeerReachability {
        if self.is_stale_at(now, stale_after_secs) || self.error.is_some() {
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

/// One entry in the relay cache: a peer some hub told us about.
///
/// Holds a materialised [`PeerSummaryState`] rather than the wire shape, so the
/// sidebar can render a relayed node exactly like any other peer instead of the
/// row existing only to be forwarded onward (#101 part 1 follow-up).
///
/// The wire entry's `origin` is deliberately NOT carried: it is consumed at
/// merge time, where loop prevention drops rows this server itself originated,
/// and nothing downstream needs to know which hub a row arrived through. The
/// reachable identity a receiver does need travels separately, on the peer's
/// own `proxy_jump`.
#[derive(Debug, Clone)]
pub struct RelayedEntry {
    /// The relayed peer, in the same shape as a locally polled one.
    pub peer: PeerSummaryState,
}

/// Materialise a relayed wire entry into the shape every rendering surface
/// already understands.
pub fn relayed_entry_from_wire(entry: crate::api::schema::RelayedFleetPeer) -> RelayedEntry {
    RelayedEntry {
        peer: PeerSummaryState {
            peer: entry.name,
            ssh_target: entry.ssh_target,
            host: entry.host,
            version: entry.version,
            protocol: entry.protocol,
            system: entry.system,
            latency_ms: entry.latency_ms,
            workspaces: entry.workspaces,
            last_ok: entry
                .age_secs
                .and_then(|secs| Instant::now().checked_sub(Duration::from_secs(secs))),
            error: entry.error,
            // Prefer the explicit origin assertion; fall back to `age_secs` for
            // a v(N-1) hub that does not send one.
            origin_last_ok_secs: entry.origin_last_ok_secs.or(entry.age_secs),
            // Dwell starts now — this is when the reading entered this server.
            ingested_at: Some(Instant::now()),
            proxy_jump: entry.proxy_jump,
            icon: entry.icon,
        },
    }
}

/// Wire shape of one cached peer summary (`Instant` freshness → age in
/// seconds at capture time).
pub fn peer_to_wire(peer: &PeerSummaryState) -> crate::protocol::FleetPeer {
    peer_to_wire_at(Instant::now(), peer)
}

/// Encode as of `now`, so the ages that ride the wire are testable without
/// waiting for real seconds to pass.
pub fn peer_to_wire_at(now: Instant, peer: &PeerSummaryState) -> crate::protocol::FleetPeer {
    crate::protocol::FleetPeer {
        name: peer.peer.clone(),
        ssh_target: peer.ssh_target.clone(),
        host: peer.host.clone(),
        version: peer.version.clone(),
        protocol: peer.protocol,
        system: peer.system.clone().map(Into::into),
        latency_ms: peer.latency_ms,
        workspaces: peer.workspaces.iter().cloned().map(Into::into).collect(),
        age_secs: peer
            .last_ok
            .map(|at| now.saturating_duration_since(at).as_secs()),
        error: peer.error.clone(),
        // Gossip v3 (#101 part 2): forward the frozen origin assertion when
        // the source was a snapshot / relay entry that already carried it.
        // Otherwise the local-poll last_ok IS the origin and doubles as the
        // frozen assertion at capture time (age_secs).
        // Carry the age INCLUDING our dwell, so a second hop inherits an
        // honest reading rather than the capture-time one we were handed.
        origin_last_ok_secs: peer.carried_age_secs_at(now).or_else(|| {
            peer.last_ok
                .map(|at| now.saturating_duration_since(at).as_secs())
        }),
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
        // Dwell starts now: this is the moment the reading entered this server.
        ingested_at: Some(Instant::now()),
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

/// Run a peer fetch so that a panic becomes a failed poll instead of a lost
/// completion event.
///
/// `PeerPollTracker::should_poll_now` marks a peer in-flight before its worker
/// is spawned, and the ONLY release is the `PeerSummaryFetched` the worker
/// sends back. A worker that unwound sent nothing, so that peer was never
/// polled again for the rest of the process lifetime — with no symptom but a
/// row that quietly went stale while every other peer kept updating.
///
/// Takes the fetch as a closure so the guard is testable without a reachable
/// peer: the dispatcher passes the real SSH fetch, a test passes one that
/// panics.
pub fn fetch_with_panic_guard<F>(peer_name: &str, fetch: F) -> PeerSummaryFetch
where
    F: FnOnce() -> PeerSummaryFetch,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(fetch)).unwrap_or_else(|_| {
        PeerSummaryFetch {
            peer: peer_name.to_string(),
            result: Err("peer summary fetch panicked".to_string()),
        }
    })
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
    // The held connection carries an API request; `summary_command` is a
    // shell string. They only mean the same thing while the command is the
    // shipped default, so a customized one keeps the one-shot path rather
    // than being silently reinterpreted as `peers.summary`.
    if peer.summary_command == crate::config::model::default_peer_summary_command() {
        // A push already answered this poll — the peer told us the moment its
        // state changed, so the round trip would only re-fetch what is
        // already here.
        if let Some(pushed) = crate::peer_stream::take_pushed_summary(peer) {
            crate::logging::peer_push_consumed(&peer.name);
            return Ok(pushed);
        }
        match crate::peer_stream::request(peer, "peers.summary", serde_json::json!({})) {
            Ok(response) => return Ok(response),
            // Every failure mode ends here — old `flk` without `peers relay`,
            // asleep, wedged relay — and the answer is the same for all of
            // them: take the path that already works.
            Err(err) => crate::logging::peer_stream_fallback(&peer.name, &err),
        }
    }
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
/// Wrap a value as one POSIX single-quoted shell word.
///
/// `'` cannot appear inside single quotes, so each one closes the quote, emits
/// an escaped quote, and reopens — the standard `'\''` idiom.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Hand a message to the peer that owns the recipient, for it to enqueue in
/// its own mailbox (ADR-0008).
///
/// The same SSH-invoked verb surface `run_summary_command` and
/// `run_checkout_prepare_command` use: we ask the owning server to act on its
/// own state rather than reaching into it. That keeps ADR-0001 intact — the
/// constraint there is that fleet *gossip* is pull, with no push/broadcast
/// between servers; a directed, user-initiated verb call is neither, which is
/// why cross-machine checkout-prepare already works this way.
///
/// The recipient's own flock does the queueing, the inbox and the wake, so
/// there is exactly one delivery implementation no matter which host the
/// sender was on.
pub fn send_peer_message(
    peer: &PeerConfig,
    to_agent: &str,
    from_agent: &str,
    from_host: &str,
    body: &str,
    correlation_id: &str,
    in_reply_to: Option<&str>,
    intent: crate::api::schema::MsgIntent,
) -> Result<(), String> {
    let remote = peer_message_command(
        to_agent,
        from_agent,
        from_host,
        body,
        correlation_id,
        in_reply_to,
        intent,
    )?;
    run_peer_ssh(peer, &remote).map(|_| ())
}

/// Build the `sh -lc …` the relay hands to the owning server.
///
/// Split out from [`send_peer_message`] so the quoting and the id guard — the
/// two things here that have actually been wrong in production — can be
/// asserted without an SSH round trip.
fn peer_message_command(
    to_agent: &str,
    from_agent: &str,
    from_host: &str,
    body: &str,
    correlation_id: &str,
    in_reply_to: Option<&str>,
    intent: crate::api::schema::MsgIntent,
) -> Result<String, String> {
    // Ids are server-minted and travel into a remote shell command; refuse
    // anything that could escape it (same guard shape as checkout-prepare).
    for (label, value) in [
        ("agent id", to_agent),
        ("sender id", from_agent),
        ("sender host", from_host),
        ("correlation id", correlation_id),
    ]
    .into_iter()
    .chain(in_reply_to.map(|id| ("in-reply-to id", id)))
    {
        if value.is_empty()
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ':')
        {
            return Err(format!("invalid {label}: {value:?}"));
        }
    }
    // Threading has to survive the hop as well as the message does. Without
    // `--reply-to` a cross-host answer arrived with `in_reply_to` empty, so an
    // agent that had asked two questions could not tell which one it had just
    // been answered — the reply routed home and still lost the one field that
    // made it an answer.
    let reply_to = in_reply_to
        .map(|id| format!(" --reply-to {id}"))
        .unwrap_or_default();
    // Intent has to survive the hop too, or a cross-host question arrives
    // stamped `fyi` — the exact mislabel #280 exists to remove, reintroduced
    // by the one leg that rebuilds the send from scratch (#280).
    //
    // Appended only for `needs_reply`, the same shape `--reply-to` uses. A
    // default-intent relay is then byte-identical to what shipped before this
    // flag existed, so the far side needing a build that understands
    // `--intent` is confined to the case that actually carries new signal.
    let intent_flag = match intent {
        crate::api::schema::MsgIntent::Fyi => String::new(),
        crate::api::schema::MsgIntent::NeedsReply => format!(
            " --intent {}",
            crate::api::schema::MsgIntent::NeedsReply.as_wire()
        ),
    };
    // Quote ONCE, at the outside. The body is caller-supplied and cannot be
    // validated like the ids, so it must never reach the remote shell as
    // syntax — but quoting it *inside* an already single-quoted `sh -lc '...'`
    // closes the outer quote and the shell then word-splits the message. A
    // live cross-host send arrived as "cross-machine" instead of
    // "cross-machine hello from sage" for exactly that reason.
    //
    // So: build the inner command with the body quoted, then quote the whole
    // inner command once more for `sh -lc`. Nesting handled by the same POSIX
    // idiom at both levels rather than by hand at one.
    let inner = format!(
        "flk msg send --agent {to_agent} --from-agent {from_agent} --from-host {from_host} \
         --correlation-id {correlation_id}{reply_to}{intent_flag} --json -- {}",
        shell_single_quote(body)
    );
    Ok(format!("sh -lc {}", shell_single_quote(&inner)))
}

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
        .output_traced_with_timeout(PEER_SSH_TIMEOUT)
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
    // #291: the JSON path does not pass through the bincode `From` impl, so
    // the host-declared thermal rank/label is normalized here instead.
    let system = result
        .get("system")
        .filter(|system| !system.is_null())
        .cloned()
        .map(serde_json::from_value::<crate::api::schema::PeerSystemSummary>)
        .transpose()
        .map_err(|err| format!("summary system parse error: {err}"))?
        .map(crate::api::schema::PeerSystemSummary::sanitized);
    let workspaces: Vec<PeerWorkspaceSummary> = result
        .get("workspaces")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| format!("summary workspaces parse error: {err}"))?
        .unwrap_or_default();
    // Gossip v3 (#101): relayed_fleet is additive with a serde default so a
    // v(N-1) peer that never emits the field parses cleanly.
    let mut relayed_fleet: Vec<crate::api::schema::RelayedFleetPeer> = result
        .get("relayed_fleet")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| format!("summary relayed_fleet parse error: {err}"))?
        .unwrap_or_default();
    // #291: a relayed entry is host-authored two hops back — sanitize it on
    // the same boundary rather than trusting the middle hop to have done it.
    for entry in &mut relayed_fleet {
        entry.system = entry
            .system
            .take()
            .map(crate::api::schema::PeerSystemSummary::sanitized);
    }
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
    fn shell_quoting_survives_spaces_and_quotes() {
        // A live cross-host send arrived truncated at the first space, because
        // the body was quoted INSIDE an already single-quoted `sh -lc '...'`:
        // the inner quote closed the outer one and the remote shell then word-
        // split the message. Quote once per level.
        assert_eq!(super::shell_single_quote("hello world"), "'hello world'");
        assert_eq!(super::shell_single_quote("it's fine"), r#"'it'\''s fine'"#);

        // Nesting the way the relay does: inner command quoted, then the whole
        // thing quoted again for `sh -lc`. The body must survive both.
        let inner = format!("flk msg send -- {}", super::shell_single_quote("a b c"));
        let outer = super::shell_single_quote(&inner);
        assert!(outer.starts_with('\''), "{outer}");
        assert!(outer.contains("a b c"), "body must survive: {outer}");
    }

    #[test]
    fn a_relayed_message_carries_its_threading() {
        // The reply routed home and arrived unthreaded: `relay_message_to_host`
        // filled `in_reply_to`, and the relay command dropped it on the floor
        // (#320). An answer that cannot be matched to its question is not an
        // answer when an agent has more than one outstanding.
        let command = super::peer_message_command(
            "agent_sage_1",
            "agent_mba22_2",
            "mba22",
            "pong",
            "c-reply",
            Some("c-question"),
            crate::api::schema::MsgIntent::Fyi,
        )
        .expect("valid ids");
        assert!(
            command.contains("--reply-to c-question"),
            "threading must reach the owning server: {command}"
        );

        // A first message has nothing to thread to, and must not grow an
        // empty flag the remote CLI would then reject.
        let command = super::peer_message_command(
            "agent_sage_1",
            "agent_mba22_2",
            "mba22",
            "ping",
            "c-first",
            None,
            crate::api::schema::MsgIntent::Fyi,
        )
        .expect("valid ids");
        assert!(!command.contains("--reply-to"), "{command}");
    }

    #[test]
    fn a_needs_reply_relay_carries_its_stamp_and_a_fyi_one_is_unchanged() {
        // #280. The relay is the one leg that REBUILDS the send from scratch,
        // as a `flk msg send` on the owning server — so a stamp not passed
        // here is a cross-host question arriving as a notice, which is the
        // mislabel the field exists to remove.
        let command = super::peer_message_command(
            "agent_sage_1",
            "agent_mba22_2",
            "mba22",
            "re-derive both parameters and report back",
            "c-question",
            None,
            crate::api::schema::MsgIntent::NeedsReply,
        )
        .expect("valid ids");
        assert!(
            command.contains("--intent needs_reply"),
            "the stamp must reach the owning server: {command}"
        );

        // The default relays byte-identically to what shipped before the flag
        // existed, so the far side needing a build that understands `--intent`
        // is confined to the case that actually carries new signal.
        let command = super::peer_message_command(
            "agent_sage_1",
            "agent_mba22_2",
            "mba22",
            "landed the fix",
            "c-notice",
            None,
            crate::api::schema::MsgIntent::Fyi,
        )
        .expect("valid ids");
        assert!(!command.contains("--intent"), "{command}");
    }

    #[test]
    fn a_threading_id_is_guarded_like_every_other_id() {
        // Every id in this command is interpolated into a remote shell, so the
        // new one is guarded by the same rule as the rest rather than trusted
        // for being server-minted.
        let err = super::peer_message_command(
            "agent_sage_1",
            "agent_mba22_2",
            "mba22",
            "pong",
            "c-reply",
            Some("c'; rm -rf /"),
            crate::api::schema::MsgIntent::Fyi,
        )
        .expect_err("a shell-escaping id must be refused");
        assert!(err.contains("in-reply-to id"), "{err}");
    }
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
            ingested_at: None,
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
                gpu_percent: None,
                thermal: None,
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
                agents: Vec::new(),
            }],
            last_ok: age_secs
                .and_then(|secs| Instant::now().checked_sub(std::time::Duration::from_secs(secs))),
            error: None,
            origin_last_ok_secs: None,
            ingested_at: None,
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
    fn fleet_system_wire_carries_gpu_and_thermal_both_directions() {
        use crate::api::schema::{ThermalComponent, ThermalReport};

        // #291: GPU utilization and the self-declared thermal report survive
        // the bincode roundtrip. GPU is the older half of this: it was sampled
        // locally for a long time with nowhere on the wire to go.
        let mut state = summary_state("anvil", "lars@anvil", Some(5));
        state.system.as_mut().unwrap().gpu_percent = Some(96);
        state.system.as_mut().unwrap().thermal = Some(ThermalReport {
            severity: 3,
            component: ThermalComponent::Gpu,
            label: "GPU 84".to_string(),
        });
        let system = peer_from_wire(peer_to_wire(&state)).system.unwrap();
        assert_eq!(system.gpu_percent, Some(96));
        let thermal = system.thermal.unwrap();
        assert_eq!(thermal.severity, 3);
        assert_eq!(thermal.component, ThermalComponent::Gpu);
        assert_eq!(thermal.label, "GPU 84");

        // Absent (a node that declares nothing, or any microVM) decodes to
        // None rather than a synthesized nominal reading.
        let none = summary_state("sage", "lars@sage", Some(5));
        let system = peer_from_wire(peer_to_wire(&none)).system.unwrap();
        assert_eq!(system.gpu_percent, None);
        assert_eq!(system.thermal, None);
    }

    #[test]
    fn thermal_report_from_a_peer_is_clamped_and_truncated_on_receive() {
        use crate::api::schema::{
            ThermalComponent, ThermalReport, THERMAL_LABEL_MAX_BYTES, THERMAL_SEVERITY_MAX,
        };

        // A peer running a broken reporter must not be able to push an
        // out-of-range rank or an unbounded label into our render pass.
        let mut state = summary_state("anvil", "lars@anvil", Some(5));
        state.system.as_mut().unwrap().thermal = Some(ThermalReport {
            severity: 200,
            component: ThermalComponent::Cpu,
            label: "x".repeat(4096),
        });
        let thermal = peer_from_wire(peer_to_wire(&state))
            .system
            .unwrap()
            .thermal
            .unwrap();
        assert_eq!(thermal.severity, THERMAL_SEVERITY_MAX);
        assert_eq!(thermal.label.len(), THERMAL_LABEL_MAX_BYTES);

        // Truncation lands on a char boundary — a multi-byte label must not
        // panic or produce invalid UTF-8 when it straddles the cap.
        // 3 bytes per char, so the cap at 16 lands MID-character — the case
        // that makes the char-boundary walk load-bearing. A plain
        // `String::truncate(16)` would panic here.
        let mut wide = summary_state("sage", "lars@sage", Some(5));
        wide.system.as_mut().unwrap().thermal = Some(ThermalReport {
            severity: 2,
            component: ThermalComponent::Node,
            label: "漢".repeat(20),
        });
        let thermal = peer_from_wire(peer_to_wire(&wide))
            .system
            .unwrap()
            .thermal
            .unwrap();
        assert!(thermal.label.len() <= THERMAL_LABEL_MAX_BYTES);
        assert!(thermal.label.chars().all(|c| c == '漢'));
    }

    #[test]
    fn parse_summary_response_reads_thermal_and_sanitizes_it() {
        use crate::api::schema::{ThermalComponent, THERMAL_LABEL_MAX_BYTES, THERMAL_SEVERITY_MAX};

        // #291: the JSON path does not pass through the bincode `From` impl,
        // so it sanitizes independently — regression guard for exactly that.
        let hot = concat!(
            r#"{"id":"x","result":{"host":"anvil","system":{"cpu_percent":4,"gpu_percent":97,"#,
            r#""thermal":{"severity":9,"component":"gpu","label":"aaaaaaaaaaaaaaaaaaaaaaaaaaaa"}},"#,
            r#""workspaces":[]}}"#
        );
        let system = parse_summary_response(hot, 5).unwrap().system.unwrap();
        assert_eq!(system.gpu_percent, Some(97));
        let thermal = system.thermal.unwrap();
        assert_eq!(thermal.severity, THERMAL_SEVERITY_MAX);
        assert_eq!(thermal.component, ThermalComponent::Gpu);
        assert_eq!(thermal.label.len(), THERMAL_LABEL_MAX_BYTES);

        // A node that emits neither field parses cleanly to None — the shape
        // every pre-#291 peer sends.
        let quiet =
            r#"{"id":"x","result":{"host":"sage","system":{"cpu_percent":4},"workspaces":[]}}"#;
        let system = parse_summary_response(quiet, 5).unwrap().system.unwrap();
        assert_eq!(system.gpu_percent, None);
        assert_eq!(system.thermal, None);
    }

    #[test]
    fn parse_summary_response_sanitizes_relayed_fleet_thermal() {
        use crate::api::schema::{THERMAL_LABEL_MAX_BYTES, THERMAL_SEVERITY_MAX};

        // #291: a relayed entry is host-authored two hops back. Sanitizing the
        // direct `system` block is not enough — without the relayed loop a
        // hostile rank/label reaches the render pass through the second hop.
        let relayed = concat!(
            r#"{"id":"x","result":{"host":"mba22","workspaces":[],"relayed_fleet":[{"#,
            r#""name":"anvil","ssh_target":"lars@anvil","system":{"cpu_percent":9,"#,
            r#""thermal":{"severity":250,"component":"cpu","label":"zzzzzzzzzzzzzzzzzzzzzzzzzz"}}"#,
            r#","origin":"mba22"}]}}"#
        );
        let payload = parse_summary_response(relayed, 5).unwrap();
        let thermal = payload.relayed_fleet[0]
            .system
            .as_ref()
            .unwrap()
            .thermal
            .as_ref()
            .unwrap();
        assert_eq!(thermal.severity, THERMAL_SEVERITY_MAX);
        assert_eq!(thermal.label.len(), THERMAL_LABEL_MAX_BYTES);
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
    fn carried_entry_is_judged_on_origin_age_plus_local_dwell() {
        // #101 part 2 killed the 60s-dwell ghost cliff: a carried entry must
        // not be cliffed by the RECEIVER's clock as though the receiver had
        // polled it. That half still holds — a short dwell on top of a fresh
        // origin reading stays Live.
        //
        // But it was implemented by FREEZING the origin's reading, which never
        // moves. When the relaying hub itself goes away, nothing refreshes
        // these rows again and every node it relayed renders Live forever.
        // flock exists so the fleet view can be trusted, so freshness is now
        // origin-age-at-capture PLUS dwell: honest at capture, and it decays.
        //
        // Time is a PARAMETER here, not a fact about when the test ran: the
        // entry is stamped once and the clock is advanced, which is what the
        // production path actually does.
        let ingested = Instant::now();
        let mut peer = PeerSummaryState::new(&PeerConfig {
            name: "spoke2.invalid".into(),
            ..Default::default()
        });
        // Deliberately ancient, to prove the local clock is NOT the input.
        peer.last_ok = ingested.checked_sub(Duration::from_secs(900));
        peer.origin_last_ok_secs = Some(5);
        peer.ingested_at = Some(ingested);
        peer.latency_ms = Some(20);

        // 10s later: origin polled it 5s before capture, so ~15s known age.
        let soon = ingested + Duration::from_secs(10);
        assert_eq!(peer.carried_age_secs_at(soon), Some(15));
        assert!(
            !peer.is_stale_at(soon, 60),
            "a fresh origin reading must not cliff on the receiver's own clock"
        );
        assert_eq!(peer.reachability_at(soon, 60, 200), PeerReachability::Live);

        // 90s later, nothing having refreshed it: ~95s unheard-from.
        let later = ingested + Duration::from_secs(90);
        assert_eq!(peer.carried_age_secs_at(later), Some(95));
        assert!(
            peer.is_stale_at(later, 60),
            "a carried reading must decay — a dead hub cannot leave rows Live forever"
        );
        assert_eq!(
            peer.reachability_at(later, 60, 200),
            PeerReachability::Down,
            "unbounded confident Live is the one thing the fleet view must not show"
        );

        // The origin's own reading still dominates a fresh local last_ok.
        let mut origin_stale = peer.clone();
        origin_stale.origin_last_ok_secs = Some(120);
        origin_stale.last_ok = Some(ingested);
        assert!(origin_stale.is_stale_at(ingested, 60));
        assert_eq!(
            origin_stale.reachability_at(ingested, 60, 200),
            PeerReachability::Down,
            "the origin's stale assertion wins over a fresh local last_ok"
        );
    }

    /// The ambient wrapper must not drift from the decision it delegates to.
    /// A seam is only worth having if the production path goes through it.
    #[test]
    fn ambient_clock_wrappers_agree_with_the_at_now_core() {
        let ingested = Instant::now();
        let mut peer = PeerSummaryState::new(&PeerConfig {
            name: "spoke2.invalid".into(),
            ..Default::default()
        });
        peer.origin_last_ok_secs = Some(3);
        peer.ingested_at = Some(ingested);
        peer.latency_ms = Some(10);

        // `Instant::now()` is a hair past `ingested`, so the ambient reading
        // must equal the explicit one taken at this moment.
        assert_eq!(
            peer.carried_age_secs(),
            peer.carried_age_secs_at(Instant::now())
        );
        assert_eq!(peer.is_stale_with(60), peer.is_stale_at(Instant::now(), 60));
        assert_eq!(
            peer.reachability_with(60, 200),
            peer.reachability_at(Instant::now(), 60, 200)
        );
    }

    /// Re-relaying must hand on the age INCLUDING our dwell, or each hop
    /// resets the clock and a long chain launders a stale reading into a fresh
    /// one. `FleetSnapshotState::to_wire` has always documented this ("ages are
    /// recomputed so time spent on this server keeps counting"); now it is true.
    #[test]
    fn re_relayed_age_accumulates_dwell_rather_than_resetting() {
        let ingested = Instant::now();
        let mut peer = PeerSummaryState::new(&PeerConfig {
            name: "spoke2.invalid".into(),
            ..Default::default()
        });
        peer.origin_last_ok_secs = Some(10);
        peer.ingested_at = Some(ingested);

        // Exactly 30s of dwell — no tolerance window needed now that the
        // clock is an argument rather than whatever the test machine did.
        let wire = peer_to_wire_at(ingested + Duration::from_secs(30), &peer);
        assert_eq!(
            wire.origin_last_ok_secs,
            Some(40),
            "origin's 10s at capture plus 30s of dwell here"
        );

        // And the next hop starts its own dwell from that accumulated age.
        let landed = peer_from_wire(wire);
        assert_eq!(landed.origin_last_ok_secs, Some(40));
        assert!(landed.ingested_at.is_some(), "dwell restarts on ingest");
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

    /// A fetch worker that panics must still produce a completion, or the
    /// peer's in-flight guard is never released and it is silently never
    /// polled again for the rest of the process lifetime.
    ///
    /// Drives `fetch_with_panic_guard` with a panicking fetch — remove the
    /// guard and this test unwinds instead of asserting. An earlier version
    /// called `mark_finished` directly on a synthetic `Err`, which passed just
    /// as well against the UNGUARDED code: `mark_finished` never looks at the
    /// result, so it proved nothing about the panic path.
    #[test]
    fn a_panicking_fetch_still_completes_and_frees_the_peer_to_poll_again() {
        // The panic is deliberate; keep the default hook from printing a
        // backtrace that makes a passing test read like a failing one.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let fetched = fetch_with_panic_guard("anvil", || panic!("summary parser blew up"));
        std::panic::set_hook(previous_hook);

        assert_eq!(fetched.peer, "anvil", "the completion names the right peer");
        assert!(
            fetched.result.is_err(),
            "a panicked fetch reports as a failed poll, not a success"
        );

        // And that completion is what frees the peer: the dispatcher hands it
        // to the tracker exactly like any other result.
        let mut tracker = PeerPollTracker::new();
        let now = Instant::now();
        assert!(tracker.should_poll_now(&fetched.peer, now, Duration::from_secs(15)));
        assert!(tracker.in_flight("anvil"));
        tracker.mark_finished(&fetched.peer);
        assert!(
            tracker.should_poll_now(
                "anvil",
                now + Duration::from_secs(15),
                Duration::from_secs(15)
            ),
            "the peer must be polled again on the next round"
        );
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
