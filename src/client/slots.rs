//! Connection slots (#65) — the multi-connection client core.
//!
//! A *slot* is one framed server connection the client holds open: the home
//! (local) unix socket, or a fleet peer reached over the existing ssh-stdio
//! bridge (which presents the remote server as a local forwarded socket). The
//! client owns the terminal forever; switching servers flips which slot feeds
//! the painter and receives input, instead of exiting and relaunching an
//! attach leg.
//!
//! Policy is *warm-all*: at start the client background-dials every configured
//! fleet target so a later switch to any of them is an instant in-process flip
//! (resume frames + focus) rather than an ssh dial. Home is always warm. Failed
//! dials fall back to cold with gentle backoff; the exit-and-relaunch legs in
//! `main.rs` remain as the cold-dial / ssh-bootstrap path.
//!
//! Two layers live here. [`SlotRegistry`] is the pure flip / pause / resume /
//! demote / backoff state machine plus the warm-all target derivation — no
//! I/O, so it is exercised entirely in unit tests. [`SlotManager`] wraps it
//! with the live warm/active [`SlotConnection`]s and turns registry effects
//! into `SetFrameSubscription` wire messages; the reader threads and the
//! active-stream swap live in the client event loop (`client/mod.rs`).

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::protocol::{ClientMessage, HOME_SWITCH_TARGET};

/// Base backoff applied to a cold slot after its first failed dial, before it
/// is eligible to be re-dialed. Gentle — a down server should ghost, not spin.
/// Subsequent consecutive failures escalate it via [`redial_backoff`].
const COLD_REDIAL_BACKOFF: Duration = Duration::from_secs(15);

/// Cap on the escalating redial backoff (the open-circuit cooldown ceiling). A
/// persistently unreachable peer is retried at most this often (#176).
const MAX_REDIAL_BACKOFF: Duration = Duration::from_secs(300);

/// A slot's dial target. `Home` is the local server (always warm); `Ssh`
/// names a fleet peer's ssh destination (the same string a `SwitchServer`
/// carries and the launcher would dial).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum SlotTarget {
    /// The local server — home. Reached via the local client socket.
    Home,
    /// A fleet peer reached over the ssh-stdio bridge by this ssh destination.
    Ssh(String),
}

impl SlotTarget {
    /// The switch-target string for this slot: the reserved home sentinel for
    /// home, or the ssh destination for a peer. This is exactly the key a
    /// `SwitchServer { ssh_target }` resolves against.
    pub(crate) fn key(&self) -> &str {
        match self {
            SlotTarget::Home => HOME_SWITCH_TARGET,
            SlotTarget::Ssh(target) => target.as_str(),
        }
    }

    /// Build a slot target from a switch-target string (the inverse of
    /// [`key`]): the reserved sentinel maps to home, anything else to an ssh
    /// peer.
    pub(crate) fn from_key(key: &str) -> Self {
        if key == HOME_SWITCH_TARGET {
            SlotTarget::Home
        } else {
            SlotTarget::Ssh(key.to_string())
        }
    }
}

/// Lifecycle phase of one slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotPhase {
    /// The active slot: frames painted, input forwarded. At most one.
    Active,
    /// Connection held, frames paused. An instant flip away.
    Warm,
    /// No connection. Lazy-dialed on first switch (or by the warm-all dialer).
    /// `failed_at` records the last failed dial so backoff can gate redials.
    Cold { failed_at: Option<Instant> },
}

/// One registry entry.
#[derive(Debug)]
struct Slot {
    target: SlotTarget,
    phase: SlotPhase,
    /// Consecutive dial failures / transport deaths since this slot was last
    /// healthy (warmed). Drives the exponential redial backoff — the circuit
    /// breaker that stops the 2s warm-sweep from re-dialing a flaky peer every
    /// tick and piling up multi-second ssh probes (#176). Reset on a successful
    /// warm.
    consecutive_failures: u32,
}

/// Redial backoff for a cold slot given its consecutive-failure count: the
/// breaker cooldown. Starts at [`COLD_REDIAL_BACKOFF`] and doubles per failure,
/// capped at [`MAX_REDIAL_BACKOFF`] — 15s, 30s, 60s, 2m, 4m, 5m(cap). The sweep
/// only re-dials once the cooldown elapses, so an open circuit yields at most
/// one half-open probe per cooldown instead of a per-tick storm.
fn redial_backoff(consecutive_failures: u32) -> Duration {
    if consecutive_failures <= 1 {
        return COLD_REDIAL_BACKOFF;
    }
    let shifted = COLD_REDIAL_BACKOFF
        .checked_mul(1u32 << (consecutive_failures - 1).min(31))
        .unwrap_or(MAX_REDIAL_BACKOFF);
    shifted.min(MAX_REDIAL_BACKOFF)
}

/// What the caller must do after a registry mutation, so the registry stays
/// pure (no I/O) and the effects are unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlotEffect {
    /// Pause frames on this slot (it stopped being active): send
    /// `SetFrameSubscription { enabled: false }`.
    Pause(SlotTarget),
    /// Resume frames + full redraw on this slot (it became active): send
    /// `SetFrameSubscription { enabled: true }`.
    Resume(SlotTarget),
    /// Background-dial this cold target to warm it.
    Dial(SlotTarget),
}

/// Outcome of a switch request resolved against the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwitchOutcome {
    /// The target was warm: it is now active. Effects pause the old active slot
    /// and resume the new one — an instant in-process flip, no process exit.
    Flipped { effects: Vec<SlotEffect> },
    /// The target is cold or unknown: fall back to the dial / relaunch-leg
    /// path (the #67 frozen-frame UX) to establish it.
    ColdDial(SlotTarget),
    /// The target is already the active slot: nothing to do.
    AlreadyActive,
}

/// The connection-slot registry.
pub(crate) struct SlotRegistry {
    slots: HashMap<String, Slot>,
    /// Key of the currently active slot, if any.
    active: Option<String>,
    /// Sanity cap on warmed slots (`[slots] max`), including home and active.
    max_warm: usize,
}

