//! #553 — the `settled` wait, and the one status vocabulary both wait verbs
//! speak.
//!
//! ## What `settled` means, and what it does not
//!
//! `settled` is **observed quiescence**: after a cursor, the server saw this
//! agent enter `working` (or the cursor was captured while it was working), and
//! then saw its reported status hold `idle`/`done` with **no state transition at
//! all** for the settle window. It is a statement about what flock observed, not
//! about what the agent did. The result of a turn is `flk agent result` (#575).
//!
//! ## Why one module
//!
//! There were two wait verbs with two status parsers and one shared meaning that
//! neither stated: `agent wait --status idle` matched an unseen `done` pane,
//! while `wait agent-status --status idle` matched only a literal `idle` and so
//! hung on exactly the unattended agent a supervisor is waiting for. One
//! [`WaitTarget`], one parser, one settle loop and one vocabulary string is what
//! makes the two verbs the same signal rather than two similar ones.
//!
//! `settled` is deliberately CLI-only. It is not an [`AgentStatus`], not a
//! server-side status and not a subscription: the server has no notion of a
//! settle window, and adding one would have made every status consumer carry a
//! concept none of them can act on.

use std::time::{Duration, Instant};

use crate::api::client::ApiClient;
use crate::api::schema::{AgentStatus, Method, Request};
use crate::api::schema::{AgentTarget, PaneTarget};

/// How long a reported `idle`/`done` must be HELD, with no transition at all,
/// before the wait calls it quiescence. Long enough that an idle blip inside a
/// turn — the agent pausing between tool calls — is not mistaken for the end of
/// one.
pub(super) const DEFAULT_SETTLE_MS: u64 = 5_000;

/// How often the wait samples the agent's record.
///
/// Cheap (one small `agent.get`), and short relative to the settle window so a
/// transition is observed within a fraction of the dwell it resets.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Cap on any single request during the wait.
///
/// A wait is a long-lived client, and a live handoff replaces the socket
/// underneath it mid-wait. Bounding each request means such a stall costs one
/// poll rather than the whole wait.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// `flk agent wait` / `flk wait agent-status` settled exit codes.
///
/// Distinct from the plain-status waits (which keep their historical codes,
/// including `1` for a timeout) because a supervisor driving these has to tell
/// four outcomes apart without parsing prose: it settled, it needs a human, the
/// pane went away, or it ran out of time.
pub(super) mod exit {
    pub(super) const SETTLED: i32 = 0;
    pub(super) const BLOCKED: i32 = 3;
    pub(super) const GONE: i32 = 4;
    pub(super) const TIMEOUT: i32 = 124;
}

/// What a `--status` value means. CLI-only: `Settled` has no server-side
/// counterpart, and `Idle` means "ready for input" rather than "the literal
/// string idle".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WaitTarget {
    Status(AgentStatus),
    Settled,
}

impl WaitTarget {
    /// The one vocabulary, in one place, for both verbs.
    ///
    /// A caller that learns `done` from `agent wait` must be able to type it
    /// into `wait agent-status` and get the same answer, so the string, the
    /// parser and the two help surfaces are all built from this one list.
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "idle" => Ok(Self::Status(AgentStatus::Idle)),
            "working" => Ok(Self::Status(AgentStatus::Working)),
            "blocked" => Ok(Self::Status(AgentStatus::Blocked)),
            "done" => Ok(Self::Status(AgentStatus::Done)),
            "unknown" => Ok(Self::Status(AgentStatus::Unknown)),
            // #175 C3: operators wait for a pane to finish going away.
            "hibernated" => Ok(Self::Status(AgentStatus::Hibernated)),
            "settled" => Ok(Self::Settled),
            _ => Err(format!(
                "invalid agent status: {value} (expected {})",
                crate::cli::wait_status_vocab!()
            )),
        }
    }

    /// Whether a reported status answers this target.
    ///
    /// `idle` is "ready for input", which is what an unattended agent goes quiet
    /// as: `done` is an *effective* idle (idle plus an unseen finished pane), so
    /// matching only the literal `idle` is what made
    /// `wait agent-status --status idle` hang on every agent nobody was
    /// watching.
    pub(super) fn satisfied_by(self, current: &str) -> bool {
        match self {
            Self::Settled => false,
            Self::Status(AgentStatus::Idle) => matches!(current, "idle" | "done"),
            Self::Status(status) => current == reported_status_name(status),
        }
    }

    /// The statuses a subscription has to watch to answer this target. Two for
    /// `idle`, one for everything else — a subscription filters on an exact
    /// status, so "ready for input" is only expressible as the pair.
    pub(super) fn watched_statuses(self) -> Vec<AgentStatus> {
        match self {
            Self::Settled => Vec::new(),
            Self::Status(AgentStatus::Idle) => vec![AgentStatus::Idle, AgentStatus::Done],
            Self::Status(status) => vec![status],
        }
    }
}

/// Which record the initial resolve asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InitialTarget<'a> {
    /// `agent wait <target>` — an agent name, pane id, terminal id or label.
    Agent(&'a str),
    /// `wait agent-status <pane_id>` — a pane, which need not be an agent.
    Pane(&'a str),
}

/// The flags that only mean something with `--status settled`.
///
/// Held apart from the parsers because the check is a property of the target,
/// not of a verb: `--after` with `--status idle` is the same mistake in both
/// commands and has to be the same refusal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SettleFlags {
    pub(super) after: Option<String>,
    pub(super) settle_ms: Option<u64>,
}

/// Validate the settle-only flags against the chosen target.
///
/// Returns the reason to exit 2. Checked before any request is sent: a usage
/// error that first waited on the server would report a timeout for a typo.
pub(super) fn check_settle_flags(
    target: Option<WaitTarget>,
    flags: &SettleFlags,
) -> Result<(), String> {
    if target != Some(WaitTarget::Settled) {
        if flags.after.is_some() {
            return Err("--after needs --status settled: it names the turn to wait past".into());
        }
        if flags.settle_ms.is_some() {
            return Err(format!(
                "--settle needs --status settled (default {DEFAULT_SETTLE_MS} ms; nothing else settles)"
            ));
        }
    }
    Ok(())
}

/// Parse a turn cursor minted by `agent get`, `pane get` or `agent list` (#553).
///
/// `"{terminal_id}:{execution_epoch}:{working_entries}:{state_seq}:{w|i}"`.
///
/// Parsed from the RIGHT: the terminal id is a `term_<hex>` the server mints, so
/// treating it as an opaque prefix is what lets the four trailing fields be read
/// off reliably instead of pattern-matched by eye.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cursor {
    pub(super) terminal_id: String,
    /// Bumped when the pane's child was replaced. The terminal id is REUSED on
    /// a shell respawn, so this is what separates a continuing agent from a
    /// fresh one sitting where the old one did.
    pub(super) epoch: u64,
    /// How many times the terminal has been observed entering `working`.
    pub(super) entries: u64,
    /// Bumped on every change of the reported status. This is what a dwell is
    /// anchored on: without it, `idle -> blocked -> idle` would read as one
    /// continuous idle.
    pub(super) seq: u64,
    /// Whether the agent was working when the cursor was minted. A cursor
    /// captured mid-turn may already have a follow-up queued into that turn, so
    /// it settles at the next quiescence rather than waiting for a new entry.
    pub(super) working: bool,
    /// The string as minted, for the result line.
    pub(super) raw: String,
}

impl Cursor {
    pub(super) fn parse(raw: &str) -> Result<Self, String> {
        let mut fields: Vec<&str> = raw.split(':').collect();
        if fields.len() < 5 {
            return Err(format!(
                "invalid turn cursor {raw:?}: expected \
                 <terminal_id>:<epoch>:<working_entries>:<state_seq>:<w|i>"
            ));
        }
        let flag = fields.pop().expect("five fields leaves one to pop");
        let mut number = |label: &str| -> Result<u64, String> {
            let raw = fields.pop().expect("three counters remain");
            raw.parse::<u64>()
                .map_err(|_| format!("invalid turn cursor: {label} field {raw:?} is not a number"))
        };
        let seq = number("state_seq")?;
        let entries = number("working_entries")?;
        let epoch = number("epoch")?;
        let terminal_id = fields.join(":");
        let working = match flag {
            "w" => true,
            "i" => false,
            other => {
                return Err(format!(
                    "invalid turn cursor: working flag {other:?} is neither \"w\" nor \"i\""
                ))
            }
        };
        if terminal_id.is_empty() {
            return Err(format!("invalid turn cursor {raw:?}: no terminal id"));
        }
        Ok(Self {
            terminal_id,
            epoch,
            entries,
            seq,
            working,
            raw: raw.to_string(),
        })
    }
}

