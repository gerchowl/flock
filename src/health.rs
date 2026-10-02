//! Source-scoped health for a periodic in-process poller (#295).
//!
//! One row per poller — not per workspace, not per peer — for the reasoning
//! `pr_poll` documents at length: a batched round is one round, so a failure
//! is one failure. Recording it on each of N attached things would report one
//! outage as N and destroy the distinction between "there is nothing to say
//! about this thing" and "the poller wedged". That is exactly the signal the
//! #294 incident lacked.
//!
//! ## Shared state, not a shared owner
//!
//! Four independent design reviews on #297 rejected a unifying "supervisor"
//! that would `.tick()` every periodic subsystem — the four are all periodic
//! but they run on unrelated lifetimes (git refresh is anchored to the render
//! path, the checks runner ticks off configured cron/debounce, peer polling
//! rides an SSH cadence). This module carries a shared **health** primitive
//! only: the state machine + status projection that every source-scoped
//! poller needs. Each subsystem still owns its own dispatcher, and each
//! parameterises the primitive on its own closed error-kind enum — because
//! `last_error` crosses hosts inside `PeersSummary`, and PR #300 established
//! that free-text error strings there are a disclosure bug (GitHub's raw
//! errors name private repositories).
//!
//! ## Not a render-time computation
//!
//! `status_at` is called from the peers-summary path — a per-request answer
//! that never runs on the render hot loop. The render hardening after #262 /
//! #265 (a per-frame `realpath()` storm froze every pane) is a strict rule
//! for the drawing path; a poller's health snapshot is served by the same
//! code path an external monitor already polls over SSH.

use std::time::{Duration, Instant};

use crate::platform::SessionHealth;

/// How long a cached session-health reading stays fresh.
const SESSION_HEALTH_MAX_AGE: Duration = Duration::from_secs(5);

/// Consecutive `Broken` readings before the verdict is allowed to become
/// `Broken` (#426).
///
/// A single NULL is a sample, not a verdict. `getpwuid` returning NULL once
/// does not mean the session is gone — and the banner's advice is to destroy
/// every live agent session, which is the one piece of irreversible advice in
/// flock. Acting on one unconfirmed sample is how a transient produces a red
/// strip that tells you to stop the server and then vanishes five seconds
/// later, having cost you the belief that the warning means anything.
///
/// Two readings costs ~5s at the TTL above and removes the transient class
/// outright. The cost is asymmetric in the right direction: a real orphan never
/// heals (the only recovery is a restart from a healthy terminal), so the true
/// positive pays the delay and the false positive never pays at all.
pub(crate) const SESSION_HEALTH_BROKEN_CONFIRMATIONS: u32 = 2;

/// Consecutive `Broken` readings before `live-handoff` refuses — one more than
/// the banner's threshold, deliberately.
///
/// The reason this is not simply the same number is the asymmetry between the
/// two mistakes. Refusing handoff used to take a *fresher, uncached,
/// un-debounced* single probe than the banner did, which put the weaker
/// evidence behind the more consequential decision — and this fault punishes
/// exactly that ordering, because handoff refuses a *recovery*: a false refusal
/// costs the operator one retry five seconds later, while a false banner costs
/// them their sessions and, with it, any reason to believe the next one. One
/// more consecutive reading than the banner is the cheap way to make the
/// blocking path the better-evidenced one, and reading both from the same streak
/// keeps them from ever looking like the same claim.
pub(crate) const SESSION_HEALTH_HANDOFF_CONFIRMATIONS: u32 =
    SESSION_HEALTH_BROKEN_CONFIRMATIONS + 1;

/// The blocking path must be the better-evidenced one. Checked at compile time
/// because both numbers are constants: a test would only catch this after
/// someone had already merged the inversion, and `+ 1` above can be edited to
/// `- 1` without the type system noticing anything at all.
const _: () = assert!(SESSION_HEALTH_HANDOFF_CONFIRMATIONS > SESSION_HEALTH_BROKEN_CONFIRMATIONS);