impl SlotRegistry {
    /// Create a registry over `targets` with `home` already active. Every other
    /// target starts cold; [`pending_dials`] yields them (bounded by the cap)
    /// for the warm-all dialer to warm in the background.
    pub(crate) fn new(active: SlotTarget, targets: Vec<SlotTarget>, max_warm: usize) -> Self {
        let mut slots = HashMap::new();
        let active_key = active.key().to_string();
        slots.insert(
            active_key.clone(),
            Slot {
                target: active,
                phase: SlotPhase::Active,
                consecutive_failures: 0,
            },
        );
        for target in targets {
            slots
                .entry(target.key().to_string())
                .or_insert_with(|| Slot {
                    target: target.clone(),
                    phase: SlotPhase::Cold { failed_at: None },
                    consecutive_failures: 0,
                });
        }
        Self {
            slots,
            active: Some(active_key),
            max_warm: max_warm.max(1),
        }
    }

    /// Number of slots currently holding a connection (active + warm).
    fn warm_count(&self) -> usize {
        self.slots
            .values()
            .filter(|s| matches!(s.phase, SlotPhase::Active | SlotPhase::Warm))
            .count()
    }

    /// The active slot's target, if any.
    #[cfg(test)]
    pub(crate) fn active_target(&self) -> Option<&SlotTarget> {
        self.active
            .as_ref()
            .and_then(|k| self.slots.get(k))
            .map(|s| &s.target)
    }

    /// Phase of a target, if it is registered.
    #[cfg(test)]
    pub(crate) fn phase(&self, target: &SlotTarget) -> Option<SlotPhase> {
        self.slots.get(target.key()).map(|s| s.phase)
    }

    /// True if `target` is currently the active slot. Used by the switch
    /// popup flow (#93) to short-circuit a SwitchServer for the slot we are
    /// already on without arming a cold dial.
    pub(crate) fn is_active(&self, target: &SlotTarget) -> bool {
        self.active.as_deref() == Some(target.key())
    }

    /// Cold targets eligible to be warmed now: under the cap, and past their
    /// backoff if a prior dial failed. The warm-all dialer drives these, then
    /// reports each result back via [`mark_warm`] / [`mark_dial_failed`].
    pub(crate) fn pending_dials(&self, now: Instant) -> Vec<SlotEffect> {
        let mut budget = self.max_warm.saturating_sub(self.warm_count());
        let mut effects = Vec::new();
        // Deterministic order: home first, then ssh targets by key, so the
        // dialer and tests see a stable sequence.
        let mut cold: Vec<&Slot> = self
            .slots
            .values()
            .filter(|s| match s.phase {
                SlotPhase::Cold { failed_at } => failed_at
                    .map(|at| now.duration_since(at) >= redial_backoff(s.consecutive_failures))
                    .unwrap_or(true),
                _ => false,
            })
            .collect();
        cold.sort_by(|a, b| dial_order(&a.target).cmp(&dial_order(&b.target)));
        for slot in cold {
            if budget == 0 {
                break;
            }
            effects.push(SlotEffect::Dial(slot.target.clone()));
            budget -= 1;
        }
        effects
    }

    /// A background dial succeeded: the target now holds a paused connection.
    /// Returns the pause effect so the caller sends the slot straight into its
    /// warm (frames-off) state.
    pub(crate) fn mark_warm(&mut self, target: &SlotTarget) -> Option<SlotEffect> {
        let slot = self.slots.get_mut(target.key())?;
        if matches!(slot.phase, SlotPhase::Active) {
            return None;
        }
        // A healthy warm closes the breaker: reset the failure streak so the
        // next death starts backoff from the base again.
        slot.consecutive_failures = 0;
        slot.phase = SlotPhase::Warm;
        Some(SlotEffect::Pause(target.clone()))
    }

    /// A background dial failed: keep the target cold, count the failure, and
    /// stamp it so the escalating backoff ([`redial_backoff`]) gates the next
    /// re-dial. A flaky peer thus ghosts with an ever-widening cooldown instead
    /// of being re-dialed every sweep tick (#176).
    pub(crate) fn mark_dial_failed(&mut self, target: &SlotTarget, now: Instant) {
        if let Some(slot) = self.slots.get_mut(target.key()) {
            if !matches!(slot.phase, SlotPhase::Active | SlotPhase::Warm) {
                slot.consecutive_failures = slot.consecutive_failures.saturating_add(1);
                slot.phase = SlotPhase::Cold {
                    failed_at: Some(now),
                };
            }
        }
    }

    /// Transport death of a warm (or active) slot: demote it to cold silently.
    /// The failure surfaces only if the user later switches to it (#65). A
    /// dead active slot leaves the registry with no active slot — the caller
    /// must reattach it (the only slot driving the terminal). Counts toward the
    /// breaker and stamps `failed_at` so a flapping peer (connect→die→connect)
    /// backs off instead of re-dialing on the very next sweep tick (#176).
    pub(crate) fn demote_dead(&mut self, target: &SlotTarget, now: Instant) {
        if let Some(slot) = self.slots.get_mut(target.key()) {
            slot.consecutive_failures = slot.consecutive_failures.saturating_add(1);
            slot.phase = SlotPhase::Cold {
                failed_at: Some(now),
            };
            if self.active.as_deref() == Some(target.key()) {
                self.active = None;
            }
        }
    }

