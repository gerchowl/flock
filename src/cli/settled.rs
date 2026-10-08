//! #553 — the `settled` wait, and the one status vocabulary both wait verbs
//! speak.
//!
//! ## What `settled` means, and what it does not
//!
//! `settled` is **observed quiescence**: after a cursor, the server saw this
//! agent enter `working` (or the cursor was captured while it was working), and
//! then saw its reported status hold `idle`/`done` with **no state transition at
//! all** for the settle window. It is a statement about what flock observed, not
//! about what the agent did. Read the result of a turn with `flk agent result`.
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

/// How long the wait keeps retrying a server it cannot REACH, measured across
/// back-to-back transport failures and separate from `--timeout`.
///
/// The retry exists for a live handoff, whose gap is a few seconds, so 30 s is
/// generous enough to ride out any of them and short enough that a supervisor
/// waiting without `--timeout` on a server that died gets an answer instead of
/// retrying forever (#614).
const UNREACHABLE_LIMIT: Duration = Duration::from_secs(30);

/// `flk agent wait` / `flk wait agent-status` settled exit codes.
///
/// Distinct from the plain-status waits (which keep their historical codes,
/// including `1` for a timeout) because a supervisor driving these has to tell
/// four outcomes apart without parsing prose: it settled, it needs a human, the
/// pane went away, or it ran out of time.
pub(super) mod exit {
    // `pub(crate)` rather than `pub(super)`: #578's `flk delegate` reports the
    // same four outcomes with the same codes, and a delegate that answered
    // `blocked` with a code of its own would send a supervisor to a different
    // remedy for the same signal. The codes themselves are unchanged, and 5 and
    // 6 (the delegate's own) deliberately live in `cli::delegate` instead.
    pub(crate) const SETTLED: i32 = 0;
    pub(crate) const BLOCKED: i32 = 3;
    pub(crate) const GONE: i32 = 4;
    pub(crate) const TIMEOUT: i32 = 124;
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

/// A target the caller has ALREADY pinned, so there is nothing to resolve.
///
/// The delegate owns a terminal id and a pane id it captured at start, and the
/// pane may well be gone by the time it polls. Resolving a name here would
/// answer about whatever holds the name NOW, which for a delegate is not the
/// thing being waited on: it is the difference between a pane that closed
/// (exit 4) and a new agent that inherited a recycled pane id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PinnedTarget {
    pub(super) terminal_id: String,
    /// Only used for the `pane.get` fallback and for the initial `last`, so a
    /// closed pane can still be asked about by id once.
    pub(super) pane_id: String,
}

/// Where a settled core starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SettleTarget<'a> {
    /// Resolve a name or pane id first (the two existing wait verbs).
    Resolve(InitialTarget<'a>),
    /// The caller already knows the terminal, and a miss is `gone` rather than
    /// a resolve failure.
    Pinned(PinnedTarget),
}

/// The most recent record a settle saw, carried on every outcome so a terminal
/// answer can still name what it saw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct LastRecord {
    pub(super) pane_id: Option<String>,
    pub(super) agent_status: Option<String>,
    pub(super) turn_cursor: Option<String>,
}

/// What a settled core concluded, before anything is printed (#578 P17).
///
/// The two wait verbs print this through [`print_settled`]; the delegate maps it
/// into its own outcome object. Keeping the decision and the rendering apart is
/// what lets three verbs share one settle loop without any of them re-deciding
/// an outcome in its own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SettledOutcome {
    /// Quiescence held. The cursor is the one that settled, which is what a
    /// caller resuming from it must wait past.
    Settled { held_ms: u64, last: LastRecord },
    /// A held `blocked`: a human has to look, so it never settles.
    Blocked { held_ms: u64, last: LastRecord },
    /// The delegate observer found a stall. Other wait verbs install no observer.
    Stalled { last: LastRecord },
    /// The pane, the agent or the execution is over.
    Gone {
        reason: GoneReason,
        last: LastRecord,
    },
    /// A cursor the server has not reached, or one naming another terminal.
    Refused(String),
    /// The deadline arrived first, on any phase.
    TimedOut,
    /// The server REFUSED the initial resolve, and its response is carried whole.
    ///
    /// Its own arm, not a string inside [`Self::Error`], because at the base this
    /// printed `eprintln!("{response}")` — the raw server reply with no verb
    /// prefix — while every other failure printed `{verb}: {reason}`. Folding the
    /// two together put a `wait agent-status: ` in front of a line that had never
    /// carried one, and the two verbs' refusals were no longer the bytes a caller
    /// had been matching on (R5).
    ServerRefused(String),
    /// The server answered with something this client cannot act on.
    Error(String),
}

impl SettledOutcome {
    /// The record the wait last saw.
    pub(super) fn last(&self) -> Option<&LastRecord> {
        match self {
            Self::Settled { last, .. }
            | Self::Blocked { last, .. }
            | Self::Gone { last, .. }
            | Self::Stalled { last } => Some(last),
            Self::Refused(_) | Self::TimedOut | Self::ServerRefused(_) | Self::Error(_) => None,
        }
    }

    /// The cursor the wait ended on. `None` when it never saw a record.
    pub(super) fn turn_cursor(&self) -> Option<&str> {
        self.last().and_then(|last| last.turn_cursor.as_deref())
    }