/// Cached answer to "does this process still have a usable user session?" (#426).
///
/// A *cache*, not a poller. It holds the last reading, when it was taken, and
/// how many consecutive readings have said `Broken`. The render path only ever
/// calls [`SessionHealthCore::health`], which is a field read — no probe, no
/// syscall, no subprocess.
///
/// ## Why the probe runs on its own thread
///
/// `getpwuid` is a blocking call: it hands a mach message to opendirectoryd and
/// waits for the reply. That is cheap when opendirectoryd is healthy and
/// unbounded when it is not — and opendirectoryd being unreachable is *the
/// fault being detected*, so the worst case for this probe is correlated
/// exactly with its trigger. Calling it inline would put an unbounded block on
/// the server's event loop, where a wedged resolver would stop the whole UI
/// rather than merely being reported by it. So each probe is a short-lived
/// thread and this side never blocks: [`SessionHealthCore::refresh_if_due`]
/// launches, collects with `try_recv`, and returns. A hung probe shows up as a
/// stale reading, which is the safe direction — it cannot invent a `Broken`
/// verdict that the confirmations did not earn.
///
/// This is deliberately a different shape from [`PollerHealthCore`], which
/// models a task that runs and can fail. Here the "task" is a libc call whose
/// answer is a property of the process, so there is no failure *kind* to
/// classify and no staleness grading: either the passwd database answers for
/// our uid or it does not.
#[derive(Debug, Default)]
pub(crate) struct SessionHealthCore {
    checked_at: Option<Instant>,
    /// Consecutive raw readings that said `Broken`. Reset by any `Healthy`.
    broken_streak: u32,
    /// The debounced verdict the banner and `flk status` read.
    confirmed_broken: bool,
    /// A probe is out and has not reported yet.
    in_flight: Option<std::sync::mpsc::Receiver<SessionHealth>>,
    /// Whether the transition into `Broken` has been logged. #426 wants the
    /// server's log to say why agents stopped being able to reach anything,
    /// and #318 established that a warning repeating every tick is what makes
    /// `grep WARN` useless — so the transition is logged once.
    logged_broken: bool,
}

impl SessionHealthCore {
    /// The confirmed verdict. Never probes, so this is safe on the render path.
    pub(crate) fn health(&self) -> SessionHealth {
        if self.confirmed_broken {
            SessionHealth::Broken
        } else {
            SessionHealth::Healthy
        }
    }

    /// Whether `live-handoff` should refuse: a *stricter* confirmation count
    /// than the banner's, read from the same streak so the two can never
    /// disagree about what has been observed.
    pub(crate) fn handoff_should_refuse(&self) -> bool {
        self.broken_streak >= SESSION_HEALTH_HANDOFF_CONFIRMATIONS
    }

    /// Whether a probe is out and has not reported yet.
    ///
    /// Observable state, not a verdict — which is what makes the dual-loop
    /// wiring testable: it changes when the tick runs and cannot change when
    /// the tick does not, on a healthy machine as much as a broken one.
    #[cfg(test)]
    pub(crate) fn probe_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Whether the transition into `Broken` is still waiting to be logged.
    pub(crate) fn take_broken_log_pending(&mut self) -> bool {
        let pending = self.confirmed_broken && !self.logged_broken;
        self.logged_broken = self.confirmed_broken;
        pending
    }

    /// Collect a finished probe and launch a new one if one is due. Never
    /// blocks. Returns true when the confirmed verdict changed.
    pub(crate) fn refresh_if_due(&mut self, now: Instant) -> bool {
        self.collect_finished_probe(now);

        let due = self
            .checked_at
            .is_none_or(|checked| now.saturating_duration_since(checked) >= SESSION_HEALTH_MAX_AGE);
        if due && self.in_flight.is_none() {
            self.launch_probe();
        }
        false
    }