/// Why the wait ended with the pane gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GoneReason {
    /// The pane — and with it the terminal — is gone.
    Closed,
    /// The agent was hibernated: its child is gone and a resume plan is
    /// stashed. A wait cannot observe a turn that has no process.
    Hibernated,
    /// The terminal id resolved to a different child (a respawn), so the
    /// execution this cursor named is over.
    Restarted,
}

impl GoneReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Hibernated => "hibernated",
            Self::Restarted => "restarted",
        }
    }
}

/// One sample of the agent's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Sample {
    Record {
        pane_id: String,
        status: AgentStatus,
        cursor: Cursor,
    },
    /// The agent is hibernated.
    ///
    /// Its own arm rather than a `Record` with a cursor, because a hibernated
    /// agent reports from the stashed resume plan and there is no live turn to
    /// have a cursor for — and the ordering matters: "is it hibernated" is
    /// decided BEFORE the record is required to carry one, so a hibernated agent
    /// is `gone` rather than a parse error about a missing field.
    Hibernated {
        pane_id: String,
    },
    Gone(GoneReason),
}

/// What a reported status means for a settled wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Idle or done, AND started after the cursor. Holding this is the answer.
    Settled,
    /// Blocked and holding: a human has to look, so this is its own outcome
    /// rather than a wait that runs on. Never settles.
    Blocked,
    /// Working, unknown, or idle/done that has not started after the cursor yet.
    Waiting,
}

/// What to do with a sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Step {
    /// Keep polling.
    Continue,
    Settled {
        held: Duration,
    },
    Blocked {
        held: Duration,
    },
    Gone(GoneReason),
    /// A refused cursor: exit 2, never read as "it worked".
    Refused(String),
    TimedOut,
}

/// The settle state machine, over (time, record) samples.
///
/// Pure with respect to the server: it never fetches, and every decision
/// depends only on the samples it is handed and the clock readings it is given.
/// That is what makes the deadline-versus-success precedence and the dwell reset
/// testable without a running flock.
pub(super) struct SettledWait {
    after: Option<Cursor>,
    settle: Duration,
    deadline: Option<Instant>,
    /// (class, state_seq, since) of the dwell in progress.
    anchor: Option<(Class, u64, Instant)>,
    /// The terminal pinned by the initial resolve. A pane id can be reused by a
    /// different terminal after a close, so every later sample is checked
    /// against this rather than trusted by position.
    pinned_terminal: String,
    /// The execution epoch of the first record this wait saw.
    ///
    /// A later sample from a HIGHER epoch is a different child behind the same
    /// terminal id — a respawn — and is `gone: restarted` whether or not the
    /// caller passed `--after`. Without this, a wait with no cursor would sit
    /// through a restart and settle on whatever the new child happened to do.
    observed_epoch: Option<u64>,
    /// The most recent record, for the result line. The cursor is optional
    /// because a hibernated sample knows its pane and its status but has no
    /// cursor, and the result line says so rather than inventing one.
    last: Option<(String, AgentStatus, Option<String>)>,
}

impl SettledWait {
    pub(super) fn new(
        after: Option<Cursor>,
        settle: Duration,
        timeout: Option<Duration>,
        pinned_terminal: String,
        now: Instant,
    ) -> Self {
        Self {
            after,
            settle,
            deadline: timeout.and_then(|timeout| now.checked_add(timeout)),
            anchor: None,
            pinned_terminal,
            observed_epoch: None,
            last: None,
        }
    }

    pub(super) fn pinned_terminal(&self) -> &str {
        &self.pinned_terminal
    }

    /// The pane the last record named, for the pinned fallback lookup.
    pub(super) fn last_pane_id(&self) -> Option<&str> {
        self.last.as_ref().map(|(pane_id, _, _)| pane_id.as_str())
    }

    /// The one JSON object both verbs print on exit 0, 3 and 4.
    ///
    /// `agent_status` and `turn_cursor` are null when nothing was ever seen,
    /// rather than invented: a `gone` answer about a pane that never resolved
    /// has to be able to say so.
    pub(super) fn result_line(
        &self,
        status: &str,
        held_ms: u64,
        reason: Option<GoneReason>,
    ) -> String {
        let (pane_id, agent_status, turn_cursor) = match &self.last {
            Some((pane_id, agent_status, cursor)) => (
                serde_json::Value::String(pane_id.clone()),
                serde_json::Value::String(reported_status_name(*agent_status).to_string()),
                match cursor {
                    Some(cursor) => serde_json::Value::String(cursor.clone()),
                    None => serde_json::Value::Null,
                },
            ),
            None => (
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::Value::Null,
            ),
        };
        let mut line = serde_json::json!({
            "status": status,
            "pane_id": pane_id,
            "agent_status": agent_status,
            "turn_cursor": turn_cursor,
            "held_ms": held_ms,
        });
        if let Some(reason) = reason {
            line["reason"] = serde_json::Value::String(reason.as_str().to_string());
        }
        line.to_string()
    }

    /// Fold one sample in and say what to do.
    pub(super) fn observe(&mut self, sample: Sample, now: Instant) -> Step {
        // The deadline wins over a late success: a wait that ran out of time
        // must not then report the quiet it found on the way past.
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            return Step::TimedOut;
        }

        let (pane_id, status, cursor) = match sample {
            Sample::Record {
                pane_id,
                status,
                cursor,
            } => (pane_id, status, cursor),
            Sample::Hibernated { pane_id } => {
                // A hibernated agent has no live child, so there is no turn left
                // to observe and nothing can settle. Decided before the record
                // was asked for a cursor.
                self.last = Some((pane_id, AgentStatus::Hibernated, None));
                return Step::Gone(GoneReason::Hibernated);
            }
            Sample::Gone(reason) => return Step::Gone(reason),
        };
        // Follow the pane: a terminal keeps its id when the pane moves, so the
        // pinned id is the identity and this is only where to find it now.
        self.last = Some((pane_id, status, Some(cursor.raw.clone())));

        // The execution this wait is watching, remembered from the first record
        // it saw. A respawn reuses the terminal id and moves the epoch, so a
        // higher epoch is a different child — `gone`, never a settle, and with
        // or without `--after`.
        match self.observed_epoch {
            None => self.observed_epoch = Some(cursor.epoch),
            Some(watched) if cursor.epoch > watched => return Step::Gone(GoneReason::Restarted),
            Some(_) => {}
        }

        if let Some(after) = &self.after {
            // Direction matters in both halves. A cursor from the future is one
            // this server has never minted — a typo, another machine, or a
            // server whose counters were reset — and must be refused rather
            // than waited out. A cursor from the past, across an execution
            // boundary, names a child that no longer exists.
            if after.epoch > cursor.epoch {
                return Step::Refused("turn cursor is ahead of the server".into());
            }
            if after.epoch < cursor.epoch {
                return Step::Gone(GoneReason::Restarted);
            }
            if cursor.entries < after.entries || cursor.seq < after.seq {
                return Step::Refused("turn cursor is ahead of the server".into());
            }
        }

        let started = match &self.after {
            None => true,
            Some(after) => {
                cursor.entries > after.entries || (after.working && cursor.seq > after.seq)
            }
        };

        let class = match status {
            AgentStatus::Idle | AgentStatus::Done => {
                if started {
                    Class::Settled
                } else {
                    Class::Waiting
                }
            }
            AgentStatus::Blocked => Class::Blocked,
            // `observe` returned above for every hibernated record.
            AgentStatus::Working | AgentStatus::Unknown | AgentStatus::Hibernated => Class::Waiting,
        };

        match class {
            Class::Waiting => {
                self.anchor = None;
                Step::Continue
            }
            settled => match self.anchor {
                // Any change of class OR of state_seq restarts the dwell. The
                // seq is what makes `idle -> blocked -> idle` inside the window
                // reset it: without the counter, samples that happen to agree on
                // `idle` again would look like one unbroken idle.
                Some((anchored, seq, since)) if anchored == settled && seq == cursor.seq => {
                    let held = now.saturating_duration_since(since);
                    if held < self.settle {
                        Step::Continue
                    } else if settled == Class::Settled {
                        Step::Settled { held }
                    } else {
                        Step::Blocked { held }
                    }
                }
                _ => {
                    self.anchor = Some((settled, cursor.seq, now));
                    Step::Continue
                }
            },
        }
    }
}

