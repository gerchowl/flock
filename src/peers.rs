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

/// Default `[gossip] dial_failure_summary_secs`: how often an UNCHANGED peer
/// poll outage is restated at WARN (#418). Ten minutes turns a five-day outage
/// on a 15s poll into ~720 lines per peer rather than ~29,000, while a WARN
/// tail read at any moment still shows the outage within minutes of history.
pub const PEER_DIAL_FAILURE_SUMMARY_SECS: u64 = 600;

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
    /// How this peer's polls have been failing, if they have (#418). Local
    /// polls only; a carried or relayed row has no dial of ours to judge.
    pub dial: PeerDialHealth,
    /// Why this peer has no held relay stream, when establishing one failed
    /// (#418). Kept visible while mesh enrollment is refused.
    pub stream_error: Option<String>,
}

/// Consecutive poll failures a reason must survive before it is shown on the
/// peer's row and in `flk peers` (#418): past ONE poll. A single failed dial
/// is a blip — a hopper changing networks — and labelling it would teach the
/// operator to ignore the label; two in a row is a state.
pub const DIAL_FAILURE_PERSISTS_AFTER: u32 = 2;

/// A peer's run of failed polls, and the bookkeeping that keeps its log
/// honest without flooding it (#418).
///
/// hopper failed every poll to every peer for five days and wrote the same
/// bare WARN 2,229 times a day, which is the same as writing nothing. This
/// logs the EDGES — the first failure, a change of reason, the recovery — and
/// one summary per `[gossip] dial_failure_summary_secs` while an unchanged
/// outage lasts, so the WARN tail still says the fleet is down on day five.
#[derive(Debug, Clone, Default)]
pub struct PeerDialHealth {
    pub consecutive_failures: u32,
    pub failing_since: Option<Instant>,
    pub reason: Option<SshFailureReason>,
    last_warned_at: Option<Instant>,
}

/// What one poll outcome should log. Decided by [`PeerDialHealth`] so the
/// rate limit is a pure function of the history and `now`, testable without
/// a clock or a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialLog {
    /// A new failure: the first after success, or a different reason.
    Failed,
    /// The same failure again; `summary` when the periodic reminder is due.
    StillFailing { summary: bool },
    /// Success after a run of failures.
    Recovered { failures: u32, failing_secs: u64 },
    /// Success after success.
    Quiet,
}

impl PeerDialHealth {
    pub fn record_failure(
        &mut self,
        reason: SshFailureReason,
        now: Instant,
        summary_every: std::time::Duration,
    ) -> DialLog {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let first = self.failing_since.is_none();
        if first {
            self.failing_since = Some(now);
        }
        let changed = self.reason != Some(reason);
        self.reason = Some(reason);
        if first || changed {
            self.last_warned_at = Some(now);
            return DialLog::Failed;
        }
        let due = self
            .last_warned_at
            .is_none_or(|at| now.saturating_duration_since(at) >= summary_every);
        if due {
            self.last_warned_at = Some(now);
        }
        DialLog::StillFailing { summary: due }
    }

    pub fn record_success(&mut self, now: Instant) -> DialLog {
        let previous = std::mem::take(self);
        match previous.failing_since {
            Some(since) => DialLog::Recovered {
                failures: previous.consecutive_failures,
                failing_secs: now.saturating_duration_since(since).as_secs(),
            },
            None => DialLog::Quiet,
        }
    }

    /// Seconds the current run of failures has lasted, as of `now`.
    pub fn failing_secs(&self, now: Instant) -> u64 {
        self.failing_since
            .map(|since| now.saturating_duration_since(since).as_secs())
            .unwrap_or(0)
    }