    /// Take whatever the probe thread sent, without waiting for it.
    fn collect_finished_probe(&mut self, now: Instant) {
        let Some(rx) = self.in_flight.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(health) => {
                self.in_flight = None;
                self.record(health, now);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // The probe thread died without reporting. Drop the slot so the
                // next tick retries, and leave the reading alone: a probe that
                // never came back is not evidence either way.
                self.in_flight = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn launch_probe(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.in_flight = Some(rx);
        std::thread::Builder::new()
            .name("flock-session-health".to_string())
            .spawn(move || {
                // The receiver is dropped when the core is dropped or the slot
                // is reused, and `send` then fails — which is the thread's cue to
                // exit. It cannot outlive the core by more than the call itself.
                let _ = tx.send(crate::platform::session_health());
            })
            .map(|_| ())
            .unwrap_or_else(|err| {
                // Could not spawn: stop claiming a probe is in flight, or the
                // slot would sit occupied forever and the reading would never
                // be retried.
                tracing::warn!(
                    event = "session.health",
                    subsystem = "platform",
                    outcome = "error",
                    // A thread-spawn failure has no schema to shape it into, and
                    // the message is the whole payload — it never crosses a
                    // host, only the local server log.
                    error = %err, // guardrails-ok(trace-fields)
                    "could not spawn the session-health probe; keeping the last reading"
                );
                self.in_flight = None;
            });
    }

    /// Fold one raw reading into the streak and the confirmed verdict.
    fn record(&mut self, health: SessionHealth, now: Instant) -> bool {
        self.checked_at = Some(now);
        if health == SessionHealth::Broken {
            self.broken_streak = self.broken_streak.saturating_add(1);
        } else {
            self.broken_streak = 0;
        }
        let was_broken = self.confirmed_broken;
        self.confirmed_broken = self.broken_streak >= SESSION_HEALTH_BROKEN_CONFIRMATIONS;
        // Re-arm the log line on every *transition*, in both directions. Arming
        // only on entry to `Broken` left the flag latched after a recovery, so a
        // server that broke, healed and broke again said nothing the second
        // time — the one moment the operator most needs to hear it, since they
        // had just watched it recover.
        if self.confirmed_broken != was_broken {
            self.logged_broken = false;
        }
        self.confirmed_broken != was_broken
    }

    /// Record a reading directly, without a thread. Test-only: the production
    /// path is `refresh_if_due`, and the two share `record` so the debounce
    /// under test is the debounce that ships.
    #[cfg(test)]
    pub(crate) fn record_reading(&mut self, health: SessionHealth, now: Instant) -> bool {
        self.in_flight = None;
        self.record(health, now)
    }

    /// A core already holding `confirmed_broken` with the streak that produced
    /// it, for projection tests. Test-only, for the same reason as above.
    #[cfg(test)]
    pub(crate) fn seeded_confirmed(confirmed: bool, streak: u32) -> Self {
        Self {
            checked_at: Some(Instant::now()),
            broken_streak: if confirmed { streak } else { 0 },
            confirmed_broken: confirmed,
            in_flight: None,
            logged_broken: confirmed,
        }
    }
}

// ---------------------------------------------------------------------------
// The process-wide session-health mirror (#426)
// ---------------------------------------------------------------------------
//
// ## Exactly one of these per process
//
// This is deliberately **process-wide, not per-`App`**, and the distinction is
// load-bearing: the correctness argument is that the banner and `flk status`
// are two *readers of one debounced verdict*. Two writers would be two opinions
// that can disagree, and an operator seeing a banner from one and a status line
// from the other saying something different is a worse failure than either
// being stale. A server process has one `App` and one core; a mirror per `App`
// would make "which App?" a question every reader has to answer.
//
// ## Why it is global and not a field on `App`
//
// Because `Pong` — the reply `flk status` reads — is answered on an API
// connection task that has no handle on the `App` holding the core. The two
// ways to fix that are both worse than a mirror: threading the value through
// would mean routing it past the App that owns it, and having `Pong` call
// `session_health()` itself means a second, *undebounced* opinion about the
// same fact — free to say `Broken` a full reading before the banner is allowed
// to. A stale-by-one-TTL mirror is the only option that leaves a single source
// of truth.
//
// **This is the reason, and it belongs here rather than in a PR body**, because
// "tidying" the mirror into an `AppState` field is the obvious-looking next
// edit and it would silently reintroduce the second opinion.
//
// ## Why an atomic and not a lock
//
// The state is one bit, written by the event loop's tick and read by whichever
// API connection threads happen to be answering a status call. There is no
// compound invariant to protect — nothing else has to be updated in step with
// it — so a mutex would be a cost with no benefit, and a contended one on the
// write side, which is the event loop. The read side also has to be wait-free:
// a status request must not be able to block behind the loop it is asking
// about. A thread-local is wrong in the other direction: the readers are not
// the writer's thread.
//
// `Relaxed` is correct because the flag stands alone. Nothing else in memory
// has to be ordered against it; the *timestamp* of the reading lives in the
// core, and a reader that wants the age reads the core, not this.

/// Whether this process has **confirmed** a lost session. One per process.
///
/// Named for what it is rather than for the enum it mirrors: an `AtomicU8`
/// encoding the two variants was the first attempt, and its
/// `1 => Broken, _ => Healthy` decode silently mapped every other value to
/// `Healthy`. That wildcard would have turned a future third state into a
/// false all-clear.
static CONFIRMED_SESSION_BROKEN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Publish the core's confirmed verdict for the API task to read.
///
/// Called from `App::refresh_session_health` on every tick, not only when the
/// verdict changes, so a reader arriving after a restart cannot be served a
/// value the loop stopped maintaining.
pub(crate) fn publish_confirmed(health: SessionHealth) {
    CONFIRMED_SESSION_BROKEN.store(
        health == SessionHealth::Broken,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The last confirmed verdict for this process. `Healthy` until something
/// confirms otherwise — which is the same reading `#[serde(default)]` gives the
/// wire, so a server that has not yet ticked and a healthy one agree rather
/// than one of them claiming an absence of knowledge.
pub(crate) fn confirmed() -> SessionHealth {
    if CONFIRMED_SESSION_BROKEN.load(std::sync::atomic::Ordering::Relaxed) {
        SessionHealth::Broken
    } else {
        SessionHealth::Healthy
    }
}

/// Consecutive failures past which a poller is Degraded even when its last
/// success is still inside the staleness window. Mirrors the PR poll's
/// threshold on purpose: an operator's mental model of "how many misses is
/// bad" must not be different from one poller to the next.
pub(crate) const DEGRADED_FAILURE_STREAK: u32 = 3;

/// The per-poller state machine.
///
/// Generic over `E` — the poller's own closed error-kind enum — because that
/// enum is the type-level fence between "an internal reason we can log" and
/// "a string we ship to another machine". A poller that swaps this for
/// `String` immediately regresses to the #294 disclosure bug.
#[derive(Debug, Clone)]
pub(crate) struct PollerHealthCore<E: Copy + Eq> {
    pub last_success: Option<Instant>,
    pub last_attempt: Option<Instant>,
    pub consecutive_failures: u32,
    pub last_error: Option<E>,
    /// Set while a round is running. Also the overlap guard: a tick that
    /// finds this set skips instead of spawning a second round. Unbounded
    /// spawning is what turned a slow host into a collapsing one (#294) —
    /// rounds piled up rather than draining.
    pub in_flight_since: Option<Instant>,
    /// Rounds skipped because one was already running. Visible rather than
    /// silent: a poller quietly skipping every tick looks identical to a
    /// healthy one that has nothing to report.
    pub skipped_rounds: u64,
}

impl<E: Copy + Eq> Default for PollerHealthCore<E> {
    fn default() -> Self {
        Self {
            last_success: None,
            last_attempt: None,
            consecutive_failures: 0,
            last_error: None,
            in_flight_since: None,
            skipped_rounds: 0,
        }
    }
}

impl<E: Copy + Eq> PollerHealthCore<E> {
    pub(crate) fn mark_started(&mut self, now: Instant) {
        self.in_flight_since = Some(now);
        self.last_attempt = Some(now);
    }

    pub(crate) fn mark_success(&mut self, now: Instant) {
        self.in_flight_since = None;
        self.last_success = Some(now);
        self.consecutive_failures = 0;
        self.last_error = None;
    }

    pub(crate) fn mark_failure(&mut self, _now: Instant, error: E) {
        self.in_flight_since = None;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_error = Some(error);
    }

    pub(crate) fn mark_skipped(&mut self) {
        self.skipped_rounds = self.skipped_rounds.saturating_add(1);
    }

    /// Release a guard whose round can no longer be running. `in_flight_since`
    /// is cleared when the poller's completion event arrives. If that event
    /// never arrives — worker panic, a send on a closed channel during
    /// shutdown — the guard latches and every later tick skips forever. That
    /// trades a visible pile-up for a silent stall, which is strictly worse:
    /// the poller stops and nothing says so. Returns true if it reaped one.
    pub(crate) fn reap_stuck_round(
        &mut self,
        now: Instant,
        max_round_secs: u64,
        timeout_kind: E,
    ) -> bool {
        let Some(started) = self.in_flight_since else {
            return false;
        };
        if now.saturating_duration_since(started).as_secs() <= max_round_secs {
            return false;
        }
        self.mark_failure(now, timeout_kind);
        true
    }

    /// Age of the last success, or `None` if it has never succeeded.
    pub(crate) fn last_success_age_secs(&self, now: Instant) -> Option<u64> {
        self.last_success
            .map(|at| now.saturating_duration_since(at).as_secs())
    }

    /// Age of the current in-flight round, or `None` if nothing is running.
    /// The rising-in-flight number is the early signal a poller is wedging
    /// rather than merely failing. Callers that project an aggregate (e.g.
    /// the peer poller's OLDEST across many in-flight fetches) compute this
    /// on their own tracked instant instead of using this field.
    #[allow(
        dead_code,
        reason = "the aggregate peer poller projects its own oldest-in-flight; this method serves single-round pollers and tests"
    )]
    pub(crate) fn in_flight_age_secs(&self, now: Instant) -> Option<u64> {
        self.in_flight_since
            .map(|at| now.saturating_duration_since(at).as_secs())
    }

    /// `stale_after` is the caller's freshness window; `broken_multiple`
    /// mirrors gossip's TTL multiple so both subsystems agree on "long gone".
    pub(crate) fn status_at(
        &self,
        now: Instant,
        stale_after_secs: u64,
        broken_multiple: u64,
    ) -> PollerStatus {
        let Some(age) = self.last_success_age_secs(now) else {
            // Never succeeded is Broken, not Ok — an empty poller that has
            // never answered must not render as healthy.
            return PollerStatus::Broken;
        };
        if age > stale_after_secs.saturating_mul(broken_multiple) {
            return PollerStatus::Broken;
        }
        if age > stale_after_secs || self.consecutive_failures >= DEGRADED_FAILURE_STREAK {
            return PollerStatus::Degraded;
        }
        PollerStatus::Ok
    }
}

/// Three-state verdict, stringified for the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PollerStatus {
    Ok,
    Degraded,
    Broken,
}

impl PollerStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Broken => "broken",
        }
    }
}