    /// The settled core's own exit code, for the two existing verbs.
    ///
    /// `print_settled` uses this so their behaviour cannot drift: every arm
    /// below is the mapping those verbs had before the split.
    fn exit_code(&self) -> i32 {
        match self {
            Self::Stalled { .. } => 7,
            Self::Settled { .. } => exit::SETTLED,
            Self::Blocked { .. } => exit::BLOCKED,
            Self::Gone { .. } => exit::GONE,
            Self::Refused(_) => 2,
            Self::TimedOut => exit::TIMEOUT,
            Self::ServerRefused(_) | Self::Error(_) => 1,
        }
    }
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

    /// Give the wait a pane id to fall back on before it has seen any record.
    ///
    /// A pinned caller (the delegate) already knows the pane; without this the
    /// first `pane.get` fallback would ask about the empty string and read a
    /// handoff as `gone: closed`.
    pub(super) fn seed_pane(&mut self, pane_id: &str) {
        if self.last.is_none() && !pane_id.is_empty() {
            self.last = Some((pane_id.to_string(), AgentStatus::Unknown, None));
        }
    }

    /// The last record in the shape the result line takes.
    pub(super) fn last_record(&self) -> LastRecord {
        match &self.last {
            Some((pane_id, agent_status, cursor)) => LastRecord {
                pane_id: Some(pane_id.clone()),
                agent_status: Some(reported_status_name(*agent_status).to_string()),
                turn_cursor: cursor.clone(),
            },
            None => LastRecord::default(),
        }
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
            // Defence in depth. `sample_from_record` answers hibernation from the
            // status alone, before it ever asks for a cursor, so a hibernated
            // agent arrives as `Sample::Hibernated` and never reaches here. If a
            // future caller ever hands `observe` a full record that says
            // `hibernated`, it must still not become `Waiting`: that is how an
            // agent with no process behind it would settle.
            AgentStatus::Hibernated => return Step::Gone(GoneReason::Hibernated),
            AgentStatus::Working | AgentStatus::Unknown => Class::Waiting,
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

/// Run the settled wait for `flk agent wait` and `flk wait agent-status`.
/// One implementation; both verbs land here.
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
    let outcome = settled_wait(
        verb,
        SettleTarget::Resolve(target),
        after,
        settle_ms,
        timeout_ms,
    )?;
    print_settled(verb, &outcome);
    Ok(outcome.exit_code())
}

/// Print a settled outcome the way both settled wait verbs always have.
///
/// The rendering half of the split: the words, the stream and the exit code are
/// decided here and nowhere else, so the delegate (which prints its own object
/// instead) cannot change what either existing verb says.
pub(super) fn print_settled(verb: &str, outcome: &SettledOutcome) {
    match outcome {
        SettledOutcome::Settled { held_ms, last } => {
            println!("{}", settled_result_line("settled", *held_ms, None, last));
        }
        SettledOutcome::Blocked { held_ms, last } => {
            println!("{}", settled_result_line("blocked", *held_ms, None, last));
        }
        SettledOutcome::Gone { reason, last } => {
            println!("{}", settled_result_line("gone", 0, Some(*reason), last));
        }
        SettledOutcome::Stalled { last } => {
            println!("{}", settled_result_line("stalled", 0, None, last));
        }
        SettledOutcome::Refused(reason) => eprintln!("{verb}: {reason}"),
        SettledOutcome::TimedOut => eprintln!("timed out waiting for the agent to settle"),
        // Verbatim, with no verb prefix: the bytes at the base.
        SettledOutcome::ServerRefused(response) => eprintln!("{response}"),
        SettledOutcome::Error(reason) => eprintln!("{verb}: {reason}"),
    }
}

/// The one JSON object both settled verbs print on exit 0, 3 and 4.
///
/// Taken off [`SettledWait`] because the outcome outlives the state machine: a
/// caller that resumes from a settle needs the line, not the machine that
/// produced it. `SettledWait::result_line` is the same object over a live wait,
/// and both call this so one shape cannot drift from the other.
///
/// `agent_status` and `turn_cursor` are null when nothing was ever seen, rather
/// than invented: a `gone` answer about a pane that never resolved has to be
/// able to say so.
pub(super) fn settled_result_line(
    status: &str,
    held_ms: u64,
    reason: Option<GoneReason>,
    last: &LastRecord,
) -> String {
    let mut line = serde_json::json!({
        "status": status,
        "pane_id": last.pane_id,
        "agent_status": last.agent_status,
        "turn_cursor": last.turn_cursor,
        "held_ms": held_ms,
    });
    if let Some(reason) = reason {
        line["reason"] = serde_json::Value::String(reason.as_str().to_string());
    }
    line.to_string()
}

/// The settled core: run the wait and say what concluded, printing nothing.
///
/// The single place the settle decision is made. `run_settled_wait` prints it
/// for the two existing verbs; `delegate` reads it and maps it into the
/// delegate's own outcome object (P17), which is what keeps a delegate's `gone`
/// and its `agent_blocked` decided by this state machine rather than by a
/// second, subtly different loop beside it.
///
/// One outcome carries the server's response VERBATIM, because two of the verbs
/// that used to do this work printed it that way and a caller matching on those
/// bytes is still matching on them.
///
/// Only a transport failure escapes, because that is the one thing a caller
/// cannot turn into an outcome: the two existing verbs have always propagated
/// it as an io error and keep doing so. A transport failure that never recovers
/// is not an escape, though — see [`UNREACHABLE_LIMIT`].
pub(super) fn settled_wait(
    verb: &str,
    target: SettleTarget<'_>,
    after: Option<&str>,
    settle_ms: u64,
    timeout_ms: Option<u64>,
) -> std::io::Result<SettledOutcome> {
    settled_wait_observed(verb, target, after, settle_ms, timeout_ms, &mut |_| false)
}

pub(super) fn settled_wait_observed(
    verb: &str,
    target: SettleTarget<'_>,
    after: Option<&str>,
    settle_ms: u64,
    timeout_ms: Option<u64>,
    observer: &mut dyn FnMut(Option<&Sample>) -> bool,
) -> std::io::Result<SettledOutcome> {
    let client = ApiClient::local();
    let mut requests = SocketRequests { client: &client };
    settled_core_observed(
        &mut requests,
        verb,
        target,
        after,
        settle_ms,
        timeout_ms,
        UNREACHABLE_LIMIT,
        observer,
    )
}

/// The wait itself, over an injected transport and an injected unreachable
/// window.
///
/// Both exist for the same reason and follow [`PinnedRequests`]: the loop's
/// decisions — when to give up on a server, whether a deadline got there first —
/// are decisions about a transport, so testing them through a real socket would
/// mean waiting 30 s for the answer. The shipped window is
/// [`UNREACHABLE_LIMIT`], and it applies only when the caller passed no
/// `--timeout` (see [`Interrupted`]).
#[cfg(test)]
fn settled_core(
    requests: &mut dyn PinnedRequests,
    verb: &str,
    target: SettleTarget<'_>,
    after: Option<&str>,
    settle_ms: u64,
    timeout_ms: Option<u64>,
    unreachable_window: Duration,
) -> std::io::Result<SettledOutcome> {
    settled_core_observed(
        requests,
        verb,
        target,
        after,
        settle_ms,
        timeout_ms,
        unreachable_window,
        &mut |_| false,
    )
}

// The transport recovery and cursor checks are the same for every caller.
fn settled_core_observed(
    requests: &mut dyn PinnedRequests,
    verb: &str,
    target: SettleTarget<'_>,
    after: Option<&str>,
    settle_ms: u64,
    timeout_ms: Option<u64>,
    unreachable_window: Duration,
    observer: &mut dyn FnMut(Option<&Sample>) -> bool,
) -> std::io::Result<SettledOutcome> {
    let now = Instant::now();
    let timeout = timeout_ms.map(Duration::from_millis);
    let deadline = timeout.and_then(|timeout| now.checked_add(timeout));
    // The window bounds a wait the caller left UNBOUNDED. Given a `--timeout`,
    // they chose the bound themselves and a supervisor keys on 124, so that wait
    // retries to its deadline exactly as it always did — a shorter window here
    // would trade a code a caller already handles for one it does not (#614).
    let window = match deadline {
        Some(_) => None,
        None => Some(unreachable_window),
    };
    let mut interrupted = Interrupted::new(window);

    let after = match after {
        Some(raw) => match Cursor::parse(raw) {
            Ok(cursor) => Some(cursor),
            Err(reason) => return Ok(SettledOutcome::Refused(reason)),
        },
        None => None,
    };

    let mut wait = match &target {
        SettleTarget::Pinned(pinned) => {
            if let Some(after) = &after {
                if after.terminal_id != pinned.terminal_id {
                    return Ok(SettledOutcome::Refused(format!(
                        "turn cursor belongs to another terminal \
                         ({} is not {})",
                        after.terminal_id, pinned.terminal_id
                    )));
                }
            }
            let mut wait = SettledWait::new(
                after,
                Duration::from_millis(settle_ms),
                timeout,
                pinned.terminal_id.clone(),
                now,
            );
            // Seeded so the first `pane.get` fallback has an id to ask about
            // even if no record has been seen yet.
            wait.seed_pane(&pinned.pane_id);
            wait
        }
        SettleTarget::Resolve(target) => {
            let rec0 = match resolve_initial(requests, *target, deadline, verb, &mut interrupted) {
                Ok(rec) => rec,
                Err(InitialFailure::TimedOut) => return Ok(SettledOutcome::TimedOut),
                Err(InitialFailure::Refused(response)) => {
                    return Ok(SettledOutcome::ServerRefused(response))
                }
                Err(InitialFailure::Unusable(reason)) => return Ok(SettledOutcome::Error(reason)),
                // #614: the server never answered at all. Exit 1 rather than
                // 124, because nothing has been established about the agent
                // — but the wait is over, not out of time.
                Err(InitialFailure::Unreachable(reason)) => {
                    return Ok(SettledOutcome::Error(reason))
                }
            };

            let terminal_id = rec0
                .get("terminal_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            if terminal_id.is_empty() {
                return Ok(SettledOutcome::Error(
                    "the server's record named no terminal".into(),
                ));
            }
            if let Some(after) = &after {
                if after.terminal_id != terminal_id {
                    return Ok(SettledOutcome::Refused(format!(
                        "turn cursor belongs to another terminal \
                         ({} is not {terminal_id})",
                        after.terminal_id
                    )));
                }
            }

            let mut wait = SettledWait::new(
                after,
                Duration::from_millis(settle_ms),
                timeout,
                terminal_id,
                now,
            );
            // The resolved record is a sample like any other: folding it in here
            // means the cursor checks, the epoch comparison and a `--settle 0`
            // quiescence all behave the same on the first read as on the
            // hundredth.
            let sample = match sample_from_record(&rec0) {
                Ok(sample) => sample,
                Err(reason) => return Ok(SettledOutcome::Error(reason)),
            };
            if let Some(outcome) = settle(&mut wait, sample) {
                return Ok(outcome);
            }
            wait
        }
    };

    loop {
        match sample_pinned(requests, &wait, deadline) {
            Ok(sample) => {
                // The server answered, so the run of failures this wait was
                // surviving is over however long it had lasted.
                interrupted.reached();
                let stalled = observer(Some(&sample));
                if let Some(outcome) = settle(&mut wait, sample) {
                    return Ok(outcome);
                }
                if stalled {
                    return Ok(SettledOutcome::Stalled {
                        last: wait.last_record(),
                    });
                }
            }
            // Retried rather than reported: a live handoff that replaces the
            // socket mid-wait can land between two polls. Said once per
            // invocation, though — a wait that cannot reach the server is
            // otherwise silent for as long as its timeout, which reads as a
            // hang. And retried for a WINDOW rather than forever when the caller
            // set no `--timeout`: without one, a server that has stopped
            // answering entirely would keep a supervisor waiting indefinitely
            // (#614). With one, the deadline is the bound and it keeps that.
            Err(PinnedFailure::Retry) => {
                observer(None);
                interrupted.note(verb);
                if let Some(window) = interrupted.spent_window() {
                    return Ok(SettledOutcome::Error(unreachable_reason(window)));
                }
            }
            Err(PinnedFailure::TimedOut) => return Ok(SettledOutcome::TimedOut),
            Err(PinnedFailure::Fatal(reason)) => return Ok(SettledOutcome::Error(reason)),
        }
        sleep_bounded(deadline);
    }
}

/// Fold one sample in and turn a terminal step into an outcome.
///
/// `None` means keep polling, which is the only case the loop does not act on.
fn settle(wait: &mut SettledWait, sample: Sample) -> Option<SettledOutcome> {
    let step = wait.observe(sample, Instant::now());
    let last = wait.last_record();
    match step {
        Step::Continue => None,
        Step::Settled { held } => Some(SettledOutcome::Settled {
            held_ms: held.as_millis() as u64,
            last,
        }),
        Step::Blocked { held } => Some(SettledOutcome::Blocked {
            held_ms: held.as_millis() as u64,
            last,
        }),
        Step::Gone(reason) => Some(SettledOutcome::Gone { reason, last }),
        Step::Refused(reason) => Some(SettledOutcome::Refused(reason)),
        Step::TimedOut => Some(SettledOutcome::TimedOut),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum InitialFailure {
    /// The deadline arrived before a usable answer. Reported as a timeout rather
    /// than as an error, because nothing has been established as wrong: the
    /// server may have been mid-handoff the whole time.
    TimedOut,
    /// The server answered with an error (not found, ambiguous).
    Refused(String),
    /// The server answered, but the record cannot be waited on.
    Unusable(String),
    /// The server could not be reached at all for the whole unreachable window,
    /// so nothing was ever established about the agent (#614).
    Unreachable(String),
}

/// Says the connection is interrupted, once per invocation, and times the run.
///
/// One latch for the whole wait rather than one per phase: a live handoff that
/// interrupts the initial resolve is very often the same one that interrupts the
/// polls after it, and a caller who sees the line twice learns nothing the first
/// line did not say. Worded as an interruption rather than a fault because the
/// overwhelmingly common cause recovers on its own.
///
/// The clock is the second half of the same problem (#614). A retry is right for
/// a handoff and wrong forever, so this measures the run of failures from the
/// FIRST one and any answer clears it: a wait that recovers and later drops gets
/// a whole window rather than the remainder of one that ended minutes ago.
///
/// The window is present only when the caller gave no `--timeout`, the one case
/// where the wait had no bound of its own. `None` here is not "no limit": it is
/// the deadline doing that job, and the deadline's answer is `124`.
struct Interrupted {
    said: bool,
    /// When the current run of transport failures began.
    since: Option<Instant>,
    /// How long that run may last before the wait gives up, or `None` when a
    /// `--timeout` bounds the wait instead.
    window: Option<Duration>,
}

impl Interrupted {
    fn new(window: Option<Duration>) -> Self {
        Self {
            said: false,
            since: None,
            window,
        }
    }

    /// Record one transport failure, and say so the first time.
    fn note(&mut self, verb: &str) {
        // Stamped once per run and never re-stamped: measuring from the LATEST
        // failure instead is what would make the window a sliding one, and a
        // server that fails every 199 ms would then never reach it.
        if self.since.is_none() {
            self.since = Some(Instant::now());
        }
        if self.said {
            return;
        }
        self.said = true;
        eprintln!("{}", interrupted_notice(verb, self.window));
    }

    /// The server answered: the run of failures is over, latch or no latch.
    fn reached(&mut self) {
        self.since = None;
    }

    /// The window, once a run of failures has outlasted it.
    ///
    /// `None` covers both "no failures yet" and "the caller gave a `--timeout`,
    /// so the deadline is the bound" — and in both cases the wait must go on
    /// retrying. Returning the window rather than a bool is what lets the
    /// message name the one number the notice already printed.
    fn spent_window(&self) -> Option<Duration> {
        let window = self.window?;
        self.since
            .filter(|since| since.elapsed() >= window)
            .map(|_| window)
    }
}

/// The reason a wait ends on once it has given up on the server, naming the
/// window in the same number the notice used.
fn unreachable_reason(window: Duration) -> String {
    format!("server unreachable for {}s; giving up", window.as_secs())
}

/// The one interruption notice, worded by the bound this wait will stop at.
///
/// The window and the deadline are two answers to one question, and exactly one
/// of them is ever present — so the notice names the one that is. Without a
/// `--timeout` there was nothing to name, and saying there was a deadline is the
/// other half of this bug: a wait that could not end described itself as one that
/// would (#614).
fn interrupted_notice(verb: &str, window: Option<Duration>) -> String {
    let tail = match window {
        Some(window) => format!("retrying for up to {}s", window.as_secs()),
        None => "retrying until the deadline".to_string(),
    };
    format!("{verb}: connection to the server interrupted, {tail}")
}

/// Resolve the target to the record the wait will pin itself to.
///
/// Same two requests, and the same deadline rule, as every sample after it: a
/// reply that lands after the deadline is a timeout whether it is a record or an
/// error. The initial resolve used to skip that check, so a `--timeout` spent
/// entirely inside one slow first request came back as whatever that request
/// happened to return — a fault, or a settle with no time left to hold it.
fn resolve_initial(
    requests: &mut dyn PinnedRequests,
    target: InitialTarget<'_>,
    deadline: Option<Instant>,
    verb: &str,
    interrupted: &mut Interrupted,
) -> Result<serde_json::Value, InitialFailure> {
    loop {
        let Some(timeout) = request_timeout(deadline) else {
            return Err(InitialFailure::TimedOut);
        };
        let reply = match target {
            InitialTarget::Agent(name) => requests.agent_get(name, timeout),
            InitialTarget::Pane(pane_id) => requests.pane_get(pane_id, timeout),
        };
        let value = match reply {
            Err(()) => {
                // Retried rather than reported: the same live handoff that
                // interrupts a wait can land between this process starting and
                // its first request. Bounded by the same window when the caller
                // set no `--timeout`, so a server that is not there at all does
                // not leave them waiting in the resolve forever (#614).
                interrupted.note(verb);
                if let Some(window) = interrupted.spent_window() {
                    return Err(InitialFailure::Unreachable(unreachable_reason(window)));
                }
                sleep_bounded(deadline);
                continue;
            }
            Ok(value) => value,
        };
        // An answer of any kind ends the run: whatever else is decided from
        // here, this server was reachable a moment ago.
        interrupted.reached();
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(InitialFailure::TimedOut);
        }
        if value.get("error").is_some() {
            return Err(InitialFailure::Refused(value.to_string()));
        }
        return initial_record(&value, target)
            .ok_or_else(|| InitialFailure::Unusable("response did not include the record".into()));
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
                    id: "cli:settled:agent-get".into(),
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
                    id: "cli:settled:pane-get".into(),
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
    /// The remembered epoch must fire ONLY on a higher one. A `!=` where this
    /// wants a `>` would report a restart the moment a server's counters read
    /// lower than the first sample saw them, so both directions are pinned:
    /// unchanged continues the dwell, and strictly lower continues it too.
    #[test]
    fn only_a_higher_epoch_leaves_the_dwell_alone() {
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
                sample(AgentStatus::Idle, cursor(3, 0, 1, false)),
                start + Duration::from_millis(400)
            ),
            Step::Continue,
            "a LOWER epoch is not a restart either; only `>` is"
        );
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Idle, cursor(4, 0, 1, false)),
                start + Duration::from_millis(500)
            ),
            Step::Settled {
                held: Duration::from_millis(500)
            },
            "and the dwell is measured from the first sample, across both"
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
        let line: serde_json::Value = serde_json::from_str(&settled_result_line(
            "gone",
            0,
            Some(GoneReason::Restarted),
            &settle.last_record(),
        ))
        .unwrap();
        assert_eq!(line["status"], "gone");
        assert_eq!(line["pane_id"], "w1:p1");
        assert_eq!(line["agent_status"], "idle");
        assert_eq!(line["turn_cursor"], format!("{TERM}:0:0:1:i"));
        assert_eq!(line["held_ms"], 0);
        assert_eq!(line["reason"], "restarted");

