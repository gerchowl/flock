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
///
/// The probe is a single `getpwuid` (~200ns warm, ~1ms cold), so this is not
/// about cost so much as about not putting a libc call on the render hot loop
/// — the rule #262 / #265 established. Five seconds is far below the time a
/// user needs to notice a banner, and far above the rate at which anything
/// about the answer actually changes: the session does not come back on its
/// own, and the only recovery is a restart from a healthy terminal.
pub(crate) const SESSION_HEALTH_MAX_AGE: Duration = Duration::from_secs(5);

/// Cached answer to "does this process still have a usable user session?" (#426).
///
/// A *cache*, not a poller: it holds the last reading and a timestamp, and
/// re-reads at most once per [`SESSION_HEALTH_MAX_AGE`]. The render path only
/// ever calls [`SessionHealthCore::health`], which is a field read — no probe,
/// no syscall, no subprocess. The probe itself runs from
/// `App::handle_scheduled_tasks`, which is already the loop's TTL-driven work.
///
/// This is deliberately a different shape from [`PollerHealthCore`], which
/// models a task that runs and can fail. Here the "task" is a libc call whose
/// answer is a property of the process, so there is no streak, no in-flight
/// guard and no staleness grading: either the passwd database answers for our
/// uid or it does not, and the only thing worth tracking is when that last
/// changed.
#[derive(Debug, Clone)]
pub(crate) struct SessionHealthCore {
    checked_at: Option<Instant>,
    health: SessionHealth,
    /// Whether the transition into `Broken` has been logged. #426 wants the
    /// server's log to say why agents stopped being able to reach anything,
    /// and #318 established that a warning repeating every tick is what makes
    /// `grep WARN` useless — so the transition is logged once, and the periodic
    /// re-read stays silent while the answer is unchanged.
    logged_broken: bool,
}

impl Default for SessionHealthCore {
    fn default() -> Self {
        Self {
            checked_at: None,
            health: SessionHealth::Healthy,
            logged_broken: false,
        }
    }
}

impl SessionHealthCore {
    /// The cached reading. Never probes, so this is safe on the render path.
    pub(crate) fn health(&self) -> SessionHealth {
        self.health
    }

    /// Whether the transition into `Broken` is still waiting to be logged.
    pub(crate) fn take_broken_log_pending(&mut self) -> bool {
        let pending = self.health == SessionHealth::Broken && !self.logged_broken;
        self.logged_broken = self.health == SessionHealth::Broken;
        pending
    }

    /// A core already holding `health`, timestamped as a reading taken just now.
    ///
    /// Exists so the *projection* (health → banner, status label) can be tested
    /// on the arm a healthy machine cannot otherwise produce. It is
    /// deliberately not a way to fake detection: the timestamp means the real
    /// probe will overwrite it as soon as the TTL expires, and the primitive
    /// itself is covered against a genuinely unreachable passwd database in
    /// `platform::macos` rather than through this constructor.
    #[cfg(test)]
    pub(crate) fn seeded(health: SessionHealth) -> Self {
        Self {
            checked_at: Some(Instant::now()),
            health,
            logged_broken: health == SessionHealth::Broken,
        }
    }

    /// Re-probe if the cached reading has aged out. Returns true when the
    /// health changed, which is the caller's cue to ask for a redraw.
    pub(crate) fn refresh_if_due(&mut self, now: Instant) -> bool {
        self.refresh_with(now, crate::platform::session_health)
    }