/// Why a git-status refresh round failed.
///
/// The refresh worker sends its result back through `AppEvent::
/// GitStatusRefreshed`. A worker panic (or a send on a closed channel during
/// shutdown) never delivers that event, and the reap turns that silent stall
/// into a stamped `Timeout`. There is deliberately no free-text variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitRefreshErrorKind {
    Timeout,
}

impl GitRefreshErrorKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
        }
    }
}

/// Longest a git-status refresh round can legitimately be in flight. Git
/// operations against a slow filesystem (or an SMB-mounted checkout) can take
/// several seconds; anything past this ceiling is a worker that never
/// delivered its `GitStatusRefreshed` event.
pub(crate) const GIT_REFRESH_MAX_ROUND_SECS: u64 = 60;

/// Why the checks runner tick did not stamp success.
///
/// Present so the type-level guarantee "no free-text errors on the wire" is
/// uniform across every poller. Current code paths do not stamp failures on
/// the runner (per-check outcomes are tracked separately, and are neither the
/// runner's liveness nor the operator's watchdog signal); the variant exists
/// so a future runner-level fault has a place to land without opening the
/// wire to arbitrary strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChecksRunnerErrorKind {
    /// The runner's own state carried a fault (unused today).
    #[allow(dead_code, reason = "reserved for future runner-level faults")]
    Stalled,
}