    /// The failure reason, once it has persisted past one poll — the gate on
    /// showing it to the operator.
    pub fn persistent_reason(&self) -> Option<SshFailureReason> {
        self.reason
            .filter(|_| self.consecutive_failures >= DIAL_FAILURE_PERSISTS_AFTER)
    }
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
            dial: PeerDialHealth::default(),
            stream_error: None,
        }
    }

    /// The dial report `flk peers` carries for this peer (#418): the reason
    /// once it has persisted past one poll, and why no stream is held. `None`
    /// when there is nothing to report.
    pub fn dial_report(&self, now: Instant) -> Option<crate::api::schema::PeerDialReport> {
        let reason = self.dial.persistent_reason();
        if reason.is_none() && self.stream_error.is_none() {
            return None;
        }
        Some(crate::api::schema::PeerDialReport {
            reason: reason.map(|reason| reason.as_str().to_string()),
            consecutive_failures: self.dial.consecutive_failures,
            failing_secs: self
                .dial
                .failing_since
                .is_some()
                .then(|| self.dial.failing_secs(now)),
            stream_reason: self
                .stream_error
                .as_deref()
                .map(|detail| SshFailureReason::classify(detail).as_str().to_string()),
        })
    }

    /// The failure reason to SHOW on this peer's row, if any (#418).
    ///
    /// A locally polled peer shows it only once it has persisted past one
    /// poll. A carried or relayed row has no local dial history, so it keeps
    /// the #410 behaviour of reading the reason from the error it arrived with.
    pub fn shown_failure_reason(&self) -> Option<SshFailureReason> {
        let reason = if self.dial.consecutive_failures > 0 {
            self.dial.persistent_reason()
        } else {
            self.error.as_deref().map(SshFailureReason::classify)
        };
        reason.filter(|reason| *reason != SshFailureReason::Other)
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
    /// `kiln`, not a raw OS hostname like `mac-atlas-12345.local`.
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

impl FleetSnapshotState {
    /// Drop pushed rows that claim to be this snapshot's ORIGIN (#424 review).
    ///
    /// The home row is refreshed only from the bound hub's own `hub_self`, see
    /// [`Self::absorb_hub_self`]. Anything else naming the home machine is a
    /// claim by whichever node the hub polled, keyed on a host it chose, so it
    /// is neither absorbed into the home row nor stored as a second one.
    pub fn without_origin_claims(
        &self,
        rows: Vec<crate::api::schema::RelayedFleetPeer>,
    ) -> Vec<crate::api::schema::RelayedFleetPeer> {
        let origin_key = self.origin.to_ascii_lowercase();
        rows.into_iter()
            .filter(|row| wire_row_identity(row) != origin_key)
            .collect()
    }

    /// Refresh the carried origin from the hub's own row (#424). The caller has
    /// already checked that the row names the hub bound to the relay AND that
    /// this hub is the snapshot's origin.
    ///
    /// The origin summary is the only view a spoke has of the server the
    /// client came from, and it was stamped once, at switch time. A fresher
    /// reading replaces it in place, so renames and new spaces there show up
    /// here, while the row keeps what makes it the home row: it switches via
    /// the reserved home target, never an ssh dial, whatever the reading says.
    pub fn absorb_hub_self(&mut self, row: crate::api::schema::RelayedFleetPeer) {
        let Some(entry) = relayed_entry_from_wire(row) else {
            return;
        };
        let mut reading = entry.peer;
        let fresher = match (
            self.origin_summary
                .as_ref()
                .and_then(PeerSummaryState::carried_age_secs),
            reading.carried_age_secs(),
        ) {
            (Some(current), Some(new)) => new <= current,
            (None, _) => true,
            (Some(_), None) => false,
        };
        if !fresher {
            return;
        }
        reading.ssh_target = crate::protocol::HOME_SWITCH_TARGET.to_string();
        reading.proxy_jump = None;
        self.origin_summary = Some(reading);
    }
}

/// The identity a relayed row is stored under: its reported host, else its ssh
/// target, lowercased. Exactly the relay cache key, so a row cannot match one
/// machine here and be stored as another there. Unlike [`normalized_host_key`]
/// nothing is stripped, because identity must not fold `a.lan` into `a.tailnet`.
pub fn wire_row_identity(row: &crate::api::schema::RelayedFleetPeer) -> String {
    row.host
        .as_deref()
        .filter(|host| !host.is_empty())
        .unwrap_or(&row.ssh_target)
        .to_ascii_lowercase()
}

/// A host identity every viewer derives the same way (#422): lowercased, any
/// `user@` prefix dropped, and cut at the first dot so `atlas`, `atlas.local` and
/// a tailnet `atlas.tail1234.ts.net` are one machine. An IP address is kept
/// whole — cutting it would fold unrelated hosts together.
pub fn normalized_host_key(raw: &str) -> String {
    let host = raw.rsplit_once('@').map_or(raw, |(_, host)| host).trim();
    let lower = host.to_ascii_lowercase();
    if lower.parse::<std::net::IpAddr>().is_ok() {
        return lower;
    }
    match lower.split_once('.') {
        Some((short, _)) if !short.is_empty() => short.to_string(),
        _ => lower,
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
    /// The `[[peers]]` entry whose poll delivered this row (#410): the edge a
    /// message for one of its agents is handed to. Stamped at merge time, where
    /// the answering peer is known; `None` straight off the wire.
    pub via: Option<String>,
    /// A row a HUB pushed down to this spoke (#410), not one a peer we poll
    /// relayed up. Display-only: its ssh target and ProxyJump are never
    /// dialled. A spoke holds no key to anything, so a switch could not work
    /// anyway, and treating such a row as dialable would let whoever planted
    /// it choose where an operator's click sends their ssh.
    pub hub_pushed: bool,
}

/// Materialise a relayed wire entry into the shape every rendering surface
/// already understands, or `None` if the entry is one this host will not dial.
///
/// This is the boundary that matters for #392: `ssh_target` and `proxy_jump`
/// arrive verbatim from another host's gossip and end up in this machine's ssh
/// argv, where a value beginning with `-` is an OPTION and `-oProxyCommand=...`
/// runs a local command. Peers ride host-CA + tailnet rather than being
/// strangers, but one compromised or misconfigured fleet member is enough, and
/// choosing our ssh options is more authority than gossip is meant to carry.
///
/// A rejected row is DROPPED AND LOGGED, never fatal: the caller keeps
/// iterating the snapshot, so a bad row costs the fleet that row and no other.
pub fn relayed_entry_from_wire(
    entry: crate::api::schema::RelayedFleetPeer,
) -> Option<RelayedEntry> {
    if !crate::remote::is_valid_ssh_destination(&entry.ssh_target) {
        crate::logging::peer_relay_entry_rejected(
            &entry.origin,
            &entry.name,
            "ssh_target",
            &entry.ssh_target,
        );
        return None;
    }
    // An absent or empty `proxy_jump` means "dial directly" and is the normal
    // case for a row that needs no hub in the middle; only a value that will
    // actually reach `-o ProxyJump=` is checked.
    if let Some(jump) = entry
        .proxy_jump
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        if !crate::remote::is_valid_ssh_proxy_jump(jump) {
            crate::logging::peer_relay_entry_rejected(
                &entry.origin,
                &entry.name,
                "proxy_jump",
                jump,
            );
            return None;
        }
    }
    // Everything below renders in the sidebar, so it is cleaned the way every
    // other receive boundary cleans it: the host-declared system summary is
    // clamped, and free-form names lose any control bytes a peer planted.
    let strip = |text: String| crate::control_bytes::strip(&text);
    Some(RelayedEntry {
        peer: PeerSummaryState {
            dial: Default::default(),
            stream_error: None,
            peer: strip(entry.name),
            ssh_target: entry.ssh_target,
            host: entry.host.map(strip),
            version: entry.version.map(strip),
            protocol: entry.protocol,
            system: entry
                .system
                .map(crate::api::schema::PeerSystemSummary::sanitized),
            latency_ms: entry.latency_ms,
            workspaces: entry
                .workspaces
                .into_iter()
                .map(|mut ws| {
                    ws.workspace = strip(ws.workspace);
                    ws.project_label = ws.project_label.map(strip);
                    ws.branch = ws.branch.map(strip);
                    ws.agent = ws.agent.map(strip);
                    ws
                })
                .collect(),
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
        via: None,
        hub_pushed: false,
    })
}

/// Merge relayed rows that `via` told us about into the relay cache.
///
/// The ONE merge, shared by the two directions gossip now flows (#410): rows a
/// polled peer relays UP to its poller, and rows a hub pushes DOWN to a spoke
/// it polls. Two copies of this loop would be two answers to "which reading of
/// that host wins", and that drift is the defect the directory exists to stop.
///
/// Loop prevention rides on the origin field: an entry whose origin is us (the
/// one full cycle we could see — hub A polls hub B, B relayed A's own peers
/// back) and an entry ABOUT us are both dropped. Freshest-wins across sources,
/// on the LIVE age (origin's reading plus dwell here), so a source that has
/// gone quiet cannot keep winning against one still polling. #392: a row whose
/// ssh_target or proxy_jump this host refuses to dial is dropped and logged,
/// and the rest still merges.
pub(crate) fn merge_relayed_fleet(
    cache: &mut std::collections::HashMap<String, RelayedEntry>,
    entries: Vec<crate::api::schema::RelayedFleetPeer>,
    via: &str,
) {
    merge_relayed_rows(cache, entries, via, false);
}

/// The same merge for rows a hub pushed DOWN (#410): marked display-only, and
/// their ProxyJump forced to the hub, so nothing about where they would be
/// dialled comes from the row itself.
pub(crate) fn merge_hub_pushed_fleet(
    cache: &mut std::collections::HashMap<String, RelayedEntry>,
    entries: Vec<crate::api::schema::RelayedFleetPeer>,
    hub: &str,
) {
    merge_relayed_rows(cache, entries, hub, true);
}

fn merge_relayed_rows(
    cache: &mut std::collections::HashMap<String, RelayedEntry>,
    entries: Vec<crate::api::schema::RelayedFleetPeer>,
    via: &str,
    hub_pushed: bool,
) {
    let self_host = crate::app::short_host_name();
    let self_host_lower = self_host.to_ascii_lowercase();
    for entry in entries {
        if entry.origin.eq_ignore_ascii_case(&self_host) {
            continue;
        }
        let host_key = wire_row_identity(&entry);
        // Identity is keyed before the row's names are cleaned, so a host
        // carrying control bytes (`hopper\x07`) would slip past every exact
        // comparison and then render as a second `hopper`. No real host has
        // one: drop the row.
        if host_key.chars().any(char::is_control) {
            continue;
        }
        if host_key == self_host_lower {
            // Never store an entry about ourselves as a relayed row — the self
            // row lives on the origin_summary path.
            continue;
        }
        let Some(mut materialised) = relayed_entry_from_wire(entry) else {
            continue;
        };
        // #410: remember which edge told us, so a message for this row's
        // agents has a route instead of a refusal.
        materialised.via = Some(via.to_string());
        if hub_pushed {
            materialised.hub_pushed = true;
            materialised.peer.proxy_jump = Some(via.to_string());
        }
        let challenger_age = materialised.peer.carried_age_secs();
        let insert = match cache.get(&host_key) {
            Some(existing) => match (existing.peer.carried_age_secs(), challenger_age) {
                (Some(cur), Some(new)) => new <= cur,
                (None, Some(_)) => true,
                (Some(_), None) => false,
                (None, None) => true,
            },
            None => true,
        };
        if insert {
            cache.insert(host_key, materialised);
        }
    }
}

/// A dial error as it leaves this machine (#428): the classified token, never
/// ssh's stderr line. That line names hosts, ports and paths on this side, and
/// every spoke and next server would otherwise receive it verbatim. The words
/// stay in this host's own log, where the dial failure is recorded.
pub fn wire_error(detail: &str) -> String {
    SshFailureReason::classify(detail).as_str().to_string()
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
        error: peer.error.as_deref().map(wire_error),
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
        dial: Default::default(),
        stream_error: None,
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
    /// Why this peer has no held relay stream, when it has none because
    /// establishing one failed (#418). Independent of `result`: a peer whose
    /// stream cannot be held still answers the one-shot fallback, and that is
    /// exactly the degradation that used to be silent.
    pub stream_error: Option<String>,
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
            stream_error: None,
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
        stream_error: crate::peer_stream::establish_failure(peer),
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
    if let Some(pushed) = crate::peer_stream::take_pushed_summary(peer) {
        crate::logging::peer_push_consumed(&peer.name);
        return Ok(pushed);
    }
    crate::peer_stream::request(peer, "peers.summary", serde_json::json!({}))
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
) -> Result<(), PeerMessageFailure> {
    // A command this host refuses to build never leaves the machine, so it is
    // a refusal too — and a terminal one. Retrying an id that cannot be
    // shell-quoted safely produces the same answer forever.
    let attempt = |intent| {
        let remote = peer_message_command(
            to_agent,
            from_agent,
            from_host,
            body,
            correlation_id,
            in_reply_to,
            intent,
        )
        .map_err(PeerMessageFailure::Refused)?;
        run_peer_ssh_status(peer, &remote)
            .map(|_| ())
            .map_err(classify_message_failure)
    };
    match attempt(intent) {
        // ADR-0018 §1, the other direction of skew. A peer that predates
        // `blocking` but has #380 refuses the tier by name; asking again as
        // `needs_reply` keeps the message heard — it still nudges — and only
        // loses the escalation that peer could not have performed anyway.
        Err(PeerMessageFailure::Refused(detail))
            if intent == crate::api::schema::MsgIntent::Blocking && refused_the_intent(&detail) =>
        {
            attempt(crate::api::schema::MsgIntent::NeedsReply)
        }
        outcome => outcome,
    }
}

/// Whether a peer's refusal was about the intent VALUE — the one refusal a
/// retry at a lower tier can answer. Matched on `unknown --intent`, the
/// refusal every build since #280 prints for a tier it does not know; the bare
/// flag name is not enough, because the unknown-option refusal lists every
/// flag a build understands, `--intent` among them.
fn refused_the_intent(detail: &str) -> bool {
    detail.contains("unknown --intent")
}

/// Why a relayed message did not land on the peer that owns the recipient.
///
/// The split is #380's point. A message the far side never saw and one it read
/// and rejected want different answers from the caller, and before this both
/// arrived as "could not reach" — which is a lie about the second, and the
/// wrong advice: an unreachable peer is worth retrying and a refused flag
/// never will be. A refusal carries the remote CLI's own words, so a flag a
/// peer's build does not understand comes back as data instead of being glued
/// to the front of the message body. Same posture as [`crate::spawn::SpawnRefusal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerMessageFailure {
    /// The hop itself failed: ssh transport, auth, timeout, or a peer with no
    /// `flk` on its PATH. Retry can succeed.
    Unreachable(String),
    /// The peer's `flk msg send` ran and rejected the command — or this host
    /// refused to build one. Terminal: the identical relay is refused again.
    Refused(String),
}

impl PeerMessageFailure {
    /// The stable `error.code`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unreachable(_) => "peer_unreachable",
            Self::Refused(_) => "peer_refused_message",
        }
    }

    /// Whether retrying the identical relay could ever succeed.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Unreachable(_))
    }

    /// The far side's own words, unedited.
    pub fn detail(&self) -> &str {
        match self {
            Self::Unreachable(detail) | Self::Refused(detail) => detail,
        }
    }

    /// The caller-facing message, naming the hop as a whole — which machine
    /// failed to reach which, and how (#410). Once a message can cross a hub,
    /// "could not reach node-b" no longer says enough: the reader needs to know it
    /// was the HUB that could not, so it is not chasing the spoke's own network.
    pub fn hop_message(&self, from: &str, host: &str, reason: SshFailureReason) -> String {
        match self {
            Self::Unreachable(detail) => {
                format!(
                    "{from} cannot reach {host} ({}): {detail}",
                    reason.describe()
                )
            }
            Self::Refused(detail) => format!("{host} refused the relay from {from}: {detail}"),
        }
    }
}