    /// Resolve a switch request against the registry. A warm target flips in
    /// process (pause old, resume new). A cold/unknown target falls back to the
    /// dial path. Re-selecting the active slot is a no-op.
    pub(crate) fn request_switch(&mut self, target: &SlotTarget) -> SwitchOutcome {
        let key = target.key().to_string();
        if self.active.as_deref() == Some(key.as_str()) {
            return SwitchOutcome::AlreadyActive;
        }
        match self.slots.get(&key).map(|s| s.phase) {
            Some(SlotPhase::Warm) => {
                let mut effects = Vec::new();
                if let Some(old_key) = self.active.take() {
                    if let Some(old) = self.slots.get_mut(&old_key) {
                        old.phase = SlotPhase::Warm;
                        effects.push(SlotEffect::Pause(old.target.clone()));
                    }
                }
                if let Some(new) = self.slots.get_mut(&key) {
                    new.phase = SlotPhase::Active;
                    effects.push(SlotEffect::Resume(new.target.clone()));
                }
                self.active = Some(key);
                SwitchOutcome::Flipped { effects }
            }
            // Cold, dead, or never-registered: dial it the slow way. Register
            // an unknown target cold so a subsequent warm reattaches it.
            _ => {
                self.slots.entry(key).or_insert_with(|| Slot {
                    target: target.clone(),
                    phase: SlotPhase::Cold { failed_at: None },
                    consecutive_failures: 0,
                });
                SwitchOutcome::ColdDial(target.clone())
            }
        }
    }
}

/// Stable dial ordering: home first, then ssh targets alphabetically.
fn dial_order(target: &SlotTarget) -> (u8, &str) {
    match target {
        SlotTarget::Home => (0, ""),
        SlotTarget::Ssh(t) => (1, t.as_str()),
    }
}

/// Derive the warm-all target list for a client, deduplicated and bounded by
/// the slots cap. Home is always included and always first. The rest come from
/// the active server's fleet: the carried snapshot's peers and origin (a spoke
/// learns its fleet from the down-gossip, #73) plus the locally configured
/// `[[peers]]` (a hub knows its own fleet). The reserved home sentinel is never
/// re-added as a peer.
pub(crate) fn warm_all_targets(
    config_peers: &[String],
    carried_peer_targets: &[String],
    max: usize,
) -> Vec<SlotTarget> {
    let mut out = vec![SlotTarget::Home];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(HOME_SWITCH_TARGET.to_string());
    for key in config_peers.iter().chain(carried_peer_targets.iter()) {
        if key.is_empty() || key == HOME_SWITCH_TARGET {
            continue;
        }
        if seen.insert(key.clone()) {
            out.push(SlotTarget::Ssh(key.clone()));
        }
        // Cap includes home, so stop once the list reaches `max`.
        if out.len() >= max.max(1) {
            break;
        }
    }
    out
}

/// What the client loop should do with a slot-tagged event at APPLY time,
/// decided by comparing the reader's slot key against the currently-active
/// slot (#65). This is the apply-time check that makes warm-slot death silent
/// and stale frames harmless — a frame queued by the old reader before a flip
/// arrives tagged with the old slot's key and is [`Drop`](SlotRouting::Drop)ped
/// instead of painting over the new slot's redraw (blocker 1 + 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotRouting {
    /// The event is from the active slot: apply it normally.
    Apply,
    /// The event is from a non-active slot and carries no lifecycle meaning
    /// (a frame, notify, etc.): drop it silently.
    Drop,
    /// The event is a non-active slot's transport/lifecycle death (its reader
    /// disconnected, or its server sent ServerShutdown): demote that slot to
    /// cold silently. The active session is untouched.
    DemoteDead,
}

/// Route a slot-tagged event read by `event_slot` against the `active` slot.
/// `is_lifecycle_death` is true for a reader disconnect or a `ServerShutdown`
/// message (the two signals that a non-active slot's transport is gone).
pub(crate) fn route_slot_event(
    event_slot: &str,
    active: &str,
    is_lifecycle_death: bool,
) -> SlotRouting {
    if event_slot == active {
        SlotRouting::Apply
    } else if is_lifecycle_death {
        SlotRouting::DemoteDead
    } else {
        SlotRouting::Drop
    }
}

/// Bounded depth of a slot's writer queue. Interactive traffic is tiny; a full
/// queue means the writer thread has been unable to drain a wedged/slow peer's
/// transport for a sustained stretch. The loop treats a full queue as the slot
/// going dead (its `send` returns `WouldBlock`) rather than blocking on it — the
/// bulkhead that keeps one flaky peer from freezing the render loop (#176).
const WRITER_QUEUE_DEPTH: usize = 512;

/// Per-write socket timeout on a PEER slot's transport. Bounds how long a single
/// `write` syscall parks when the kernel send buffer is full before the writer
/// thread loops and retries. Writes are offset-tracked, so a timeout never
/// leaves the stream mid-frame — a transient stall delays delivery instead of
/// corrupting framing (#176). Home (local socket) writes carry no timeout.
const PEER_WRITE_TIMEOUT: Duration = Duration::from_millis(200);

/// Hard ceiling on how long the writer thread keeps retrying a single frame
/// against a wedged transport before declaring the slot dead. Bounds the
/// lifetime of a thread stuck on a black-holed peer (it still owns the ssh
/// bridge, so we cannot let it park forever). The loop's bounded queue usually
/// trips first; this backstops a peer that wedges mid-frame (#176).
const WRITER_HARD_DEADLINE: Duration = Duration::from_secs(5);

/// A cheap, cloneable handle to a slot's dedicated writer thread. The loop holds
/// one for the active slot and enqueues every keystroke/resize/subscription
/// toggle through [`send`](SlotWriter::send) — a non-blocking channel push. The
/// writer thread owns the transport's write half and (for a client-built ssh
/// peer) the [`SshStdioBridge`](crate::remote::SshStdioBridge), so blocking
/// socket I/O and the bridge's blocking teardown never touch the loop thread
/// (#176). This is the per-peer bulkhead: a wedged peer can only ever fill its
/// own queue, never stall the render loop.
#[derive(Clone)]
pub(crate) struct SlotWriter {
    tx: SyncSender<ClientMessage>,
    /// Set by the writer thread when its transport dies (or it gives up on a
    /// wedged peer). Lets `send` fail fast without waiting for the channel to
    /// disconnect, so the loop demotes promptly.
    dead: Arc<AtomicBool>,
}