/// Run the settled wait. One implementation; `flk agent wait --status settled`
/// and `flk wait agent-status --status settled` both land here.
///
/// `verb` is the caller's own name, because every message this prints is read
/// by someone who typed one command or the other and must not have to work out
/// which one produced it.
pub(super) fn run_settled_wait(
    verb: &str,
    target: InitialTarget<'_>,
    after: Option<&str>,
    settle_ms: u64,
    timeout_ms: Option<u64>,
) -> std::io::Result<i32> {
    let client = ApiClient::local();
    let now = Instant::now();
    let timeout = timeout_ms.map(Duration::from_millis);
    let deadline = timeout.and_then(|timeout| now.checked_add(timeout));

    let after = match after {
        Some(raw) => match Cursor::parse(raw) {
            Ok(cursor) => Some(cursor),
            Err(reason) => {
                eprintln!("{verb}: {reason}");
                return Ok(2);
            }
        },
        None => None,
    };

    let rec0 = match resolve_initial(&client, target, deadline, verb) {
        Ok(rec) => rec,
        Err(InitialFailure::TimedOut) => {
            eprintln!("timed out waiting for the agent to settle");
            return Ok(exit::TIMEOUT);
        }
        Err(InitialFailure::Refused(response)) => {
            eprintln!("{response}");
            return Ok(1);
        }
        Err(InitialFailure::Unusable(reason)) => {
            eprintln!("{verb}: {reason}");
            return Ok(1);
        }
    };

    let terminal_id = rec0
        .get("terminal_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    if terminal_id.is_empty() {
        eprintln!("{verb}: the server's record named no terminal");
        return Ok(1);
    }
    if let Some(after) = &after {
        if after.terminal_id != terminal_id {
            eprintln!(
                "{verb}: turn cursor belongs to another terminal \
                 ({} is not {terminal_id})",
                after.terminal_id
            );
            return Ok(2);
        }
    }

    let mut wait = SettledWait::new(
        after,
        Duration::from_millis(settle_ms),
        timeout,
        terminal_id,
        now,
    );

    // The initial record is a sample like any other: folding it in here means
    // the cursor checks, the epoch comparison and a `--settle 0` quiescence all
    // behave the same on the first read as on the hundredth.
    let sample = match sample_from_record(&rec0) {
        Ok(sample) => sample,
        Err(reason) => {
            eprintln!("{verb}: {reason}");
            return Ok(1);
        }
    };
    let step = wait.observe(sample, Instant::now());
    if let Some(code) = finish(verb, &wait, step) {
        return Ok(code);
    }

    let mut requests = SocketRequests { client: &client };
    let mut reported_unreachable = false;
    loop {
        match sample_pinned(&mut requests, &wait, deadline) {
            Ok(sample) => {
                let step = wait.observe(sample, Instant::now());
                if let Some(code) = finish(verb, &wait, step) {
                    return Ok(code);
                }
            }
            // Retried rather than reported: the same live handoff that replaces
            // the socket mid-wait can land between two polls. Said ONCE, though
            // — a wait that cannot reach the server is otherwise silent for as
            // long as its timeout, which reads as a hang.
            Err(PinnedFailure::Retry) => {
                if !reported_unreachable {
                    reported_unreachable = true;
                    eprintln!("{verb}: server unreachable, retrying until the deadline");
                }
            }
            Err(PinnedFailure::TimedOut) => {
                eprintln!("timed out waiting for the agent to settle");
                return Ok(exit::TIMEOUT);
            }
            Err(PinnedFailure::Fatal(reason)) => {
                eprintln!("{verb}: {reason}");
                return Ok(1);
            }
        }
        sleep_bounded(deadline);
    }
}

/// Print a terminal step's outcome and turn it into an exit code.
fn finish(verb: &str, wait: &SettledWait, step: Step) -> Option<i32> {
    match step {
        Step::Continue => None,
        Step::Settled { held } => {
            println!(
                "{}",
                wait.result_line("settled", held.as_millis() as u64, None)
            );
            Some(exit::SETTLED)
        }
        Step::Blocked { held } => {
            println!(
                "{}",
                wait.result_line("blocked", held.as_millis() as u64, None)
            );
            Some(exit::BLOCKED)
        }
        Step::Gone(reason) => {
            println!("{}", wait.result_line("gone", 0, Some(reason)));
            Some(exit::GONE)
        }
        Step::Refused(reason) => {
            eprintln!("{verb}: {reason}");
            Some(2)
        }
        Step::TimedOut => {
            eprintln!("timed out waiting for the agent to settle");
            Some(exit::TIMEOUT)
        }
    }
}

enum InitialFailure {
    /// The server could not be reached before the deadline: a handoff in
    /// progress, most often. Reported as a timeout rather than an error,
    /// because nothing has been established as wrong.
    TimedOut,
    /// The server answered with an error (not found, ambiguous).
    Refused(String),
    /// The server answered, but the record cannot be waited on.
    Unusable(String),
}

fn resolve_initial(
    client: &ApiClient,
    target: InitialTarget<'_>,
    deadline: Option<Instant>,
    verb: &str,
) -> Result<serde_json::Value, InitialFailure> {
    let request = initial_request(target);
    let mut reported_unreachable = false;
    loop {
        let Some(timeout) = request_timeout(deadline) else {
            return Err(InitialFailure::TimedOut);
        };
        match client.request_value_with_timeout(&request, timeout) {
            Ok(value) if value.get("error").is_some() => {
                return Err(InitialFailure::Refused(value.to_string()))
            }
            Ok(value) => {
                return initial_record(&value, target).ok_or_else(|| {
                    InitialFailure::Unusable("response did not include the record".into())
                })
            }
            // A transport failure at the initial resolve is retried rather than
            // reported: the same live handoff that interrupts a wait can land in
            // the window between this process starting and its first request.
            // Said once, so a wait against a dead socket says so.
            Err(_) => {
                if !reported_unreachable {
                    reported_unreachable = true;
                    eprintln!("{verb}: server unreachable, retrying until the deadline");
                }
                sleep_bounded(deadline)
            }
        }
    }
}

fn initial_request(target: InitialTarget<'_>) -> Request {
    match target {
        InitialTarget::Agent(target) => Request {
            id: "cli:settled:resolve".into(),
            method: Method::AgentGet(AgentTarget {
                target: target.to_string(),
            }),
        },
        InitialTarget::Pane(pane_id) => Request {
            id: "cli:settled:resolve".into(),
            method: Method::PaneGet(PaneTarget {
                pane_id: pane_id.to_string(),
            }),
        },
    }
}

fn initial_record(
    value: &serde_json::Value,
    target: InitialTarget<'_>,
) -> Option<serde_json::Value> {
    let record = value.get("result")?;
    match target {
        InitialTarget::Agent(_) => record.get("agent").cloned(),
        InitialTarget::Pane(_) => record.get("pane").cloned(),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PinnedFailure {
    /// The server could not be reached this tick. A live handoff replaces the
    /// socket mid-wait, and a transport error is not an answer — the next tick
    /// finds the new server, or the deadline takes the wait.
    Retry,
    /// A reply arrived after the deadline. Reported as 124 rather than as the
    /// error or the settle it happened to carry: a wait that ran out of clock
    /// does not get to reinterpret what it read on the way past.
    TimedOut,
    /// The server answered with something this client cannot act on. Exit 1,
    /// carrying the server's own words rather than a paraphrase.
    Fatal(String),
}

/// The two requests a pinned sample needs, behind one seam.
///
/// The seam exists so the fetch decisions — which fallback error means `gone`,
/// which means fatal, and whether the second request is still inside the
/// deadline — are reachable from a test without a server or a live socket.
trait PinnedRequests {
    /// `Err` is a transport failure: not an answer.
    fn agent_get(&mut self, terminal_id: &str, timeout: Duration) -> Result<serde_json::Value, ()>;

    fn pane_get(&mut self, pane_id: &str, timeout: Duration) -> Result<serde_json::Value, ()>;
}

struct SocketRequests<'a> {
    client: &'a ApiClient,
}

impl PinnedRequests for SocketRequests<'_> {
    fn agent_get(&mut self, terminal_id: &str, timeout: Duration) -> Result<serde_json::Value, ()> {
        self.client
            .request_value_with_timeout(
                &Request {
                    id: "cli:settled:sample".into(),
                    method: Method::AgentGet(AgentTarget {
                        target: terminal_id.to_string(),
                    }),
                },
                timeout,
            )
            .map_err(|_| ())
    }

    fn pane_get(&mut self, pane_id: &str, timeout: Duration) -> Result<serde_json::Value, ()> {
        self.client
            .request_value_with_timeout(
                &Request {
                    id: "cli:settled:sample:pane".into(),
                    method: Method::PaneGet(PaneTarget {
                        pane_id: pane_id.to_string(),
                    }),
                },
                timeout,
            )
            .map_err(|_| ())
    }
}