impl ChecksRunnerErrorKind {
    #[allow(dead_code, reason = "reserved for future runner-level faults")]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Stalled => "stalled",
        }
    }
}

/// Why a peer-summary fetch failed.
///
/// Classified from `PeerSummaryFetch::result`'s free-text `Err(String)` at
/// the health-update site so nothing crosses hosts as a raw error string.
/// The classification is coarse on purpose: an operator asking "is my peer
/// poller alive?" wants the shape of the failure, not the text of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerPollErrorKind {
    Transport,
    Protocol,
    Timeout,
}

impl PeerPollErrorKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Protocol => "protocol",
            Self::Timeout => "timeout",
        }
    }

    /// Classify from the free-text `PeerSummaryFetch` error without
    /// retaining it. A message that looks like a JSON / protocol mismatch is
    /// Protocol; a message that names a timeout is Timeout; everything else
    /// is Transport (the modal failure — an unreachable ssh target).
    pub(crate) fn classify(message: &str) -> Self {
        let lowered = message.to_ascii_lowercase();
        if lowered.contains("timed out") || lowered.contains("timeout") {
            Self::Timeout
        } else if lowered.contains("protocol")
            || lowered.contains("parse")
            || lowered.contains("json")
            || lowered.contains("unexpected response")
        {
            Self::Protocol
        } else {
            Self::Transport
        }
    }
}

/// Longest a peer-summary fetch may be in flight before the guard is reaped.
/// SSH connect + one round-trip against a slow ProxyJump peer can take tens
/// of seconds; anything past this is a fetch that never delivered its
/// `PeerSummaryFetched` event.
pub(crate) const PEER_POLL_MAX_ROUND_SECS: u64 = 90;

/// One place the words live, because this warning has three audiences that must
/// not disagree: the banner over the panes, the line in `flk status`, and the
/// refusal message from `live-handoff`. Three hand-written strings would drift,
/// and the drift would be invisible until someone compared them.
pub(crate) mod session_warning {
    /// The banner text, as prose.
    ///
    /// Wrapped to whatever width the pane has at render time, so this is one
    /// long string rather than pre-broken lines. It used to be hand-wrapped
    /// against an assumed width, which meant it silently lost its tail at 80
    /// columns — including the recovery — while looking fine in a 100-column
    /// test. Hand-tuned line breaks are a claim about a width nobody controls.
    pub(crate) const BANNER: &str = "no usable login session — panes cannot resolve names, use sudo, or reach the network. run `flk server stop`, then `flk` from a good terminal";

    /// The `flk status` verdict word.
    ///
    /// The JSON surface carries its own literal for the same state rather than
    /// aliasing this one. They have to agree today and nothing mechanically
    /// keeps them agreeing, which is the point: a wire value is a contract with
    /// whatever parses it, and tying it to a human-facing label means editing
    /// the label silently edits the API.
    pub(crate) const STATUS_LINE: &str = "broken";