impl SlotWriter {
    /// Spawn the writer thread for a slot, taking ownership of `stream`'s write
    /// side and any client-built `bridge`. `apply_write_timeout` bounds each
    /// write on a peer transport. `on_death` fires once if the transport dies
    /// mid-session (a hard write error or a wedged-peer give-up), letting the
    /// caller post the slot's disconnect to the loop; it does NOT fire on a
    /// clean shutdown (all handles dropped).
    pub(crate) fn spawn(
        mut stream: UnixStream,
        bridge: Option<crate::remote::SshStdioBridge>,
        apply_write_timeout: bool,
        on_death: Box<dyn FnOnce() + Send>,
    ) -> Self {
        let (tx, rx) = sync_channel::<ClientMessage>(WRITER_QUEUE_DEPTH);
        let dead = Arc::new(AtomicBool::new(false));
        let thread_dead = Arc::clone(&dead);
        std::thread::spawn(move || {
            // Writes must block (the reader clone shares this fd's O_NONBLOCK via
            // try_clone; both sides want blocking). A nonblocking fd would busy-
            // spin the pump on WouldBlock instead of honoring the write timeout.
            let _ = stream.set_nonblocking(false);
            if apply_write_timeout {
                // A failure to arm the timeout is non-fatal: the write loop still
                // makes progress on a healthy transport; it just can't bound a
                // stall. The bounded queue remains the primary bulkhead.
                let _ = stream.set_write_timeout(Some(PEER_WRITE_TIMEOUT));
            }
            let errored = writer_pump(&mut stream, &rx);
            thread_dead.store(true, Ordering::Release);
            // Teardown runs HERE, on the writer thread: dropping the bridge joins
            // its listener, which can block for seconds on a live ssh child —
            // never on the loop thread (#176).
            drop(stream);
            drop(bridge);
            if errored {
                on_death();
            }
        });
        Self { tx, dead }
    }

    /// Enqueue a message for the slot's writer thread. Never blocks: returns an
    /// error the caller treats as the slot going dead — either the writer
    /// already died, or its queue is full (a sustained stall on this peer).
    pub(crate) fn send(&self, msg: ClientMessage) -> std::io::Result<()> {
        if self.dead.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "slot writer terminated",
            ));
        }
        match self.tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "slot writer backpressured (peer stalled)",
            )),
            Err(TrySendError::Disconnected(_)) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "slot writer gone",
            )),
        }
    }

    /// Test-only: whether the writer thread has marked this slot dead.
    #[cfg(test)]
    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }
}

/// Drain `rx`, framing each message into the transport with offset-tracked
/// writes. Returns `true` if it stopped on a transport ERROR (a death to
/// report), `false` if it stopped because all senders dropped (clean shutdown).
///
/// A `WouldBlock`/`TimedOut` from the per-write timeout is retried — the stream
/// stays consistent at `off`, so a transient stall never corrupts framing or
/// kills the slot. Only a hard error, an EOF, or exceeding
/// [`WRITER_HARD_DEADLINE`] on a single frame ends the slot; the loop's bounded
/// queue normally trips first and demotes the peer.
fn writer_pump(stream: &mut UnixStream, rx: &Receiver<ClientMessage>) -> bool {
    for msg in rx.iter() {
        let mut buf = Vec::new();
        if crate::protocol::write_message(&mut buf, &msg).is_err() {
            // Serializing into an in-memory buffer cannot fail on I/O; a framing
            // error here is an oversized payload. Drop the message, keep the
            // slot alive.
            continue;
        }
        let mut off = 0;
        let started = Instant::now();
        while off < buf.len() {
            match stream.write(&buf[off..]) {
                Ok(0) => return true, // EOF: peer closed the transport.
                Ok(n) => off += n,
                Err(e) => match e.kind() {
                    std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted => {
                        // Send buffer full for the whole timeout (or a signal).
                        // Framing is intact at `off`; retry until the peer drains
                        // or we hit the hard deadline on a truly wedged link.
                        if started.elapsed() >= WRITER_HARD_DEADLINE {
                            return true;
                        }
                        continue;
                    }
                    _ => return true,
                },
            }
        }
    }
    false
}

/// A live warm/active slot connection the client holds open: a cloneable
/// [`SlotWriter`] handle to the slot's dedicated writer thread, plus a reserved
/// read-half clone so a flip can spawn the active reader from the same
/// transport. Frames arrive on the shared loop event channel (the reader half
/// lives in a spawned thread); a paused slot's server stops streaming, so the
/// active slot is the only one painting.
///
/// The writer thread owns the transport write half and any client-built
/// ssh-stdio bridge (#93 cold switch dial); when this connection is dropped or
/// demoted, the last writer handle drops, the writer thread exits, and the
/// bridge's teardown runs there — off the loop thread (#176).
pub(crate) struct SlotConnection {
    pub(crate) target: SlotTarget,
    writer: SlotWriter,
    /// A read-half clone reserved so a flip TO this slot can spawn its reader
    /// from the same transport the writer drives. Warm slots hold it idle.
    reader_src: UnixStream,
}

impl SlotConnection {
    /// Build a live slot connection: spawn the writer thread (owning `stream`'s
    /// write side + any `bridge`) and reserve a read-half clone for a future
    /// flip's reader. `on_death` posts this slot's disconnect to the loop if the
    /// transport dies mid-session. A peer (`Ssh`) slot arms the per-write
    /// timeout; home does not.
    pub(crate) fn new(
        target: SlotTarget,
        stream: UnixStream,
        bridge: Option<crate::remote::SshStdioBridge>,
        on_death: Box<dyn FnOnce() + Send>,
    ) -> std::io::Result<Self> {
        let reader_src = stream.try_clone()?;
        let apply_write_timeout = matches!(target, SlotTarget::Ssh(_));
        let writer = SlotWriter::spawn(stream, bridge, apply_write_timeout, on_death);
        Ok(Self {
            target,
            writer,
            reader_src,
        })
    }

    /// Send the frame-subscription toggle for this slot (pause when it stops
    /// being active, resume + full redraw when it becomes active). Non-blocking:
    /// the enqueue fails only if the slot's transport is already dead/stalled.
    pub(crate) fn set_frame_subscription(&self, enabled: bool) -> std::io::Result<()> {
        self.writer
            .send(ClientMessage::SetFrameSubscription { enabled })
    }