/// Why an ssh dial failed, read from ssh's own last stderr line (#410 P1).
///
/// ssh has no machine-readable failure status — every transport failure is
/// exit 255 — so the reason has to come from its words. Coarse on purpose:
/// the operator question is "which kind of broken", and each kind has a
/// different fix (start sshd, fix the key, accept the host key, wait).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshFailureReason {
    ConnectRefused,
    AuthRefused,
    /// Auth failed because the ssh-agent socket this server dials with
    /// refuses connections (#418). Distinct from `AuthRefused` because the
    /// fix is on THIS machine — the far side never saw a usable key.
    AgentUnreachable,
    HostKey,
    Timeout,
    /// The `ProxyJump` host answered and the hop BEYOND it did not. Observed
    /// as `Connection closed by UNKNOWN port 65535` (#406): the first hop
    /// worked, which is exactly what a bare "unreachable" hides.
    JumpHopRefused,
    UnknownHost,
    /// The far side has no `flk` on its PATH.
    NoFlk,
    Other,
}

impl SshFailureReason {
    pub fn classify(detail: &str) -> Self {
        // A row from another host carries the token, not the words (#428).
        if let Some(reason) = Self::from_token(detail.trim()) {
            return reason;
        }
        let lowered = detail.to_ascii_lowercase();
        if lowered.contains("unknown port 65535")
            || lowered.contains("stdio forwarding failed")
            || lowered.contains("channel 0: open failed")
        {
            Self::JumpHopRefused
        } else if lowered.contains("ssh agent unreachable")
            || lowered.contains("error connecting to agent")
            || lowered.contains("communication with agent failed")
            || lowered.contains("agent refused operation")
        {
            Self::AgentUnreachable
        } else if lowered.contains("permission denied")
            || lowered.contains("authentication")
            || lowered.contains("too many authentication failures")
        {
            Self::AuthRefused
        } else if lowered.contains("host key verification failed")
            || lowered.contains("remote host identification has changed")
            || lowered.contains("no matching host key")
        {
            Self::HostKey
        } else if lowered.contains("connection refused") {
            Self::ConnectRefused
        } else if lowered.contains("timed out") || lowered.contains("timeout") {
            Self::Timeout
        } else if lowered.contains("could not resolve hostname")
            || lowered.contains("name or service not known")
            || lowered.contains("nodename nor servname")
        {
            Self::UnknownHost
        } else if lowered.contains("flk: not found") || lowered.contains("flk: command not found") {
            Self::NoFlk
        } else {
            Self::Other
        }
    }

    /// Stable wire token, for `error.data.reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConnectRefused => "connect_refused",
            Self::AuthRefused => "auth_refused",
            Self::AgentUnreachable => "agent_unreachable",
            Self::HostKey => "host_key",
            Self::Timeout => "timeout",
            Self::JumpHopRefused => "jump_hop_refused",
            Self::UnknownHost => "unknown_host",
            Self::NoFlk => "no_flk",
            Self::Other => "other",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        [
            Self::ConnectRefused,
            Self::AuthRefused,
            Self::AgentUnreachable,
            Self::HostKey,
            Self::Timeout,
            Self::JumpHopRefused,
            Self::UnknownHost,
            Self::NoFlk,
            Self::Other,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == token)
    }

    /// The reason whose [`Self::describe`] is exactly `text`, if any. Reads a
    /// classified reason back out of a notice (#420).
    pub fn from_description(text: &str) -> Option<Self> {
        [
            Self::ConnectRefused,
            Self::AuthRefused,
            Self::AgentUnreachable,
            Self::HostKey,
            Self::Timeout,
            Self::JumpHopRefused,
            Self::UnknownHost,
            Self::NoFlk,
        ]
        .into_iter()
        .find(|reason| reason.describe() == text.trim())
    }

    /// Short human phrase, for messages and the servers band.
    pub fn describe(self) -> &'static str {
        match self {
            Self::ConnectRefused => "connection refused",
            Self::AuthRefused => "auth refused",
            Self::AgentUnreachable => "ssh agent unreachable",
            Self::HostKey => "host key rejected",
            Self::Timeout => "timed out",
            Self::JumpHopRefused => "jump host reached, next hop refused",
            Self::UnknownHost => "unknown host",
            Self::NoFlk => "no flk on the far side",
            Self::Other => "ssh failed",
        }
    }
}

/// The words an operator sees for a failure `detail` (#420): the classified
/// reason when ssh's text says which kind of broken it is, otherwise the first
/// line of flock's own error. Raw ssh stderr is for the log, not the screen.
pub fn failure_text(detail: &str) -> String {
    match SshFailureReason::classify(detail) {
        SshFailureReason::Other => {
            let first = detail.lines().next().unwrap_or(detail).trim();
            if first.is_empty() {
                "connection failed".to_string()
            } else {
                first.to_string()
            }
        }
        reason => reason.describe().to_string(),
    }
}