    /// Why `live-handoff` cannot help, and what does. The whole point of
    /// refusing is that the obvious next move — handoff — is the one move that
    /// cannot work here, because the new server is spawned *by* this one and
    /// inherits its bootstrap. Saying so is the difference between a dead end
    /// and a diagnosis.
    pub(crate) const HANDOFF_REFUSAL: &str = "refusing live handoff: this server has no usable macOS session, so the passwd lookup for its own uid fails and panes cannot resolve names, use sudo, or reach the network. Handoff cannot fix it — the new server is spawned by this one and inherits the same broken launchd session, which is why running handoff from a healthy terminal does not help. Run `flk server stop`, then start `flk` from a terminal that works. That costs every live agent session, which is the one thing handoff exists to protect.";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ---- SessionHealthCore (#426) ----
    //
    // These drive the debounce through `record_reading`, which shares `record`
    // with the production path, so the threshold under test is the threshold
    // that ships. What the primitive *returns* is a separate matter, proven
    // against a genuinely unreachable passwd database in `platform::macos` —
    // these tests deliberately inject readings, because the debounce is the
    // policy and the policy is what is being tested here.

    fn reading(core: &mut SessionHealthCore, health: SessionHealth, step: u32) -> bool {
        core.record_reading(health, Instant::now() + Duration::from_secs(step.into()))
    }

    #[test]
    fn a_fresh_core_reads_healthy_before_anything_probed() {
        assert_eq!(
            SessionHealthCore::default().health(),
            SessionHealth::Healthy
        );
    }

    /// The whole point of the confirmation: one `Broken` sample is not a verdict.
    #[test]
    fn one_broken_reading_does_not_confirm() {
        let mut core = SessionHealthCore::default();
        // It reports no verdict change, because the confirmed verdict did not
        // move — which is the whole content of this test.
        assert!(!reading(&mut core, SessionHealth::Broken, 1));
        assert_eq!(
            core.health(),
            SessionHealth::Healthy,
            "a single unconfirmed NULL must not raise the banner"
        );
    }

    #[test]
    fn consecutive_broken_readings_confirm() {
        let mut core = SessionHealthCore::default();
        reading(&mut core, SessionHealth::Broken, 1);
        assert!(reading(&mut core, SessionHealth::Broken, 2));
        assert_eq!(core.health(), SessionHealth::Broken);
    }

    /// A transient — the exact case the review raised: a red banner that tells
    /// you to destroy every session and then goes away.
    #[test]
    fn a_transient_broken_reading_never_becomes_a_verdict() {
        let mut core = SessionHealthCore::default();
        // Asserted on the *transition*, not just the end state. Checking only
        // the final value would pass with a threshold of 1 — the transient would
        // confirm, then clear, and the test would never notice that flock had
        // spent those five seconds telling the operator to destroy their
        // sessions. This version fails if the verdict is ever reached.
        assert!(
            !reading(&mut core, SessionHealth::Broken, 1),
            "the transient must not confirm on its first reading"
        );
        assert!(
            !reading(&mut core, SessionHealth::Healthy, 2),
            "and clearing it is not a transition into the fault"
        );
        assert_eq!(core.health(), SessionHealth::Healthy);
        assert!(
            !core.handoff_should_refuse(),
            "a transient must never refuse handoff"
        );
    }

    /// Interleaved readings do not accumulate toward confirmation. Only
    /// *consecutive* evidence counts, or a fault that flickers would eventually
    /// pass the threshold by arithmetic rather than by observation.
    #[test]
    fn interleaved_readings_do_not_accumulate() {
        let mut core = SessionHealthCore::default();
        for step in 1..=6 {
            let health = if step % 2 == 0 {
                SessionHealth::Broken
            } else {
                SessionHealth::Healthy
            };
            reading(&mut core, health, step);
        }
        assert_eq!(core.health(), SessionHealth::Healthy);
    }

    /// The inversion the review caught: handoff used to take a fresher, less
    /// evidenced reading than the banner. It must now need strictly more.
    #[test]
    fn handoff_needs_strictly_more_evidence_than_the_banner() {
        let mut core = SessionHealthCore::default();
        for step in 1..=SESSION_HEALTH_BROKEN_CONFIRMATIONS {
            reading(&mut core, SessionHealth::Broken, step);
        }
        assert_eq!(core.health(), SessionHealth::Broken, "banner is up");
        assert!(
            !core.handoff_should_refuse(),
            "the banner's threshold alone must not block handoff"
        );
        for step in SESSION_HEALTH_BROKEN_CONFIRMATIONS + 1..=SESSION_HEALTH_HANDOFF_CONFIRMATIONS {
            reading(&mut core, SessionHealth::Broken, step);
        }
        assert!(core.handoff_should_refuse());
    }

    /// Both thresholds read one streak, so they cannot contradict each other.
    #[test]
    fn a_healthy_reading_clears_both_thresholds() {
        let mut core = SessionHealthCore::default();
        for step in 1..=SESSION_HEALTH_HANDOFF_CONFIRMATIONS {
            reading(&mut core, SessionHealth::Broken, step);
        }
        assert!(core.handoff_should_refuse());
        reading(&mut core, SessionHealth::Healthy, 9);
        assert!(!core.handoff_should_refuse());
        assert_eq!(core.health(), SessionHealth::Healthy);
    }

    #[test]
    fn the_broken_log_line_is_owed_once_not_every_reading() {
        let mut core = SessionHealthCore::default();
        for step in 1..=SESSION_HEALTH_BROKEN_CONFIRMATIONS {
            reading(&mut core, SessionHealth::Broken, step);
        }
        assert!(
            core.take_broken_log_pending(),
            "confirmation is worth a log"
        );
        assert!(
            !core.take_broken_log_pending(),
            "repeating it every reading is what makes grep WARN useless (#318)"
        );
        // And a later transition back into Broken owes the line again.
        reading(&mut core, SessionHealth::Healthy, 3);
        for step in 4..=5 {
            reading(&mut core, SessionHealth::Broken, step);
        }
        assert!(core.take_broken_log_pending());
    }

    /// An unconfirmed reading must not be logged as a fault at all — the log
    /// should say "broken" when flock decides it is broken, not when it first
    /// suspects it.
    #[test]
    fn an_unconfirmed_reading_owes_no_log() {
        let mut core = SessionHealthCore::default();
        reading(&mut core, SessionHealth::Broken, 1);
        assert!(!core.take_broken_log_pending());
    }

    #[test]
    fn a_healthy_core_owes_no_log() {
        let mut core = SessionHealthCore::default();
        reading(&mut core, SessionHealth::Healthy, 1);
        assert!(!core.take_broken_log_pending());
    }

    /// `refresh_if_due` must not block and must launch a probe, which is what
    /// keeps an unbounded `getpwuid` off the event loop.
    #[test]
    fn the_tick_launches_a_probe_without_blocking() {
        let mut core = SessionHealthCore::default();
        assert!(!core.probe_in_flight());
        core.refresh_if_due(Instant::now());
        assert!(
            core.probe_in_flight(),
            "the tick must launch the probe on its own thread"
        );
    }

    /// A probe that has not reported yet must not be launched again — otherwise
    /// a wedged resolver would accumulate one thread per tick.
    #[test]
    fn an_unreported_probe_is_not_relaunched() {
        let mut core = SessionHealthCore::default();
        core.refresh_if_due(Instant::now());
        let launched = core.probe_in_flight();
        core.refresh_if_due(Instant::now() + Duration::from_secs(600));
        assert_eq!(
            core.probe_in_flight(),
            launched,
            "one probe in flight must stay one probe in flight"
        );
    }

    /// The mirror round-trips, and defaults to healthy.
    ///
    /// Exercises the process-wide flag directly because it is the one piece of
    /// mutable state here that is not owned by the core, and a mirror stuck at
    /// the wrong value would make `flk status` a permanent lie. Restores it at
    /// the end so the shared flag cannot leak into another test in this
    /// process — nextest gives each test its own, but a plain `cargo test`
    /// would not, and nothing should depend on the runner.
    #[test]
    fn the_process_wide_mirror_round_trips() {
        publish_confirmed(SessionHealth::Broken);
        assert_eq!(confirmed(), SessionHealth::Broken);
        publish_confirmed(SessionHealth::Healthy);
        assert_eq!(confirmed(), SessionHealth::Healthy);
        assert_eq!(
            confirmed(),
            SessionHealth::Healthy,
            "an unstarted server reads as healthy, not as unknown"
        );
    }

    /// `flk status` and the banner must be able to disagree in wording without
    /// disagreeing in fact: the JSON value is its own contract (item 7) and this
    /// is the pin that keeps the two honest.
    #[test]
    fn the_json_value_and_the_status_label_still_agree() {
        assert_eq!(
            crate::cli::status::session_health_label(Some(SessionHealth::Broken)),
            Some(crate::health::session_warning::STATUS_LINE),
        );
    }

    #[test]
    fn the_banner_names_the_symptom_not_the_cause() {
        // "broken session" alone would leave the reader to re-derive ssh and
        // sudo from first principles, which is the diagnostic expense #426 is
        // about. Every clause here is something a pane demonstrably cannot do.
        for clause in ["resolve names", "use sudo", "reach the network"] {
            assert!(
                session_warning::BANNER.contains(clause),
                "banner must name {clause:?}"
            );
        }
        assert!(
            session_warning::BANNER.contains("flk server stop"),
            "banner must name the recovery"
        );
    }

    #[test]
    fn the_handoff_refusal_says_handoff_cannot_help_and_what_does() {
        let refusal = session_warning::HANDOFF_REFUSAL;
        assert!(
            refusal.contains("inherits"),
            "the refusal must explain that handoff propagates the fault"
        );
        assert!(
            refusal.contains("flk server stop"),
            "the refusal must name the recovery"
        );
    }

    /// Placeholder enum for exercising the generic — a poller's own kinds are
    /// its own concern; the core state machine must work for any of them.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestKind {
        A,
    }