    /// A cheap clone of this slot's writer handle — the loop's active-write path
    /// after a flip makes this slot active.
    pub(crate) fn writer(&self) -> SlotWriter {
        self.writer.clone()
    }

    /// A fresh read-half clone for spawning this slot's reader on a flip.
    pub(crate) fn reader_clone(&self) -> std::io::Result<UnixStream> {
        self.reader_src.try_clone()
    }
}

/// Owns the live warm/active slot connections alongside the [`SlotRegistry`]
/// state machine, and turns [`SlotEffect`]s into wire messages. The active
/// slot's write stream is what the client loop forwards input to; a flip swaps
/// it. Held by the client across an in-process server switch, so the terminal
/// is never released.
pub(crate) struct SlotManager {
    pub(crate) registry: SlotRegistry,
    /// Warm connections keyed by slot key. The active slot is also here.
    connections: HashMap<String, SlotConnection>,
}

impl SlotManager {
    pub(crate) fn new(active: SlotConnection, targets: Vec<SlotTarget>, max_warm: usize) -> Self {
        let registry = SlotRegistry::new(active.target.clone(), targets, max_warm);
        let mut connections = HashMap::new();
        connections.insert(active.target.key().to_string(), active);
        Self {
            registry,
            connections,
        }
    }

    /// True if this slot already holds a live connection (active or warm). The
    /// loop uses this to drop a redundant background pre-warm / switch dial (#139)
    /// instead of overwriting — and orphaning — an established connection when
    /// two dials race for the same peer.
    pub(crate) fn has_connection(&self, target: &SlotTarget) -> bool {
        self.connections.contains_key(target.key())
    }

    /// Register a freshly-dialed warm connection and pause its frames at the
    /// server. The connection joins the registry as warm. The caller must only
    /// call this for a slot with no existing connection (see [`has_connection`]),
    /// so a racing dial can never replace a live transport.
    pub(crate) fn add_warm(&mut self, conn: SlotConnection) -> std::io::Result<()> {
        let key = conn.target.key().to_string();
        if let Some(SlotEffect::Pause(_)) = self.registry.mark_warm(&conn.target) {
            conn.set_frame_subscription(false)?;
        }
        self.connections.insert(key, conn);
        Ok(())
    }

    /// Resolve a switch and, when the target is warm, perform the in-process
    /// flip: pause the old active slot, resume the new one (full redraw), and
    /// return the new active slot's writer handle plus a read-half clone so the
    /// loop can rebind input and spawn the new reader. A cold/unknown target
    /// returns `Ok(None)` — the caller falls back to the dial / relaunch-leg
    /// path (#67 frozen frame).
    pub(crate) fn flip_to(
        &mut self,
        target: &SlotTarget,
    ) -> std::io::Result<Option<(SlotWriter, UnixStream)>> {
        match self.registry.request_switch(target) {
            SwitchOutcome::AlreadyActive => Ok(None),
            SwitchOutcome::ColdDial(_) => Ok(None),
            SwitchOutcome::Flipped { effects } => {
                for effect in &effects {
                    match effect {
                        SlotEffect::Pause(t) => {
                            if let Some(conn) = self.connections.get(t.key()) {
                                // Best-effort: a dead warm slot we are leaving
                                // is harmless, it just stops painting.
                                let _ = conn.set_frame_subscription(false);
                            }
                        }
                        SlotEffect::Resume(t) => {
                            let conn = self.connections.get(t.key()).ok_or_else(|| {
                                std::io::Error::other("warm slot missing connection")
                            })?;
                            conn.set_frame_subscription(true)?;
                        }
                        SlotEffect::Dial(_) => {}
                    }
                }
                let flipped = self
                    .connections
                    .get(target.key())
                    .map(|c| Ok::<_, std::io::Error>((c.writer(), c.reader_clone()?)))
                    .transpose()?;
                Ok(flipped)
            }
        }
    }