/// One sample from the pinned terminal, from whatever the two requests said.
///
/// `agent.get <terminal_id>` is the primary lookup because a terminal id
/// survives a pane move while a pane id does not. It answers `agent_not_found`
/// for a terminal that is not (or is no longer) an agent terminal at all — a
/// plain shell, or one whose identity a respawn just cleared — so a miss falls
/// back to the pane record rather than reading as "the pane is gone". The
/// fallback is also what turns a respawn into `restarted` rather than `closed`:
/// the respawn keeps the terminal id and moves the execution epoch inside it.
fn classify_pinned(
    terminal_id: &str,
    agent: &serde_json::Value,
    pane: Option<&serde_json::Value>,
) -> Result<Sample, PinnedFailure> {
    if agent.get("error").is_some() {
        if !is_agent_not_found(agent) {
            return Err(PinnedFailure::Fatal(agent.to_string()));
        }
        let pane = match pane {
            // The fallback request did not come back either — a handoff, or a
            // server still catching up. Not an answer.
            None => return Err(PinnedFailure::Retry),
            Some(pane) if pane.get("error").is_some() => {
                // Only `pane_not_found` is an answer about the pane. Anything
                // else the fallback says is a problem with the server, and
                // reporting it as "your pane closed" would turn a flock-side
                // fault into a lie about the agent.
                return if is_pane_not_found(pane) {
                    Ok(Sample::Gone(GoneReason::Closed))
                } else {
                    Err(PinnedFailure::Fatal(pane.to_string()))
                };
            }
            Some(pane) => pane,
        };
        let record = match pane_record(pane) {
            Some(record) => record,
            None => return Err(PinnedFailure::Fatal(NO_PANE_RECORD.into())),
        };
        return if record_terminal(record) == Some(terminal_id) {
            sample_from_record(record).map_err(PinnedFailure::Fatal)
        } else {
            Ok(Sample::Gone(GoneReason::Closed))
        };
    }

    let record = match agent.get("result").and_then(|result| result.get("agent")) {
        Some(record) => record,
        None => return Err(PinnedFailure::Fatal(NO_AGENT_RECORD.into())),
    };
    match record_terminal(record) {
        // A record naming a DIFFERENT terminal is a respawn or a reassignment:
        // either way the child this wait pinned is not the one answering.
        Some(other) if other != terminal_id => Ok(Sample::Gone(GoneReason::Restarted)),
        Some(_) => sample_from_record(record).map_err(PinnedFailure::Fatal),
        // A success with no terminal id is a malformed record, not a
        // different terminal. Calling it `restarted` would report an agent as
        // replaced when the server simply answered wrong.
        None => Err(PinnedFailure::Fatal(NO_AGENT_RECORD.into())),
    }
}

/// Fetch the pinned terminal's record, and the pane fallback it may need.
///
/// The request timeout is recomputed from the absolute deadline before EACH
/// request: the fallback is a second round trip, and giving it the first
/// request's remaining budget would let the pair run to twice the deadline.
/// And a reply that lands after the deadline is `TimedOut` whichever way it went
/// — a record is not a settle, and an error is not a fault.
fn sample_pinned(
    requests: &mut dyn PinnedRequests,
    wait: &SettledWait,
    deadline: Option<Instant>,
) -> Result<Sample, PinnedFailure> {
    let terminal_id = wait.pinned_terminal().to_string();
    let agent_timeout = request_timeout(deadline).ok_or(PinnedFailure::TimedOut)?;
    let agent_value = requests
        .agent_get(&terminal_id, agent_timeout)
        .map_err(|()| PinnedFailure::Retry)?;
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(PinnedFailure::TimedOut);
    }

    // The pane lookup only happens on the miss path, so the ordinary case stays
    // one request per poll.
    if !(agent_value.get("error").is_some() && is_agent_not_found(&agent_value)) {
        return classify_pinned(&terminal_id, &agent_value, None);
    }
    let pane_timeout = request_timeout(deadline).ok_or(PinnedFailure::TimedOut)?;
    let pane_value = requests
        .pane_get(wait.last_pane_id().unwrap_or_default(), pane_timeout)
        .ok();
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(PinnedFailure::TimedOut);
    }
    classify_pinned(&terminal_id, &agent_value, pane_value.as_ref())
}

const NO_AGENT_RECORD: &str = "server sent no agent record";
const NO_PANE_RECORD: &str = "server sent no pane record";

fn record_terminal(record: &serde_json::Value) -> Option<&str> {
    record
        .get("terminal_id")
        .and_then(serde_json::Value::as_str)
}

/// `agent.get` reporting that the terminal is not (or is no longer) an agent
/// terminal. `pane_not_found` is accepted too: a server that answered about the
/// pane has still said the agent is not addressable there, and the pane fallback
/// will say whether it is really gone.
fn is_agent_not_found(value: &serde_json::Value) -> bool {
    matches!(
        value["error"]["code"].as_str(),
        Some("agent_not_found") | Some("pane_not_found")
    )
}

/// `pane.get` reporting that the pane is gone. The ONLY error that means
/// `gone: closed`.
fn is_pane_not_found(value: &serde_json::Value) -> bool {
    matches!(value["error"]["code"].as_str(), Some("pane_not_found"))
}

/// The record inside a `pane_info` response, if the response carries one.
fn pane_record(value: &serde_json::Value) -> Option<&serde_json::Value> {
    value
        .get("result")
        .and_then(|result| result.get("pane"))
        .filter(|record| record.is_object())
}

fn sample_from_record(record: &serde_json::Value) -> Result<Sample, String> {
    let pane_id = record
        .get("pane_id")
        .and_then(serde_json::Value::as_str)
        .ok_or("the server's record named no pane")?
        .to_string();
    let status = record
        .get("agent_status")
        .and_then(serde_json::Value::as_str)
        .ok_or("the server's record named no agent_status")?;
    let status = parse_reported_status(status)?;
    // Hibernation is answered BEFORE the cursor is asked for. The status is
    // derived from the stashed resume plan rather than from a live turn, so
    // demanding a cursor first would turn "this agent is parked" into a parse
    // error about a field that has nothing to say here.
    if status == AgentStatus::Hibernated {
        return Ok(Sample::Hibernated { pane_id });
    }
    let cursor = record
        .get("turn_cursor")
        .and_then(serde_json::Value::as_str)
        .ok_or("the server sent no turn cursor; it predates #553")?;
    Ok(Sample::Record {
        pane_id,
        status,
        cursor: Cursor::parse(cursor)?,
    })
}

fn parse_reported_status(value: &str) -> Result<AgentStatus, String> {
    match value {
        "idle" => Ok(AgentStatus::Idle),
        "working" => Ok(AgentStatus::Working),
        "blocked" => Ok(AgentStatus::Blocked),
        "done" => Ok(AgentStatus::Done),
        "unknown" => Ok(AgentStatus::Unknown),
        "hibernated" => Ok(AgentStatus::Hibernated),
        other => Err(format!(
            "the server reported an agent status this build does not know: {other:?}"
        )),
    }
}

/// The wire spelling of a reported status — the same `snake_case` the schema
/// serializes, named once so a comparison and a result line cannot disagree.
fn reported_status_name(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Unknown => "unknown",
        AgentStatus::Hibernated => "hibernated",
    }
}