    #[test]
    fn never_succeeded_is_broken_not_ok() {
        let health: PollerHealthCore<TestKind> = PollerHealthCore::default();
        assert_eq!(
            health.status_at(Instant::now(), 60, 10),
            PollerStatus::Broken,
            "a poller that has never answered must not render as healthy"
        );
        assert_eq!(health.last_success_age_secs(Instant::now()), None);
    }

    #[test]
    fn status_degrades_on_age_then_breaks() {
        let now = Instant::now();
        let mut health: PollerHealthCore<TestKind> = PollerHealthCore::default();
        health.mark_success(now - Duration::from_secs(30));
        assert_eq!(health.status_at(now, 60, 10), PollerStatus::Ok);

        health.mark_success(now - Duration::from_secs(90));
        assert_eq!(health.status_at(now, 60, 10), PollerStatus::Degraded);

        health.mark_success(now - Duration::from_secs(601));
        assert_eq!(health.status_at(now, 60, 10), PollerStatus::Broken);
    }

    /// A failure streak degrades even while the last value is still inside the
    /// freshness window — otherwise a poller failing every round looks fine
    /// right up until it silently crosses the staleness line.
    #[test]
    fn a_failure_streak_degrades_inside_the_freshness_window() {
        let now = Instant::now();
        let mut health: PollerHealthCore<TestKind> = PollerHealthCore::default();
        health.mark_success(now - Duration::from_secs(5));
        for _ in 0..DEGRADED_FAILURE_STREAK {
            health.mark_failure(now, TestKind::A);
        }
        assert_eq!(health.status_at(now, 60, 10), PollerStatus::Degraded);
        assert_eq!(health.consecutive_failures, DEGRADED_FAILURE_STREAK);
    }