    /// Drop a dead slot's connection and demote it in the registry. `now` stamps
    /// the demotion for the escalating redial backoff (#176) — dropping the
    /// connection here releases the last [`SlotWriter`] handle, so the writer
    /// thread exits and tears down its ssh bridge off the loop thread.
    pub(crate) fn handle_dead(&mut self, target: &SlotTarget, now: Instant) {
        self.connections.remove(target.key());
        self.registry.demote_dead(target, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(t: &str) -> SlotTarget {
        SlotTarget::Ssh(t.to_string())
    }

    #[test]
    fn warm_all_targets_dedup_home_first_and_capped() {
        let targets = warm_all_targets(
            &["anvil".into(), "sage".into()],
            &["sage".into(), "<home>".into(), "mba".into()],
            8,
        );
        assert_eq!(
            targets,
            vec![SlotTarget::Home, ssh("anvil"), ssh("sage"), ssh("mba")]
        );

        // Cap of 2 keeps home + one peer.
        let capped = warm_all_targets(&["anvil".into(), "sage".into()], &[], 2);
        assert_eq!(capped, vec![SlotTarget::Home, ssh("anvil")]);
    }

    #[test]
    fn new_registry_makes_home_active_and_rest_cold() {
        let reg = SlotRegistry::new(SlotTarget::Home, vec![ssh("anvil"), ssh("sage")], 8);
        assert_eq!(reg.active_target(), Some(&SlotTarget::Home));
        assert_eq!(reg.phase(&SlotTarget::Home), Some(SlotPhase::Active));
        assert_eq!(
            reg.phase(&ssh("anvil")),
            Some(SlotPhase::Cold { failed_at: None })
        );
    }

    #[test]
    fn pending_dials_are_capped_and_home_first() {
        let reg = SlotRegistry::new(SlotTarget::Home, vec![ssh("b"), ssh("a")], 2);
        // Cap 2, home is active (warm_count 1), so only one cold dial fits.
        let dials = reg.pending_dials(Instant::now());
        assert_eq!(dials, vec![SlotEffect::Dial(ssh("a"))]);
    }

    #[test]
    fn dial_failure_applies_backoff_then_redials() {
        let mut reg = SlotRegistry::new(SlotTarget::Home, vec![ssh("a")], 8);
        let t0 = Instant::now();
        reg.mark_dial_failed(&ssh("a"), t0);
        // Immediately: still cold but inside backoff, not redialed.
        assert_eq!(reg.pending_dials(t0), vec![]);
        // After the backoff window: eligible again.
        let later = t0 + COLD_REDIAL_BACKOFF + Duration::from_secs(1);
        assert_eq!(reg.pending_dials(later), vec![SlotEffect::Dial(ssh("a"))]);
    }

    #[test]
    fn mark_warm_pauses_the_slot() {
        let mut reg = SlotRegistry::new(SlotTarget::Home, vec![ssh("a")], 8);
        let effect = reg.mark_warm(&ssh("a"));
        assert_eq!(effect, Some(SlotEffect::Pause(ssh("a"))));
        assert_eq!(reg.phase(&ssh("a")), Some(SlotPhase::Warm));
    }

    #[test]
    fn switch_to_warm_flips_without_dial() {
        let mut reg = SlotRegistry::new(SlotTarget::Home, vec![ssh("a")], 8);
        reg.mark_warm(&ssh("a"));
        let outcome = reg.request_switch(&ssh("a"));
        match outcome {
            SwitchOutcome::Flipped { effects } => {
                // Old active (home) paused, new active (a) resumed.
                assert_eq!(
                    effects,
                    vec![
                        SlotEffect::Pause(SlotTarget::Home),
                        SlotEffect::Resume(ssh("a")),
                    ]
                );
            }
            other => panic!("expected Flipped, got {other:?}"),
        }
        assert_eq!(reg.active_target(), Some(&ssh("a")));
        assert_eq!(reg.phase(&SlotTarget::Home), Some(SlotPhase::Warm));
    }

    #[test]
    fn switch_to_cold_falls_back_to_dial() {
        let mut reg = SlotRegistry::new(SlotTarget::Home, vec![ssh("a")], 8);
        // a is still cold (never warmed).
        assert_eq!(
            reg.request_switch(&ssh("a")),
            SwitchOutcome::ColdDial(ssh("a"))
        );
        // Active is unchanged — we never left home.
        assert_eq!(reg.active_target(), Some(&SlotTarget::Home));
    }

    #[test]
    fn switch_to_unknown_registers_cold_and_dials() {
        let mut reg = SlotRegistry::new(SlotTarget::Home, vec![], 8);
        assert_eq!(
            reg.request_switch(&ssh("new")),
            SwitchOutcome::ColdDial(ssh("new"))
        );
        assert_eq!(
            reg.phase(&ssh("new")),
            Some(SlotPhase::Cold { failed_at: None })
        );
    }

    #[test]
    fn switch_to_active_is_a_noop() {
        let mut reg = SlotRegistry::new(SlotTarget::Home, vec![], 8);
        assert_eq!(
            reg.request_switch(&SlotTarget::Home),
            SwitchOutcome::AlreadyActive
        );
    }

    #[test]
    fn demote_dead_warm_slot_is_silent_and_redials_later() {
        let mut reg = SlotRegistry::new(SlotTarget::Home, vec![ssh("a")], 8);
        reg.mark_warm(&ssh("a"));
        let t0 = Instant::now();
        reg.demote_dead(&ssh("a"), t0);
        // Demotion stamps `failed_at` for the breaker backoff (#176): the death
        // is silent but the peer now ghosts with a cooldown, not an instant
        // re-dial.
        assert_eq!(
            reg.phase(&ssh("a")),
            Some(SlotPhase::Cold {
                failed_at: Some(t0)
            })
        );
        // Active (home) is untouched: the death is silent.
        assert_eq!(reg.active_target(), Some(&SlotTarget::Home));
        // A switch to the dead slot now falls back to a fresh dial.
        assert_eq!(
            reg.request_switch(&ssh("a")),
            SwitchOutcome::ColdDial(ssh("a"))
        );
    }

    #[test]
    fn demote_dead_active_slot_clears_active() {
        let mut reg = SlotRegistry::new(SlotTarget::Home, vec![], 8);
        reg.demote_dead(&SlotTarget::Home, Instant::now());
        assert_eq!(reg.active_target(), None);
    }

    #[test]
    fn breaker_escalates_backoff_on_repeated_dial_failures() {
        let mut reg = SlotRegistry::new(SlotTarget::Home, vec![ssh("a")], 8);
        let t0 = Instant::now();
        // First failure: eligible again after the base 15s backoff.
        reg.mark_dial_failed(&ssh("a"), t0);
        assert_eq!(reg.pending_dials(t0 + Duration::from_secs(14)), vec![]);
        assert_eq!(
            reg.pending_dials(t0 + Duration::from_secs(16)),
            vec![SlotEffect::Dial(ssh("a"))]
        );
        // Second consecutive failure doubles the cooldown to ~30s: still gated
        // at 16s, eligible past 30s. This is the sweep-storm suppression (#176).
        let t1 = t0 + Duration::from_secs(16);
        reg.mark_dial_failed(&ssh("a"), t1);
        assert_eq!(reg.pending_dials(t1 + Duration::from_secs(16)), vec![]);
        assert_eq!(
            reg.pending_dials(t1 + Duration::from_secs(31)),
            vec![SlotEffect::Dial(ssh("a"))]
        );
        // A successful warm closes the breaker: the streak resets, so the next
        // failure is gated by the base backoff again.
        reg.mark_warm(&ssh("a"));
        let t2 = t1 + Duration::from_secs(31);
        reg.demote_dead(&ssh("a"), t2);
        assert_eq!(reg.pending_dials(t2 + Duration::from_secs(14)), vec![]);
        assert_eq!(
            reg.pending_dials(t2 + Duration::from_secs(16)),
            vec![SlotEffect::Dial(ssh("a"))]
        );
    }

    // --- SlotManager transport tests (real socketpairs, no server) ---

    fn read_one_client_message(stream: &mut UnixStream) -> ClientMessage {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        crate::protocol::read_message(stream, crate::protocol::MAX_FRAME_SIZE).unwrap()
    }

    /// Build a `SlotConnection` over a socketpair local end, with a no-op death
    /// hook. Spawns the slot's real writer thread — subscription toggles reach
    /// the peer end asynchronously, which `read_one_client_message` waits for.
    fn test_conn(target: SlotTarget, local: UnixStream) -> SlotConnection {
        SlotConnection::new(target, local, None, Box::new(|| {})).unwrap()
    }

    /// The load-bearing #65 behavior: switching to a WARM slot flips in process
    /// — the manager swaps the active write stream and toggles subscriptions on
    /// the wire — WITHOUT any relaunch/respawn. The observable here is the
    /// returned new stream (no switch-file path) plus the pause/resume frames
    /// the peers actually receive.
    #[test]
    fn flip_to_warm_slot_swaps_in_process_no_respawn() {
        // Active = home; its peer end is `home_peer`.
        let (home_local, mut home_peer) = UnixStream::pair().unwrap();
        let mut manager = SlotManager::new(
            test_conn(SlotTarget::Home, home_local),
            vec![ssh("anvil")],
            8,
        );

        // Warm the anvil slot with its own socketpair; add_warm pauses it.
        let (anvil_local, mut anvil_peer) = UnixStream::pair().unwrap();
        manager
            .add_warm(test_conn(ssh("anvil"), anvil_local))
            .unwrap();
        // anvil received a pause on warm registration.
        assert_eq!(
            read_one_client_message(&mut anvil_peer),
            ClientMessage::SetFrameSubscription { enabled: false }
        );

        // Flip to anvil: returns its stream (in-process swap, no relaunch).
        let new_stream = manager.flip_to(&ssh("anvil")).unwrap();
        assert!(
            new_stream.is_some(),
            "warm flip must return a stream to rebind input, not fall back to dial"
        );
        // home (old active) was paused; anvil (new active) was resumed.
        assert_eq!(
            read_one_client_message(&mut home_peer),
            ClientMessage::SetFrameSubscription { enabled: false }
        );
        assert_eq!(
            read_one_client_message(&mut anvil_peer),
            ClientMessage::SetFrameSubscription { enabled: true }
        );
        assert_eq!(manager.registry.active_target(), Some(&ssh("anvil")));

        // Switching BACK to home must ALSO be an instant flip (home stayed
        // warm), not a respawn — the previous server is still a held slot.
        let back = manager.flip_to(&SlotTarget::Home).unwrap();
        assert!(back.is_some(), "switching back must flip, not respawn");
        assert_eq!(
            read_one_client_message(&mut anvil_peer),
            ClientMessage::SetFrameSubscription { enabled: false }
        );
        assert_eq!(
            read_one_client_message(&mut home_peer),
            ClientMessage::SetFrameSubscription { enabled: true }
        );
        assert_eq!(manager.registry.active_target(), Some(&SlotTarget::Home));
    }

    // --- Apply-time slot routing (#65 blockers 1 + 2) ---

    #[test]
    fn route_active_slot_frame_applies() {
        // A frame tagged with the active slot's key paints normally.
        assert_eq!(
            route_slot_event(HOME_SWITCH_TARGET, HOME_SWITCH_TARGET, false),
            SlotRouting::Apply
        );
    }

    #[test]
    fn route_stale_frame_from_old_slot_is_dropped() {
        // The load-bearing apply-time check: after a flip to "anvil", a frame
        // the OLD home reader had already queued arrives tagged "<home>". It is
        // dropped, not painted over the new active slot's redraw (blocker 2).
        assert_eq!(
            route_slot_event(HOME_SWITCH_TARGET, "anvil", false),
            SlotRouting::Drop
        );
    }

    #[test]
    fn route_non_active_slot_death_demotes_not_drops() {
        // A non-active slot's lifecycle death (reader disconnect / ServerShutdown)
        // demotes that slot — it never tears the active session down (blocker 1).
        assert_eq!(
            route_slot_event("anvil", HOME_SWITCH_TARGET, true),
            SlotRouting::DemoteDead
        );
    }

    #[test]
    fn route_active_slot_death_applies_connection_lost() {
        // The active slot's death routes to Apply — the loop then returns
        // ConnectionLost, today's semantics for the slot driving the terminal.
        assert_eq!(route_slot_event("anvil", "anvil", true), SlotRouting::Apply);
    }

    /// A warm slot's transport dying (its socketpair peer closes) must demote
    /// the slot in the manager+registry while the ACTIVE slot is untouched —
    /// the session survives (blocker 1). The loop drives this by routing the
    /// dead warm reader's `ServerDisconnected` to `handle_dead`; here we invoke
    /// `handle_dead` directly after killing the peer, asserting the registry
    /// state the session depends on.
    #[test]
    fn warm_slot_death_demotes_and_session_survives() {
        let (home_local, _home_peer) = UnixStream::pair().unwrap();
        let mut manager = SlotManager::new(
            test_conn(SlotTarget::Home, home_local),
            vec![ssh("anvil")],
            8,
        );
        // Warm anvil over its own socketpair.
        let (anvil_local, anvil_peer) = UnixStream::pair().unwrap();
        manager
            .add_warm(test_conn(ssh("anvil"), anvil_local))
            .unwrap();
        assert_eq!(manager.registry.phase(&ssh("anvil")), Some(SlotPhase::Warm));

        // Kill the warm slot's transport: drop its peer end (EOF on the reader).
        drop(anvil_peer);
        // The loop's reaction to that reader's ServerDisconnected:
        let died_at = Instant::now();
        manager.handle_dead(&ssh("anvil"), died_at);

        // The warm slot is demoted to cold (stamped for breaker backoff); the
        // ACTIVE (home) slot is intact — the session did NOT tear down.
        assert_eq!(
            manager.registry.phase(&ssh("anvil")),
            Some(SlotPhase::Cold {
                failed_at: Some(died_at)
            })
        );
        assert_eq!(manager.registry.active_target(), Some(&SlotTarget::Home));
        // A later switch to the dead slot re-dials it (cold fallback).
        assert_eq!(
            manager.registry.request_switch(&ssh("anvil")),
            SwitchOutcome::ColdDial(ssh("anvil"))
        );
    }

    /// `has_connection` is the race guard for #139 pre-warming: it reports a
    /// live transport for the active slot and any warmed slot, and false for a
    /// cold or unknown one — so a redundant background dial is dropped instead
    /// of overwriting an established connection.
    #[test]
    fn has_connection_reflects_active_and_warm_only() {
        let (home_local, _home_peer) = UnixStream::pair().unwrap();
        let mut manager = SlotManager::new(
            test_conn(SlotTarget::Home, home_local),
            vec![ssh("anvil")],
            8,
        );
        // Active slot: connected. Cold/unknown peers: not.
        assert!(manager.has_connection(&SlotTarget::Home));
        assert!(!manager.has_connection(&ssh("anvil")));
        assert!(!manager.has_connection(&ssh("never-registered")));

        // Warming a slot gives it a connection.
        let (anvil_local, _anvil_peer) = UnixStream::pair().unwrap();
        manager
            .add_warm(test_conn(ssh("anvil"), anvil_local))
            .unwrap();
        assert!(manager.has_connection(&ssh("anvil")));

        // A demoted (dead) slot loses its connection, so a re-dial may re-add it.
        manager.handle_dead(&ssh("anvil"), Instant::now());
        assert!(!manager.has_connection(&ssh("anvil")));
    }

    #[test]
    fn flip_to_cold_slot_returns_none_for_relaunch_fallback() {
        let (home_local, _home_peer) = UnixStream::pair().unwrap();
        let mut manager = SlotManager::new(
            test_conn(SlotTarget::Home, home_local),
            vec![ssh("anvil")],
            8,
        );
        // anvil is cold (never warmed): flip falls back to the dial/leg path.
        assert!(manager.flip_to(&ssh("anvil")).unwrap().is_none());
    }

    // --- SlotWriter actor (#176): the per-peer write bulkhead ---

    /// A message enqueued on a healthy writer reaches the peer end intact — the
    /// happy path proving the writer thread frames and flushes correctly.
    #[test]
    fn slot_writer_delivers_enqueued_message() {
        let (local, mut peer) = UnixStream::pair().unwrap();
        let writer = SlotWriter::spawn(local, None, true, Box::new(|| {}));
        writer
            .send(ClientMessage::SetFrameSubscription { enabled: true })
            .unwrap();
        assert_eq!(
            read_one_client_message(&mut peer),
            ClientMessage::SetFrameSubscription { enabled: true }
        );
    }

    /// The load-bearing #176 invariant: `send` NEVER blocks. Even when the peer
    /// never reads and the kernel send buffer fills, enqueues return promptly —
    /// eventually `Err(WouldBlock)` once the bounded queue saturates — instead of
    /// parking the caller (the render loop) on a wedged transport. The writer is
    /// then marked dead so the loop can demote the slot.
    #[test]
    fn slot_writer_send_never_blocks_on_a_wedged_peer() {
        // A peer we never read from: once both the socket buffer and the bounded
        // queue fill, the writer can make no progress.
        let (local, _peer) = UnixStream::pair().unwrap();
        let writer = SlotWriter::spawn(local, None, true, Box::new(|| {}));
        // Push far more than the queue depth of large frames. The point is not
        // that every send succeeds — it's that not one of them blocks the caller.
        let payload = vec![0u8; 32 * 1024];
        let mut saw_backpressure = false;
        let start = Instant::now();
        for _ in 0..(WRITER_QUEUE_DEPTH * 4) {
            match writer.send(ClientMessage::Input {
                data: payload.clone(),
            }) {
                Ok(()) => {}
                Err(_) => {
                    saw_backpressure = true;
                    break;
                }
            }
            // No single iteration may approach the writer's hard deadline; if the
            // caller were blocking on the socket this would blow past it.
            assert!(
                start.elapsed() < WRITER_HARD_DEADLINE,
                "send() blocked the caller on a wedged peer"
            );
        }
        assert!(
            saw_backpressure,
            "a never-draining peer must eventually backpressure the bounded queue"
        );
    }

    /// When the transport dies, the writer thread fires its `on_death` hook
    /// exactly once and marks itself dead — the signal the loop turns into a slot
    /// demotion (#176).
    #[test]
    fn slot_writer_reports_death_when_transport_closes() {
        use std::sync::mpsc::channel;
        let (local, peer) = UnixStream::pair().unwrap();
        let (death_tx, death_rx) = channel::<()>();
        let writer = SlotWriter::spawn(
            local,
            None,
            true,
            Box::new(move || {
                let _ = death_tx.send(());
            }),
        );
        // Close the peer, then write: the pump hits EOF/EPIPE and dies.
        drop(peer);
        // A few sends to ensure the pump observes the broken transport.
        for _ in 0..4 {
            let _ = writer.send(ClientMessage::Input {
                data: vec![1, 2, 3],
            });
            std::thread::sleep(Duration::from_millis(20));
        }
        death_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer must report death once its transport dies");
        assert!(
            writer.is_dead(),
            "writer marks itself dead after a transport error"
        );
    }

    /// A clean shutdown (all handles dropped) must NOT fire `on_death`: dropping
    /// a slot is not a transport failure, and a spurious disconnect would demote
    /// a slot the caller intentionally retired (#176).
    #[test]
    fn slot_writer_clean_drop_does_not_report_death() {
        use std::sync::mpsc::channel;
        let (local, _peer) = UnixStream::pair().unwrap();
        let (death_tx, death_rx) = channel::<()>();
        let writer = SlotWriter::spawn(
            local,
            None,
            true,
            Box::new(move || {
                let _ = death_tx.send(());
            }),
        );
        drop(writer); // last handle → channel closes → clean exit.
        assert!(
            death_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "a clean shutdown must not fire the death hook"
        );
    }
}