/// How long the next request may take, clamped to something a read timeout can
/// express and to the time the deadline has left.
fn request_timeout(deadline: Option<Instant>) -> Option<Duration> {
    match deadline {
        None => Some(REQUEST_TIMEOUT),
        Some(deadline) => {
            let left = deadline.saturating_duration_since(Instant::now());
            (!left.is_zero()).then(|| left.min(REQUEST_TIMEOUT).max(Duration::from_millis(1)))
        }
    }
}

/// Sleep one poll interval, or less if the deadline arrives first.
fn sleep_bounded(deadline: Option<Instant>) {
    let nap = match deadline {
        None => POLL_INTERVAL,
        Some(deadline) => deadline
            .saturating_duration_since(Instant::now())
            .min(POLL_INTERVAL),
    };
    std::thread::sleep(nap);
}
#[cfg(test)]
mod tests {
    use super::*;

    const TERM: &str = "term_1f2e3";

    fn cursor(epoch: u64, entries: u64, seq: u64, working: bool) -> Cursor {
        Cursor {
            terminal_id: TERM.to_string(),
            epoch,
            entries,
            seq,
            working,
            raw: format!(
                "{TERM}:{epoch}:{entries}:{seq}:{}",
                if working { "w" } else { "i" }
            ),
        }
    }

    fn sample(status: AgentStatus, cursor: Cursor) -> Sample {
        Sample::Record {
            pane_id: "w1:p1".into(),
            status,
            cursor,
        }
    }

    fn wait(after: Option<Cursor>, settle_ms: u64, timeout_ms: Option<u64>) -> SettledWait {
        let now = Instant::now();
        SettledWait::new(
            after,
            Duration::from_millis(settle_ms),
            timeout_ms.map(Duration::from_millis),
            TERM.to_string(),
            now,
        )
    }