    #[test]
    fn success_clears_the_failure_streak_and_the_in_flight_flag() {
        let now = Instant::now();
        let mut health: PollerHealthCore<TestKind> = PollerHealthCore::default();
        health.mark_started(now);
        assert!(health.in_flight_since.is_some());
        health.mark_failure(now, TestKind::A);
        assert!(
            health.in_flight_since.is_none(),
            "a failed round must release the guard"
        );
        health.mark_started(now);
        health.mark_success(now);
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.last_error.is_none());
        assert!(health.in_flight_since.is_none());
    }

    /// A guard that outlives its round turns a visible pile-up into a silent
    /// stall — the poller stops and nothing reports it.
    #[test]
    fn a_guard_outliving_its_round_is_reaped() {
        let now = Instant::now();
        let mut health: PollerHealthCore<TestKind> = PollerHealthCore::default();
        health.mark_started(now - Duration::from_secs(50));
        assert!(
            health.reap_stuck_round(now, 40, TestKind::A),
            "stale guard should be reaped"
        );
        assert!(health.in_flight_since.is_none());
        assert_eq!(
            health.consecutive_failures, 1,
            "reaping counts as a failure"
        );
    }

    #[test]
    fn a_round_still_within_its_deadline_is_left_alone() {
        let now = Instant::now();
        let mut health: PollerHealthCore<TestKind> = PollerHealthCore::default();
        health.mark_started(now - Duration::from_secs(1));
        assert!(!health.reap_stuck_round(now, 40, TestKind::A));
        assert!(
            health.in_flight_since.is_some(),
            "a live round must not be reaped"
        );
    }

    /// The wedge signal an operator would alert on: `in_flight_age_secs`
    /// climbing toward the round ceiling.
    #[test]
    fn a_wedged_round_shows_a_rising_in_flight_age() {
        let now = Instant::now();
        let mut health: PollerHealthCore<TestKind> = PollerHealthCore::default();
        health.mark_started(now - Duration::from_secs(30));
        assert_eq!(health.in_flight_age_secs(now), Some(30));
        // A little later, without a completion event — the age advances.
        assert_eq!(
            health.in_flight_age_secs(now + Duration::from_secs(5)),
            Some(35),
            "in-flight age must keep climbing while the round is stuck"
        );
    }

    /// Peer-poll classification never carries the message text on the wire.
    #[test]
    fn peer_poll_kinds_never_carry_the_free_text_message() {
        let leaky = "ssh: connect to host prod-01.internal.example.com \
                     port 22: Connection refused";
        let kind = PeerPollErrorKind::classify(leaky);
        assert_eq!(kind, PeerPollErrorKind::Transport);
        assert!(
            !kind.as_str().contains("prod-01") && !kind.as_str().contains("example"),
            "the classification must not carry the hostname"
        );
        assert_eq!(
            PeerPollErrorKind::classify("connection timed out"),
            PeerPollErrorKind::Timeout
        );
        assert_eq!(
            PeerPollErrorKind::classify("protocol mismatch"),
            PeerPollErrorKind::Protocol
        );
        assert_eq!(
            PeerPollErrorKind::classify("unexpected response body"),
            PeerPollErrorKind::Protocol
        );
    }
}