/// A fleet transport failure the launcher hands the leg it falls back to, as
/// the attach `notice` (#63, #420). One format, written by the launcher and
/// read back by the server, so the server can put the reason on that host's
/// row and file it in the operator's notification log (ADR-0016) — the notice
/// itself stays plain text, so an older server still shows it as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetFailureNotice {
    /// A switch to `target` never established.
    SwitchFailed { target: String, reason: String },
    /// An established leg to `target` dropped and gave up reconnecting.
    ConnectionLost { target: String },
}

const SWITCH_FAILED_PREFIX: &str = "switch to ";
const SWITCH_FAILED_INFIX: &str = " failed: ";
const CONNECTION_LOST_PREFIX: &str = "lost connection to ";

impl FleetFailureNotice {
    pub fn parse(notice: &str) -> Option<Self> {
        if let Some(target) = notice.strip_prefix(CONNECTION_LOST_PREFIX) {
            let target = target.trim();
            return (!target.is_empty()).then(|| Self::ConnectionLost {
                target: target.to_string(),
            });
        }
        let rest = notice.strip_prefix(SWITCH_FAILED_PREFIX)?;
        let (target, reason) = rest.split_once(SWITCH_FAILED_INFIX)?;
        (!target.is_empty()).then(|| Self::SwitchFailed {
            target: target.to_string(),
            reason: reason.to_string(),
        })
    }

    pub fn target(&self) -> &str {
        match self {
            Self::SwitchFailed { target, .. } | Self::ConnectionLost { target } => target,
        }
    }

    /// The classified reason, when the notice names one.
    pub fn reason(&self) -> Option<SshFailureReason> {
        match self {
            Self::SwitchFailed { reason, .. } => SshFailureReason::from_description(reason),
            Self::ConnectionLost { .. } => None,
        }
    }
}

impl std::fmt::Display for FleetFailureNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SwitchFailed { target, reason } => {
                write!(
                    f,
                    "{SWITCH_FAILED_PREFIX}{target}{SWITCH_FAILED_INFIX}{reason}"
                )
            }
            Self::ConnectionLost { target } => write!(f, "{CONNECTION_LOST_PREFIX}{target}"),
        }
    }
}

/// `flk`'s usage/refusal exit code, and the one thing separating a peer that
/// refused from a peer that was never reached: ssh reports the remote
/// command's own status and keeps 255 for its own transport failures, so a 2
/// here is the far side's CLI answering rather than a network that never got
/// there.
const REMOTE_REFUSAL_EXIT: i32 = 2;

fn classify_message_failure(failure: PeerSshFailure) -> PeerMessageFailure {
    if failure.exit_code == Some(REMOTE_REFUSAL_EXIT) {
        PeerMessageFailure::Refused(failure.detail)
    } else {
        PeerMessageFailure::Unreachable(failure.detail)
    }
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
    // Appended only for `needs_reply`, the same shape `--reply-to` uses. That
    // containment shipped with #377 because `flk msg send` swallowed unknown
    // flags into the body; #380 fixed the swallowing, and this KEPT it anyway.
    // The reason is that the fix lives on the RECEIVING side: a peer only
    // refuses `--intent` once it runs a build that has #380 in it, and the
    // hosts this protects are precisely the ones that do not. Dropping the
    // containment now would trade a silent corruption for a loud one on every
    // relay to an already-deployed peer, including the `fyi` majority that
    // carries no new signal at all. It costs one match arm and buys the whole
    // roll-forward window, so it stays until the fleet has crossed #380.
    let intent_flag = match intent {
        crate::api::schema::MsgIntent::Fyi => String::new(),
        stamped => format!(" --intent {}", stamped.as_wire()),
    };
    // Quote ONCE, at the outside. The body is caller-supplied and cannot be
    // validated like the ids, so it must never reach the remote shell as
    // syntax — but quoting it *inside* an already single-quoted `sh -lc '...'`
    // closes the outer quote and the shell then word-splits the message. A
    // live cross-host send arrived as "cross-machine" instead of
    // "cross-machine hello from atlas" for exactly that reason.
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
    run_peer_ssh_status(peer, remote_command).map_err(|failure| failure.detail)
}

/// The ssh options on every dial flock makes to a peer, one-shot or held.
///
/// `BatchMode` refuses to prompt, `ConnectTimeout` bounds the dial, and the
/// ServerAlive pair lets ssh itself notice a dead link and exit — which is how
/// a held stream learns it died (see `peer_stream`).
///
/// `ControlMaster=no` + `ControlPath=none` (#418) make each dial its own
/// connection, whatever the operator's ssh config says. With a global
/// `ControlMaster auto`, flock's dials silently rode any mux an interactive
/// session had left behind: that is what hid hopper's dead agent for five days
/// (dials "worked" exactly while some shell's mux was alive), and a STALE mux
/// hangs a dial instead of failing it, which ADR-0009's transport notes
/// already rejected ControlMaster for. flock's fast path is its own held
/// stream; the one-shot fallback is the rare path and can afford an honest
/// handshake.
///
/// Scope: these reach only the ssh flock starts. A `ProxyJump` in the
/// operator's ssh config runs its own child ssh for the jump hop, which reads
/// the config afresh and does NOT inherit `ControlMaster=no` — that hop can
/// still ride a mux.
pub(crate) const PEER_DIAL_SSH_OPTIONS: [&str; 13] = [
    "-C",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=5",
    "-o",
    "ServerAliveInterval=5",
    "-o",
    "ServerAliveCountMax=2",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
];

/// Hand a peer dial the agent socket that is live NOW, not the one the server
/// inherited at launch (#418). Returns what was resolved, so a failure can be
/// attributed to a dead agent rather than to the far side.
pub(crate) fn apply_dial_agent(
    command: &mut crate::process::TracedCommand,
) -> crate::platform::ssh_agent::AgentSocket {
    let agent = crate::platform::ssh_agent::agent_for_dial();
    if let Some(path) = agent.path() {
        command.env("SSH_AUTH_SOCK", path);
    }
    agent
}

/// Rewrite an auth failure as the agent failure it really is.
///
/// ssh cannot tell us: an agent it fails to reach is a debug-level message,
/// so a passphrase-protected key under `BatchMode` just reads as
/// `Permission denied (publickey)` — "the peer refused my key", which sends
/// the operator to the wrong machine. flock knows the socket refused, so it
/// says so, and the classifier then reads `agent_unreachable`.
pub(crate) fn attribute_dial_failure(
    detail: String,
    agent: &crate::platform::ssh_agent::AgentSocket,
) -> String {
    match agent {
        crate::platform::ssh_agent::AgentSocket::Dead(path)
            if SshFailureReason::classify(&detail) == SshFailureReason::AuthRefused =>
        {
            // The path is NOT in this text: `error` is relayed to other hosts,
            // and this machine's socket path is none of their business. It is
            // logged locally, once, as `ssh.agent.unreachable`.
            let _ = path;
            format!("ssh agent unreachable (SSH_AUTH_SOCK refuses connections): {detail}")
        }
        _ => detail,
    }
}

/// How often a dial runs, which decides how its failures are logged.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DialCadence {
    /// A user- or message-driven dial: every failure is its own event.
    OneShot,
    /// The summary poll, every few seconds for as long as the server runs:
    /// `process.exec` reports it per peer on the edge only (#318, #418), and
    /// the peer-level `peer.dial.*` events carry the ongoing story.
    Poll,
}

/// A failed peer command, with the remote exit status kept.
///
/// `run_peer_ssh` flattens this to its message, which is all its callers ever
/// wanted. The relay is the exception: it is the one caller for which "the far
/// side answered, and said no" is a different outcome from "the far side never
/// answered", and the status code is the only place that distinction survives.
struct PeerSshFailure {
    /// The remote command's exit status, or `None` when it was killed by a
    /// signal before producing one.
    exit_code: Option<i32>,
    detail: String,
}

fn run_peer_ssh_status(peer: &PeerConfig, remote_command: &str) -> Result<String, PeerSshFailure> {
    run_peer_ssh_with(peer, remote_command, DialCadence::OneShot)
}