    fn refresh_with(&mut self, now: Instant, probe: impl FnOnce() -> SessionHealth) -> bool {
        if self
            .checked_at
            .is_some_and(|checked| now.saturating_duration_since(checked) < SESSION_HEALTH_MAX_AGE)
        {
            return false;
        }
        self.checked_at = Some(now);
        let health = probe();
        let changed = health != self.health;
        // Re-arm the log line on every *transition*, in both directions. Arming
        // only on entry to `Broken` left the flag latched after a recovery, so a
        // server that broke, healed and broke again said nothing the second
        // time — the one moment the operator most needs to hear it, since they
        // had just watched it recover.
        if changed {
            self.logged_broken = false;
        }
        self.health = health;
        changed
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
    /// The banner text, without the surface's own prefix styling.
    ///
    /// Hand-wrapped, and the wrapping is load-bearing. The renderer
    /// right-aligns the prefixed strip and silently drops whatever runs past
    /// the pane's width, so one long sentence loses its tail — and the tail is
    /// where the worst symptom and the recovery live. Each line is kept under
    /// 57 columns, which is the widest content that survives the 17-character
    /// `session warning: ` prefix in a 74-column pane area (a 100-column
    /// terminal with the sidebar open). Nothing here auto-wraps; `\n` is the
    /// only break, so a line that is too long is a line that disappears.
    /// Wrapped at clause boundaries, never mid-phrase: a reader scanning a
    /// strip should not have to hold "cannot" in their head across a row break
    /// to learn what cannot happen.
    pub(crate) const BANNER: &str = "no usable login session — panes cannot resolve names,\nuse sudo, or reach the network\nrun `flk server stop`, then `flk` from a good terminal";

    /// The `flk status` line.
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
    // These exercise the cache's state machine with an injected probe, which is
    // the right scope for them: what is under test is the TTL and the
    // transition bookkeeping, not whether `getpwuid` works. That the real
    // primitive actually returns NULL in a genuinely unreachable session is a
    // separate test — `session_health_probe_against_real_primitive` — because a
    // test that only ever passes a boolean in proves nothing about the fault.

    #[test]
    fn a_fresh_core_reads_healthy_before_anything_probed() {
        let core = SessionHealthCore::default();
        assert_eq!(core.health(), SessionHealth::Healthy);
    }

    #[test]
    fn the_first_refresh_probes_and_reports_a_change() {
        let mut core = SessionHealthCore::default();
        let now = Instant::now();
        assert!(core.refresh_with(now, || SessionHealth::Broken));
        assert_eq!(core.health(), SessionHealth::Broken);
    }

    #[test]
    fn a_reading_inside_the_ttl_is_served_from_cache() {
        let mut core = SessionHealthCore::default();
        let now = Instant::now();
        core.refresh_with(now, || SessionHealth::Broken);
        // A probe that would panic if it ran: inside the TTL the cached answer
        // is what gets read, which is what keeps the render path off libc.
        assert!(
            !core.refresh_with(now + SESSION_HEALTH_MAX_AGE / 2, || panic!(
                "probe must not run inside the TTL"
            ))
        );
        assert_eq!(core.health(), SessionHealth::Broken);
    }

    #[test]
    fn an_expired_reading_probes_again() {
        let mut core = SessionHealthCore::default();
        let now = Instant::now();
        core.refresh_with(now, || SessionHealth::Healthy);
        // The health flips, which is how we know the second probe really ran
        // rather than the cache serving the first answer.
        assert!(core.refresh_with(now + SESSION_HEALTH_MAX_AGE, || SessionHealth::Broken));
        assert_eq!(core.health(), SessionHealth::Broken);
    }

    #[test]
    fn a_recovery_back_to_healthy_is_reported_as_a_change() {
        let mut core = SessionHealthCore::default();
        let now = Instant::now();
        core.refresh_with(now, || SessionHealth::Broken);
        // The banner has to come back down when the session does, or the fix
        // for the fault becomes its own permanent false alarm.
        assert!(core.refresh_with(now + SESSION_HEALTH_MAX_AGE, || SessionHealth::Healthy));
        assert_eq!(core.health(), SessionHealth::Healthy);
    }

    #[test]
    fn the_broken_log_line_is_owed_once_not_every_reading() {
        let mut core = SessionHealthCore::default();
        let now = Instant::now();
        core.refresh_with(now, || SessionHealth::Broken);
        assert!(
            core.take_broken_log_pending(),
            "the transition is worth a log"
        );
        assert!(
            !core.take_broken_log_pending(),
            "repeating it every reading is what makes grep WARN useless (#318)"
        );
        // And a later transition back into Broken owes the line again.
        core.refresh_with(now + SESSION_HEALTH_MAX_AGE, || SessionHealth::Healthy);
        core.refresh_with(now + SESSION_HEALTH_MAX_AGE * 2, || SessionHealth::Broken);
        assert!(core.take_broken_log_pending());
    }

    #[test]
    fn a_healthy_core_owes_no_log() {
        let mut core = SessionHealthCore::default();
        let now = Instant::now();
        core.refresh_with(now, || SessionHealth::Healthy);
        assert!(!core.take_broken_log_pending());
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