    /// An idle agent with no `--after` is settled once it has held. This is the
    /// E2 shape: a supervisor that did not capture a cursor gets the next quiet.
    #[test]
    fn an_idle_agent_settles_once_it_has_held_the_window() {
        let mut settle = wait(None, 500, Some(30_000));
        let start = Instant::now();
        assert_eq!(
            settle.observe(sample(AgentStatus::Idle, cursor(0, 0, 1, false)), start),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 0, 1, false)),
                start + Duration::from_millis(499)
            ),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 0, 1, false)),
                start + Duration::from_millis(500)
            ),
            Step::Settled {
                held: Duration::from_millis(500)
            }
        );
    }

    /// E3, and the reason `--after` exists: an agent that has been sitting idle
    /// since before the prompt must NOT be read as "the turn I asked for is
    /// done". Without a working entry after the cursor, this waits out its
    /// whole timeout.
    #[test]
    fn an_idle_agent_that_never_started_is_not_settled() {
        let mut settle = wait(Some(cursor(0, 0, 1, false)), 500, Some(30_000));
        let start = Instant::now();
        for step in 0..100 {
            assert_eq!(
                settle.observe(
                    sample(AgentStatus::Idle, cursor(0, 0, 1, false)),
                    start + Duration::from_millis(step * 200)
                ),
                Step::Continue,
                "sample {step} must keep waiting"
            );
        }
    }

    /// E5: a whole working -> idle sequence that happened before the wait was
    /// issued still counts, because the cursor records it rather than an event
    /// the client had to be listening for.
    #[test]
    fn a_turn_that_finished_before_the_wait_still_counts() {
        let mut settle = wait(Some(cursor(0, 0, 1, false)), 500, Some(30_000));
        let start = Instant::now();
        // entries 1 and seq 3: one working entry and the working -> idle move.
        assert_eq!(
            settle.observe(sample(AgentStatus::Idle, cursor(0, 1, 3, false)), start),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 1, 3, false)),
                start + Duration::from_millis(500)
            ),
            Step::Settled {
                held: Duration::from_millis(500)
            }
        );
    }

    /// E15: a cursor captured mid-turn may have a follow-up already queued into
    /// that turn, so it settles at the next quiescence with no new entry — the
    /// reason the cursor ends in a `w`/`i` flag at all.
    #[test]
    fn a_cursor_captured_while_working_settles_at_the_next_quiescence() {
        let mut settle = wait(Some(cursor(0, 1, 2, true)), 500, Some(30_000));
        let start = Instant::now();
        assert_eq!(
            settle.observe(sample(AgentStatus::Working, cursor(0, 1, 2, true)), start),
            Step::Continue,
            "still working is never settled"
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 1, 3, false)),
                start + Duration::from_millis(100)
            ),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 1, 3, false)),
                start + Duration::from_millis(600)
            ),
            Step::Settled {
                held: Duration::from_millis(500)
            }
        );
    }

    /// E4: the dwell is anchored on `state_seq`, so an idle blip shorter than
    /// the window inside a turn does not settle — and the answer is measured
    /// from the LAST idle, not from the first.
    #[test]
    fn a_mid_turn_idle_blip_restarts_the_dwell() {
        let mut settle = wait(Some(cursor(0, 0, 1, false)), 500, Some(30_000));
        let start = Instant::now();
        assert_eq!(
            settle.observe(sample(AgentStatus::Working, cursor(0, 1, 2, true)), start),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 1, 3, false)),
                start + Duration::from_millis(200)
            ),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Working, cursor(0, 2, 4, true)),
                start + Duration::from_millis(400)
            ),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 2, 5, false)),
                start + Duration::from_millis(600)
            ),
            Step::Continue,
            "the blip's 200 ms must not count toward the last idle's 500 ms"
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 2, 5, false)),
                start + Duration::from_millis(1100)
            ),
            Step::Settled {
                held: Duration::from_millis(500)
            }
        );
    }

    /// E14, and the load-bearing case for anchoring on `state_seq` at all:
    /// `idle -> blocked -> idle` with no working entry anywhere resets the
    /// dwell. Three samples that agree on `idle` again are not one idle.
    #[test]
    fn a_non_working_excursion_restarts_the_dwell() {
        let mut settle = wait(Some(cursor(0, 0, 1, false)), 500, Some(30_000));
        let start = Instant::now();
        assert_eq!(
            settle.observe(sample(AgentStatus::Idle, cursor(0, 1, 3, false)), start),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Blocked, cursor(0, 1, 4, false)),
                start + Duration::from_millis(300)
            ),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 1, 5, false)),
                start + Duration::from_millis(310)
            ),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 1, 5, false)),
                start + Duration::from_millis(800)
            ),
            Step::Continue,
            "the excursion moved the anchor 490 ms ago, not 500"
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 1, 5, false)),
                start + Duration::from_millis(810)
            ),
            Step::Settled {
                held: Duration::from_millis(500)
            }
        );
    }

    /// E6 / I4: a held `blocked` is its own outcome. It never settles, however
    /// long it holds, because a blocked agent is waiting on a human and calling
    /// that quiescence would hand a supervisor a green light to walk away.
    #[test]
    fn blocked_is_its_own_outcome_and_never_settles() {
        let mut settle = wait(Some(cursor(0, 0, 1, false)), 500, Some(30_000));
        let start = Instant::now();
        assert_eq!(
            settle.observe(sample(AgentStatus::Blocked, cursor(0, 0, 2, false)), start),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Blocked, cursor(0, 0, 2, false)),
                start + Duration::from_millis(500)
            ),
            Step::Blocked {
                held: Duration::from_millis(500)
            }
        );
    }

    /// The case a status-only dwell cannot see coming: consecutive samples that
    /// all report `idle`, with the transition counter moving between them.
    ///
    /// Something happened between two polls that the poll itself never caught —
    /// `idle -> blocked -> idle`, a sub-sample flicker, a `working` phase the
    /// detector missed — and by the time this wait looks again the pane reads
    /// exactly as it did before. Three agreeing samples are not one unbroken
    /// quiet, and the window has to start at the last `state_seq` it saw.
    #[test]
    fn an_unseen_excursion_between_two_idle_samples_restarts_the_window() {
        let mut settle = wait(None, 500, Some(30_000));
        let start = Instant::now();
        // seq 7 idle, then seq 8 idle: identical status, moved counter.
        assert_eq!(
            settle.observe(sample(AgentStatus::Idle, cursor(0, 0, 7, false)), start),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 0, 7, false)),
                start + Duration::from_millis(400)
            ),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 0, 8, false)),
                start + Duration::from_millis(410)
            ),
            Step::Continue,
            "a moved state_seq is a transition nobody sampled, so the dwell restarts here"
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 0, 8, false)),
                start + Duration::from_millis(850)
            ),
            Step::Continue,
            "490 ms since the change is not a full window"
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 0, 8, false)),
                start + Duration::from_millis(910)
            ),
            Step::Settled {
                held: Duration::from_millis(500)
            }
        );
    }

    /// A `done` is an effective idle — the same quiescence as `idle`, reached by
    /// an agent nobody is watching. It must settle, and must be reachable from
    /// either spelling.
    #[test]
    fn done_is_an_effective_idle_and_settles_like_one() {
        let mut settle = wait(Some(cursor(0, 0, 1, false)), 500, Some(30_000));
        let start = Instant::now();
        assert_eq!(
            settle.observe(sample(AgentStatus::Working, cursor(0, 1, 2, true)), start),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Done, cursor(0, 1, 3, false)),
                start + Duration::from_millis(10)
            ),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Done, cursor(0, 1, 3, false)),
                start + Duration::from_millis(510)
            ),
            Step::Settled {
                held: Duration::from_millis(500)
            }
        );
    }

    /// The deadline wins over a late success. A wait whose clock ran out must
    /// not then report the quiet it found on the way past — the caller asked
    /// "settled within my budget", and the answer to that was no.
    #[test]
    fn the_deadline_beats_a_late_success() {
        let mut settle = wait(None, 500, Some(1_000));
        let start = Instant::now();
        assert_eq!(
            settle.observe(sample(AgentStatus::Idle, cursor(0, 0, 1, false)), start),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 0, 1, false)),
                start + Duration::from_millis(1_000)
            ),
            Step::TimedOut,
            "a sample landing exactly on the deadline is already too late"
        );
    }

    /// `--settle 0` settles on the first qualifying sample's successor: the
    /// anchor has to be observed once before there is a window to have elapsed.
    #[test]
    fn a_zero_settle_window_settles_on_the_next_sample() {
        let mut settle = wait(None, 0, Some(30_000));
        let start = Instant::now();
        assert_eq!(
            settle.observe(sample(AgentStatus::Idle, cursor(0, 0, 1, false)), start),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(0, 0, 1, false)),
                start + Duration::from_millis(1)
            ),
            Step::Settled {
                held: Duration::from_millis(1)
            }
        );
    }

    /// I3: a cursor from another terminal, from another machine, or from a
    /// server whose counters were reset names counters this one has not reached.
    /// Refusing it is the whole point — read as "already satisfied" it would
    /// report a turn nobody ran.
    #[test]
    fn a_cursor_the_server_has_not_reached_is_refused() {
        let start = Instant::now();
        for bad in [
            cursor(1, 0, 1, false),
            cursor(0, 5, 1, false),
            cursor(0, 0, 9, false),
        ] {
            let mut settle = wait(Some(bad.clone()), 500, Some(30_000));
            assert_eq!(
                settle.observe(sample(AgentStatus::Idle, cursor(0, 0, 1, false)), start),
                Step::Refused("turn cursor is ahead of the server".into()),
                "{bad:?} is ahead of the server"
            );
        }
    }

    /// #553 round 1: a respawn must end the wait even with NO cursor.
    ///
    /// The epoch comparison against `--after` cannot catch this: without a cursor
    /// there is nothing to compare, so a wait would settle on whatever the NEW
    /// child happened to do and report a turn nobody asked it to run. The
    /// execution this wait started watching is remembered instead.
    #[test]
    fn a_respawn_is_gone_restarted_even_without_a_cursor() {
        let mut settle = wait(None, 500, Some(30_000));
        let start = Instant::now();
        // First sample establishes the execution at epoch 0.
        assert_eq!(
            settle.observe(sample(AgentStatus::Idle, cursor(0, 0, 1, false)), start),
            Step::Continue
        );
        // A higher epoch later is a different child behind the same terminal id.
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(1, 0, 0, false)),
                start + Duration::from_millis(200)
            ),
            Step::Gone(GoneReason::Restarted)
        );
    }

    /// The remembered epoch must not fire on a LOWER one, and an unchanged epoch
    /// must keep the dwell running — otherwise the check above would replace a
    /// timeout with a false `restarted`.
    #[test]
    fn an_unchanged_epoch_leaves_the_dwell_alone() {
        let mut settle = wait(None, 500, Some(30_000));
        let start = Instant::now();
        assert_eq!(
            settle.observe(sample(AgentStatus::Idle, cursor(4, 0, 1, false)), start),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(4, 0, 1, false)),
                start + Duration::from_millis(300)
            ),
            Step::Continue
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(4, 0, 1, false)),
                start + Duration::from_millis(500)
            ),
            Step::Settled {
                held: Duration::from_millis(500)
            }
        );
    }

    /// The other direction: a cursor from before a respawn names an execution
    /// that no longer exists. That is `gone`, because the agent it was watching
    /// is not the agent that would answer.
    #[test]
    fn a_cursor_from_before_a_restart_is_gone_restarted() {
        let mut settle = wait(Some(cursor(0, 0, 1, false)), 500, Some(30_000));
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(1, 0, 0, false)),
                Instant::now()
            ),
            Step::Gone(GoneReason::Restarted)
        );
    }

    /// The result line is the contract for exit 0/3/4: one JSON object, and a
    /// `gone` line that says WHY, because "gone" alone leaves a supervisor
    /// guessing between a closed pane and one it should go and look at.
    #[test]
    fn the_result_line_names_the_outcome_and_its_reason() {
        let mut settle = wait(None, 500, Some(30_000));
        let seen = sample(AgentStatus::Idle, cursor(0, 0, 1, false));
        settle.observe(seen, Instant::now());
        let line: serde_json::Value =
            serde_json::from_str(&settle.result_line("gone", 0, Some(GoneReason::Restarted)))
                .unwrap();
        assert_eq!(line["status"], "gone");
        assert_eq!(line["pane_id"], "w1:p1");
        assert_eq!(line["agent_status"], "idle");
        assert_eq!(line["turn_cursor"], format!("{TERM}:0:0:1:i"));
        assert_eq!(line["held_ms"], 0);
        assert_eq!(line["reason"], "restarted");

        // Never having seen a record, the line says so rather than inventing one.
        let unseen = wait(None, 500, Some(30_000));
        let line: serde_json::Value =
            serde_json::from_str(&unseen.result_line("gone", 0, Some(GoneReason::Closed))).unwrap();
        assert!(line["agent_status"].is_null());
        assert!(line["turn_cursor"].is_null());
    }

    /// A cursor is opaque, so it is parsed by shape and from the right. The
    /// terminal id is whatever the server minted and must never be guessed at.
    #[test]
    fn cursors_are_parsed_from_the_right() {
        let parsed = Cursor::parse("term_1f2e3:2:7:11:w").expect("a cursor this build mints");
        assert_eq!(parsed.terminal_id, "term_1f2e3");
        assert_eq!((parsed.epoch, parsed.entries, parsed.seq), (2, 7, 11));
        assert!(parsed.working);
        assert!(!Cursor::parse("term_1f2e3:2:7:11:i").unwrap().working);

        for bad in [
            "not-a-cursor",
            "term_1f2e3:2:7:11",
            "term_1f2e3:x:7:11:i",
            "term_1f2e3:2:7:11:x",
            ":2:7:11:i",
        ] {
            let err = Cursor::parse(bad).expect_err("refuse a cursor this build did not mint");
            assert!(err.contains("cursor"), "{bad}: {err}");
        }
    }

    /// One vocabulary, and one meaning per word — in both verbs, because they
    /// call this one parser.
    #[test]
    fn idle_means_ready_for_input_and_done_is_a_real_target() {
        assert_eq!(
            WaitTarget::parse("idle"),
            Ok(WaitTarget::Status(AgentStatus::Idle))
        );
        assert_eq!(
            WaitTarget::parse("done"),
            Ok(WaitTarget::Status(AgentStatus::Done))
        );
        assert_eq!(
            WaitTarget::parse("hibernated"),
            Ok(WaitTarget::Status(AgentStatus::Hibernated))
        );
        assert_eq!(WaitTarget::parse("settled"), Ok(WaitTarget::Settled));
        assert!(WaitTarget::parse("nonsense").is_err());

        let idle = WaitTarget::Status(AgentStatus::Idle);
        assert!(idle.satisfied_by("idle") && idle.satisfied_by("done"));
        assert!(!idle.satisfied_by("working"));
        assert_eq!(
            idle.watched_statuses(),
            vec![AgentStatus::Idle, AgentStatus::Done],
            "a subscription filters on one exact status, so 'ready for input' is the pair"
        );
        assert_eq!(
            WaitTarget::Status(AgentStatus::Done).watched_statuses(),
            vec![AgentStatus::Done]
        );
    }

    /// The settle-only flags are a property of the TARGET, so both verbs refuse
    /// them identically, before anything is waited on.
    #[test]
    fn settle_flags_are_refused_without_settled() {
        let after = SettleFlags {
            after: Some("term_1:0:0:1:i".into()),
            settle_ms: None,
        };
        assert!(check_settle_flags(Some(WaitTarget::Settled), &after).is_ok());
        for status in [
            "idle",
            "working",
            "blocked",
            "done",
            "unknown",
            "hibernated",
        ] {
            let target = WaitTarget::parse(status).unwrap();
            assert!(
                check_settle_flags(Some(target), &after).is_err(),
                "{status} --after"
            );
        }
        let settle = SettleFlags {
            after: None,
            settle_ms: Some(100),
        };
        assert!(check_settle_flags(Some(WaitTarget::Status(AgentStatus::Idle)), &settle).is_err());
        assert!(
            check_settle_flags(None, &settle).is_err(),
            "--ready --settle"
        );
        assert!(check_settle_flags(None, &SettleFlags::default()).is_ok());
    }

    fn response(record: &str) -> serde_json::Value {
        serde_json::from_str(record).expect("fixture json")
    }

    /// The respawn path, end to end through the decision the fetch makes: a
    /// respawn clears the pane's agent identity, so `agent.get` answers
    /// `agent_not_found` — and the wait must read the PANE record (same
    /// terminal id, moved epoch) rather than call the pane closed. This is the
    /// only reason the pane fallback exists, and it is why the cursor carries
    /// an execution epoch at all.
    #[test]
    fn a_respawn_reports_restarted_through_the_pinned_fallback() {
        let before = Cursor::parse("term_1f2e3:0:1:4:i").unwrap();
        let mut settle = wait(Some(before), 500, Some(30_000));
        let agent = response(r#"{"error":{"code":"agent_not_found","message":"gone"}}"#);
        let pane = response(
            r#"{"result":{"pane":{"pane_id":"w1:p1","terminal_id":"term_1f2e3",
                "agent_status":"unknown","turn_cursor":"term_1f2e3:1:1:5:i"}}}"#,
        );
        let sample = classify_pinned(TERM, &agent, Some(&pane)).expect("the pane still answers");
        assert_eq!(
            settle.observe(sample, Instant::now()),
            Step::Gone(GoneReason::Restarted),
            "same terminal id, new execution: gone, not closed and not settled"
        );
    }

    /// The same fallback for a pane that is still alive and was never an agent:
    /// it is a record to wait on, not a gone line.
    #[test]
    fn a_pane_that_is_not_an_agent_still_answers_from_the_pane_record() {
        let settle = wait(None, 500, Some(30_000));
        let agent = response(r#"{"error":{"code":"agent_not_found","message":"no agent here"}}"#);
        let pane = response(
            r#"{"result":{"pane":{"pane_id":"w1:p1","terminal_id":"term_1f2e3",
                "agent_status":"idle","turn_cursor":"term_1f2e3:0:0:1:i"}}}"#,
        );
        assert_eq!(
            classify_pinned(TERM, &agent, Some(&pane)).expect("a live pane answers"),
            sample(AgentStatus::Idle, cursor(0, 0, 1, false))
        );
        let _ = settle;
    }

    /// `gone: closed` is reserved for a pane that really is gone. Three ways to
    /// get there and one that is only a hiccup: a transport failure is not an
    /// answer, and treating it as one would end a wait during a live handoff.
    #[test]
    fn only_a_really_gone_pane_reports_closed() {
        let agent = response(r#"{"error":{"code":"agent_not_found","message":"gone"}}"#);
        assert_eq!(
            classify_pinned(TERM, &agent, None).expect_err("a handoff is not an answer"),
            PinnedFailure::Retry
        );
        let missing = response(r#"{"error":{"code":"pane_not_found","message":"gone"}}"#);
        assert_eq!(
            classify_pinned(TERM, &agent, Some(&missing)).expect("the pane really is gone"),
            Sample::Gone(GoneReason::Closed)
        );
        // The pane id was reused by a different terminal: still not this agent.
        let reused = response(
            r#"{"result":{"pane":{"pane_id":"w1:p1","terminal_id":"term_other",
                "agent_status":"idle","turn_cursor":"term_other:0:0:1:i"}}}"#,
        );
        assert_eq!(
            classify_pinned(TERM, &agent, Some(&reused))
                .expect("a reused pane id is not this agent"),
            Sample::Gone(GoneReason::Closed)
        );
    }

    /// A server error that is not "not found" is reported with the server's own
    /// words, on the server's own error path — never as a gone line and never
    /// as a timeout, which would both be lies about an agent that is fine.
    #[test]
    fn a_server_error_is_never_disguised_as_a_gone_pane() {
        let agent = response(r#"{"error":{"code":"permission_denied","message":"nope"}}"#);
        match classify_pinned(TERM, &agent, None) {
            Err(PinnedFailure::Fatal(reason)) => {
                assert!(reason.contains("permission_denied"), "{reason}")
            }
            other => panic!("a permission error is not a sample: {other:?}"),
        }
    }

    /// #553 round 1: only `pane_not_found` is the pane's own answer. Any other
    /// error from the fallback is a flock-side fault, and reporting it as
    /// `gone: closed` would tell the caller its agent vanished when what
    /// actually happened is that flock could not answer.
    #[test]
    fn only_pane_not_found_from_the_fallback_means_the_pane_is_closed() {
        let agent = response(r#"{"error":{"code":"agent_not_found","message":"no agent here"}}"#);
        for code in [
            "permission_denied",
            "internal_error",
            "rate_limited",
            "protocol_mismatch",
        ] {
            let pane = response(&format!(
                r#"{{"error":{{"code":"{code}","message":"flock could not answer"}}}}"#
            ));
            match classify_pinned(TERM, &agent, Some(&pane)) {
                Err(PinnedFailure::Fatal(reason)) => {
                    assert!(
                        reason.contains(code),
                        "{code} must be reported, got {reason}"
                    )
                }
                other => panic!("{code} must not read as a gone pane: {other:?}"),
            }
        }

        // The one that does mean it.
        let missing = response(r#"{"error":{"code":"pane_not_found","message":"gone"}}"#);
        assert_eq!(
            classify_pinned(TERM, &agent, Some(&missing)).expect("the pane really is gone"),
            Sample::Gone(GoneReason::Closed)
        );
    }

    /// #553 round 1: a success response that carries no agent record, or one
    /// whose record names no terminal, is a MALFORMED reply. Only a record that
    /// positively names a different terminal means the child was replaced;
    /// calling either case `restarted` would report an agent as replaced when
    /// the server simply answered wrong.
    #[test]
    fn a_malformed_success_is_fatal_not_restarted() {
        for reply in [
            r#"{"result":{}}"#,
            r#"{"result":{"agent":null}}"#,
            r#"{"result":{"agent":{}}}"#,
            r#"{"result":{"agent":{"pane_id":"w1:p1"}}}"#,
        ] {
            let agent = response(reply);
            match classify_pinned(TERM, &agent, None) {
                Err(PinnedFailure::Fatal(reason)) => assert_eq!(reason, NO_AGENT_RECORD, "{reply}"),
                other => panic!("{reply} is not a restart: {other:?}"),
            }
        }

        // The one case that IS a restart: a record naming another terminal.
        let other = response(
            r#"{"result":{"agent":{"pane_id":"w1:p1","terminal_id":"term_elsewhere",
                "agent_status":"idle","turn_cursor":"term_elsewhere:0:0:1:i"}}}"#,
        );
        assert_eq!(
            classify_pinned(TERM, &other, None).expect("a positive mismatch is a restart"),
            Sample::Gone(GoneReason::Restarted)
        );
    }

    /// #553 round 1: hibernation is answered BEFORE the cursor is required. The
    /// record below carries no `turn_cursor` at all — a hibernated agent is
    /// reported from the stashed plan and has no live turn — and demanding one
    /// first would have turned "your agent is parked" into a parse error about a
    /// field that has nothing to say here.
    #[test]
    fn a_hibernated_agent_needs_no_turn_cursor_to_be_reported_as_gone() {
        let agent = response(
            r#"{"result":{"agent":{"pane_id":"w1:p1","terminal_id":"term_1f2e3",
                "agent_status":"hibernated"}}}"#,
        );
        let fetched = classify_pinned(TERM, &agent, None).expect("a hibernated record is a sample");
        assert_eq!(
            fetched,
            Sample::Hibernated {
                pane_id: "w1:p1".into()
            }
        );

        let mut settle = wait(None, 500, Some(30_000));
        assert_eq!(
            settle.observe(fetched, Instant::now()),
            Step::Gone(GoneReason::Hibernated)
        );
        // The result line names the pane and says the cursor is unknown, rather
        // than reporting the wait as if it had never seen anything.
        let line: serde_json::Value =
            serde_json::from_str(&settle.result_line("gone", 0, Some(GoneReason::Hibernated)))
                .unwrap();
        assert_eq!(line["pane_id"], "w1:p1");
        assert_eq!(line["agent_status"], "hibernated");
        assert!(line["turn_cursor"].is_null());
        assert_eq!(line["held_ms"], 0);
    }

    /// A scripted transport, so the two-request sequence is reachable without a
    /// server. `cost` is slept per reply, which is what makes the deadline tests
    /// real rather than simulated.
    struct Scripted {
        replies: std::collections::VecDeque<Result<serde_json::Value, ()>>,
        cost: Duration,
        /// The timeout each request was actually granted, in order.
        granted: Vec<Duration>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<serde_json::Value, ()>>, cost: Duration) -> Self {
            Self {
                replies: replies.into(),
                cost,
                granted: Vec::new(),
            }
        }

        fn next(&mut self, timeout: Duration) -> Result<serde_json::Value, ()> {
            self.granted.push(timeout);
            std::thread::sleep(self.cost);
            self.replies
                .pop_front()
                .expect("a scripted reply for every request the wait makes")
        }
    }

    impl PinnedRequests for Scripted {
        fn agent_get(
            &mut self,
            _terminal_id: &str,
            timeout: Duration,
        ) -> Result<serde_json::Value, ()> {
            self.next(timeout)
        }

        fn pane_get(&mut self, _pane_id: &str, timeout: Duration) -> Result<serde_json::Value, ()> {
            self.next(timeout)
        }
    }

    /// #553 round 1: the respawn path, through `sample_pinned` rather than the
    /// classifier — so the fallback REQUEST is part of what is under test.
    ///
    /// A respawn clears the pane's agent identity, so `agent.get` answers
    /// `agent_not_found` and the wait has to fall back to `pane.get` for a
    /// record that names the SAME terminal id at a HIGHER epoch. Reading that as
    /// "the pane closed" would be a lie: the pane is right there, with a new
    /// child in it. This is the only reason the pane fallback exists.
    #[test]
    fn a_respawn_is_restarted_through_the_pinned_fetch_itself() {
        let mut settle = wait(None, 500, Some(30_000));
        // Seed the remembered pane id the fallback looks up.
        settle.observe(
            sample(AgentStatus::Idle, cursor(0, 0, 1, false)),
            Instant::now(),
        );

        let mut requests = Scripted::new(
            vec![
                Ok(response(
                    r#"{"error":{"code":"agent_not_found","message":"no agent here"}}"#,
                )),
                Ok(response(
                    r#"{"result":{"pane":{"pane_id":"w1:p1","terminal_id":"term_1f2e3",
                        "agent_status":"unknown","turn_cursor":"term_1f2e3:1:0:0:i"}}}"#,
                )),
            ],
            Duration::ZERO,
        );
        let fetched = sample_pinned(
            &mut requests,
            &settle,
            Some(Instant::now() + Duration::from_secs(5)),
        )
        .expect("the pane fallback answers");

        // The record arrived intact — it is the EPOCH, not a missing field, that
        // says the child was replaced.
        match &fetched {
            Sample::Record {
                status,
                cursor: seen,
                ..
            } => {
                assert_eq!(*status, AgentStatus::Unknown);
                assert_eq!(seen.epoch, 1);
            }
            other => panic!("expected a record, got {other:?}"),
        }
        assert_eq!(
            settle.observe(fetched, Instant::now()),
            Step::Gone(GoneReason::Restarted),
            "same terminal id, new execution: gone, not closed and not settled"
        );
        assert_eq!(requests.granted.len(), 2, "the respawn costs both requests");
    }

    /// #553 round 1: the fallback is a SECOND round trip, so it must be given
    /// what is left of the budget rather than the first request's budget — and a
    /// reply that lands after the deadline is a timeout whichever way it went.
    ///
    /// Two replies that each eat most of the remaining time is the only way to
    /// tell "recomputed before each request" apart from "reused the first
    /// request's timeout": under the latter the second request would be granted a
    /// budget that had already expired, and this would settle instead of timing
    /// out.
    #[test]
    fn the_deadline_covers_both_requests_and_outranks_the_second_reply() {
        let mut settle = wait(None, 500, Some(30_000));
        settle.observe(
            sample(AgentStatus::Idle, cursor(0, 0, 1, false)),
            Instant::now(),
        );

        // 120 ms of budget, 80 ms per reply: the first fits, the pair does not.
        let deadline = Instant::now() + Duration::from_millis(120);
        let mut requests = Scripted::new(
            vec![
                Ok(response(
                    r#"{"error":{"code":"agent_not_found","message":"no agent here"}}"#,
                )),
                Ok(response(
                    r#"{"result":{"pane":{"pane_id":"w1:p1","terminal_id":"term_1f2e3",
                        "agent_status":"idle","turn_cursor":"term_1f2e3:0:0:1:i"}}}"#,
                )),
            ],
            Duration::from_millis(80),
        );
        assert_eq!(
            sample_pinned(&mut requests, &settle, Some(deadline))
                .expect_err("both replies land past the deadline"),
            PinnedFailure::TimedOut,
            "a record that arrives too late is a timeout, not a settle"
        );
        assert_eq!(requests.granted.len(), 2, "both requests were issued");
        assert!(
            requests.granted[1] < requests.granted[0],
            "the fallback must be given what is LEFT, not the first request's budget: {:?}",
            requests.granted
        );
        assert!(
            requests.granted[1] <= Duration::from_millis(60),
            "and what is left is under the 80 ms the first reply cost: {:?}",
            requests.granted
        );
    }

    /// The same precedence for the ERROR direction: a server error arriving after
    /// the deadline is a timeout, because by then nothing has been established
    /// about the agent.
    #[test]
    fn an_error_arriving_after_the_deadline_is_a_timeout_not_a_fatal() {
        let mut settle = wait(None, 500, Some(30_000));
        settle.observe(
            sample(AgentStatus::Idle, cursor(0, 0, 1, false)),
            Instant::now(),
        );

        let deadline = Instant::now() + Duration::from_millis(40);
        let mut requests = Scripted::new(
            vec![Ok(response(
                r#"{"error":{"code":"permission_denied","message":"nope"}}"#,
            ))],
            Duration::from_millis(80),
        );
        assert_eq!(
            sample_pinned(&mut requests, &settle, Some(deadline)).expect_err("the reply is late"),
            PinnedFailure::TimedOut
        );
        assert_eq!(
            requests.granted.len(),
            1,
            "an error short-circuits the fallback"
        );
    }

    /// No time left at all: the request is skipped rather than issued with a
    /// nonsense timeout, so the wait takes the timeout path instead of blocking
    /// on a socket it cannot afford to read.
    #[test]
    fn an_exhausted_deadline_skips_the_request_entirely() {
        let settle = wait(None, 500, Some(30_000));
        let mut requests = Scripted::new(
            vec![Ok(response(
                r#"{"result":{"agent":{"pane_id":"w1:p1","terminal_id":"term_1f2e3",
                    "agent_status":"idle","turn_cursor":"term_1f2e3:0:0:1:i"}}}"#,
            ))],
            Duration::ZERO,
        );
        let past = Instant::now() - Duration::from_millis(1);
        assert_eq!(
            sample_pinned(&mut requests, &settle, Some(past)).expect_err("no budget left"),
            PinnedFailure::TimedOut
        );
        assert!(
            requests.granted.is_empty(),
            "nothing was sent: {:?}",
            requests.granted
        );
    }
}