fn run_peer_ssh_with(
    peer: &PeerConfig,
    remote_command: &str,
    cadence: DialCadence,
) -> Result<String, PeerSshFailure> {
    let mut command = crate::process::TracedCommand::new("ssh", "peers");
    command
        .args(PEER_DIAL_SSH_OPTIONS)
        .args([peer.ssh_target(), remote_command])
        .stdin(std::process::Stdio::null());
    if cadence == DialCadence::Poll {
        command.periodic().edge_scope(peer.name.clone());
    }
    let agent = apply_dial_agent(&mut command);
    let output = command
        .output_traced_with_timeout(PEER_SSH_TIMEOUT)
        .map_err(|err| PeerSshFailure {
            exit_code: None,
            // A dial that hung past the deadline was killed, not refused to
            // start: saying "spawn failed" would send the operator after the
            // local ssh binary instead of the link (#418).
            detail: if err.kind() == std::io::ErrorKind::TimedOut {
                format!("ssh {err}")
            } else {
                format!("ssh spawn failed: {err}")
            },
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            output.status.to_string()
        } else {
            // Keep the tail: ssh banners/motd come first, the error last.
            stderr.lines().next_back().unwrap_or(stderr).to_string()
        };
        return Err(PeerSshFailure {
            exit_code: output.status.code(),
            detail: attribute_dial_failure(detail, &agent),
        });
    }
    String::from_utf8(output.stdout).map_err(|_| PeerSshFailure {
        exit_code: output.status.code(),
        detail: "non-utf8 ssh output".to_string(),
    })
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

    /// A gossiped row with the destination and jump under test, everything
    /// else inert. `origin` is the hub that relayed it — the name a drop must
    /// carry, so an operator can tell which member of the fleet sent the row.
    fn wire_peer(
        ssh_target: &str,
        proxy_jump: Option<&str>,
    ) -> crate::api::schema::RelayedFleetPeer {
        crate::api::schema::RelayedFleetPeer {
            dial: None,
            name: "spoke2.invalid".into(),
            ssh_target: ssh_target.into(),
            host: Some("spoke2.invalid".into()),
            version: None,
            protocol: None,
            system: None,
            latency_ms: None,
            workspaces: Vec::new(),
            age_secs: Some(3),
            error: None,
            origin: "kiln".into(),
            origin_last_ok_secs: Some(3),
            proxy_jump: proxy_jump.map(Into::into),
            icon: None,
        }
    }

    #[test]
    fn a_fleet_failure_notice_reads_back_what_the_launcher_wrote() {
        let failed = FleetFailureNotice::SwitchFailed {
            target: "operator@atlas".to_string(),
            reason: SshFailureReason::HostKey.describe().to_string(),
        };
        let text = failed.to_string();
        assert_eq!(text, "switch to operator@atlas failed: host key rejected");
        let parsed = FleetFailureNotice::parse(&text).expect("parses");
        assert_eq!(parsed, failed);
        assert_eq!(parsed.reason(), Some(SshFailureReason::HostKey));

        let lost = FleetFailureNotice::ConnectionLost {
            target: "atlas".to_string(),
        };
        assert_eq!(FleetFailureNotice::parse(&lost.to_string()), Some(lost));
        // A reason that is flock's own words still files, with no row reason.
        let own = FleetFailureNotice::parse(
            "switch to atlas failed: matching remote flock not installed",
        )
        .expect("parses");
        assert_eq!(own.reason(), None);
        assert_eq!(FleetFailureNotice::parse("already home"), None);
    }

    #[test]
    fn failure_text_says_the_kind_of_broken_not_ssh_stderr() {
        assert_eq!(
            failure_text("ssh: connect to host atlas port 22: Connection refused"),
            "connection refused"
        );
        assert_eq!(
            failure_text("operator@atlas: Permission denied (publickey)."),
            "auth refused"
        );
        assert_eq!(
            failure_text("matching remote flock 0.6.8 is not installed\nrun it interactively"),
            "matching remote flock 0.6.8 is not installed"
        );
        assert_eq!(failure_text("  "), "connection failed");
    }

    #[test]
    fn a_gossiped_destination_that_would_become_an_ssh_option_is_dropped_and_logged() {
        // #392: `ssh_target` arrives verbatim from another host and lands in
        // THIS machine's ssh argv. A leading `-` makes it an option, and
        // `-oProxyCommand=` is local command execution. The wire path is the
        // one an attacker influences, so validating config alone is not a fix.
        let logs = crate::logging::capture_logs(|| {
            assert!(
                super::relayed_entry_from_wire(wire_peer("-oProxyCommand=id", None)).is_none(),
                "a destination starting with `-` must never reach an ssh argv"
            );
        });
        assert!(
            logs.contains("kiln"),
            "the drop must name the sending peer: {logs}"
        );
        assert!(
            logs.contains("ssh_target"),
            "the drop must name the offending field: {logs}"
        );

        // A space is not injection here (no shell is involved) but it is still
        // not a destination, and ssh would read the tail as another argument.
        assert!(
            super::relayed_entry_from_wire(wire_peer("operator@kiln -oProxyCommand=id", None))
                .is_none()
        );
    }

    #[test]
    fn a_gossiped_proxy_jump_carrying_an_option_is_dropped_and_logged() {
        // The second wire-controlled value: `proxy_jump` is interpolated into
        // `-o ProxyJump=<value>`, so it needs the same rule.
        let logs = crate::logging::capture_logs(|| {
            assert!(super::relayed_entry_from_wire(wire_peer(
                "operator@spoke2.invalid",
                Some("-oProxyCommand=id")
            ))
            .is_none());
        });
        assert!(
            logs.contains("proxy_jump") && logs.contains("kiln"),
            "the drop must name the field and the sender: {logs}"
        );

        // ProxyJump is a comma-separated CHAIN and ssh dials every hop, so one
        // bad hop rejects the chain even when the first one looks fine.
        assert!(
            super::relayed_entry_from_wire(wire_peer(
                "operator@spoke2.invalid",
                Some("hub,-oProxyCommand=id")
            ))
            .is_none(),
            "every hop is a destination and must be validated as one"
        );
    }

    #[test]
    fn a_gossiped_ipv6_or_ported_destination_still_dials() {
        // Pitfall 2 of #392: a rule written from the happy case would strand
        // working fleets. Bracketed IPv6 literals, `:port` suffixes and
        // multi-hop chains are all legitimate.
        for target in [
            "operator@spoke2.invalid",
            "[::1]",
            "operator@[fe80::1]:2222",
            "spoke2.invalid:22",
        ] {
            let entry = super::relayed_entry_from_wire(wire_peer(target, Some("hub,kiln")))
                .unwrap_or_else(|| panic!("{target} is a legitimate destination"));
            assert_eq!(entry.peer.ssh_target, target);
        }
    }

    #[test]
    fn one_bad_gossiped_row_does_not_cost_the_fleet_its_other_rows() {
        // Pitfall 3: rejection is per-ROW. The merge loop keeps iterating, so a
        // hostile or typo'd entry drops itself and nothing else.
        let wire = vec![
            wire_peer("-oProxyCommand=id", None),
            wire_peer("operator@spoke2.invalid", None),
        ];
        let kept: Vec<String> = wire
            .into_iter()
            .filter_map(super::relayed_entry_from_wire)
            .map(|entry| entry.peer.ssh_target)
            .collect();
        assert_eq!(kept, vec!["operator@spoke2.invalid".to_string()]);
    }

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
            "agent_atlas_1",
            "agent_hopper_2",
            "hopper",
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
            "agent_atlas_1",
            "agent_hopper_2",
            "hopper",
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
            "agent_atlas_1",
            "agent_hopper_2",
            "hopper",
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
            "agent_atlas_1",
            "agent_hopper_2",
            "hopper",
            "landed the fix",
            "c-notice",
            None,
            crate::api::schema::MsgIntent::Fyi,
        )
        .expect("valid ids");
        assert!(!command.contains("--intent"), "{command}");
    }

    #[test]
    fn a_blocking_relay_carries_its_tier_and_a_peer_refusing_it_is_recognised() {
        // ADR-0018 §1: the tier rides the envelope across the hop, so the
        // escalation happens on the recipient's own server.
        let command = super::peer_message_command(
            "agent_atlas_1",
            "agent_hopper_2",
            "hopper",
            "I cannot merge until you rebase",
            "c-blocking",
            None,
            crate::api::schema::MsgIntent::Blocking,
        )
        .expect("valid ids");
        assert!(command.contains("--intent blocking"), "{command}");

        // What a peer that has #380 but predates the tier prints: the refusal
        // the relay retries at `needs_reply` rather than surfacing as a
        // failure. Any other refusal is not the intent's fault.
        assert!(super::refused_the_intent(
            "unknown --intent \"blocking\": expected fyi or needs-reply"
        ));
        // The unknown-option refusal names `--intent` in its list of what
        // the build understands; that must not trigger a second ssh hop.
        assert!(!super::refused_the_intent(
            "flk msg send: unknown option \"--from-host\" — this build understands --repo \
             --intent --correlation-id --reply-to --agent --from-agent --json, and `--` ends \
             flag parsing so a body may begin with dashes"
        ));
    }

    /// #418 with a REAL ssh: a dial that fails comes back classified from
    /// ssh's own words, and those words reach the `process.exec` record as
    /// its stderr tail. `.invalid` is reserved (RFC 2606) and never resolves,
    /// so this asserts about ssh and flock, not about any fleet host.
    #[test]
    fn a_real_failed_dial_is_classified_and_its_stderr_reaches_the_log() {
        let peer = PeerConfig {
            name: "flk418".into(),
            ssh: "nobody@flk-418-no-such-host.invalid".into(),
            ..Default::default()
        };
        let mut outcome = None;
        let logs = crate::logging::capture_logs(|| {
            outcome = Some(run_peer_ssh_status(&peer, "true"));
        });
        let failure = match outcome {
            Some(Err(failure)) => failure,
            Some(Ok(_)) => panic!("a dial to an .invalid host succeeded"),
            None => panic!("the dial never ran"),
        };
        if failure.detail.starts_with("ssh spawn failed") {
            // No ssh client on this runner: nothing real to observe.
            return;
        }
        assert_eq!(
            SshFailureReason::classify(&failure.detail),
            SshFailureReason::UnknownHost,
            "{}",
            failure.detail
        );
        assert_eq!(failure.exit_code, Some(255), "ssh's own failure status");
        assert!(logs.contains("stderr_tail="), "{logs}");
        assert!(
            logs.to_ascii_lowercase()
                .contains("could not resolve hostname"),
            "ssh's reason is in the log, not only its exit status: {logs}"
        );
        // flock's own dials never ride, or leave behind, a shared mux.
        assert!(logs.contains("ControlMaster=no") && logs.contains("ControlPath=none"));
        assert!(
            logs.contains("-C"),
            "peer dial must enable compression: {logs}"
        );
    }

    /// ssh cannot say its agent is gone — that is a debug-level message — so
    /// a passphrase key under BatchMode reads "Permission denied". flock knows
    /// the socket refused and says so; only for an AUTH failure, though: a
    /// refused TCP connection is not the agent's fault.
    #[test]
    fn an_auth_failure_through_a_dead_agent_reads_agent_unreachable() {
        use crate::platform::ssh_agent::AgentSocket;
        let socket = std::env::temp_dir().join("flk418-dead-agent.sock");
        let dead = AgentSocket::Dead(socket.clone());
        let auth = "operator@atlas: Permission denied (publickey).".to_string();

        let blamed = attribute_dial_failure(auth.clone(), &dead);
        assert_eq!(
            SshFailureReason::classify(&blamed),
            SshFailureReason::AgentUnreachable,
            "{blamed}"
        );
        assert!(
            blamed.contains("Permission denied"),
            "ssh's words kept: {blamed}"
        );
        assert!(
            !blamed.contains(&socket.display().to_string()),
            "the local socket path must not ride the relayed error: {blamed}"
        );

        let refused = "ssh: connect to host atlas port 22: Connection refused".to_string();
        assert_eq!(
            SshFailureReason::classify(&attribute_dial_failure(refused, &dead)),
            SshFailureReason::ConnectRefused
        );
        assert_eq!(
            SshFailureReason::classify(&attribute_dial_failure(auth, &AgentSocket::Live(socket))),
            SshFailureReason::AuthRefused,
            "a live agent's auth failure is the far side's refusal"
        );
        // ssh-add's and a verbose ssh's own words for the same state.
        assert_eq!(
            SshFailureReason::classify("Error connecting to agent: Connection refused"),
            SshFailureReason::AgentUnreachable
        );
    }

    /// #418 rate limit: one WARN at onset, silence while unchanged, one
    /// summary per window, an edge on a change of reason, INFO on recovery.
    #[test]
    fn a_persistent_failure_warns_on_edges_and_summarises_per_window() {
        use std::time::Duration;
        let t0 = Instant::now();
        let every = Duration::from_secs(600);
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut health = PeerDialHealth::default();

        assert_eq!(
            health.record_failure(SshFailureReason::AuthRefused, t0, every),
            DialLog::Failed
        );
        assert_eq!(health.persistent_reason(), None, "one failure is a blip");
        assert_eq!(
            health.record_failure(SshFailureReason::AuthRefused, at(15), every),
            DialLog::StillFailing { summary: false }
        );
        assert_eq!(
            health.persistent_reason(),
            Some(SshFailureReason::AuthRefused),
            "past one poll, it is a state"
        );
        for k in 2..40 {
            assert_eq!(
                health.record_failure(SshFailureReason::AuthRefused, at(15 * k), every),
                DialLog::StillFailing { summary: false },
                "poll {k} is inside the window"
            );
        }
        assert_eq!(
            health.record_failure(SshFailureReason::AuthRefused, at(600), every),
            DialLog::StillFailing { summary: true }
        );
        assert_eq!(
            health.record_failure(SshFailureReason::AuthRefused, at(615), every),
            DialLog::StillFailing { summary: false }
        );
        assert_eq!(
            health.record_failure(SshFailureReason::AgentUnreachable, at(630), every),
            DialLog::Failed,
            "a different kind of broken is news"
        );
        let failures = health.consecutive_failures;
        assert_eq!(
            health.record_success(at(645)),
            DialLog::Recovered {
                failures,
                failing_secs: 645
            }
        );
        assert_eq!(health.record_success(at(660)), DialLog::Quiet);
        assert_eq!(health.persistent_reason(), None);
    }

    /// The budget the rate limit buys, in one node's own numbers: five days of a
    /// 15s poll failing the same way.
    #[test]
    fn a_five_day_outage_is_hundreds_of_warns_not_tens_of_thousands() {
        use std::time::Duration;
        let t0 = Instant::now();
        let every = Duration::from_secs(PEER_DIAL_FAILURE_SUMMARY_SECS);
        let mut health = PeerDialHealth::default();
        let polls = 5 * 24 * 3600 / 15;
        let warns = (0..polls)
            .map(|k| {
                health.record_failure(
                    SshFailureReason::AgentUnreachable,
                    t0 + Duration::from_secs(15 * k),
                    every,
                )
            })
            .filter(|log| {
                matches!(
                    log,
                    DialLog::Failed | DialLog::StillFailing { summary: true }
                )
            })
            .count();
        assert_eq!(polls, 28_800);
        assert!(warns <= 721, "{warns} WARNs for one unchanged outage");
        assert!(warns >= 700, "the outage must still be restated: {warns}");
    }

    #[test]
    fn a_reason_token_from_another_host_classifies_back_to_itself() {
        // #428: a peer now sends the token as `error`, and the receiver still
        // has to read the reason out of it.
        use super::SshFailureReason as R;
        for reason in [
            R::ConnectRefused,
            R::AuthRefused,
            R::AgentUnreachable,
            R::HostKey,
            R::Timeout,
            R::JumpHopRefused,
            R::UnknownHost,
            R::NoFlk,
            R::Other,
        ] {
            assert_eq!(R::classify(reason.as_str()), reason);
            assert_eq!(super::wire_error(reason.as_str()), reason.as_str());
        }
        assert_eq!(
            super::wire_error("ssh: Could not resolve hostname ws00860001: nodename nor servname"),
            "unknown_host"
        );
    }

    #[test]
    fn an_old_peers_free_text_error_is_passed_on_as_a_token() {
        // #428 review: a v(N-1) peer still relays ssh's words as `error`. This
        // hub reads the reason out of them, and passes on only the token.
        let mut row = wire_peer("ws00860001", Some("kiln"));
        row.error = Some("node-b.invalid: Permission denied (publickey).".into());
        let entry = super::relayed_entry_from_wire(row).expect("valid row");
        assert_eq!(
            entry.peer.shown_failure_reason(),
            Some(super::SshFailureReason::AuthRefused)
        );
        let onward = super::peer_to_wire(&entry.peer);
        assert_eq!(onward.error.as_deref(), Some("auth_refused"));
    }

    #[test]
    fn an_ssh_failure_names_which_kind_of_broken() {
        // #410 P1: "unreachable" hides the fix. Each of these has a different
        // one, and the #406 case — a ProxyJump whose SECOND hop was refused —
        // must not read as the first hop being down.
        use super::SshFailureReason as R;
        for (stderr, expected) in [
            (
                "ssh: connect to host node-b port 22: Connection refused",
                R::ConnectRefused,
            ),
            (
                "operator@node-b: Permission denied (publickey).",
                R::AuthRefused,
            ),
            ("Host key verification failed.", R::HostKey),
            (
                "ssh: connect to host node-b port 22: Operation timed out",
                R::Timeout,
            ),
            ("Connection closed by UNKNOWN port 65535", R::JumpHopRefused),
            (
                "ssh: Could not resolve hostname node-b: nodename nor servname provided",
                R::UnknownHost,
            ),
            ("sh: 1: flk: not found", R::NoFlk),
            ("something else entirely", R::Other),
        ] {
            assert_eq!(R::classify(stderr), expected, "{stderr}");
        }
    }

    #[test]
    fn a_hop_failure_names_both_ends_and_the_reason() {
        let failure = super::PeerMessageFailure::Unreachable(
            "ssh: connect to host node-b port 22: Connection refused".into(),
        );
        let reason = super::SshFailureReason::classify(failure.detail());
        let message = failure.hop_message("hopper", "node-b", reason);
        assert!(
            message.starts_with("hopper cannot reach node-b (connection refused)"),
            "{message}"
        );
    }

    #[test]
    fn a_peer_that_refuses_is_not_a_peer_that_was_never_reached() {
        // #380. Both used to arrive as "could not reach {host}", which is a
        // lie about the second and the wrong advice about both: an unreachable
        // peer is worth retrying and a rejected flag never will be.
        //
        // ssh reports the remote command's own exit status and keeps 255 for
        // its own transport failures, so the status is the whole distinction.
        let refused = super::classify_message_failure(super::PeerSshFailure {
            exit_code: Some(super::REMOTE_REFUSAL_EXIT),
            detail: "flk msg send: unknown option \"--intent\" — this build understands …"
                .to_string(),
        });
        assert_eq!(refused.code(), "peer_refused_message");
        assert!(!refused.retryable(), "the identical relay is refused again");
        assert!(
            refused
                .hop_message("hopper", "atlas", super::SshFailureReason::Other)
                .contains("--intent"),
            "the peer's own words ARE the diagnosis and must survive the hop: {}",
            refused.hop_message("hopper", "atlas", super::SshFailureReason::Other)
        );

        for (exit_code, what) in [
            (Some(255), "ssh itself failed"),
            (None, "killed by a signal"),
        ] {
            let failure = super::classify_message_failure(super::PeerSshFailure {
                exit_code,
                detail: "Connection timed out".to_string(),
            });
            assert_eq!(failure.code(), "peer_unreachable", "{what}");
            assert!(failure.retryable(), "{what}");
        }
    }

    #[test]
    fn a_command_this_host_will_not_build_is_a_refusal_too() {
        // An id that cannot be shell-quoted safely never leaves the machine,
        // so it is terminal for the same reason a rejected flag is — and used
        // to be reported as the host being unreachable, which it plainly was
        // not.
        // Never dials: the guard rejects shell-unsafe ids before ssh is spawned,
        // which is what lets this drive the real entry point rather than
        // hand-building the variant it is supposed to produce.
        let peer = PeerConfig {
            name: "atlas".into(),
            ..Default::default()
        };
        let failure = super::send_peer_message(
            &peer,
            "agent_atlas_1",
            "agent_hopper_2",
            "hopper",
            "pong",
            "c-reply",
            Some("c'; rm -rf /"),
            crate::api::schema::MsgIntent::Fyi,
        )
        .expect_err("a shell-escaping id must be refused");
        assert_eq!(failure.code(), "peer_refused_message");
        assert!(!failure.retryable());
        assert!(
            failure.detail().contains("in-reply-to id"),
            "the caller learns which id was rejected: {}",
            failure.detail()
        );
    }

    #[test]
    fn a_threading_id_is_guarded_like_every_other_id() {
        // Every id in this command is interpolated into a remote shell, so the
        // new one is guarded by the same rule as the rest rather than trusted
        // for being server-minted.
        let err = super::peer_message_command(
            "agent_atlas_1",
            "agent_hopper_2",
            "hopper",
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
            dial: Default::default(),
            stream_error: None,
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
        peers.push(mk("hopper")); // a hub that lists itself in [[peers]]
        let snapshot = FleetSnapshotState {
            origin: "hopper".into(),
            peers,
            origin_summary: None,
            received_at: Instant::now(),
        };

        let wire = snapshot.to_wire("p0");

        assert!(
            wire.peers.iter().all(|p| p.name != "hopper"),
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
            dial: Default::default(),
            stream_error: None,
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
        let state = summary_state("kiln", "operator@kiln", Some(5));
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
            "atlas",
            "operator@atlas",
            Some(PEER_STALE_AFTER_SECS + 30),
        )));
        assert_eq!(stale.reachability(), PeerReachability::Down);

        // Never-reached peers stay never-reached.
        let never = peer_from_wire(peer_to_wire(&summary_state(
            "node-b",
            "operator@node-b",
            None,
        )));
        assert!(never.last_ok.is_none());
    }

    #[test]
    fn fleet_peer_wire_carries_icon_both_directions() {
        // #164: the self-declared icon survives the bincode roundtrip present...
        let mut state = summary_state("kiln", "operator@kiln", Some(5));
        state.icon = Some("toad".to_string());
        assert_eq!(
            peer_from_wire(peer_to_wire(&state)).icon.as_deref(),
            Some("toad")
        );

        // ...and absent (a v(N-1) peer never sets it) decodes to None.
        let mut none = summary_state("atlas", "operator@atlas", Some(5));
        none.icon = None;
        assert_eq!(peer_from_wire(peer_to_wire(&none)).icon, None);
    }

    #[test]
    fn fleet_system_wire_carries_gpu_and_thermal_both_directions() {
        use crate::api::schema::{ThermalComponent, ThermalReport};

        // #291: GPU utilization and the self-declared thermal report survive
        // the bincode roundtrip. GPU is the older half of this: it was sampled
        // locally for a long time with nowhere on the wire to go.
        let mut state = summary_state("kiln", "operator@kiln", Some(5));
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
        let none = summary_state("atlas", "operator@atlas", Some(5));
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
        let mut state = summary_state("kiln", "operator@kiln", Some(5));
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
        let mut wide = summary_state("atlas", "operator@atlas", Some(5));
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
            r#"{"id":"x","result":{"host":"kiln","system":{"cpu_percent":4,"gpu_percent":97,"#,
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
            r#"{"id":"x","result":{"host":"atlas","system":{"cpu_percent":4},"workspaces":[]}}"#;
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
            r#"{"id":"x","result":{"host":"hopper","workspaces":[],"relayed_fleet":[{"#,
            r#""name":"kiln","ssh_target":"operator@kiln","system":{"cpu_percent":9,"#,
            r#""thermal":{"severity":250,"component":"cpu","label":"zzzzzzzzzzzzzzzzzzzzzzzzzz"}}"#,
            r#","origin":"hopper"}]}}"#
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
            origin: "hopper".to_string(),
            peers: vec![
                summary_state("kiln", "operator@kiln", Some(3)),
                summary_state("atlas", "operator@atlas", Some(9)),
            ],
            origin_summary: None,
            received_at: Instant::now(),
        };

        let wire = snapshot.to_wire("operator@atlas");
        // Pass-through: the ORIGINAL origin survives nested leaps.
        assert_eq!(wire.origin, "hopper");
        // The hop target becomes the self row on the receiving end.
        assert_eq!(wire.peers.len(), 1);
        assert_eq!(wire.peers[0].ssh_target, "operator@kiln");
    }

    #[test]
    fn origin_summary_survives_wire_roundtrip_and_passthrough() {
        let mut origin = summary_state("hopper", crate::protocol::HOME_SWITCH_TARGET, Some(0));
        origin.workspaces[0].workspace = "flock".to_string();
        let snapshot = FleetSnapshotState {
            origin: "hopper".to_string(),
            peers: vec![summary_state("kiln", "operator@kiln", Some(3))],
            origin_summary: Some(origin),
            received_at: Instant::now(),
        };

        // Round-trip carries the hub's own workspaces home-targeted.
        let back = FleetSnapshotState::from_wire(snapshot.to_wire("operator@kiln"));
        let carried = back
            .origin_summary
            .clone()
            .expect("origin summary survives");
        assert_eq!(carried.ssh_target, crate::protocol::HOME_SWITCH_TARGET);
        assert_eq!(carried.workspaces[0].workspace, "flock");
        // A nested leap (pass-through) keeps the hub's own summary too.
        let nested = FleetSnapshotState::from_wire(back.to_wire("operator@kiln"));
        assert!(nested.origin_summary.is_some());
    }

    #[test]
    fn parse_summary_response_reads_envelope() {
        let stdout = r#"
Last login: whatever banner
{"id":"cli:peers:summary","result":{"host":"kiln","version":"0.6.8","system":{"cpu_percent":71,"mem_used":48000000000,"mem_total":64000000000,"disk_free":200000000000},"workspaces":[{"workspace":"flock","project_key":"github.com/gerchowl/flock","project_label":"flock","branch":"fix/pty","is_linked_worktree":true,"agent":"cc","status":"blocked","status_age_secs":840}]}}
"#;
        let payload = parse_summary_response(stdout, 34).unwrap();
        assert_eq!(payload.host, "kiln");
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
        let stdout = r#"{"id":"x","result":{"host":"hub","workspaces":[],"relayed_fleet":[{"name":"spoke2","ssh_target":"operator@spoke2","host":"spoke2","workspaces":[],"origin":"hub"}]}}"#;
        let payload = parse_summary_response(stdout, 4).unwrap();
        assert_eq!(payload.relayed_fleet.len(), 1);
        assert_eq!(payload.relayed_fleet[0].name, "spoke2");
        assert_eq!(payload.relayed_fleet[0].origin, "hub");
    }

    #[test]
    fn parse_summary_response_treats_missing_relayed_fleet_as_empty() {
        // Additive-with-default: a v(N-1) peer that never emits relayed_fleet
        // parses cleanly and the merged cache stays empty.
        let stdout = r#"{"id":"x","result":{"host":"atlas","workspaces":[]}}"#;
        let payload = parse_summary_response(stdout, 5).unwrap();
        assert!(payload.relayed_fleet.is_empty());
    }

    #[test]
    fn parse_summary_response_reads_icon_and_tolerates_absence() {
        // #164: the self-declared icon name parses from the JSON envelope...
        let with = r#"{"id":"x","result":{"host":"hopper","icon":"toad","workspaces":[]}}"#;
        assert_eq!(
            parse_summary_response(with, 5).unwrap().icon.as_deref(),
            Some("toad")
        );
        // ...and a v(N-1) peer that never emits it parses as None.
        let without = r#"{"id":"x","result":{"host":"atlas","workspaces":[]}}"#;
        assert_eq!(parse_summary_response(without, 5).unwrap().icon, None);
    }

    #[test]
    fn parse_summary_response_tolerates_missing_system_block() {
        let stdout = r#"{"id":"x","result":{"host":"atlas","workspaces":[]}}"#;
        let payload = parse_summary_response(stdout, 5).unwrap();
        assert_eq!(payload.host, "atlas");
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
            name: "kiln".into(),
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
{"id":"cli:peers:logs","result":{"type":"peers_logs","host":"kiln","lines":[{"ts":"2026-06-29T00:00:01Z","level":"INFO","target":"flock::app","message":"up","source":"flock-server.log"}]}}
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
            "result": { "type": "peers_logs", "host": "kiln", "lines": [original.clone()] },
        });
        let parsed = parse_logs_response(&envelope.to_string()).unwrap();
        assert_eq!(parsed, vec![original]);
    }

    #[test]
    fn reachability_reflects_latency_and_staleness() {
        let mut peer = PeerSummaryState::new(&PeerConfig {
            name: "kiln".into(),
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
            name: "kiln".into(),
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
            ssh_target: "operator@old".into(),
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
            r#"{"name":"atlas","ssh_target":"operator@atlas","workspaces":[],"origin":"kiln"}"#;
        let decoded: RelayedFleetPeer = serde_json::from_str(json_old).expect("parse old wire");
        assert_eq!(decoded.origin_last_ok_secs, None);

        // v(N) struct → JSON → v(N) struct: value preserved.
        let full = RelayedFleetPeer {
            dial: None,
            name: "atlas".into(),
            ssh_target: "operator@atlas".into(),
            host: None,
            version: None,
            protocol: None,
            system: None,
            latency_ms: None,
            workspaces: Vec::new(),
            age_secs: Some(3),
            error: None,
            origin: "kiln".into(),
            origin_last_ok_secs: Some(3),
            proxy_jump: Some("kiln".into()),
            icon: None,
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: RelayedFleetPeer = serde_json::from_str(&json).unwrap();
        assert_eq!(back, full);

        // v(N) JSON → hypothetical v(N-1) struct: unknown fields ignored is
        // serde_json's default; simulate by decoding into a value and checking
        // known fields, which is the only cross-version compat guarantee.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["name"], "atlas");
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
        let fetched = fetch_with_panic_guard("kiln", || panic!("summary parser blew up"));
        std::panic::set_hook(previous_hook);

        assert_eq!(fetched.peer, "kiln", "the completion names the right peer");
        assert!(
            fetched.result.is_err(),
            "a panicked fetch reports as a failed poll, not a success"
        );

        // And that completion is what frees the peer: the dispatcher hands it
        // to the tracker exactly like any other result.
        let mut tracker = PeerPollTracker::new();
        let now = Instant::now();
        assert!(tracker.should_poll_now(&fetched.peer, now, Duration::from_secs(15)));
        assert!(tracker.in_flight("kiln"));
        tracker.mark_finished(&fetched.peer);
        assert!(
            tracker.should_poll_now(
                "kiln",
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
            tracker.should_poll_now("kiln", now, Duration::from_secs(15)),
            "first call must dispatch"
        );
        assert!(tracker.in_flight("kiln"));
        assert!(
            !tracker.should_poll_now("kiln", now, Duration::from_secs(15)),
            "second call while in-flight must skip (overlap guard)"
        );
        tracker.mark_finished("kiln");
        assert!(
            !tracker.should_poll_now(
                "kiln",
                now + Duration::from_secs(1),
                Duration::from_secs(15)
            ),
            "not-yet-due skips even after the previous call finished"
        );
        assert!(
            tracker.should_poll_now(
                "kiln",
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
        assert!(tracker.should_poll_now("atlas", t0, Duration::from_secs(2)));

        // Two rounds later, the slow SSH is still running.
        assert!(
            !tracker.should_poll_now("atlas", t0 + Duration::from_secs(4), Duration::from_secs(2)),
            "in-flight guard MUST hold even past next_due — a hung SSH cannot pile"
        );
        // Config reload (peer still present): the in-flight lock survives.
        tracker.retain_only(vec!["atlas"]);
        assert!(
            !tracker.should_poll_now("atlas", t0 + Duration::from_secs(8), Duration::from_secs(2)),
            "reload must NOT drop the in-flight lock for a surviving peer"
        );
        // Retain that drops the peer clears its state.
        tracker.retain_only::<Vec<&str>>(vec![]);
        assert!(
            tracker.should_poll_now("atlas", t0 + Duration::from_secs(9), Duration::from_secs(2)),
            "peer dropped from config, then re-added, starts fresh"
        );
    }

    #[test]
    fn normalized_host_key_is_one_spelling_per_machine() {
        // #422: every viewer must derive the same key for the same machine.
        for raw in [
            "atlas",
            "ATLAS",
            "atlas.local",
            "operator@atlas.tail1234.ts.net",
        ] {
            assert_eq!(normalized_host_key(raw), "atlas", "{raw}");
        }
        // An address is kept whole: cutting it would fold unrelated hosts.
        assert_eq!(normalized_host_key("100.64.0.7"), "100.64.0.7");
        assert_eq!(normalized_host_key("operator@::1"), "::1");
    }
}