        // Never having seen a record, the line says so rather than inventing one.
        let unseen = wait(None, 500, Some(30_000));
        let line: serde_json::Value = serde_json::from_str(&settled_result_line(
            "gone",
            0,
            Some(GoneReason::Closed),
            &unseen.last_record(),
        ))
        .unwrap();
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

    /// The classifier's JSON mapping, and nothing else: given these two
    /// responses, `classify_pinned` returns a record rather than a `gone` line,
    /// and the epoch in that record is what later says `restarted`.
    ///
    /// It is NOT the respawn proof — it calls the classifier with canned JSON, so
    /// it cannot fail if the fallback REQUEST stops being issued, or is issued
    /// with the wrong target or the wrong budget. That is
    /// `a_respawn_is_restarted_through_the_pinned_fetch_itself`, which drives
    /// `sample_pinned` through a scripted client and asserts both requests were
    /// made. Kept because the two fail differently: this one pins the mapping,
    /// that one pins the sequence.
    #[test]
    fn the_classifier_maps_a_miss_plus_matching_pane_to_a_record() {
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

    /// #553 round 2: the deadline rule applies to the INITIAL resolve too.
    ///
    /// It used to skip it, so a `--timeout` spent entirely inside one slow first
    /// request came back as whatever that request happened to return — a fault,
    /// or a record with no clock left to hold it in. Three directions, because
    /// each is a different mistake: a record, an error, and the case where the
    /// budget is gone before the request is even sent.
    #[test]
    fn a_reply_after_the_deadline_is_a_timeout_at_the_initial_resolve_too() {
        // A record that arrives late.
        let mut requests = Scripted::new(
            vec![Ok(response(
                r#"{"result":{"agent":{"pane_id":"w1:p1","terminal_id":"term_1f2e3",
                    "agent_status":"idle","turn_cursor":"term_1f2e3:0:0:1:i"}}}"#,
            ))],
            Duration::from_millis(80),
        );
        let deadline = Instant::now() + Duration::from_millis(40);
        let mut interrupted = Interrupted::new(Some(UNREACHABLE_LIMIT));
        assert!(matches!(
            resolve_initial(
                &mut requests,
                InitialTarget::Agent("worker"),
                Some(deadline),
                "agent wait",
                &mut interrupted
            ),
            Err(InitialFailure::TimedOut)
        ));
        assert!(!interrupted.said, "the server answered; it did not drop us");

        // An error that arrives late is the same answer: at that point nothing
        // has been established about the agent.
        let mut requests = Scripted::new(
            vec![Ok(response(
                r#"{"error":{"code":"agent_not_found","message":"nope"}}"#,
            ))],
            Duration::from_millis(80),
        );
        assert!(matches!(
            resolve_initial(
                &mut requests,
                InitialTarget::Agent("worker"),
                Some(deadline),
                "agent wait",
                &mut interrupted
            ),
            Err(InitialFailure::TimedOut)
        ));

        // No budget at all: nothing is sent.
        let mut requests = Scripted::new(
            vec![Ok(response(r#"{"result":{"agent":{}}}"#))],
            Duration::ZERO,
        );
        let past = Instant::now() - Duration::from_millis(1);
        assert!(matches!(
            resolve_initial(
                &mut requests,
                InitialTarget::Agent("worker"),
                Some(past),
                "agent wait",
                &mut interrupted
            ),
            Err(InitialFailure::TimedOut)
        ));
        assert!(requests.granted.is_empty());
    }

    /// The same resolve, inside its budget: an error is still the server's own
    /// refusal, reported with its words, and the latch has NOT been tripped —
    /// so a later interruption still gets to say so once.
    #[test]
    fn an_early_error_at_the_initial_resolve_is_the_servers_refusal() {
        let mut requests = Scripted::new(
            vec![Ok(response(
                r#"{"error":{"code":"agent_not_found","message":"no such agent"}}"#,
            ))],
            Duration::ZERO,
        );
        let mut interrupted = Interrupted::new(Some(UNREACHABLE_LIMIT));
        match resolve_initial(
            &mut requests,
            InitialTarget::Agent("ghost"),
            Some(Instant::now() + Duration::from_secs(5)),
            "agent wait",
            &mut interrupted,
        ) {
            Err(InitialFailure::Refused(reason)) => {
                assert!(reason.contains("no such agent"), "{reason}")
            }
            other => panic!("a refusal is not a timeout: {other:?}"),
        }
        assert!(!interrupted.said);
    }

    /// #553 round 2: the interruption line is said ONCE per invocation, across
    /// BOTH phases. A handoff that breaks the first request usually breaks the
    /// polls after it too, and a caller who sees the line twice learns nothing
    /// the first one did not say.
    #[test]
    fn the_interruption_line_is_said_once_per_invocation() {
        let mut interrupted = Interrupted::new(Some(UNREACHABLE_LIMIT));
        interrupted.note("agent wait");
        interrupted.note("agent wait");
        interrupted.note("agent wait");
        assert!(interrupted.said);
    }

    /// #614: the notice says what the wait is going to DO, which depends on
    /// whether the caller gave a `--timeout` to stop at. A window names itself,
    /// and without one there is only the deadline to name — which is why a wait
    /// with no `--timeout` used to promise a deadline it did not have.
    ///
    /// Both wordings are pinned to [`UNREACHABLE_LIMIT`] on purpose: the help
    /// text says "30 s" in a literal, and a test that read the number from
    /// anything but this constant would let the two drift apart silently.
    #[test]
    fn the_interruption_notice_names_the_window_when_there_is_no_deadline() {
        assert_eq!(
            interrupted_notice("agent wait", Some(UNREACHABLE_LIMIT)),
            "agent wait: connection to the server interrupted, retrying for up to 30s"
        );
        assert_eq!(
            interrupted_notice("agent wait", None),
            "agent wait: connection to the server interrupted, retrying until the deadline"
        );
    }

    /// #614: the window is measured from the FIRST failure of a run, and any
    /// answer clears it — so a wait that recovers and later drops is given a
    /// whole window rather than the remainder of one that ended minutes ago.
    ///
    /// A zero window stands in for the reached limit without sleeping 30 s, and
    /// `Instant` has no fake here: the point is that the run's clock starts once
    /// and stops on an answer, neither of which needs time to have passed.
    #[test]
    fn a_run_of_failures_is_timed_from_its_first_and_cleared_by_an_answer() {
        let mut interrupted = Interrupted::new(Some(Duration::ZERO));
        assert!(interrupted.spent_window().is_none(), "no failure is no run");
        interrupted.note("agent wait");
        let first = interrupted.since.expect("a failure starts the run");
        assert!(
            interrupted.spent_window().is_some(),
            "a zero window is already spent"
        );
        interrupted.note("agent wait");
        assert_eq!(
            interrupted.since,
            Some(first),
            "measured from the FIRST failure, not the last one noticed"
        );
        interrupted.reached();
        assert!(interrupted.since.is_none(), "an answer ends the run");
        assert!(
            interrupted.spent_window().is_none(),
            "so the next drop starts over"
        );

        // No window at all, which is what a `--timeout` gets: the run is still
        // measured, and never spent, because the deadline is the bound that ends
        // this wait — with 124, which a supervisor is written to read.
        let mut bounded = Interrupted::new(None);
        bounded.note("agent wait");
        assert!(bounded.since.is_some(), "the failure is still recorded");
        assert!(
            bounded.spent_window().is_none(),
            "with a --timeout the window does not apply at all"
        );
    }

    /// A window a test can wait out: one whole second, which at the 200 ms poll
    /// interval is five retries before it is spent. Still whole seconds, so the
    /// reason it produces is formatted exactly as the shipped 30 s one is.
    const TEST_UNREACHABLE: Duration = Duration::from_secs(1);

    /// The resolve's one answer: an agent working, mid-turn.
    fn working_agent() -> serde_json::Value {
        response(
            r#"{"result":{"agent":{"pane_id":"w1:p1","terminal_id":"term_1f2e3",
                "agent_status":"working","turn_cursor":"term_1f2e3:0:0:1:w"}}}"#,
        )
    }

    /// The same agent one turn later, quiet — what a settle waits for.
    fn idle_agent() -> serde_json::Value {
        response(
            r#"{"result":{"agent":{"pane_id":"w1:p1","terminal_id":"term_1f2e3",
                "agent_status":"idle","turn_cursor":"term_1f2e3:0:1:2:i"}}}"#,
        )
    }

    /// The whole wait, over a scripted transport — plus how long it took and how
    /// many requests it made.
    ///
    /// The elapsed clock is returned because every test below is about a wait
    /// that has to END: `Scripted` panics rather than inventing a reply, so a
    /// regression that keeps polling fails the test rather than hanging it, and
    /// the bound catches the case where the wait is slow rather than endless.
    fn wait_over_script(
        replies: Vec<Result<serde_json::Value, ()>>,
        settle_ms: u64,
        timeout_ms: Option<u64>,
        unreachable: Duration,
    ) -> (SettledOutcome, Duration, usize) {
        let mut requests = Scripted::new(replies, Duration::ZERO);
        let started = Instant::now();
        let outcome = settled_core(
            &mut requests,
            "agent wait",
            SettleTarget::Resolve(InitialTarget::Agent("worker")),
            None,
            settle_ms,
            timeout_ms,
            unreachable,
        )
        .expect("the wait core does no io of its own");
        (outcome, started.elapsed(), requests.granted.len())
    }

    /// How long any of these waits may take before it is a hang rather than a
    /// window: the shipped one is 30 s, and none of these comes near it.
    const TEST_CEILING: Duration = Duration::from_secs(5);

    /// #614, the bug itself. With no `--timeout` and a server that answers one
    /// `agent.get` and then fails at the transport level, the wait used to retry
    /// forever — the reported shape was a `delegate wait` a `timeout` wrapper had
    /// to kill. It ends on the unreachable window instead, as the error outcome
    /// both wait verbs and the delegate already map to exit 1.
    #[test]
    fn a_wait_with_no_timeout_gives_up_on_a_server_that_stays_unreachable() {
        // One answer, then nothing at all for the rest of the run. The scripted
        // budget is more than the window can spend, so a regression that keeps
        // polling runs out of replies and fails rather than hanging.
        let mut replies = vec![Ok(working_agent())];
        replies.extend(vec![Err(()); 8]);

        let (outcome, elapsed, asked) = wait_over_script(replies, 0, None, TEST_UNREACHABLE);
        match outcome {
            SettledOutcome::Error(reason) => assert_eq!(
                reason, "server unreachable for 1s; giving up",
                "the window is named from the limit the wait was given"
            ),
            other => panic!("a server that stopped answering is not a settle: {other:?}"),
        }
        assert!(
            asked >= 2,
            "the retry is kept: giving up takes more than the one request that failed"
        );
        assert!(elapsed < TEST_CEILING, "{elapsed:?}");
    }

    /// The same bound in the phase before it. A server that is not there at all
    /// never answers the resolve, and a caller who passed no `--timeout` used to
    /// sit in that loop forever having learned nothing about the agent — the
    /// commonest shape of "the server died", and the one phase a test that only
    /// answers once could miss entirely.
    #[test]
    fn a_resolve_that_never_answers_ends_on_the_window_too() {
        let (outcome, elapsed, asked) =
            wait_over_script(vec![Err(()); 8], 0, None, TEST_UNREACHABLE);
        match outcome {
            SettledOutcome::Error(reason) => assert_eq!(
                reason, "server unreachable for 1s; giving up",
                "the resolve ends the wait the same way the polls do"
            ),
            // A timeout here would mean the unreachable window answered for a
            // deadline nobody set, which is the bug with the sign flipped.
            other => panic!("no deadline was set, so this is not a timeout: {other:?}"),
        }
        assert!(asked >= 2, "the resolve retries before it gives up");
        assert!(elapsed < TEST_CEILING, "{elapsed:?}");
    }

    /// #614: the window bounds a RUN of failures, not the whole wait. Two
    /// failures and then an answer — a live handoff, which is what the retry
    /// exists for — carries on and settles normally.
    ///
    /// This is the half the fix must not break. A window that ended the wait on
    /// its second failed request would break the handoff it exists to survive,
    /// and `wait agent-status` on a server being restarted would fail.
    #[test]
    fn failures_that_stop_before_the_window_do_not_end_the_wait() {
        let (outcome, elapsed, _) = wait_over_script(
            vec![
                Ok(working_agent()),
                Err(()),
                Err(()),
                Ok(idle_agent()),
                Ok(idle_agent()),
            ],
            0,
            None,
            UNREACHABLE_LIMIT,
        );
        match outcome {
            SettledOutcome::Settled { last, .. } => assert_eq!(
                last.agent_status.as_deref(),
                Some("idle"),
                "the settle is reported from the record that answered, not from the failures"
            ),
            other => panic!("the agent went quiet and the wait settled: {other:?}"),
        }
        assert!(elapsed < TEST_CEILING, "{elapsed:?}");
    }

    /// #614: the window does not take a `--timeout`'s job in either direction.
    /// A deadline LONGER than the window arrives second, and the wait still ends
    /// on that deadline with exit 124 — the answer a supervisor is written to
    /// read. Applying the window here would trade a code every caller already
    /// handles for one it does not, on a wait whose bound the caller chose.
    #[test]
    fn a_timeout_longer_than_the_window_is_still_a_timeout() {
        let mut replies = vec![Ok(working_agent())];
        replies.extend(vec![Err(()); 4]);

        // 60 ms of window under a 250 ms deadline: under the old rule the run is
        // spent at the first poll (200 ms in), and this would report the
        // unreachable error instead of the timeout the caller asked for.
        let (outcome, elapsed, _) =
            wait_over_script(replies, 0, Some(250), Duration::from_millis(60));
        assert_eq!(outcome, SettledOutcome::TimedOut);
        assert!(elapsed < TEST_CEILING, "{elapsed:?}");
    }

    /// #614: a `--timeout` shorter than the shipped window is the same rule from
    /// the other side, and the case the first version of this fix got wrong: the
    /// window was reaching in and answering with exit 1 for a wait that had a
    /// deadline of its own.
    #[test]
    fn a_timeout_shorter_than_the_window_is_still_a_timeout() {
        let mut replies = vec![Ok(working_agent())];
        replies.extend(vec![Err(()); 3]);

        let (outcome, elapsed, _) = wait_over_script(replies, 0, Some(40), UNREACHABLE_LIMIT);
        assert_eq!(outcome, SettledOutcome::TimedOut);
        assert!(elapsed < TEST_CEILING, "{elapsed:?}");
    }

    /// #553 round 2: defence in depth. `sample_from_record` already answers
    /// hibernation before it asks for a cursor, so this state is unreachable
    /// from the wire — but `observe` is handed samples by callers, and the one
    /// wrong answer available for an agent with no process behind it is
    /// `Waiting`, which settles. Pinned so a future `Sample::Record` producer
    /// cannot reintroduce it.
    #[test]
    fn a_record_that_says_hibernated_is_gone_not_waiting() {
        let mut settle = wait(None, 500, Some(30_000));
        assert_eq!(
            settle.observe(
                sample(AgentStatus::Hibernated, cursor(0, 1, 7, false)),
                Instant::now()
            ),
            Step::Gone(GoneReason::Hibernated)
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
        let line: serde_json::Value = serde_json::from_str(&settled_result_line(
            "gone",
            0,
            Some(GoneReason::Hibernated),
            &settle.last_record(),
        ))
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

        // 1000 ms of budget, 600 ms per reply: the first fits, the pair does not.
        // The margins are wide so a loaded CI runner's oversleeping (macOS
        // overshoots tens of ms) cannot push the first reply past the deadline.
        let deadline = Instant::now() + Duration::from_millis(1000);
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
            Duration::from_millis(600),
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
            requests.granted[1] <= Duration::from_millis(400),
            "and what is left is under the 600 ms the first reply cost: {:?}",
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
