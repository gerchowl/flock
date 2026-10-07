#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
//! #578 — `flk delegate`, a whole task handed to an agent, in one command.
//!
//! ## What this is
//!
//! The commands an operator runs to put an agent to work, in the order they run
//! them: make a checkout, make a space for it, start the harness in it, wait
//! until it is actually up, type the task, wait for it to settle, read the
//! answer, and eventually throw the checkout away. Six socket verbs and a
//! remembered cursor between each pair. That is a script, and the script was
//! living in somebody's shell history.
//!
//! `flk delegate` is that script, composed client-side out of the socket
//! methods that already exist. No new socket method, no new dependency, and
//! exactly ONE additive server change: `worktree.create`'s response now
//! carries `parent_workspace_id` so the rollback knows which parent workspace
//! IT opened and must not close somebody else's (#595). Every other claim in
//! the docs is checkable against a method `flk agent wait` or `flk worktree
//! kill` already used. What the composition adds is the part that was never
//! written down anywhere — which cursor belongs to which round, which errors
//! mean "not yet", what a bare `idle` does and does not say, and what has to
//! be cleaned up if the start fails halfway.
//!
//! ## Why `delegate start`, not `delegate <name>`
//!
//! The issue sketched `flk delegate <name>`. A verb that is both a group and an
//! action makes the six action verbs unreachable by name, and `flk delegate
//! wait` has to mean something specific. `start` is what `flk agent start` does,
//! and the subverb keeps the six verbs in the help table where a reader is
//! already looking.
//!
//! ## The three things that are easy to get wrong
//!
//! **Focus.** A delegate never asks for focus, and never runs in the workspace
//! the operator is looking at. `workspace.create` and `worktree.create` are both
//! sent with `focus: false`, so a delegate lands beside the operator rather than
//! under their cursor. Creating the first workspace on an empty server, and
//! reaping a workspace somebody focused by hand, follow the server's own rules —
//! the delegate does not override them.
//!
//! **A bare `idle` is not an answer.** Settling is not finishing: an agent goes
//! quiet between its tool calls. So an await settles, then waits a further grace
//! for a reply whose recorded time is at or after this round's submit, and only
//! then reports one. A round is therefore reported only when the agent actually
//! entered `working` after that round's cursor AND a reply exists from after the
//! submit — which is what `settled` alone would have let it get wrong.
//!
//! **What may be destroyed.** Every request this module makes is bounded, and
//! every kill or close is preceded by an identity check against what the
//! registry recorded. A workspace id is a name, not a claim: one can be
//! reassigned by a server restart, and closing the workspace a stale entry names
//! would take the operator's checkout with it. So [`tear_down`] — the one
//! routine a failed start and a `reap` both run — closes nothing whose recorded
//! identity it has not just re-checked.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api::client::{ApiClient, ApiClientError};
use crate::api::schema::{
    AgentResultParams, AgentStartParams, AgentTarget, EmptyParams, EventsSubscribeParams, Method,
    PaneListParams, PaneReadParams, PaneSendInputParams, PaneSendKeysParams, PaneTarget,
    ReadFormat, ReadSource, Request, Subscription, WorkspaceCreateParams, WorkspaceTarget,
    WorktreeCreateParams, WorktreeKillParams, WorktreeListParams,
};

use super::pane::PANE_RUN_SUBMIT_GAP;
use super::ready::{classify_ready_event, status_is_ready, ReadySignal};
use super::settled::{Cursor, PinnedTarget, SettleTarget};

/// `delegate start`'s usage. A `pub(super) const` rather than a literal in the
/// help table so `flk delegate start --help` and `flk delegate --help` cannot
/// answer two different things.
pub(super) const START_USAGE: &str = concat!(
    "flk delegate start <name> --brief FILE (--cwd PATH | --worktree --branch B [--repo PATH] [--base REF])\n",
    "                     [--harness opencode|claude] [--model M] [--await] [--timeout MS]\n",
    "                     [--settle MS] [--ready-timeout MS] [--max-chars N] [--json]\n",
    "  --brief FILE        a readable file; exactly `Read <path> and execute it exactly.` is typed\n",
    "  --cwd PATH          run in a workspace the delegate creates for that directory\n",
    "  --harness NAME      the agent to run, default opencode; claude's folder-trust dialog is\n",
    "                      named rather than typed into (see #605)\n",
    "  --worktree          run in a fresh linked worktree: --branch is required, --repo and --base optional\n",
    "  --await             stay and report the round's outcome instead of returning after the submit\n",
    "  --timeout MS        bound the AWAIT only, counted from the submit; absent waits forever\n",
    "  --settle MS         how long the agent's quiet must hold, default 5000\n",
    "  --ready-timeout MS  how long the agent may take to become ready and reach its prompt, default 60000\n",
    "  --max-chars N       characters of the reply to print, default 4000\n",
    "  the delegate runs in a workspace it created, never in the focused one, and never asks for focus",
);

pub(super) const SEND_USAGE: &str = concat!(
    "flk delegate send <name> --brief FILE [--await] [--timeout MS] [--settle MS]\n",
    "                    [--ready-timeout MS] [--max-chars N] [--json]\n",
    "  one round at a time: a send while another round is being awaited is refused as busy\n",
    "  --ready-timeout MS  how long the agent may take to reach its prompt, default 60000",
);

pub(super) const WAIT_USAGE: &str = concat!(
    "flk delegate wait <name> [--after CURSOR] [--timeout MS] [--settle MS] [--max-chars N] [--json]\n",
    "  without --after it waits from the cursor the delegate recorded with its latest submit\n",
    "  --timeout MS        counted from this command, not from the submit that started the round",
);

pub(super) const RESULT_USAGE: &str = "flk delegate result <name> [--max-chars N] [--json]";
pub(super) const STATUS_USAGE: &str = "flk delegate status <name> [--json]";
pub(super) const REAP_USAGE: &str = concat!(
    "flk delegate reap <name> [--force] [--json]\n",
    "  removes only the workspace and checkout recorded at start; needs no live agent\n",
    "  --force             clears the dirty-checkout refusal and proceeds through a busy\n",
    "                      lock; two concurrent forced reaps of the same delegate are\n",
    "                      idempotent (each sees the same end state)",
);

/// How often the grace asks for the reply.
const RESULT_POLL: Duration = Duration::from_millis(250);

/// How often the readiness gate asks whether the agent is at its prompt.
const READY_POLL: Duration = Duration::from_millis(200);

/// Cap on any single socket request this module makes.
///
/// A delegate is a long-lived client and a live handoff replaces the socket
/// underneath it, so bounding each request means such a stall costs one poll
/// rather than the whole command. The same cap the settled core uses.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Result of a deadline-bounded request (error variant only).
#[derive(Debug)]
enum BoundedError {
    /// The deadline was exceeded before or during the request.
    TimedOut,
    /// A transport error occurred.
    Transport(io::Error),
}

/// Classification of a worktree.create failure.
#[derive(Debug)]
enum CreateFailure {
    /// Server refused the request (e.g., branch already exists).
    Refused(ServerError),
    /// Deadline was exceeded.
    TimedOut,
    /// Transport-level failure (connection error, etc.).
    Transport(io::ErrorKind),
}

/// Result of the leftover checkout check decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeftoverCheck {
    /// Do nothing; server created nothing.
    Nothing,
    /// The pre-create list failed; cannot determine leftovers.
    CannotCheck,
    /// Look for a new checkout and remove it.
    Look,
}

/// Decide whether to check for a leftover checkout based on the failure type
/// and whether the pre-create list succeeded.
fn leftover_check(failure: &CreateFailure, listed_before: bool) -> LeftoverCheck {
    match failure {
        CreateFailure::Refused(_) => LeftoverCheck::Nothing,
        CreateFailure::TimedOut | CreateFailure::Transport(_) => {
            if listed_before {
                LeftoverCheck::Look
            } else {
                LeftoverCheck::CannotCheck
            }
        }
    }
}

/// Result of a failed request inside an await.
///
/// The three cases a caller inside an await must distinguish: the deadline has
/// passed (become a `Timeout` outcome, exit 124), the transport blipped before
/// it did (`NotYet`, poll again), or the request failed for a reason that is
/// not a slow socket (`Fail`, exit 1 with the error on stderr).
#[derive(Debug, PartialEq, Eq)]
enum AwaitFailure {
    /// The deadline was exceeded, either on the clock or by a transport error
    /// whose deadline has already passed.
    Timeout,
    /// A `TimedOut`/`WouldBlock` transport error, with the deadline still in
    /// the future: a slow poll, not a failure.
    NotYet,
    /// Any other transport error: exit 1 with the message on stderr.
    Fail(String),
}

/// Classify a request error inside an await.
///
/// Mapping (per the P578 r4-2 brief, V4):
/// - `TimedOut` → `Timeout`;
/// - any `Transport` with `deadline <= now` → `Timeout`;
/// - else `TimedOut`/`WouldBlock` kinds → `NotYet`;
/// - else `Fail(err.to_string())`.
fn await_failure(err: &BoundedError, deadline: Option<Instant>, now: Instant) -> AwaitFailure {
    match err {
        BoundedError::TimedOut => AwaitFailure::Timeout,
        BoundedError::Transport(transport) => {
            if deadline.is_some_and(|d| d <= now) {
                return AwaitFailure::Timeout;
            }
            match transport.kind() {
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => AwaitFailure::NotYet,
                _ => AwaitFailure::Fail(err.to_string()),
            }
        }
    }
}

impl fmt::Display for BoundedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => write!(f, "request timed out"),
            Self::Transport(err) => write!(f, "{err}"),
        }
    }
}
/// The one cap table: how long THIS module waits for each method on a quiet
/// socket. The actual socket timeout is `cap.min(remaining deadline)` — a
/// caller with a shorter deadline wins (K5). A cap handed in by hand would be
/// a second table drifting from this one; `bounded` reads it, nothing else.
///
/// - 60 s: creates / kills / closes / `AgentStart` — a write that may block
///   on filesystem work the operator sees.
/// - 10 s: `PaneSendInput` — the brief submit is a human-latency write and
///   must not stall the whole command if the socket goes quiet mid-type.
/// - 2 s: every poll, get, and list.
fn cap_for(method: &Method) -> Duration {
    match method {
        Method::WorktreeCreate(_)
        | Method::WorktreeKill(_)
        | Method::WorkspaceCreate(_)
        | Method::WorkspaceClose(_)
        | Method::AgentStart(_) => Duration::from_secs(60),
        Method::PaneSendInput(_) => Duration::from_secs(10),
        _ => REQUEST_TIMEOUT,
    }
}

/// Make a socket request bounded by its cap and the caller's deadline.
///
/// The socket timeout is `cap_for(method).min(deadline - now)`; past the
/// deadline, nothing is sent. On the way back the deadline is re-checked —
/// a reply that lands late is a `TimedOut` regardless of its content, so a
/// caller that has run out of clock does not act on what it read on the way
/// past.
///
/// Returns the raw response; the caller checks for a server-level `error`.
fn bounded(method: Method, deadline: Option<Instant>) -> Result<serde_json::Value, BoundedError> {
    let cap = cap_for(&method);
    // Check if deadline has already passed.
    if let Some(deadline) = deadline {
        if Instant::now() >= deadline {
            return Err(BoundedError::TimedOut);
        }
    }

    let timeout = match deadline {
        None => cap,
        Some(deadline) => {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(BoundedError::TimedOut);
            }
            left.min(cap)
        }
    };

    let response = ApiClient::local()
        .request_value_with_timeout(
            &Request {
                id: "cli:delegate".into(),
                method,
            },
            timeout,
        )
        .map_err(|e| BoundedError::Transport(super::api_client_error_to_io(e)))?;

    // Check if deadline passed after the response.
    if let Some(deadline) = deadline {
        if Instant::now() >= deadline {
            return Err(BoundedError::TimedOut);
        }
    }

    // Return raw response; caller handles server errors.
    Ok(response)
}

/// `delegate`'s own exit codes.
///
/// `0`/`3`/`4`/`124` are `settled::exit`, shared with the two wait verbs so a
/// supervisor reading one exit code reads it the same way. `5` and `6` are the
/// delegate's own and live here rather than in `settled`, because nothing else
/// means them.
mod exit {
    /// A finished reply, `no_result`, or an unfinished one under `result`.
    pub(super) const OK: i32 = 0;
    /// A usage error, a refused cursor, or something that is not a delegate.
    pub(super) const USAGE: i32 = 2;
    /// A held `blocked` — the agent is sitting on a prompt a human must answer.
    pub(super) const AGENT_BLOCKED: i32 = 6;
    /// A finished reply with no `DONE:` / `BLOCKED:` / `VERDICT:` line, or a
    /// settle with no reply at all inside the grace.
    pub(super) const NO_SENTINEL: i32 = 5;
}

/// Characters a brief path may not contain.
///
/// The brief sentence is TYPED into a terminal, so the path travels through two
/// shells' worth of interpretation on its way to the agent's input box: the one
/// that runs the agent, and whatever the agent itself runs. A path containing
/// any of these is refused rather than escaped, because a quote that survives one
/// of those two hops is a different string by the time the agent reads it (#566).
const UNSAFE_PATH_CHARS: [char; 6] = ['!', '$', '`', '\u{22}', '\'', '\\'];

// ------------------------------------------------------------- harnesses

/// Who tells flock which session a pane's transcript belongs to.
///
/// Both harnesses report through the same `pane.report_agent_session` method,
/// but they report it from different places, and that is the whole reason the
/// delegate cannot assume a session exists the moment a pane looks ready: the
/// opencode plugin reports asynchronously (mid-turn, when it commits), and a
/// Claude `SessionStart` hook reports from inside the process, so by the time
/// the TUI is up the report is merely in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionSource {
    /// opencode's plugin, reporting the session mid-turn as it commits it.
    Plugin,
    /// Claude Code's `SessionStart` hook, reporting from inside the harness.
    Hook,
}

impl SessionSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Plugin => "opencode's plugin",
            Self::Hook => "Claude's SessionStart hook",
        }
    }
}

/// Everything `delegate` needs to know about one agent harness.
///
/// The delegate is harness-agnostic by design (#578) and this is where that
/// claim is kept honest: the argv, the session source, the grace, the not-yet
/// codes and the startup dialog all arrive from ONE row, so adding a harness is
/// a row here rather than a `match harness` in the argv builder, the grace, the
/// result poll and the readiness gate at once (#612).
#[derive(Debug)]
struct HarnessSpec {
    /// What `--harness` accepts and what the registry records.
    name: &'static str,
    /// The argv this harness starts with, given the `--model` the caller passed.
    ///
    /// A function rather than a name because the flag is not the same
    /// everywhere: both current harnesses take `--model`, and a third one that
    /// spelled it `--model-id` should not need this module to learn a second
    /// flag name.
    argv: fn(Option<&str>) -> Vec<String>,
    /// Where the session id comes from — read for what the operator has to be
    /// told when a turn produces no reply at all.
    session_source: SessionSource,
    /// How long after a settle the delegate keeps looking for the reply.
    result_grace: Duration,
    /// A dialog this harness puts up before it will take input, matched on
    /// screen so readiness can name it instead of reporting a bare `blocked`.
    ///
    /// `None` for a harness with no such dialog. The marker is matched against
    /// the pane's recent lines (ANSI stripped), so it must be text the dialog
    /// itself prints rather than incidental text from the transcript above it.
    startup_dialog: Option<StartupDialog>,
}

/// A known blocking dialog, and the refusal that names it.
#[derive(Debug, Clone, Copy)]
struct StartupDialog {
    /// Lowercase text the dialog itself draws, and nothing else on screen.
    marker: &'static str,
    /// What the delegate says when readiness finds it.
    refusal: &'static str,
}

/// The `agent.result` refusals that are ordinary whichever harness asked.
///
/// Every one of these is a statement about the STORE rather than about the
/// turn, and all of them are ordinary in the seconds between the agent going
/// quiet and the transcript being committed. Treating any of them as a failure
/// would make a normal turn a `result` error; treating a real refusal as "not
/// yet" would make the grace hang until the deadline and then report
/// `no_result`, which is a lie about a server that answered.
///
/// A harness that gained a store of its own would add its own codes here rather
/// than widen this list for everyone.
const STORE_NOT_YET_CODES: [&str; 3] =
    ["no_result", "transcript_not_found", "transcript_unreadable"];

/// The no-session refusal: the pane has not reported a session yet, so there is
/// no store to read at all. Ordinary for both current harnesses, and listed per
/// harness rather than globally because a harness whose session is known
/// synchronously must not have this treated as a transient.
const NO_SESSION_CODE: &str = "no_agent_session";

/// The harnesses this build drives, in the order `--harness` help lists them.
const HARNESSES: [HarnessSpec; 2] = [
    HarnessSpec {
        name: "opencode",
        argv: opencode_argv,
        session_source: SessionSource::Plugin,
        result_grace: Duration::from_secs(10),
        startup_dialog: None,
    },
    HarnessSpec {
        name: "claude",
        argv: claude_argv,
        session_source: SessionSource::Hook,
        // The TUI clears its spinner before Claude has flushed its last
        // transcript entry, so the gap this covers is the same one it covers
        // for opencode: the store, not the screen, is the slow side.
        result_grace: Duration::from_secs(10),
        startup_dialog: Some(StartupDialog {
            marker: "quick safety check:",
            refusal: "Claude Code is waiting on its folder-trust dialog (see #605)",
        }),
    },
];

/// `opencode [--model M]`.
///
/// The model string is passed through exactly as the caller wrote it — it is
/// `provider/model` for opencode and nothing in this module should second-guess
/// a name it does not resolve.
fn opencode_argv(model: Option<&str>) -> Vec<String> {
    harness_argv("opencode", model)
}

/// `claude [--model M]`.
///
/// Claude Code takes the same `--model M` shape as opencode does (#612), which
/// is why `argv` is one shared builder rather than two hand-written vectors.
fn claude_argv(model: Option<&str>) -> Vec<String> {
    harness_argv("claude", model)
}

fn harness_argv(program: &str, model: Option<&str>) -> Vec<String> {
    let mut argv = vec![program.to_string()];
    if let Some(model) = model {
        argv.push("--model".to_string());
        argv.push(model.to_string());
    }
    argv
}

/// The `agent.result` codes that mean "not yet" for THIS harness.
///
/// `no_agent_session` is included per harness rather than once for all of them:
/// both current harnesses report their session asynchronously, so both need it,
/// and a harness that reported synchronously must not spend a grace treating a
/// missing session as a transient.
fn not_yet_codes(harness: &HarnessSpec) -> Vec<&'static str> {
    let mut codes = STORE_NOT_YET_CODES.to_vec();
    if matches!(
        harness.session_source,
        SessionSource::Plugin | SessionSource::Hook
    ) {
        codes.push(NO_SESSION_CODE);
    }
    codes
}

/// The harness table as it would be printed back to a caller.
fn harness_names() -> String {
    HARNESSES
        .iter()
        .map(|harness| harness.name)
        .collect::<Vec<_>>()
        .join("|")
}

/// `worktree.kill` refusals that mean "the checkout is not there to remove".
///
/// A path that is already gone, or that is no longer a linked worktree, is what
/// a reap wanted to achieve — so these are success, not a failure to report.
const ALREADY_GONE_CODES: [&str; 3] = [
    "not_linked_worktree",
    "not_git_worktree",
    "workspace_not_found",
];

pub(super) fn run_delegate_command(args: &[String]) -> io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_delegate_help();
        return Ok(exit::USAGE);
    };

    match subcommand {
        "start" => delegate_start(&args[1..]),
        "send" => delegate_send(&args[1..]),
        "wait" => delegate_wait(&args[1..]),
        "result" => delegate_result(&args[1..]),
        "status" => delegate_status(&args[1..]),
        "reap" => delegate_reap(&args[1..]),
        "help" | "--help" | "-h" => {
            print_delegate_help();
            Ok(0)
        }
        _ => {
            print_delegate_help();
            Ok(exit::USAGE)
        }
    }
}

fn print_delegate_help() {
    eprintln!("flk delegate commands:");
    eprintln!("  {START_USAGE}");
    eprintln!("  {SEND_USAGE}");
    eprintln!("  {WAIT_USAGE}");
    eprintln!("  {RESULT_USAGE}");
    eprintln!("  {STATUS_USAGE}");
    eprintln!("  {REAP_USAGE}");
}

/// A usage error: the line on stderr, exit 2, and nothing created.
///
/// Every refusal in this module goes through here so that no parse failure can
/// print on stdout — with or without `--json`, a caller piping stdout gets either
/// the object it asked for or nothing at all, never a diagnostic.
fn usage(reason: impl fmt::Display) -> i32 {
    eprintln!("{reason}");
    exit::USAGE
}

fn fail(reason: impl fmt::Display) -> i32 {
    eprintln!("{reason}");
    1
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- registry

/// What a delegate recorded at start and updated with every round.
///
/// The file is the only thing that makes a name a delegate (P13): without it,
/// `flk delegate send` would type into anything the server happens to call that.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Entry {
    name: String,
    terminal_id: String,
    pane_id: String,
    /// The shell pane `workspace.create` opened, before the delegate closed it.
    ///
    /// Recorded because it is the only identity a cwd-mode workspace still has
    /// when the agent's own pane is gone: a start whose harness died at launch has
    /// no agent pane, and the root pane is what was created with the workspace.
    root_pane: String,
    workspace_id: String,
    mode: String,
    worktree: Option<String>,
    branch: Option<String>,
    /// The repository-root workspace `worktree.create` opened as the new
    /// checkout's parent, when it opened one.
    ///
    /// Recorded rather than re-derived, because the alternative is a list diff
    /// across a window in which the operator may open a workspace of their own —
    /// and a rollback that closes a list diff closes whatever appeared in it.
    /// See #595 for the server-side change that provides this value.
    #[serde(default)]
    parent_workspace_id: Option<String>,
    #[serde(default)]
    repo_root: Option<String>,
    #[serde(default)]
    repo_key: Option<String>,
    harness: String,
    model: Option<String>,
    round: u64,
    brief: String,
    submitted_at_ms: u64,
    /// Captured BEFORE the latest submit, so a caller that times out can resume
    /// from it and cannot double-count the turn it already waited for.
    cursor: String,
    created_at_ms: u64,
    /// The terminal_id of the root pane (from workspace.create or worktree.create response).
    /// Used for positive identity in cwd-mode reap/rollback.
    #[serde(default)]
    root_pane_terminal_id: String,
}

/// The directory holding this server's delegates: one file per name, under the
/// process's own state dir.
///
/// Keyed by the socket path rather than a server name because the socket path is
/// the thing a client actually connects to — two sessions, two sockets, two
/// delegates with the same name, and a shared registry would have them closing
/// each other's workspaces.
fn registry_dir() -> PathBuf {
    crate::config::state_dir()
        .join("delegates")
        .join(server_key())
}

/// FNV-1a 64 over the socket path string, as 16 lowercase hex digits.
///
/// Not canonicalized on purpose: the socket is addressed by the string the CLI
/// was given, and two spellings of the same file are two connections for this
/// purpose. Sixteen hex digits keeps the directory name a safe file name on
/// every platform without a lossy short form.
fn server_key() -> String {
    let path = ApiClient::local().socket_path();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn entry_path(name: &str) -> PathBuf {
    registry_dir().join(format!("{name}.json"))
}

fn lock_path(name: &str) -> PathBuf {
    registry_dir().join(format!("{name}.lock"))
}

fn read_entry(name: &str) -> Option<Entry> {
    let body = std::fs::read_to_string(entry_path(name)).ok()?;
    serde_json::from_str(&body).ok()
}

/// Write the entry atomically, and never leave a temp file behind.
///
/// Mode 0600 at CREATION rather than by a later `set_permissions`: the window
/// between the two is a window in which a registry entry — which names a
/// checkout and a session — is world-readable. The rename is what makes the
/// update atomic: a `delegate status` running against a half-written file reads
/// the previous round, not a prefix of the next one.
fn write_entry(entry: &Entry) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let path = entry_path(&entry.name);
    let dir = path.parent().expect("the entry path always has a parent");
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".{}.json.tmp", entry.name));
    let body = serde_json::to_vec(entry).map_err(|err| {
        io::Error::other(format!("could not serialize the registry entry: {err}"))
    })?;

    let written = (|| -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&body)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    Ok(())
}

/// The exclusive lock one round at a time is enforced with.
///
/// Held by an open `File` for the whole command, through the await: the two
/// ways a second round could start are a second `start`/`send` and a second
/// process, and only an OS lock catches the second. `try_lock` rather than
/// `lock` because a busy delegate is an answer, not a queue: blocking here would
/// turn `delegate send` into an unbounded wait nobody asked for.
struct Lock {
    /// Held open and never dropped early: the lock exists for exactly as long as
    /// the `File` does, so closing it early would hand a second round the
    /// delegate while this one is still typing into it.
    _file: std::fs::File,
}

fn take_lock(name: &str) -> Result<Lock, &'static str> {
    let path = lock_path(name);
    let Some(dir) = path.parent() else {
        return Err("the lock path has no parent");
    };
    std::fs::create_dir_all(dir).map_err(|_| "could not create the delegate registry directory")?;
    let file = open_lock_file(&path).map_err(|_| "could not open the delegate lock")?;
    match file.try_lock() {
        Ok(()) => Ok(Lock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err("busy"),
        Err(std::fs::TryLockError::Error(_)) => Err("could not lock"),
    }
}

fn open_lock_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

// ------------------------------------------------------------------ flags

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct StartFlags {
    brief: Option<String>,
    cwd: Option<String>,
    worktree: bool,
    repo: Option<String>,
    branch: Option<String>,
    base: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    await_result: bool,
    timeout_ms: Option<u64>,
    settle_ms: Option<u64>,
    ready_timeout_ms: Option<u64>,
    max_chars: Option<u32>,
    json: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SendFlags {
    brief: Option<String>,
    await_result: bool,
    timeout_ms: Option<u64>,
    settle_ms: Option<u64>,
    ready_timeout_ms: Option<u64>,
    max_chars: Option<u32>,
    json: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct WaitFlags {
    after: Option<String>,
    timeout_ms: Option<u64>,
    settle_ms: Option<u64>,
    max_chars: Option<u32>,
    json: bool,
}

/// What one verb accepts, and what each of its flags means.
///
/// One table rather than six parse loops, because every verb accepts a subset of
/// the same set and a hand-copied parser is how `--ready-timeout` ends up
/// documented on `start` and unknown to `send`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Start,
    Send,
    Wait,
    Result,
    Status,
    Reap,
}

impl Verb {
    fn usage(self) -> &'static str {
        match self {
            Self::Start => START_USAGE,
            Self::Send => SEND_USAGE,
            Self::Wait => WAIT_USAGE,
            Self::Result => RESULT_USAGE,
            Self::Status => STATUS_USAGE,
            Self::Reap => REAP_USAGE,
        }
    }

    fn accepts(self, flag: &str) -> bool {
        match flag {
            "--json" => true,
            "--max-chars" => !matches!(self, Self::Status | Self::Reap),
            "--await" => matches!(self, Self::Start | Self::Send),
            "--timeout" => matches!(self, Self::Start | Self::Send | Self::Wait),
            "--settle" => matches!(self, Self::Start | Self::Send | Self::Wait),
            "--ready-timeout" => matches!(self, Self::Start | Self::Send),
            "--brief" => matches!(self, Self::Start | Self::Send),
            "--cwd" | "--worktree" | "--repo" | "--branch" | "--base" | "--harness" | "--model" => {
                self == Self::Start
            }
            "--after" => self == Self::Wait,
            "--force" => self == Self::Reap,
            _ => false,
        }
    }
}

/// Parse one verb's flags, after the name.
///
/// A flag the verb does not accept is a usage error rather than a value, so
/// `--after` on `start` cannot be silently read as something else.
fn parse_flags(verb: Verb, args: &[String]) -> Result<Parsed, String> {
    let mut flags = Parsed::default();
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        if !verb.accepts(flag) {
            return Err(format!(
                "unknown option for `flk delegate {}`: {flag}\nusage: {}",
                verb_word(verb),
                verb.usage()
            ));
        }
        let value = |index: &mut usize| -> Result<String, String> {
            let next = args
                .get(*index + 1)
                .cloned()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            *index += 2;
            Ok(next)
        };
        match flag {
            "--json" => {
                flags.json = true;
                index += 1;
            }
            "--await" => {
                flags.await_result = true;
                index += 1;
            }
            "--worktree" => {
                flags.worktree = true;
                index += 1;
            }
            "--force" => {
                flags.force = true;
                index += 1;
            }
            "--timeout" => flags.timeout_ms = Some(number(flag, &value(&mut index)?)?),
            "--settle" => flags.settle_ms = Some(number(flag, &value(&mut index)?)?),
            "--ready-timeout" => flags.ready_timeout_ms = Some(number(flag, &value(&mut index)?)?),
            "--max-chars" => flags.max_chars = Some(number(flag, &value(&mut index)?)?),
            "--brief" => flags.brief = Some(value(&mut index)?),
            "--cwd" => flags.cwd = Some(value(&mut index)?),
            "--repo" => flags.repo = Some(value(&mut index)?),
            "--branch" => flags.branch = Some(value(&mut index)?),
            "--base" => flags.base = Some(value(&mut index)?),
            "--harness" => flags.harness = Some(value(&mut index)?),
            "--model" => flags.model = Some(value(&mut index)?),
            "--after" => flags.after = Some(value(&mut index)?),
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(flags)
}

fn verb_word(verb: Verb) -> &'static str {
    match verb {
        Verb::Start => "start",
        Verb::Send => "send",
        Verb::Wait => "wait",
        Verb::Result => "result",
        Verb::Status => "status",
        Verb::Reap => "reap",
    }
}

fn number<T: std::str::FromStr>(flag: &str, value: &str) -> Result<T, String> {
    value
        .parse::<T>()
        .map_err(|_| format!("invalid value for {flag}: {value}"))
}

/// Every flag, in one flat struct. Six verbs over one flag set: a struct per
/// verb would be six types to keep in step for no gain, since each verb's parser
/// already rejects the flags it does not take.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Parsed {
    brief: Option<String>,
    cwd: Option<String>,
    worktree: bool,
    repo: Option<String>,
    branch: Option<String>,
    base: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    after: Option<String>,
    await_result: bool,
    timeout_ms: Option<u64>,
    settle_ms: Option<u64>,
    ready_timeout_ms: Option<u64>,
    max_chars: Option<u32>,
    force: bool,
    json: bool,
}

fn as_start(flags: &Parsed) -> StartFlags {
    StartFlags {
        brief: flags.brief.clone(),
        cwd: flags.cwd.clone(),
        worktree: flags.worktree,
        repo: flags.repo.clone(),
        branch: flags.branch.clone(),
        base: flags.base.clone(),
        harness: flags.harness.clone(),
        model: flags.model.clone(),
        await_result: flags.await_result,
        timeout_ms: flags.timeout_ms,
        settle_ms: flags.settle_ms,
        ready_timeout_ms: flags.ready_timeout_ms,
        max_chars: flags.max_chars,
        json: flags.json,
    }
}

fn as_send(flags: &Parsed) -> SendFlags {
    SendFlags {
        brief: flags.brief.clone(),
        await_result: flags.await_result,
        timeout_ms: flags.timeout_ms,
        settle_ms: flags.settle_ms,
        ready_timeout_ms: flags.ready_timeout_ms,
        max_chars: flags.max_chars,
        json: flags.json,
    }
}

fn as_wait(flags: &Parsed) -> WaitFlags {
    WaitFlags {
        after: flags.after.clone(),
        timeout_ms: flags.timeout_ms,
        settle_ms: flags.settle_ms,
        max_chars: flags.max_chars,
        json: flags.json,
    }
}

// -------------------------------------------------------------- validation

/// A delegate name has to be safe as a file name and safe to type.
///
/// The registry stores one `<name>.json` per delegate and one lock beside it, so
/// a name carrying a separator or a dot would either escape the directory or name
/// a hidden file. Both are refused rather than escaped.
fn validate_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("a delegate needs a name".to_string());
    };
    let head_ok = first.is_ascii_alphanumeric();
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if head_ok && rest_ok && name.len() <= 64 {
        return Ok(());
    }
    Err(format!(
        "delegate name {name:?} must be 1-64 characters of letters, digits, dot, underscore or \
         dash, starting with a letter or digit"
    ))
}

/// The one harness this build drives when `--harness` is absent.
///
/// opencode, as it has always been: a default that moved would silently change
/// what an unqualified `delegate start` runs.
const DEFAULT_HARNESS: &str = "opencode";

/// Resolve `--harness` against the table, naming the refusal with the harnesses
/// this build does drive — a caller who typed `codex` learns what does exist,
/// rather than only that their word was rejected.
fn validate_harness(harness: Option<&str>) -> Result<&'static HarnessSpec, String> {
    let asked = harness.unwrap_or(DEFAULT_HARNESS);
    HARNESSES
        .iter()
        .find(|spec| spec.name == asked)
        .ok_or_else(|| {
            format!(
                "harness {asked} is not supported yet (this build drives {})",
                harness_names()
            )
        })
}

/// Check the placement flags say exactly one thing.
fn validate_placement(flags: &StartFlags) -> Result<Mode, String> {
    let named = flags.cwd.is_some() as usize + flags.worktree as usize;
    if named != 1 {
        return Err("pass exactly one of --cwd PATH or --worktree".to_string());
    }
    if flags.worktree {
        Ok(Mode::Worktree)
    } else {
        Ok(Mode::Cwd)
    }
}

/// `--repo`, `--branch` and `--base` only mean something with `--worktree`.
///
/// With `--cwd` there is no checkout to branch, so accepting them would be
/// accepting a flag that cannot be honoured — the same reason
/// `--ready-timeout` needs `--wait-ready` on `agent start`.
fn validate_worktree_flags(flags: &StartFlags) -> Result<(), String> {
    if flags.cwd.is_some() {
        for (flag, value) in [
            ("--repo", &flags.repo),
            ("--branch", &flags.branch),
            ("--base", &flags.base),
        ] {
            if value.is_some() {
                return Err(format!("{flag} only means something with --worktree"));
            }
        }
    }
    if flags.worktree && flags.branch.is_none() {
        return Err("--worktree needs --branch".to_string());
    }
    Ok(())
}

/// The brief path, made absolute, checked, readable, and returned as the one
/// sentence to type.
///
/// Absolute without resolving symlinks: the sentence names the path the caller
/// wrote, and canonicalizing it would type a path nobody asked for on a machine
/// where the two differ.
///
/// Opened for reading, and the handle dropped, here — before the lock and before
/// any request. `metadata` says a file exists and is regular; it does not say the
/// caller can read it, and a brief that cannot be opened is a refusal, not a
/// failure three requests later with a workspace already created.
fn prepare_brief(path: &str) -> Result<(String, String), String> {
    let absolute = std::path::absolute(Path::new(path))
        .map_err(|err| format!("brief path {path:?} could not be made absolute: {err}"))?;
    let text = absolute
        .to_str()
        .ok_or_else(|| format!("brief path {path:?} is not valid UTF-8"))?
        .to_string();
    for character in text.chars() {
        if character.is_whitespace()
            || character.is_control()
            || UNSAFE_PATH_CHARS.contains(&character)
        {
            return Err(format!(
                "brief path contains a character that is unsafe to type: {text}"
            ));
        }
    }
    let metadata = std::fs::metadata(&absolute)
        .map_err(|err| format!("brief {text} cannot be read: {err}"))?;
    if !metadata.is_file() {
        return Err(format!("brief {text} is not a regular file"));
    }
    drop(
        std::fs::File::open(&absolute)
            .map_err(|err| format!("brief {text} cannot be opened for reading: {err}"))?,
    );
    let sentence = format!("Read {text} and execute it exactly.");
    Ok((text, sentence))
}

// --------------------------------------------------------------- requests

/// One socket request, as an `io::Result` for callers that read it that way.
///
/// A thin wrapper over `bounded`: the cap is `cap_for(method)` (one table),
/// a server refusal becomes `io::Error::other(server_error(..))`, a timeout
/// becomes `ErrorKind::TimedOut`, a transport error unwraps to its original
/// `io::Error`. Nothing in this module passes a cap by hand (W2).
fn request(method: Method, deadline: Option<Instant>) -> io::Result<serde_json::Value> {
    let response = match bounded(method, deadline) {
        Ok(response) => response,
        Err(BoundedError::TimedOut) => return Err(io::Error::from(io::ErrorKind::TimedOut)),
        Err(BoundedError::Transport(e)) => return Err(e),
    };
    if let Some(error) = response.get("error") {
        return Err(io::Error::other(server_error(error)));
    }
    Ok(response)
}

/// Has the caller's deadline passed?
///
/// Re-checked after every response: a reply that lands after the deadline is a
/// timeout whichever way it went, so a command that has run out of clock does not
/// then act on what it read on the way past.
fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

/// How the one `agent.get` for identity resolution ended.
///
/// Three outcomes, kept apart for the one caller that cares — `require_delegate`
/// routes `TimedOut` to `Outcome::Timeout` when a `delegate wait` or `send
/// --await` has a deadline, and treats a transport failure like a missing agent
/// (`None`): we can't prove the delegate is still the one running, so we say
/// "not a delegate" rather than close over an uncertain identity.
enum AgentFetch {
    /// The server named an agent; the record is the body.
    Found(serde_json::Value),
    /// The server answered that no such agent exists (an identity failure, not
    /// a transport one): a reap or status call reads this as "not a delegate".
    Missing,
    /// The caller's deadline passed on the way to or during this request.
    TimedOut,
    /// A transport error that is NOT a deadline event — a dead socket, a
    /// closed connection. Collapsing this into `Missing` (as earlier revisions
    /// did) made an unreachable server look like "not a delegate" (W4). The
    /// reason is carried so the caller can print it.
    Failed(String),
}

/// How often `agent_record` polls when a `NotYet` fires before the deadline.
///
/// Short enough that a slow transport does not keep a caller waiting past its
/// cap; long enough that a frozen server does not get bombarded.
const AGENT_POLL: Duration = Duration::from_millis(200);

/// Look up the agent record bounded by the caller's deadline. Classifies the
/// outcome via `await_failure`: `Timeout` → `TimedOut`, `Fail` → `Failed`,
/// `NotYet` → poll again until the deadline. Without a deadline a `NotYet`
/// is treated as `Failed`: there is no clock to retry against.
fn agent_record(target: &str, deadline: Option<Instant>) -> AgentFetch {
    loop {
        let bounded_res = bounded(
            Method::AgentGet(AgentTarget {
                target: target.to_string(),
            }),
            deadline,
        );
        let response = match bounded_res {
            Ok(response) => response,
            Err(err) => {
                return match await_failure(&err, deadline, Instant::now()) {
                    AwaitFailure::Timeout => AgentFetch::TimedOut,
                    AwaitFailure::Fail(reason) => AgentFetch::Failed(reason),
                    AwaitFailure::NotYet => {
                        // No deadline → nothing to retry against; a persistent
                        // transport blip must not be a silent "not a delegate".
                        if let Some(d) = deadline {
                            if Instant::now() >= d {
                                return AgentFetch::TimedOut;
                            }
                            sleep_bounded(d, AGENT_POLL);
                            continue;
                        }
                        return AgentFetch::Failed(err.to_string());
                    }
                };
            }
        };
        if let Some(error) = response.get("error") {
            let err = parse_server_error(error);
            // `agent_not_found` and friends are what the server says when the
            // agent is gone — NotOurs for identity, Missing here.
            if err.code == "agent_not_found" || err.code == "pane_not_found" {
                return AgentFetch::Missing;
            }
            return AgentFetch::Failed(err.to_string());
        }
        // A success envelope without a `/result/agent` object is malformed:
        // the server did not say "no such agent" (that would be an error
        // code, handled above) and it did not say "here is the agent" either.
        // Collapsing this into `Missing` would make the delegate report "not
        // a delegate" against a buggy or old server (W13 / G6).
        return match response
            .pointer("/result/agent")
            .filter(|record| record.is_object())
            .cloned()
        {
            Some(record) => AgentFetch::Found(record),
            None => AgentFetch::Failed("malformed agent.get answer".to_string()),
        };
    }
}

/// Shortcut for callers that cannot tell timeout from "missing"/"failed" apart.
///
/// Most callers of `agent_record` want the record if it exists and nothing if
/// it doesn't — the deadline distinction is only interesting to the one caller
/// that routes it to `Outcome::Timeout`. This helper keeps those sites short.
fn agent_record_opt(target: &str, deadline: Option<Instant>) -> Option<serde_json::Value> {
    match agent_record(target, deadline) {
        AgentFetch::Found(record) => Some(record),
        AgentFetch::Missing | AgentFetch::TimedOut | AgentFetch::Failed(_) => None,
    }
}

fn field<'a>(record: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    record.get(key).and_then(serde_json::Value::as_str)
}

/// A field at a JSON pointer, for the places the answer is nested inside a
/// result object rather than sitting beside the other agent fields.
fn at<'a>(value: &'a serde_json::Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(serde_json::Value::as_str)
}

/// Two paths that name the same place, compared as paths rather than as strings.
///
/// The registry records what the server sent; a later check compares it against
/// what the server sends now. Comparing the raw strings would refuse to recognise
/// the same checkout after the caller and the server spell a relative component
/// differently, and a refused identity check here means never cleaning up.
fn same_path(left: &str, right: &str) -> bool {
    let absolute = |value: &str| {
        std::path::absolute(Path::new(value)).unwrap_or_else(|_| PathBuf::from(value))
    };
    absolute(left) == absolute(right)
}

// ------------------------------------------------------------------ start

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Worktree,
    Cwd,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::Cwd => "cwd",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "worktree" => Some(Self::Worktree),
            "cwd" => Some(Self::Cwd),
            _ => None,
        }
    }
}

/// The workspace a delegate start created, before any agent is in it.
struct Placement {
    workspace_id: String,
    /// The checkout the agent runs in: the new worktree, or the `--cwd` path.
    cwd: String,
    worktree: Option<String>,
    branch: Option<String>,
    root_pane: String,
    /// The terminal_id of the root pane (from workspace.create or worktree.create response).
    /// Used for positive identity in cwd-mode rollback (K3).
    root_pane_terminal_id: String,
    parent_workspace_id: Option<String>,
    repo_root: Option<String>,
    repo_key: Option<String>,
}

impl Placement {
    /// Everything a teardown is allowed to touch.
    ///
    /// Derived from the placement rather than from the registry, because a start
    /// that fails before the entry is written still has to clean up after itself,
    /// and because a registry entry can be stale in a way a live placement is not.
    fn cleanup(&self, name: &str, mode: Mode, pane_id: &str, terminal_id: &str) -> Cleanup {
        Cleanup {
            workspace_id: self.workspace_id.clone(),
            pane_id: pane_id.to_string(),
            root_pane: self.root_pane.clone(),
            root_pane_terminal_id: self.root_pane_terminal_id.clone(),
            terminal_id: terminal_id.to_string(),
            mode,
            worktree: self.worktree.clone(),
            parent_workspace_id: self.parent_workspace_id.clone(),
            repo_root: self.repo_root.clone(),
            repo_key: self.repo_key.clone(),
            name: name.to_string(),
        }
    }
}

/// Every workspace the server lists, as records. Returns `None` if the request failed.
fn workspace_records(deadline: Option<Instant>) -> Option<Vec<serde_json::Value>> {
    let bounded_res: Result<serde_json::Value, BoundedError> =
        bounded(Method::WorkspaceList(EmptyParams::default()), deadline);
    match bounded_res {
        Ok(response) => response
            .pointer("/result/workspaces")
            .and_then(|workspaces| workspaces.as_array())
            .cloned(),
        Err(_) => None,
    }
}

/// Create the delegate's workspace, and say what it created.
///
/// On any failure after the create request has gone out, this cleans up through
/// the same routine `reap` runs and returns the exit code with the cause already
/// on stderr — so there is no path where a start has asked the server to create
/// something and then walked away from the answer.
fn place(name: &str, flags: &StartFlags, mode: Mode) -> Result<Placement, i32> {
    // The pre-create list is `None` if the LIST request failed, never defaulted to
    // empty: a `[]` default would make every post-create checkout look new and
    // force-kill it. In cwd mode we never list — there is no worktree to compare.
    let (repo, worktrees_before): (String, Option<Vec<serde_json::Value>>) = match mode {
        Mode::Worktree => {
            let repo = match &flags.repo {
                Some(repo) => repo.clone(),
                None => match std::env::current_dir() {
                    Ok(cwd) => cwd.display().to_string(),
                    Err(err) => {
                        return Err(usage(format!(
                            "could not read the current directory: {err}"
                        )))
                    }
                },
            };
            let worktrees_before = worktree_list_plain(&repo, None);
            (repo, worktrees_before)
        }
        Mode::Cwd => (String::new(), None),
    };
    let bounded_res = match mode {
        Mode::Worktree => bounded(
            Method::WorktreeCreate(WorktreeCreateParams {
                cwd: Some(repo.clone()),
                branch: flags.branch.clone(),
                base: flags.base.clone(),
                focus: false,
                ..WorktreeCreateParams::default()
            }),
            None,
        ),
        Mode::Cwd => {
            let cwd = flags
                .cwd
                .clone()
                .expect("placement was validated before anything was created");
            bounded(
                Method::WorkspaceCreate(WorkspaceCreateParams {
                    cwd: Some(cwd),
                    focus: false,
                    label: None,
                }),
                None,
            )
        }
    };

    // Classify the failure, with the pre-create list carried through: a `None`
    // means the inventory failed, so no destructive check is safe (K2).
    let response = match bounded_res {
        Ok(response) => {
            if let Some(error) = response.get("error") {
                // A server refusal means the server CREATED nothing, so
                // leftover_check(Refused, _) is always Nothing: do not touch
                // the inventory, just report the refusal. Wrapping the error
                // in CreateFailure keeps the two sides of place's decision
                // table in one shape — the pure function proves the policy.
                let failure = CreateFailure::Refused(parse_server_error(error));
                return Err(teardown_after_place_failure(
                    name,
                    worktrees_before.as_deref(),
                    mode,
                    flags,
                    &repo,
                    failure,
                ));
            }
            response
        }
        Err(BoundedError::TimedOut) => {
            return Err(teardown_after_place_failure(
                name,
                worktrees_before.as_deref(),
                mode,
                flags,
                &repo,
                CreateFailure::TimedOut,
            ))
        }
        Err(BoundedError::Transport(e)) => {
            return Err(teardown_after_place_failure(
                name,
                worktrees_before.as_deref(),
                mode,
                flags,
                &repo,
                CreateFailure::Transport(e.kind()),
            ))
        }
    };

    let workspace_id = at(&response, "/result/workspace/workspace_id").map(str::to_string);
    let root_pane = at(&response, "/result/root_pane/pane_id").map(str::to_string);
    let root_pane_terminal_id = at(&response, "/result/root_pane/terminal_id").map(str::to_string);
    let worktree = at(&response, "/result/worktree/path").map(str::to_string);
    // The branch is what the server decided, not what was asked for: `--branch`
    // is optional, and a server that generated one has recorded a name the
    // operator would never have guessed.
    let branch = at(&response, "/result/worktree/branch").map(str::to_string);
    let repo_root = at(&response, "/result/workspace/worktree/repo_root").map(str::to_string);
    let repo_key = at(&response, "/result/workspace/worktree/repo_key").map(str::to_string);
    // The server tells us which parent workspace it created (if any). We record
    // exactly what the server says — no list diffs, no heuristics.
    let parent_workspace_id = at(&response, "/result/parent_workspace_id").map(str::to_string);

    // Catch the two malformed-success shapes before building a Placement
    // that would be a lie (W13 / G7). A worktree-mode success that names no
    // checkout path is just as malformed as one with no workspace id: an
    // entry with `worktree: None` written to the registry in worktree mode
    // would never be reapable, because the recorded path is the evidence.
    let has_core_fields = workspace_id.is_some()
        && root_pane.is_some()
        && root_pane_terminal_id.is_some()
        && (mode == Mode::Cwd || worktree.is_some());
    if !has_core_fields {
        // Malformed success response: server said OK but one of the fields
        // we need is missing. Destroy ONLY what the response actually
        // names: a worktree path that the pre-create list did not have.
        let reason = "the server created the workspace but its answer was malformed";
        if mode == Mode::Worktree {
            if let (Some(path), Some(before)) = (worktree.as_deref(), worktrees_before.as_ref()) {
                if checkout_is_new(before, path) {
                    match kill(None, Some(path), true) {
                        Ok(_) => {
                            eprintln!("delegate {name}: rollback removed stray checkout at {path}");
                        }
                        Err(err) if is_already_gone(&err, Some(path)) => {
                            // Already gone — the state we wanted.
                        }
                        Err(err) => {
                            eprintln!("delegate {name}: rollback also failed: {}", err.message);
                        }
                    }
                    return Err(fail(reason.to_string()));
                }
                eprintln!(
                    "delegate {name}: the create succeeded but was malformed; the response named {path}, which was present before — may be left behind"
                );
            } else if let Some(path) = worktree.as_deref() {
                eprintln!(
                    "delegate {name}: the create succeeded but was malformed; a checkout at {path} may be left behind"
                );
            } else {
                eprintln!(
                    "delegate {name}: the create succeeded but was malformed; a checkout may be left behind"
                );
            }
        } else {
            eprintln!(
                "delegate {name}: the create succeeded but was malformed; a workspace may be left behind"
            );
        }
        return Err(fail(reason.to_string()));
    }
    let workspace_id = workspace_id.expect("checked above");
    let root_pane = root_pane.expect("checked above");
    let root_pane_terminal_id = root_pane_terminal_id.expect("checked above");

    let cwd = worktree
        .clone()
        .unwrap_or_else(|| flags.cwd.clone().unwrap_or_default());
    Ok(Placement {
        workspace_id,
        cwd,
        worktree,
        branch,
        root_pane,
        root_pane_terminal_id,
        parent_workspace_id,
        repo_root,
        repo_key,
    })
}

/// Undo a create whose answer could not be used, and hand back the exit code.
///
/// On a place failure there is no placement, so nothing here closes a parent
/// workspace (K2's governing rule: with no positive evidence of what the
/// server created, destroy nothing). The one destructive branch is for a
/// transport failure whose pre-create inventory succeeded: that is the only
/// shape in which "a new checkout appeared after our timed-out create" has
/// a sound definition. Every other shape prints what might be left behind
/// and exits without touching the server.
///
/// `worktrees_before` is `None` when the pre-create `worktree.list` failed,
/// never defaulted to empty: a `[]` default would make every checkout look
/// new and the force-kill would hit the operator's work.
fn teardown_after_place_failure(
    name: &str,
    worktrees_before: Option<&[serde_json::Value]>,
    mode: Mode,
    flags: &StartFlags,
    repo: &str,
    failure: CreateFailure,
) -> i32 {
    teardown_after_place_failure_with(
        name,
        worktrees_before,
        mode,
        flags,
        failure,
        |deadline| worktree_list_plain(repo, deadline),
        |path| kill(None, Some(path), true),
    )
}

/// The decision half of [`teardown_after_place_failure`], with its two
/// side-effecting steps handed in as closures. Pure given those, so the
/// call-site tests (W7) can drive it with fakes. See the wrapper above for
/// what the policy is.
fn teardown_after_place_failure_with(
    name: &str,
    worktrees_before: Option<&[serde_json::Value]>,
    mode: Mode,
    flags: &StartFlags,
    failure: CreateFailure,
    list_after: impl FnOnce(Option<Instant>) -> Option<Vec<serde_json::Value>>,
    kill_path: impl FnOnce(&str) -> Result<serde_json::Value, ServerError>,
) -> i32 {
    let reason = failure.describe();
    if mode == Mode::Cwd {
        // workspace.create: nothing on disk belongs to the delegate yet, so
        // there is no leftover to kill (W3). Say so on stderr for a transport
        // or timeout failure — the server may have created a workspace we
        // cannot see — and destroy nothing.
        if matches!(
            failure,
            CreateFailure::TimedOut | CreateFailure::Transport(_)
        ) {
            let cwd = flags.cwd.as_deref().unwrap_or("<unknown>");
            eprintln!(
                "delegate {name}: workspace.create did not answer; a workspace for {cwd} may be left open"
            );
        }
        return fail(reason);
    }
    let branch_label = flags.branch.as_deref().unwrap_or("<unknown>");
    match leftover_check(&failure, worktrees_before.is_some()) {
        LeftoverCheck::Nothing => fail(reason),
        LeftoverCheck::CannotCheck => {
            eprintln!("delegate {name}: could not check for a leftover checkout of {branch_label}");
            fail(reason)
        }
        LeftoverCheck::Look => {
            let before = worktrees_before.expect("Look implies the pre-create list succeeded");
            let worktrees_after = match list_after(None) {
                Some(list) => list,
                None => {
                    // A failed post-create list is "cannot check": print and
                    // destroy nothing (K2).
                    eprintln!(
                        "delegate {name}: could not check for a leftover checkout of {branch_label}"
                    );
                    return fail(reason);
                }
            };
            let new_checkout = find_new_checkout(before, &worktrees_after, flags.branch.as_deref());
            if let Some(checkout) = new_checkout {
                match kill_path(&checkout) {
                    Ok(_) => {
                        eprintln!("delegate {name}: rollback removed stray checkout at {checkout}");
                    }
                    Err(err) if is_already_gone(&err, Some(&checkout)) => {
                        // Already gone — the state we wanted.
                    }
                    Err(err) => {
                        eprintln!("delegate {name}: rollback also failed: {}", err.message);
                    }
                }
            } else {
                eprintln!(
                    "delegate {name}: rollback found no new checkout to remove (branch filter: {:?})",
                    flags.branch.as_deref()
                );
            }
            fail(reason)
        }
    }
}

impl CreateFailure {
    /// A human-readable description of the failure, for stderr. Reads both
    /// variant fields so a transport error names its kind and a refusal names
    /// the server's own code and message — the dead_code lint is a consequence
    /// of that, not a reason to allow unused fields (R6: no `allow(dead_code)`
    /// in this module).
    fn describe(&self) -> String {
        match self {
            Self::Refused(err) => err.to_string(),
            Self::TimedOut => "create request timed out".to_string(),
            Self::Transport(kind) => format!("create failed: transport error ({kind:?})"),
        }
    }
}

/// Was `path` absent from the pre-create `worktree.list`? Compared with
/// `same_path`, never as raw strings: a respelled pre-existing checkout must
/// never read as new, because "new" authorises a force kill (G12, rr5 H2).
fn checkout_is_new(worktrees_before: &[serde_json::Value], path: &str) -> bool {
    !worktrees_before.iter().any(|w| {
        w.get("path")
            .and_then(|v| v.as_str())
            .is_some_and(|before| same_path(before, path))
    })
}

/// Find a checkout that appeared after the create call, matching on --branch if given.
///
/// The checkout is identified by its path, compared through `same_path` so a
/// pre-existing checkout respelled by the server (relative vs absolute,
/// trailing slash) is not treated as "new" and force-killed (W13 / G12). We
/// use `worktree.list` to find checkouts, filtering by branch if given.
fn find_new_checkout(
    worktrees_before: &[serde_json::Value],
    worktrees_after: &[serde_json::Value],
    branch: Option<&str>,
) -> Option<String> {
    for worktree in worktrees_after {
        let Some(path) = worktree.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        if !checkout_is_new(worktrees_before, path) {
            continue;
        }
        if let Some(branch_name) = branch {
            let wt_branch = worktree.get("branch").and_then(|v| v.as_str());
            if wt_branch != Some(branch_name) {
                continue;
            }
        }
        return Some(path.to_string());
    }
    None
}
fn worktree_list_plain(repo: &str, deadline: Option<Instant>) -> Option<Vec<serde_json::Value>> {
    let response = match bounded(
        Method::WorktreeList(WorktreeListParams {
            workspace_id: None,
            cwd: Some(repo.to_string()),
            scan: false,
        }),
        deadline,
    ) {
        Ok(response) => response,
        Err(_) => return None,
    };
    response
        .pointer("/result/worktrees")
        .and_then(|worktrees| worktrees.as_array())
        .cloned()
}

// ---------------------------------------------------------------- teardown

/// A server refusal, kept as code and message so a caller can map the code.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerError {
    code: String,
    message: String,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// A transport failure carries no server code, so it becomes one — named
/// `delegate_unreachable`, and mapped by `worktree.kill`'s exit table to 1, which
/// is what every other non-server failure in this command already exits with.
///
/// Without it the routine could not return a single error type, and the refusal
/// it is built to stop swallowing (a server that answered `dirty_worktree_
/// requires_force`) would be lost in a transport failure's place.
impl From<io::Error> for ServerError {
    fn from(err: io::Error) -> Self {
        Self {
            code: "delegate_unreachable".to_string(),
            message: err.to_string(),
        }
    }
}

fn server_error(error: &serde_json::Value) -> String {
    let error = parse_server_error(error);
    format!("{error}")
}

fn parse_server_error(error: &serde_json::Value) -> ServerError {
    ServerError {
        code: error
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("error")
            .to_string(),
        message: error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("the server refused the request")
            .to_string(),
    }
}

/// What a teardown may touch, and what it removed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cleanup {
    name: String,
    workspace_id: String,
    pane_id: String,
    root_pane: String,
    root_pane_terminal_id: String,
    terminal_id: String,
    mode: Mode,
    worktree: Option<String>,
    parent_workspace_id: Option<String>,
    repo_root: Option<String>,
    repo_key: Option<String>,
}

impl Cleanup {
    fn from_entry(entry: &Entry) -> Option<Self> {
        Some(Self {
            name: entry.name.clone(),
            workspace_id: entry.workspace_id.clone(),
            pane_id: entry.pane_id.clone(),
            root_pane: entry.root_pane.clone(),
            root_pane_terminal_id: entry.root_pane_terminal_id.clone(),
            terminal_id: entry.terminal_id.clone(),
            mode: Mode::parse(&entry.mode)?,
            worktree: entry.worktree.clone(),
            parent_workspace_id: entry.parent_workspace_id.clone(),
            repo_root: entry.repo_root.clone(),
            repo_key: entry.repo_key.clone(),
        })
    }
}

/// What a teardown actually removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TearDown {
    workspace_closed: bool,
    checkout_removed: bool,
    parent_closed: bool,
}

/// THE cleanup routine: a failed start's rollback and `delegate reap`'s removal,
/// and nothing else (D-B).
///
/// Three properties, each of them a thing the two callers used to get wrong
/// separately:
///
/// * **A server refusal is never ignored.** A kill that answered `dirty_worktree_
///   requires_force` and was dropped on the floor left a start reporting "rolled
///   back" over a checkout still on disk. So a refusal is returned, and each
///   caller decides what it means: a reap prints it and maps the code through
///   `kill_error_exit_code`, a rollback prints it beside its cause and keeps exit
///   1.
/// * **Nothing is destroyed before its identity is re-checked** (D-C). A
///   workspace id is a name, and a server restart can hand the same name to a
///   different workspace. In worktree mode the check is that the workspace still
///   holds the recorded checkout; in cwd mode, that it still holds a pane whose
///   terminal_id matches one we recorded (the agent's terminal_id or the root
///   pane's terminal_id). A mismatch is treated as gone — the checkout is reached
///   by path instead, and the workspace is left alone.
/// * **The parent workspace is only closed when it is still the parent.** It is
///   closed only if it exists, still holds the recorded repository root, holds a
///   single pane, and no other open workspace is a linked worktree of the same
///   repository. Any of those failing leaves it open: a repository-root workspace
///   the operator has since put panes in is not a leftover.
fn tear_down(target: &Cleanup, force: bool, close_parent: bool) -> Result<TearDown, ServerError> {
    let mut done = TearDown::default();

    if !target.workspace_id.is_empty() {
        match identity(target) {
            Identity::Ours => match target.mode {
                Mode::Worktree => {
                    // `kill` turns a server error into `Err`. "Already gone"
                    // IS the state we wanted, so match on the error rather
                    // than inspecting a response that is only ever a
                    // success envelope (W10).
                    match kill(Some(&target.workspace_id), None, force) {
                        Ok(_) => {
                            // The kill closes the workspace and removes the
                            // checkout with it, so both are accounted for.
                            done.workspace_closed = true;
                            done.checkout_removed = true;
                        }
                        Err(err) if is_already_gone(&err, None) => {
                            // The workspace is gone; the checkout might
                            // still be there and the path kill below will
                            // handle it.
                            done.workspace_closed = true;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Mode::Cwd => match close_workspace(&target.workspace_id) {
                    Ok(_) => {
                        done.workspace_closed = true;
                    }
                    Err(err) if err.code == "workspace_not_found" => {
                        done.workspace_closed = true;
                    }
                    Err(err) => return Err(err),
                },
            },
            Identity::NotOurs => {
                // The server answered and the evidence is against us: the id
                // belongs to someone else now, or the recorded panes are gone.
                // Leave the workspace alone. In worktree mode, the recorded
                // checkout path is still evidence we may act on — fall through
                // to the path kill below.
            }
            Identity::Unknown(reason) => {
                // The request failed or the answer was malformed. In cwd mode
                // there is no fallback evidence to act on, so refuse the whole
                // operation — nothing is destroyed, nothing is forgotten (W1).
                // In worktree mode the recorded checkout path is the evidence
                // we fall through to below as a path kill — a failure there
                // is already reported as an error.
                if target.mode == Mode::Cwd {
                    return Err(ServerError {
                        code: "identity_unknown".to_string(),
                        message: format!(
                            "could not establish whether workspace {} is still delegate {}'s: {reason}",
                            target.workspace_id, target.name
                        ),
                    });
                }
            }
        }
    }

    // The checkout the delegate made is still there when the workspace was
    // gone (or was never ours), and `worktree.kill` reaches it by path.
    // "Already gone" is the goal, not an error (W10 / a29).
    if let Some(checkout) = target.worktree.clone() {
        if !done.checkout_removed {
            match kill(None, Some(&checkout), force) {
                Ok(_) => {
                    done.checkout_removed = true;
                }
                Err(err) if is_already_gone(&err, Some(&checkout)) => {
                    done.checkout_removed = true;
                }
                Err(err) => return Err(err),
            }
        }
    }

    if close_parent {
        if let Some(parent) = parent_to_close(target)? {
            match close_workspace(&parent) {
                Ok(_) => {
                    done.parent_closed = true;
                }
                Err(err) if err.code == "workspace_not_found" => {
                    // The parent is already gone; the end state we wanted.
                    done.parent_closed = true;
                }
                Err(err) => return Err(err),
            }
        }
    }

    Ok(done)
}

/// Does this error from `kill` or `close_workspace` mean the target was
/// already not there to remove?
///
/// Checks both the server-reported code (one of `ALREADY_GONE_CODES`) and —
/// when a checkout path is handed in — the filesystem: a path that no longer
/// exists cannot be killed, and the server's wording may vary.
fn is_already_gone(err: &ServerError, checkout: Option<&str>) -> bool {
    // No answer means nothing is known about the server side: a missing local
    // path is not evidence that the server-side removal happened (rr5 H1).
    if matches!(
        err.code.as_str(),
        "timeout" | "transport" | "delegate_unreachable"
    ) {
        return false;
    }
    if ALREADY_GONE_CODES.contains(&err.code.as_str()) {
        return true;
    }
    if let Some(path) = checkout {
        if !Path::new(path).exists() {
            return true;
        }
    }
    false
}

/// Is the recorded workspace still the one this delegate created?
///
/// Check if any pane in the workspace holds one of the recorded terminal IDs.
/// Returns true when any pane's terminal_id equals a NON-EMPTY recorded id.
/// A `None` (failed `pane.list`) or no non-empty recorded id returns false.
fn holds_recorded_terminal(panes: Option<&[serde_json::Value]>, recorded: &[&str]) -> bool {
    let panes = match panes {
        Some(p) => p,
        None => return false,
    };
    for recorded_id in recorded {
        if recorded_id.is_empty() {
            continue;
        }
        for pane in panes {
            if pane.get("terminal_id").and_then(serde_json::Value::as_str) == Some(*recorded_id) {
                return true;
            }
        }
    }
    false
}

/// What `identity` concluded, with the three answers kept apart (W1).
///
/// `Ours` is the only case a destructive step may act on. `NotOurs` means the
/// server ANSWERED and the evidence is against us (the id has been reused, or
/// the workspace lost the pane we recorded). `Unknown` is "the question could
/// not be asked" — a failed request, a malformed answer — and `tear_down`
/// treats it as a reason to refuse the operation entirely (K-rule: when in
/// doubt, destroy nothing; also, when in doubt, forget nothing either).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Identity {
    Ours,
    NotOurs,
    Unknown(String),
}

/// How a `workspace.get` for the identity check turned out.
///
/// The three outcomes are kept apart because `identity` routes them to three
/// different `Identity` answers: a healthy response → the record itself, a
/// server-level `workspace_not_found` → NotOurs, every other failure (request
/// error, malformed response, any other refusal) → Unknown(reason).
enum WorkspaceLookup {
    Found(serde_json::Value),
    NotFound,
    Failed(String),
}

fn workspace_lookup(workspace_id: &str) -> WorkspaceLookup {
    let bounded_res = bounded(
        Method::WorkspaceGet(WorkspaceTarget {
            workspace_id: workspace_id.to_string(),
        }),
        None,
    );
    let response = match bounded_res {
        Ok(response) => response,
        Err(err) => return WorkspaceLookup::Failed(err.to_string()),
    };
    if let Some(error) = response.get("error") {
        let err = parse_server_error(error);
        if err.code == "workspace_not_found" {
            return WorkspaceLookup::NotFound;
        }
        return WorkspaceLookup::Failed(err.to_string());
    }
    match response.pointer("/result/workspace") {
        Some(record) if record.is_object() => WorkspaceLookup::Found(record.clone()),
        _ => WorkspaceLookup::Failed(
            "workspace.get: the server's answer carried no workspace record".to_string(),
        ),
    }
}

/// Which pane list the identity check should use.
///
/// The three outcomes mirror `WorkspaceLookup`: a listed workspace with a
/// pane array, a `workspace_not_found` (server answered the delete already
/// happened — NotOurs), or any other failure/malformed answer (Unknown with
/// its reason).
enum PaneLookup {
    Found(Vec<serde_json::Value>),
    NotFound,
    Failed(String),
}

fn pane_lookup(workspace_id: &str) -> PaneLookup {
    let bounded_res = bounded(
        Method::PaneList(PaneListParams {
            workspace_id: Some(workspace_id.to_string()),
        }),
        None,
    );
    let response = match bounded_res {
        Ok(response) => response,
        Err(err) => return PaneLookup::Failed(err.to_string()),
    };
    if let Some(error) = response.get("error") {
        let err = parse_server_error(error);
        if err.code == "workspace_not_found" {
            return PaneLookup::NotFound;
        }
        return PaneLookup::Failed(err.to_string());
    }
    match response.pointer("/result/panes").and_then(|v| v.as_array()) {
        Some(panes) => PaneLookup::Found(panes.clone()),
        None => {
            PaneLookup::Failed("pane.list: the server's answer carried no pane array".to_string())
        }
    }
}

/// Pure function to classify the identity of a would-be delegate workspace
/// from the two server answers that could prove it.
///
/// The two modes can only be checked against different things, because they
/// were recorded differently. A worktree workspace is identified by its
/// CHECKOUT — the thing that was actually created. A cwd workspace has no
/// checkout of its own, so it is identified by a PANE it still holds: the
/// agent's, or the shell it was created with. A public pane id is never
/// reused within a server's life, so either is a stronger claim than the
/// workspace id alone.
fn identity_from(
    mode: Mode,
    workspace: WorkspaceLookup,
    recorded_checkout: Option<&str>,
    recorded_panes: &[&str],
    cwd_panes: impl FnOnce() -> PaneLookup,
) -> Identity {
    let record = match workspace {
        WorkspaceLookup::Found(r) => r,
        WorkspaceLookup::NotFound => return Identity::NotOurs,
        WorkspaceLookup::Failed(reason) => return Identity::Unknown(reason),
    };
    match mode {
        Mode::Worktree => {
            let Some(checkout) = recorded_checkout else {
                return Identity::NotOurs;
            };
            let current = at(&record, "/worktree/checkout_path").unwrap_or_default();
            if !current.is_empty() && same_path(current, checkout) {
                Identity::Ours
            } else {
                Identity::NotOurs
            }
        }
        Mode::Cwd => match cwd_panes() {
            PaneLookup::Found(panes) => {
                if holds_recorded_terminal(Some(&panes), recorded_panes) {
                    Identity::Ours
                } else {
                    Identity::NotOurs
                }
            }
            PaneLookup::NotFound => Identity::NotOurs,
            PaneLookup::Failed(reason) => Identity::Unknown(reason),
        },
    }
}

/// What a destructive step gets to know about the workspace it was asked to
/// close. Three answers (W1): `Ours` acts, `NotOurs` leaves the workspace
/// alone (the checkout may still be killed by path in worktree mode), and
/// `Unknown` refuses the operation.
fn identity(target: &Cleanup) -> Identity {
    if target.workspace_id.is_empty() {
        return Identity::NotOurs;
    }
    let recorded = [
        target.terminal_id.as_str(),
        target.root_pane_terminal_id.as_str(),
    ];
    identity_from(
        target.mode,
        workspace_lookup(&target.workspace_id),
        target.worktree.as_deref(),
        &recorded,
        || pane_lookup(&target.workspace_id),
    )
}

/// The parent workspace to close, or `None` to leave it alone.
///
/// Pure function to decide whether a parent workspace is closable.
///
/// Returns true only when ALL of these hold:
/// - parent and repo_root are present;
/// - the parent's worktree.checkout_path equals repo_root (same_path);
/// - its pane_count is exactly 1 (missing -> false);
/// - the list is present;
/// - no row other than ours and the parent itself is a linked worktree of repo_key
///   (with no recorded key, ANY other linked worktree counts).
fn parent_closable(
    parent: Option<&serde_json::Value>,
    repo_root: Option<&str>,
    others: Option<&[serde_json::Value]>,
    ours: &str,
    repo_key: Option<&str>,
) -> bool {
    let parent = match parent {
        Some(p) => p,
        None => return false,
    };
    let repo_root = match repo_root {
        Some(r) => r,
        None => return false,
    };
    let others = match others {
        Some(o) => o,
        None => return false,
    };
    let checkout = parent
        .pointer("/worktree/checkout_path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if checkout.is_empty() || !same_path(checkout, repo_root) {
        return false;
    }
    let pane_count = parent
        .get("pane_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if pane_count != 1 {
        return false;
    }
    for other in others {
        // A row with no `workspace_id` is a malformed list, not an empty slot:
        // the list itself is evidence of trouble, so refuse to close the
        // parent on it (W6).
        let Some(id) = field(other, "workspace_id") else {
            return false;
        };
        if id == ours {
            continue;
        }
        // Also skip the parent itself.
        if let Some(parent_id) = parent
            .get("workspace_id")
            .and_then(serde_json::Value::as_str)
        {
            if id == parent_id {
                continue;
            }
        }
        // Two shapes a row carries:
        // 1. No `worktree` block at all — a workspace that is not a worktree
        //    workspace. `is_linked_worktree` is `false` by absence of the
        //    containing object, so the row does not block the parent close.
        // 2. A `worktree` block IS present — then `is_linked_worktree` MUST
        //    be a boolean. A missing or non-boolean field is a malformed
        //    row and keeps the parent OPEN (W6: ambiguity never permits).
        let worktree_block = other.get("worktree");
        let linked = match worktree_block {
            None | Some(serde_json::Value::Null) => false,
            Some(block) => match block.get("is_linked_worktree") {
                Some(serde_json::Value::Bool(b)) => *b,
                _ => return false,
            },
        };
        let same_repo = repo_key.is_none()
            || other
                .pointer("/worktree/repo_key")
                .and_then(serde_json::Value::as_str)
                == repo_key;
        if linked && same_repo {
            return false;
        }
    }
    true
}

/// All four conditions, and the last one is the expensive one: while another
/// workspace still holds a linked worktree of the same repository, the
/// repository-root workspace is that checkout's parent and closing it would take
/// an operator's work with it.
fn parent_to_close(target: &Cleanup) -> Result<Option<String>, ServerError> {
    parent_to_close_with(
        target,
        |id| match workspace_lookup(id) {
            WorkspaceLookup::Found(record) => Some(record),
            WorkspaceLookup::NotFound | WorkspaceLookup::Failed(_) => None,
        },
        || workspace_records(None),
    )
}

/// The decision half of `parent_to_close`, with its two inventory requests
/// handed in: a parent-record fetch and a workspace list. Pure given those,
/// so the call-site tests (W7) can drive it with fakes.
fn parent_to_close_with(
    target: &Cleanup,
    fetch_parent: impl FnOnce(&str) -> Option<serde_json::Value>,
    list_others: impl FnOnce() -> Option<Vec<serde_json::Value>>,
) -> Result<Option<String>, ServerError> {
    let Some(parent_id) = target.parent_workspace_id.clone() else {
        return Ok(None);
    };
    let parent = fetch_parent(&parent_id);
    let repo_root = target.repo_root.as_deref();
    let others = list_others();
    let ours = target.workspace_id.as_str();
    let repo_key = target.repo_key.as_deref();
    if parent_closable(
        parent.as_ref(),
        repo_root,
        others.as_deref(),
        ours,
        repo_key,
    ) {
        Ok(Some(parent_id))
    } else {
        Ok(None)
    }
}

fn kill(
    workspace_id: Option<&str>,
    path: Option<&str>,
    force: bool,
) -> Result<serde_json::Value, ServerError> {
    let response = match bounded(
        Method::WorktreeKill(WorktreeKillParams {
            workspace_id: workspace_id.map(str::to_string),
            path: path.map(str::to_string),
            force,
            caller_pid: Some(std::process::id()),
            ..WorktreeKillParams::default()
        }),
        None,
    ) {
        Ok(response) => response,
        Err(BoundedError::TimedOut) => {
            return Err(ServerError {
                code: "timeout".to_string(),
                message: "request timed out".to_string(),
            })
        }
        Err(BoundedError::Transport(e)) => {
            return Err(ServerError {
                code: "transport".to_string(),
                message: e.to_string(),
            })
        }
    };
    if let Some(error) = response.get("error") {
        return Err(parse_server_error(error));
    }
    Ok(response)
}

fn close_workspace(workspace_id: &str) -> Result<serde_json::Value, ServerError> {
    let response = match bounded(
        Method::WorkspaceClose(WorkspaceTarget {
            workspace_id: workspace_id.to_string(),
        }),
        None,
    ) {
        Ok(response) => response,
        Err(BoundedError::TimedOut) => {
            return Err(ServerError {
                code: "timeout".to_string(),
                message: "request timed out".to_string(),
            })
        }
        Err(BoundedError::Transport(e)) => {
            return Err(ServerError {
                code: "transport".to_string(),
                message: e.to_string(),
            })
        }
    };
    if let Some(error) = response.get("error") {
        return Err(parse_server_error(error));
    }
    Ok(response)
}

fn delegate_start(args: &[String]) -> io::Result<i32> {
    let Some(name) = args.first() else {
        return Ok(usage(format!("usage: {}", START_USAGE)));
    };
    let flags = match parse_flags(Verb::Start, args) {
        Ok(flags) => flags,
        Err(reason) => return Ok(usage(reason)),
    };
    let flags = as_start(&flags);

    // Every refusal below this line happens before a single request that could
    // create anything — including before the registry directory exists, so a
    // typo leaves the state directory byte-for-byte as it found it.
    if let Err(reason) = validate_name(name) {
        return Ok(usage(reason));
    }
    let mode = match validate_placement(&flags) {
        Ok(mode) => mode,
        Err(reason) => return Ok(usage(reason)),
    };
    if let Err(reason) = validate_worktree_flags(&flags) {
        return Ok(usage(reason));
    }
    let harness = match validate_harness(flags.harness.as_deref()) {
        Ok(harness) => harness,
        Err(reason) => return Ok(usage(reason)),
    };
    let Some(brief_path) = flags.brief.clone() else {
        return Ok(usage(format!(
            "--brief is required\nusage: {}",
            START_USAGE
        )));
    };
    let (brief, sentence) = match prepare_brief(&brief_path) {
        Ok(prepared) => prepared,
        Err(reason) => return Ok(usage(reason)),
    };

    let _lock = match take_lock(name) {
        Ok(lock) => lock,
        Err("busy") => return Ok(fail(format!("delegate {name} is busy"))),
        Err(reason) => return Ok(fail(format!("delegate {name}: {reason}"))),
    };

    if agent_record_opt(name, None).is_some() {
        return Ok(fail(format!("agent_name_taken: {name}")));
    }
    if let Some(existing) = read_entry(name) {
        if let Some(cleanup) = Cleanup::from_entry(&existing) {
            match identity(&cleanup) {
                Identity::Ours => {
                    return Ok(fail(format!("delegate {name} exists; reap it first")));
                }
                Identity::Unknown(reason) => {
                    // We cannot establish whether the workspace is still this
                    // name's — the server did not answer, or the answer was
                    // malformed. Refuse rather than start a second delegate
                    // over one that may still be running (W1 governing rule:
                    // when in doubt, destroy nothing — AND forget nothing).
                    return Ok(fail(format!(
                        "delegate {name}: could not check whether it is already running: {reason}"
                    )));
                }
                Identity::NotOurs => {
                    // The server answered: the workspace is gone or belongs
                    // to someone else. A checkout that is still on disk is
                    // not: it belongs to this name, and a second start would
                    // leave it with no delegate and no way to address it.
                    if let Some(checkout) = existing.worktree.as_deref() {
                        if Path::new(checkout).exists() {
                            return Ok(fail(format!(
                                "delegate {name} has a leftover checkout at {checkout}; reap it first"
                            )));
                        }
                    }
                }
            }
        }
    }

    let placement = match place(name, &flags, mode) {
        Ok(placement) => placement,
        Err(code) => return Ok(code),
    };
    let started = start_the_agent(name, &placement, harness, flags.model.as_deref());
    let agent = match started {
        Ok(agent) => agent,
        Err(reason) => {
            rollback(&placement.cleanup(name, mode, "", ""));
            return Ok(fail(reason));
        }
    };
    let terminal_id = field(&agent, "terminal_id").unwrap_or_default().to_string();
    let pane_id = field(&agent, "pane_id").unwrap_or_default().to_string();
    let cleanup = placement.cleanup(name, mode, &pane_id, &terminal_id);

    // The workspace the delegate created came with a shell in it. Leaving that
    // shell there would mean the operator's "where is my agent" question has two
    // panes in the answer, and an idle wake or a manual type would land in a pane
    // nobody is watching.
    if let Err(err) = request(
        Method::PaneClose(PaneTarget {
            pane_id: placement.root_pane.clone(),
        }),
        None,
    ) {
        rollback(&cleanup);
        return Ok(fail(format!(
            "could not close the workspace's root pane: {err}"
        )));
    }

    let ready_timeout = flags
        .ready_timeout_ms
        .unwrap_or(super::ready::DEFAULT_READY_TIMEOUT_MS);
    let ready_deadline = Instant::now() + Duration::from_millis(ready_timeout);
    // The gate returns the FINAL record it saw, and the cursor is taken from
    // THAT. Captured earlier it would be a cursor from before the last idle, and
    // a settle would then accept the quiet that preceded the brief instead of
    // waiting for the turn the brief caused (D-D).
    let settled_record = match await_ready(name, &agent, harness, ready_deadline) {
        Ok(record) => record,
        Err(reason) => {
            rollback(&cleanup);
            return Ok(fail(reason));
        }
    };
    let cursor = match cursor_of(&settled_record) {
        Ok(cursor) => cursor,
        Err(reason) => {
            rollback(&cleanup);
            return Ok(fail(reason));
        }
    };
    let submitted_at_ms = now_ms();

    let entry = Entry {
        name: name.to_string(),
        terminal_id: terminal_id.clone(),
        pane_id: pane_id.clone(),
        root_pane: placement.root_pane.clone(),
        workspace_id: placement.workspace_id.clone(),
        mode: mode.as_str().to_string(),
        worktree: placement.worktree.clone(),
        branch: placement.branch.clone(),
        parent_workspace_id: placement.parent_workspace_id.clone(),
        repo_root: placement.repo_root.clone(),
        repo_key: placement.repo_key.clone(),
        harness: harness.name.to_string(),
        model: flags.model.clone(),
        round: 1,
        brief: brief.clone(),
        submitted_at_ms,
        cursor: cursor.clone(),
        created_at_ms: now_ms(),
        root_pane_terminal_id: placement.root_pane_terminal_id.clone(),
    };
    if let Err(err) = write_entry(&entry) {
        // The write failed, so THIS process did not create the entry.
        // Removing `entry_path(name)` would delete a pre-existing (and
        // likely stale) entry the operator may still want to see or reap
        // (W11 / a30: forget nothing). `write_entry` already removes its
        // own temp file, so there is nothing of ours to clean up here.
        rollback(&cleanup);
        return Ok(fail(format!(
            "could not write the delegate registry entry: {err}"
        )));
    }

    // No request between persisting the cursor and typing the brief: the
    // whole point of capturing it last is that it is the cursor of the turn
    // this submit starts. The submit is bounded (W2); a server that goes
    // quiet mid-type does not hang start.
    if let Err(reason) = submit_brief(&pane_id, &sentence, None) {
        // The sentence did not land, so this start did not happen. Undo it
        // rather than leave a workspace, a checkout and a registry entry
        // describing a round nobody is running. The entry WAS written by
        // this process on the line above, so removing it here is correct.
        let _ = std::fs::remove_file(entry_path(name));
        rollback(&cleanup);
        return Ok(fail(format!(
            "delegate {name}: the brief was not submitted: {reason}"
        )));
    }

    if !flags.await_result {
        emit_submit(&entry, flags.json);
        return Ok(exit::OK);
    }

    let deadline = flags
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    Await {
        harness,
        entry: &entry,
        after: Some(cursor),
        submitted_at_ms,
        deadline,
        settle_ms: flags.settle_ms,
        max_chars: flags.max_chars,
        json: flags.json,
    }
    .run()
}

/// Undo a start that failed after its workspace existed.
///
/// The one call, and the exit stays 1 whatever it says: a rollback that partly
/// failed is still a failed start, and the reason it could not finish is on
/// stderr beside the cause.
fn rollback(cleanup: &Cleanup) {
    if let Err(err) = tear_down(cleanup, true, true) {
        eprintln!(
            "delegate {}: rollback also failed: {}",
            cleanup.name, err.message
        );
    }
}

/// Type the brief into the agent's pane, then press Enter, each as its own
/// bounded request (W2).
///
/// The `pane` module has a helper that does the same ordered writes, but it
/// routes through the untimed socket helpers upstairs: a server that goes
/// quiet mid-type can hang the whole command forever (F2). This helper takes
/// the same two steps — type, then Enter — through `bounded`, so both halves
/// inherit the 10 s `PaneSendInput` cap and the caller's deadline (if any).
/// A failure returns the server's own words; a timeout returns `TimedOut` so
/// the caller can print "the brief was not submitted: timed out".
fn submit_brief(pane_id: &str, text: &str, deadline: Option<Instant>) -> Result<(), String> {
    // 1) Type the sentence. No keys ride along: that is the whole point.
    bounded_submit(
        Method::PaneSendInput(PaneSendInputParams {
            pane_id: pane_id.to_string(),
            text: text.to_string(),
            keys: Vec::new(),
        }),
        deadline,
    )?;
    // 2) Let the pane's reader come back round before the Enter — the same
    //    gap `pane run` uses, so the submit lands at the same cadence.
    std::thread::sleep(PANE_RUN_SUBMIT_GAP);
    // 3) Press Enter, on its own, as its own write.
    bounded_submit(
        Method::PaneSendKeys(PaneSendKeysParams {
            pane_id: pane_id.to_string(),
            keys: vec!["Enter".to_string()],
        }),
        deadline,
    )?;
    Ok(())
}

/// Make a `bounded` request, map a timeout to a readable "timed out" string,
/// a transport error to its own message, and a server refusal to its code and
/// message. Shared by both halves of `submit_brief`.
fn bounded_submit(method: Method, deadline: Option<Instant>) -> Result<(), String> {
    let response = match bounded(method, deadline) {
        Ok(response) => response,
        Err(BoundedError::TimedOut) => return Err("timed out".to_string()),
        Err(BoundedError::Transport(err)) => return Err(err.to_string()),
    };
    if let Some(error) = response.get("error") {
        return Err(server_error(error));
    }
    Ok(())
}

fn start_the_agent(
    name: &str,
    placement: &Placement,
    harness: &HarnessSpec,
    model: Option<&str>,
) -> Result<serde_json::Value, String> {
    let argv = (harness.argv)(model);
    let response = request(
        Method::AgentStart(AgentStartParams {
            name: name.to_string(),
            cwd: Some(placement.cwd.clone()),
            workspace_id: Some(placement.workspace_id.clone()),
            tab_id: None,
            split: None,
            active: false,
            here: false,
            // The delegate's whole premise is that it lands BESIDE the operator.
            focus: false,
            argv,
        }),
        None,
    )
    .map_err(|err| format!("delegate {name}: could not start the agent: {err}"))?;
    // `request` returns Err on a server-level error already (see its impl),
    // so a response here is a success envelope: no second `error` check (F12).
    response
        .pointer("/result/agent")
        .filter(|record| record.is_object())
        .cloned()
        .ok_or_else(|| format!("delegate {name}: the start answered with no agent record"))
}

/// Lines of a pane's recent buffer the startup-dialog scan reads.
///
/// A dialog is drawn at the bottom of the screen, so a tail this long holds it
/// whole without reading a transcript that may be thousands of lines of
/// unrelated output.
const STARTUP_DIALOG_LINES: u32 = 40;

/// Resolve the harness an entry recorded, for a verb that only has the entry.
///
/// A name this build does not drive — an entry written by a newer flock, or a
/// harness dropped from the table — falls back to the default rather than
/// failing: the verb has a live delegate to wait on, and refusing it because
/// its registry names a harness we no longer have would be a worse answer than
/// waiting with the default's grace and not-yet codes.
fn harness_for(entry: &Entry) -> &'static HarnessSpec {
    HARNESSES
        .iter()
        .find(|spec| spec.name == entry.harness)
        .or_else(|| validate_harness(None).ok())
        .expect("the default harness is always in the table")
}

/// Block until the agent is up AND at its prompt, under one deadline, and hand
/// back the record that proved it.
///
/// Two questions, one budget. A pane that has painted nothing yet is `unknown`
/// and must not be typed into; a pane sitting on a permission prompt is `blocked`
/// and must not be typed into either — a brief typed at a dialog is a brief that
/// answers the dialog. Readiness is `unknown`-clear, the prompt is `idle`/`done`,
/// and both are waited for here so no caller has to know that they are different
/// questions.
///
/// The returned record is the LAST one sampled, not merely the first that
/// qualified: the cursor taken from it has to be the cursor of the turn the brief
/// is about to start.
fn await_ready(
    name: &str,
    agent: &serde_json::Value,
    harness: &HarnessSpec,
    deadline: Instant,
) -> Result<serde_json::Value, String> {
    let pane_id = field(agent, "pane_id").unwrap_or_default().to_string();
    let terminal_id = field(agent, "terminal_id").unwrap_or_default().to_string();

    // W9(b): the readiness wait is re-implemented here on `bounded`,
    // reusing `ready.rs`'s pure classifiers (`classify_ready_event`,
    // `status_is_ready`, `ReadySignal`) but not its I/O. The I/O helpers
    // in `ready.rs` (`current_status`, `last_pane_line`) go through the
    // unbounded `send_request` — reaching them from a delegate would make
    // a stalled server freeze `start` forever (G1).
    delegate_wait_for_ready(name, &terminal_id, &pane_id, deadline)?;

    let mut last_status: Option<String> = None;
    // Asked at most once per wait. `blocked` is derived from the pane's own
    // painted screen, so the dialog is already on it by the time the status
    // says so, and re-reading every poll would cost one request per 200 ms for
    // the whole `--ready-timeout`.
    let mut startup_dialog_asked = false;
    loop {
        match agent_record(&terminal_id, Some(deadline)) {
            AgentFetch::Found(record) => {
                let status = field(&record, "agent_status")
                    .unwrap_or("unknown")
                    .to_string();
                if matches!(status.as_str(), "idle" | "done") {
                    return Ok(record);
                }
                if status == "blocked" && !startup_dialog_asked {
                    startup_dialog_asked = true;
                    if let Some(refusal) = startup_dialog_refusal(name, harness, &pane_id, deadline)
                    {
                        return Err(refusal);
                    }
                }
                if expired(Some(deadline)) {
                    // The agent is up but sitting on a status that is not a
                    // prompt (`blocked`, `working`, `unknown`). Name the
                    // status, not the clock — that is what tells the caller
                    // what to look at.
                    return Err(format!("delegate {name} is {status}"));
                }
                last_status = Some(status);
                sleep_bounded(deadline, READY_POLL);
            }
            AgentFetch::Missing => {
                return Err(format!("delegate {name}: the agent no longer resolves"))
            }
            AgentFetch::TimedOut => {
                // The next request refused to send because the deadline had
                // passed. Report the last status we DID see, so a readiness
                // that ran out while the agent was `blocked` says `blocked`
                // rather than hiding the status behind the clock.
                return Err(match last_status {
                    Some(status) => format!("delegate {name} is {status}"),
                    None => format!("delegate {name} is not ready: timed out"),
                });
            }
            AgentFetch::Failed(reason) => return Err(format!("delegate {name}: {reason}")),
        }
    }
}

/// Name the harness's known startup dialog, if this pane is sitting on it.
///
/// Claude Code asks "Quick safety check: is this a project you created or one
/// you trust?" the first time it starts in a directory it has no trust record
/// for, and a `--worktree` checkout is always such a directory (#605). That
/// dialog has no text box, so a bare `blocked` tells an operator nothing they
/// can act on, and a brief typed at it would be answering the dialog rather
/// than the brief.
///
/// Only consulted while the pane has never reported ready, which is what makes
/// it safe: after readiness a `blocked` is a permission prompt, and by then the
/// trust text is only scrollback above the prompt box.
fn startup_dialog_refusal(
    name: &str,
    harness: &HarnessSpec,
    pane_id: &str,
    deadline: Instant,
) -> Option<String> {
    let dialog = harness.startup_dialog?;
    pane_shows_startup_dialog(pane_id, &dialog, deadline)
        .then(|| format!("delegate {name}: {}", dialog.refusal))
}

/// Does the pane's recent buffer carry this dialog's marker?
///
/// `pane.read` rather than the agent record, because the dialog is screen
/// chrome and the record carries no screen. Bounded by the caller's deadline
/// like every other request here: an unbounded read against a stalled server
/// would freeze `start` rather than report it (G1). A read that fails answers
/// `false`, so a flaky socket degrades to the plain `blocked` refusal rather
/// than inventing a diagnosis.
fn pane_shows_startup_dialog(pane_id: &str, dialog: &StartupDialog, deadline: Instant) -> bool {
    let response = match bounded(
        Method::PaneRead(PaneReadParams {
            pane_id: pane_id.to_owned(),
            source: ReadSource::Recent,
            lines: Some(STARTUP_DIALOG_LINES),
            format: ReadFormat::Text,
            strip_ansi: true,
        }),
        Some(deadline),
    ) {
        Ok(response) => response,
        Err(_) => return false,
    };
    response
        .pointer("/result/read/text")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|text| text.to_lowercase().contains(dialog.marker))
}

fn sleep_bounded(deadline: Instant, interval: Duration) {
    let left = deadline.saturating_duration_since(Instant::now());
    std::thread::sleep(left.min(interval));
}

/// The readiness subscription + snapshot loop, bounded end to end.
///
/// Shape identical to `ready::wait_for_ready` but:
/// - the subscribe's read timeout is `deadline - now`, capped at 60 s;
/// - the snapshot and any re-read go through `agent_record` (bounded via
///   `bounded`), not `send_request`;
/// - a timeout past the deadline returns `is not ready: timed out`, which
///   `start`'s caller turns into exit 1 then rolls back.
///
/// Pure classifiers (`classify_ready_event`, `status_is_ready`,
/// `ReadySignal`) are still reused from `ready.rs` — only the socket I/O
/// is re-implemented here (W9 choice (b)).
fn delegate_wait_for_ready(
    name: &str,
    terminal_id: &str,
    pane_id: &str,
    deadline: Instant,
) -> Result<(), String> {
    // One cap for the whole phase, computed once up front. If the deadline
    // has already passed, say "timed out" and skip the subscribe entirely:
    // no request is sent while out of clock.
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(format!("delegate {name} is not ready: timed out"));
    }

    // Subscribe FIRST, then snapshot: a status that lands between the ack
    // and the snapshot is delivered rather than missed. The subscribe's
    // own read_timeout is already bounded.
    let subscribe = Request {
        id: "cli:delegate:ready".into(),
        method: Method::EventsSubscribe(EventsSubscribeParams {
            subscriptions: vec![
                Subscription::PaneAgentStatusChanged {
                    pane_id: pane_id.to_owned(),
                    agent_status: None,
                },
                Subscription::PaneExited {},
            ],
        }),
    };
    let (ack, mut stream) = ApiClient::local()
        .subscribe_value(&subscribe, Some(remaining))
        .map_err(|err| format!("delegate {name}: readiness could not be watched: {err}"))?;
    if let Err(err) = crate::api::client::parse_response_value(ack) {
        return Err(match err {
            ApiClientError::ErrorResponse(response) => format!(
                "delegate {name}: {}",
                serde_json::to_string(&response).unwrap_or_else(|_| String::new())
            ),
            _ => format!("delegate {name}: readiness could not be watched: {err}"),
        });
    }

    // Snapshot after the subscribe is live, bounded by the same deadline.
    match agent_record(terminal_id, Some(deadline)) {
        AgentFetch::Found(record) => {
            let status = field(&record, "agent_status").unwrap_or("");
            if status_is_ready(status) {
                return Ok(());
            }
        }
        AgentFetch::Missing => {
            return Err(format!(
                "delegate {name} never became ready: the agent no longer resolves"
            ));
        }
        AgentFetch::TimedOut => {
            return Err(format!("delegate {name} is not ready: timed out"));
        }
        AgentFetch::Failed(reason) => {
            return Err(format!("delegate {name}: {reason}"));
        }
    }

    // Loop over the subscription stream, bounded by the deadline on every
    // read. `classify_ready_event` is the pure helper — the same one
    // `agent start --wait-ready` uses, so a Ready/Exited reading of an
    // event is identical byte for byte.
    loop {
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
        else {
            return Err(format!("delegate {name} is not ready: timed out"));
        };
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|err| format!("delegate {name}: readiness could not be watched: {err}"))?;
        match stream.next_value() {
            Ok(None) => {
                return Err(format!(
                    "delegate {name} never became ready: the readiness subscription closed"
                ));
            }
            Ok(Some(event)) => match classify_ready_event(&event, pane_id) {
                ReadySignal::Ready(_) => return Ok(()),
                ReadySignal::Exited => {
                    return Err(format!(
                        "delegate {name}: the agent's pane exited before it became ready"
                    ));
                }
                ReadySignal::KeepWaiting => continue,
            },
            Err(ApiClientError::Io(err)) if super::api_timeout_error(&err) => {
                return Err(format!("delegate {name} is not ready: timed out"));
            }
            Err(err) => {
                return Err(format!(
                    "delegate {name}: readiness could not be watched: {err}"
                ))
            }
        }
    }
}

// ------------------------------------------------------------------- send

fn delegate_send(args: &[String]) -> io::Result<i32> {
    let Some(name) = args.first() else {
        return Ok(usage(format!("usage: {}", SEND_USAGE)));
    };
    let flags = match parse_flags(Verb::Send, args) {
        Ok(flags) => flags,
        Err(reason) => return Ok(usage(reason)),
    };
    let flags = as_send(&flags);
    let Some(brief_path) = flags.brief.clone() else {
        return Ok(usage(format!("--brief is required\nusage: {}", SEND_USAGE)));
    };
    let (brief, sentence) = match prepare_brief(&brief_path) {
        Ok(prepared) => prepared,
        Err(reason) => return Ok(usage(reason)),
    };

    // The lock BEFORE the entry (D-F). Loading the entry first means reading a
    // file another round is in the middle of replacing, and validating against
    // that is validating against a snapshot of a delegate that has moved on.
    let _lock = match take_lock(name) {
        Ok(lock) => lock,
        Err("busy") => return Ok(fail(format!("delegate {name} is busy"))),
        Err(reason) => return Ok(fail(format!("delegate {name}: {reason}"))),
    };
    let mut entry = match require_delegate_no_deadline(name) {
        Ok(entry) => entry,
        Err(code) => return Ok(code),
    };
    // The whole entry, kept. A round that fails to submit has to put the file
    // back the way it found it, and decrementing one field of a mutated copy is
    // how a brief path from the failed round ends up recorded as the live one.
    let original = entry.clone();

    let ready_timeout = flags
        .ready_timeout_ms
        .unwrap_or(super::ready::DEFAULT_READY_TIMEOUT_MS);
    let ready_deadline = Instant::now() + Duration::from_millis(ready_timeout);
    let record = match await_prompt(name, &entry, ready_deadline) {
        Ok(record) => record,
        Err(reason) => return Ok(fail(reason)),
    };

    let cursor = match cursor_of(&record) {
        Ok(cursor) => cursor,
        Err(reason) => return Ok(fail(reason)),
    };
    let submitted_at_ms = now_ms();
    entry.round += 1;
    entry.brief = brief;
    entry.submitted_at_ms = submitted_at_ms;
    entry.cursor = cursor.clone();
    if let Err(err) = write_entry(&entry) {
        return Ok(fail(format!(
            "could not write the delegate registry entry: {err}"
        )));
    }

    if let Err(reason) = submit_brief(&entry.pane_id, &sentence, None) {
        // This round did not start, so the whole entry goes back, and a
        // write-back that itself fails is said rather than swallowed: a
        // registry that now claims a round nobody is running is worse than
        // the failed send that caused it.
        if let Err(err) = write_entry(&original) {
            eprintln!(
                "delegate {name}: the brief was not submitted and the registry could not be \
                 restored: {err}"
            );
        }
        return Ok(fail(format!(
            "delegate {name}: the brief was not submitted: {reason}"
        )));
    }

    if !flags.await_result {
        emit_submit(&entry, flags.json);
        return Ok(exit::OK);
    }

    let deadline = flags
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    Await {
        harness: harness_for(&entry),
        entry: &entry,
        after: Some(cursor),
        submitted_at_ms,
        deadline,
        settle_ms: flags.settle_ms,
        max_chars: flags.max_chars,
        json: flags.json,
    }
    .run()
}

/// Wait for the agent to be at its prompt, which is where a brief may be typed,
/// and hand back the record that proved it.
///
/// Not the readiness wait: the agent is already up, and what is being asked here
/// is narrower — it is not sitting on a dialog. The returned record is the last
/// one sampled, for the same reason as [`await_ready`]: the cursor has to be the
/// one this round starts from.
fn await_prompt(name: &str, entry: &Entry, deadline: Instant) -> Result<serde_json::Value, String> {
    let mut last_status: Option<String> = None;
    loop {
        match agent_record(&entry.terminal_id, Some(deadline)) {
            AgentFetch::Found(record) => {
                let status = field(&record, "agent_status")
                    .unwrap_or("unknown")
                    .to_string();
                if matches!(status.as_str(), "idle" | "done") {
                    return Ok(record);
                }
                if expired(Some(deadline)) {
                    return Err(format!("delegate {name} is {status}"));
                }
                last_status = Some(status);
                sleep_bounded(deadline, READY_POLL);
            }
            AgentFetch::Missing => {
                return Err(format!("delegate {name}: the agent no longer resolves"))
            }
            AgentFetch::TimedOut => {
                return Err(match last_status {
                    Some(status) => format!("delegate {name} is {status}"),
                    None => format!("delegate {name} is not ready: timed out"),
                });
            }
            AgentFetch::Failed(reason) => return Err(format!("delegate {name}: {reason}")),
        }
    }
}

fn cursor_of(record: &serde_json::Value) -> Result<String, String> {
    record
        .get("turn_cursor")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "the server's record carried no turn cursor".to_string())
}

// ------------------------------------------------------------ wait/result

fn delegate_wait(args: &[String]) -> io::Result<i32> {
    let Some(name) = args.first() else {
        return Ok(usage(format!("usage: {}", WAIT_USAGE)));
    };
    let flags = match parse_flags(Verb::Wait, args) {
        Ok(flags) => flags,
        Err(reason) => return Ok(usage(reason)),
    };
    let flags = as_wait(&flags);
    // The clock starts HERE, before the entry is even read (D-E). A wait that
    // spent its budget finding out what it is waiting for, and then reported a
    // timeout it had already earned, is the failure this ordering exists to stop.
    let deadline = flags
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));

    let entry = match require_delegate(name, deadline) {
        Ok(entry) => entry,
        Err(RequireFailure::NotDelegate) => return Ok(usage(format!("not a delegate: {name}"))),
        Err(RequireFailure::Failed(reason)) => {
            // The server was unreachable. Never "not a delegate" (W4): a
            // transport failure has not proved the name is gone. Exit 1 with
            // the error on stderr and nothing on stdout.
            return Ok(fail(format!("delegate {name}: {reason}")));
        }
        Err(RequireFailure::TimedOut(boxed)) => {
            // The agent lookup burned the budget. Emit the timeout outcome
            // against the registry entry we read before the clock ran out, so
            // a `--json` caller still gets the one object every outcome prints
            // (K5): exit 124 with the timeout outcome.
            let entry = *boxed;
            let reported_cursor = flags.after.clone().unwrap_or_else(|| entry.cursor.clone());
            emit_outcome(
                &entry,
                Outcome::Timeout.as_str(),
                None,
                flags.json,
                &reported_cursor,
            );
            return Ok(Outcome::Timeout.exit_code());
        }
    };
    // A caller resuming after a timeout needs to wait for the SAME round, and
    // freshness is judged against when that round was submitted — so both come
    // from the entry, and `--after` only replaces the cursor.
    let after = flags.after.clone().or_else(|| Some(entry.cursor.clone()));
    Await {
        harness: harness_for(&entry),
        entry: &entry,
        after,
        submitted_at_ms: entry.submitted_at_ms,
        deadline,
        settle_ms: flags.settle_ms,
        max_chars: flags.max_chars,
        json: flags.json,
    }
    .run()
}

/// What `delegate result` reports when the store has no reply yet: the live
/// agent decides. A gone agent is `gone` (exit 4), as `delegate wait` says; a
/// failed lookup is a failure, never a verdict.
#[derive(Debug, PartialEq, Eq)]
enum NoResultVerdict {
    Running,
    NoResult,
    Gone,
    Fail(String),
}

fn no_result_verdict(fetched: &AgentFetch) -> NoResultVerdict {
    match fetched {
        AgentFetch::Found(record) => match field(record, "agent_status").unwrap_or("unknown") {
            "working" | "blocked" | "unknown" => NoResultVerdict::Running,
            // Hibernated: its child is gone, as `delegate wait` reports it.
            "hibernated" => NoResultVerdict::Gone,
            _ => NoResultVerdict::NoResult,
        },
        AgentFetch::Missing => NoResultVerdict::Gone,
        AgentFetch::TimedOut => NoResultVerdict::Fail("timed out".to_string()),
        AgentFetch::Failed(reason) => NoResultVerdict::Fail(reason.clone()),
    }
}

fn delegate_result(args: &[String]) -> io::Result<i32> {
    let Some(name) = args.first() else {
        return Ok(usage(format!("usage: {}", RESULT_USAGE)));
    };
    let flags = match parse_flags(Verb::Result, args) {
        Ok(flags) => flags,
        Err(reason) => return Ok(usage(reason)),
    };
    let entry = match require_delegate_no_deadline(name) {
        Ok(entry) => entry,
        Err(code) => return Ok(code),
    };

    let bounded_res = bounded(
        Method::AgentResult(AgentResultParams {
            target: entry.terminal_id.clone(),
            max_chars: flags.max_chars,
            offset: None,
        }),
        None,
    );
    let response = match bounded_res {
        Ok(response) => response,
        Err(BoundedError::TimedOut) => return Ok(fail("timed out")),
        Err(BoundedError::Transport(e)) => return Ok(fail(e)),
    };
    if let Some(error) = response.get("error") {
        let code = error
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        // `no_result` is the store saying there is no reply yet, which is a
        // statement about the TURN rather than an error: the live status is what
        // says whether that turn is still running.
        if code == "no_result" {
            // The store said no reply yet — ask the live agent whether the
            // turn is still running. An unreachable server here is a FAILURE
            // (W13 / G5): "running"/"unknown" at exit 0 would make a dead
            // socket indistinguishable from a healthy running turn.
            let fetched = agent_record(&entry.terminal_id, None);
            let (outcome, code) = match no_result_verdict(&fetched) {
                NoResultVerdict::Running => ("running", exit::OK),
                NoResultVerdict::NoResult => ("no_result", exit::OK),
                // The same answer `delegate wait` gives for a gone agent (rr5 H3).
                NoResultVerdict::Gone => ("gone", super::settled::exit::GONE),
                NoResultVerdict::Fail(reason) => {
                    return Ok(fail(format!("delegate {}: {reason}", entry.name)))
                }
            };
            emit_outcome(&entry, outcome, None, flags.json, &entry.cursor);
            return Ok(code);
        }
        return Ok(fail(server_error(error)));
    }
    let Some(info) = response.pointer("/result/result") else {
        return Ok(fail("the server answered with no result"));
    };
    let finished = info
        .get("finished")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let outcome = if !finished {
        "running"
    } else {
        match info.get("status").and_then(serde_json::Value::as_str) {
            Some("done") => "done",
            Some("verdict") => "verdict",
            Some("blocked") => "blocked",
            _ => "no_sentinel",
        }
    };
    // An unfinished turn's `text` is an EARLIER turn's reply, so it is nulled
    // rather than shown: reporting it as this round's answer is the exact
    // confusion `agent result` documents for its own callers.
    let info = if outcome == "running" {
        None
    } else {
        Some(info)
    };
    emit_outcome(&entry, outcome, info, flags.json, &entry.cursor);
    Ok(exit::OK)
}

fn delegate_status(args: &[String]) -> io::Result<i32> {
    let Some(name) = args.first() else {
        return Ok(usage(format!("usage: {}", STATUS_USAGE)));
    };
    let flags = match parse_flags(Verb::Status, args) {
        Ok(flags) => flags,
        Err(reason) => return Ok(usage(reason)),
    };
    let entry = match require_delegate_no_deadline(name) {
        Ok(entry) => entry,
        Err(code) => return Ok(code),
    };
    // `agent_record`, not `_opt`: an unreachable server is a FAILURE, never
    // a cheerful "unknown" at exit 0 (W13 / G5).
    let status = match agent_record(&entry.terminal_id, None) {
        AgentFetch::Found(record) => field(&record, "agent_status")
            .unwrap_or("unknown")
            .to_string(),
        AgentFetch::Missing => "unknown".to_string(),
        AgentFetch::TimedOut => return Ok(fail(format!("delegate {}: timed out", entry.name))),
        AgentFetch::Failed(reason) => {
            return Ok(fail(format!("delegate {}: {reason}", entry.name)))
        }
    };
    if flags.json {
        // `goal` is reserved for #573 and is always present and null: a field
        // that appears with its first value is not a field a caller can test for.
        println!(
            "{}",
            serde_json::json!({
                "name": entry.name,
                "agent_status": status,
                "pane_id": entry.pane_id,
                "workspace_id": entry.workspace_id,
                "mode": entry.mode,
                "worktree": entry.worktree,
                "branch": entry.branch,
                "harness": entry.harness,
                "model": entry.model,
                "round": entry.round,
                "turn_cursor": entry.cursor,
                "goal": serde_json::Value::Null,
            })
        );
    } else {
        println!("delegate {name}: {status}");
        println!("  round {} · {}", entry.round, entry.mode);
        if let Some(worktree) = &entry.worktree {
            println!("  worktree {worktree}");
        }
    }
    Ok(exit::OK)
}

/// The name resolves to a live delegate: an entry, and an agent of that name
/// still on the terminal the entry recorded.
///
/// The terminal check is what makes a rename visible. An agent renamed away from
/// its delegate name is no longer addressable as that delegate, and a `send`
/// that still worked would type into whatever inherited the name.
/// Why `require_delegate` returned, when it did not return an `Entry`.
///
/// The two failures are kept apart because they go to different exit codes:
/// `NotDelegate` is a usage error (exit 2) with "not a delegate: {name}", and
/// `TimedOut` carries the entry so the one caller that cares (`delegate wait`
/// / `send --await`) can emit a `timeout` outcome object and exit 124 (K5).
#[derive(Debug)]
enum RequireFailure {
    /// The server answered: the name is not this process's delegate right now
    /// (no registry entry, agent not found, or the agent is a different one).
    /// Caller exits 2 with `not a delegate: <name>` on stderr.
    NotDelegate,
    /// The caller's deadline passed while looking up the agent record. The
    /// entry we found before that is handed back so `delegate wait` /
    /// `send --await` can emit a `timeout` outcome object naming the delegate
    /// (K5). Boxed so this variant does not inflate every `NotDelegate`
    /// return by hundreds of bytes (clippy::result_large_err).
    TimedOut(Box<Entry>),
    /// The server was unreachable (dead socket, closed connection). This is
    /// never `NotDelegate` — a server that cannot answer has not proved the
    /// name is gone (W4). Caller exits 1 with the reason on stderr.
    Failed(String),
}

fn require_delegate(name: &str, deadline: Option<Instant>) -> Result<Entry, RequireFailure> {
    if let Err(reason) = validate_name(name) {
        eprintln!("{reason}");
        return Err(RequireFailure::NotDelegate);
    }
    let Some(entry) = read_entry(name) else {
        return Err(RequireFailure::NotDelegate);
    };
    require_decide(entry, agent_record(name, deadline))
}

/// The pure decision half of `require_delegate`, given the entry already read
/// and the agent-lookup outcome. The call-site tests (W7) exercise this.
fn require_decide(entry: Entry, fetch: AgentFetch) -> Result<Entry, RequireFailure> {
    match fetch {
        AgentFetch::Found(record) => {
            if field(&record, "terminal_id") != Some(entry.terminal_id.as_str()) {
                Err(RequireFailure::NotDelegate)
            } else {
                Ok(entry)
            }
        }
        AgentFetch::Missing => Err(RequireFailure::NotDelegate),
        AgentFetch::TimedOut => Err(RequireFailure::TimedOut(Box::new(entry))),
        AgentFetch::Failed(reason) => Err(RequireFailure::Failed(reason)),
    }
}

/// Map a `RequireFailure` into an exit code for callers that do not want to
/// emit a `timeout` outcome object: `NotDelegate` is a usage error (exit 2),
/// `TimedOut`/`Failed` print on stderr and exit 1.
fn require_delegate_no_deadline(name: &str) -> Result<Entry, i32> {
    match require_delegate(name, None) {
        Ok(entry) => Ok(entry),
        Err(RequireFailure::NotDelegate) => Err(usage(format!("not a delegate: {name}"))),
        Err(RequireFailure::TimedOut(_)) => Err(fail(format!(
            "delegate {name}: timed out resolving the agent"
        ))),
        Err(RequireFailure::Failed(reason)) => Err(fail(format!("delegate {name}: {reason}"))),
    }
}

// ------------------------------------------------------------------- reap

fn delegate_reap(args: &[String]) -> io::Result<i32> {
    let Some(name) = args.first() else {
        return Ok(usage(format!("usage: {}", REAP_USAGE)));
    };
    let flags = match parse_flags(Verb::Reap, args) {
        Ok(flags) => flags,
        Err(reason) => return Ok(usage(reason)),
    };
    // Reap reads only the registry, so it still works for a delegate whose agent
    // is gone, whose pane was closed by hand, or which was renamed — all of
    // which `require_delegate` would refuse.
    if let Err(reason) = validate_name(name) {
        return Ok(usage(reason));
    }
    // The lock BEFORE the entry, for the same reason as `send`: the entry a
    // reap acts on must be the one the lock protects.
    //
    // `--force` proceeds without the lock. The teardown is already idempotent
    // (each step confirms the id is still ours, treats `workspace_not_found`
    // and the already-gone kill codes as "nothing to do", and derives
    // `removed` from what actually happened). Two concurrent forced reaps of
    // the same delegate therefore each succeed with the same observable
    // outcome: the entry gone, and whichever one lost the race reporting
    // `removed: false` for every field — the serialisation we gave up by
    // skipping the lock is replaced by that record of who did what (F13).
    let _lock = match take_lock(name) {
        Ok(lock) => lock,
        Err("busy") if !flags.force => return Ok(fail(format!("delegate {name} is busy"))),
        Err("busy") => match open_lock_file(&lock_path(name)) {
            Ok(file) => Lock { _file: file },
            Err(err) => return Ok(fail(format!("delegate {name}: {err}"))),
        },
        Err(reason) => return Ok(fail(format!("delegate {name}: {reason}"))),
    };
    let Some(entry) = read_entry(name) else {
        return Ok(usage(format!("not a delegate: {name}")));
    };
    let Some(cleanup) = Cleanup::from_entry(&entry) else {
        return Ok(fail(format!(
            "delegate {name}: the registry entry names a mode this build does not know: {}",
            entry.mode
        )));
    };

    // A refusal is reported and the entry KEPT: a reap that could not finish must
    // be retryable, and an entry removed on a refusal is a checkout nobody has an
    // address for any more. The exit code is `flk worktree kill`'s own mapping,
    // through the one function both verbs call.
    let removed = match tear_down(&cleanup, flags.force, false) {
        Ok(removed) => removed,
        Err(err) => {
            eprintln!("{err}");
            return Ok(super::worktree::kill_error_exit_code(&serde_json::json!({
                "code": err.code,
            })));
        }
    };

    if let Err(err) = std::fs::remove_file(entry_path(name)) {
        if err.kind() != io::ErrorKind::NotFound {
            return Ok(fail(format!(
                "could not remove the delegate registry entry: {err}"
            )));
        }
    }
    // `removed` tells a script what the reap actually did, not what it was
    // asked to do: hardcoding `true` here (as earlier revisions did) made a
    // reap that destroyed nothing — identity unknown, server down — look
    // indistinguishable from one that closed the workspace. The field is now
    // derived from `TearDown`: true iff anything the reap owns is gone (W1).
    let any_removed = removed.workspace_closed || removed.checkout_removed || removed.parent_closed;
    if flags.json {
        println!(
            "{}",
            serde_json::json!({
                "name": entry.name,
                "workspace_id": entry.workspace_id,
                "worktree": entry.worktree,
                "removed": any_removed,
                "workspace_closed": removed.workspace_closed,
                "checkout_removed": removed.checkout_removed,
                "parent_closed": removed.parent_closed,
            })
        );
    } else {
        println!(
            "delegate {name}: reaped (workspace {}, checkout {}, parent {})",
            removed.workspace_closed, removed.checkout_removed, removed.parent_closed
        );
    }
    Ok(exit::OK)
}

// ------------------------------------------------------------------ await

/// What a delegate's await concluded.
///
/// Named rather than reused as a string everywhere, because the exit code is a
/// property of the outcome and a script reads it — the two must not be two
/// tables that drift. There is deliberately no "failed" arm: an `agent.result`
/// that the server refuses during the grace is a command that exits 1 with the
/// server's own words, not an outcome a caller can read (R7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Verdict,
    Blocked,
    Gone,
    NoSentinel,
    NoResult,
    AgentBlocked,
    Timeout,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Verdict => "verdict",
            Self::Blocked => "blocked",
            Self::Gone => "gone",
            Self::NoSentinel => "no_sentinel",
            Self::NoResult => "no_result",
            Self::AgentBlocked => "agent_blocked",
            Self::Timeout => "timeout",
        }
    }

    fn exit_code(self) -> i32 {
        match self {
            Self::Done | Self::Verdict => exit::OK,
            Self::Blocked => super::settled::exit::BLOCKED,
            Self::Gone => super::settled::exit::GONE,
            Self::NoSentinel | Self::NoResult => exit::NO_SENTINEL,
            Self::AgentBlocked => exit::AGENT_BLOCKED,
            Self::Timeout => super::settled::exit::TIMEOUT,
        }
    }
}

/// What one poll of the result store said.
enum ResultPoll {
    /// A reply from THIS round, with its sentinel read. The reply is CARRIED, not
    /// re-read at print time (R6): a second read can be a different reply, and an
    /// outcome whose text is not the reply it was decided from is a lie with the
    /// right shape.
    Fresh {
        outcome: Outcome,
        info: serde_json::Value,
    },
    /// The store has no reply yet, no session to read one from, or a reply from
    /// an earlier turn. All three mean the same thing to a caller: not now.
    NotYet,
    /// The agent is gone.
    Gone,
    /// The server refused for a reason that is not "not yet".
    Refused(String),
    /// The caller's deadline has passed, or the request errored once it had.
    /// Routed to `Outcome::Timeout` (exit 124), not to `Refused` which would be
    /// exit 1 (K5).
    Timeout,
}

/// What the reply grace concluded.
enum Grace {
    /// A reply from this round, with the reply itself.
    Reply {
        outcome: Outcome,
        info: serde_json::Value,
    },
    Gone,
    /// A refusal the caller turns into exit 1: the server's words on stderr and
    /// nothing on stdout.
    Refused(String),
    /// The agent started another turn, so the round has to settle again.
    NewTurn,
    /// The grace ran out with no reply. The caller decides timeout vs no_result.
    Exhausted,
    /// A deadline-driven timeout from inside the grace: always `Outcome::Timeout`
    /// (exit 124), never `Refused`. Kept separate from `Exhausted` so a caller
    /// that reads `Grace::Timeout` cannot confuse "no reply yet" with "clock
    /// ran out mid-poll" (K5).
    Timeout,
}

/// What a settled outcome means for the delegate.
enum SettledDecision {
    /// Report this outcome and exit with its code.
    Report {
        outcome: Outcome,
        info: Option<serde_json::Value>,
    },
    /// A refused cursor: exit 2, with the reason on stderr and no object.
    Usage(String),
    /// A fatal failure: exit 1, with the server's own words.
    Failed(String),
    /// Settle again from the cursor that just settled.
    WaitAgain,
}

/// What `started_another_turn` concluded (W4).
///
/// Four answers: a new turn ends the grace with `Grace::NewTurn`, no new
/// turn keeps polling, a timeout ends the whole await as `Outcome::Timeout`,
/// and a transport failure ends it as exit 1. Earlier revisions returned
/// `bool`, which collapsed `Failed` into "no new turn" and spun silently
/// against a dead server.
enum TurnCheck {
    NewTurn,
    NoNewTurn,
    Timeout,
    Failed(String),
}

struct Await<'a> {
    /// The harness whose store this await polls and whose grace it waits out.
    ///
    /// From the table rather than from a field on the entry, so a `--harness`
    /// this build no longer drives still gets a usable grace and not-yet set
    /// instead of a refusal (see [`harness_for`]).
    harness: &'static HarnessSpec,
    entry: &'a Entry,
    /// The cursor this await started from. Reported as `turn_cursor` in every
    /// outcome so a caller that timed out can resume with `--after` on exactly
    /// the cursor this one was waiting past.
    after: Option<String>,
    submitted_at_ms: u64,
    deadline: Option<Instant>,
    settle_ms: Option<u64>,
    max_chars: Option<u32>,
    json: bool,
}

impl Await<'_> {
    /// Settle, then wait for the reply, and report the round.
    ///
    /// The outer loop exists for one case: the agent takes ANOTHER turn while the
    /// reply grace is running. The reply that answers the round the caller asked
    /// about is then not the reply that arrives next, so rather than report
    /// somebody else's turn the await settles again from the cursor it just
    /// settled on. Every other outcome ends it.
    fn run(self) -> io::Result<i32> {
        // The cursor this await STARTED from, reported in every outcome. A caller
        // that timed out resumes with `--after <turn_cursor>` and gets the same
        // turn rather than starting the search again.
        let reported_cursor = self.after.clone().unwrap_or_default();
        let mut after = self.after.clone();

        loop {
            let remaining = self.remaining_timeout_ms();
            let settled = super::settled::settled_wait(
                "delegate",
                SettleTarget::Pinned(PinnedTarget {
                    terminal_id: self.entry.terminal_id.clone(),
                    pane_id: self.entry.pane_id.clone(),
                }),
                after.as_deref(),
                self.settle_ms.unwrap_or(super::settled::DEFAULT_SETTLE_MS),
                remaining,
            )?;

            let settled_cursor = settled.turn_cursor().unwrap_or_default().to_string();
            match self.after_settle(settled, &settled_cursor) {
                SettledDecision::Report { outcome, info } => {
                    let code = outcome.exit_code();
                    emit_outcome(
                        self.entry,
                        outcome.as_str(),
                        info.as_ref(),
                        self.json,
                        &reported_cursor,
                    );
                    return Ok(code);
                }
                SettledDecision::Usage(reason) => {
                    eprintln!("delegate: {reason}");
                    return Ok(exit::USAGE);
                }
                SettledDecision::Failed(reason) => {
                    eprintln!("delegate: {reason}");
                    return Ok(1);
                }
                SettledDecision::WaitAgain => {
                    // Resume from the cursor that settled, not from the one this
                    // await started at: the new turn is what the round now means,
                    // and re-waiting from the original would accept its reply.
                    after = Some(settled_cursor);
                }
            }
        }
    }

    /// How much of `--timeout` is left, for a core that takes its own budget.
    ///
    /// `None` means wait forever, which is what an absent `--timeout` means; a
    /// deadline already passed becomes a zero budget, so a core handed it times
    /// out on its first sample rather than waiting for a wait that already ended.
    fn remaining_timeout_ms(&self) -> Option<u64> {
        let deadline = self.deadline?;
        Some(
            u64::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .unwrap_or(0),
        )
    }

    /// What a settled outcome means for the delegate, once the grace is over.
    fn after_settle(
        &self,
        settled: super::settled::SettledOutcome,
        settled_cursor: &str,
    ) -> SettledDecision {
        use super::settled::SettledOutcome;
        match settled {
            // A held `blocked` is a human question, not a turn that finished.
            SettledOutcome::Blocked { .. } => SettledDecision::Report {
                outcome: Outcome::AgentBlocked,
                info: None,
            },
            SettledOutcome::Gone { .. } => SettledDecision::Report {
                outcome: Outcome::Gone,
                info: None,
            },
            SettledOutcome::TimedOut => SettledDecision::Report {
                outcome: Outcome::Timeout,
                info: None,
            },
            // A refused cursor is the caller's mistake rather than an outcome:
            // exit 2 and no object, so nothing downstream can read it as a turn
            // that ran.
            SettledOutcome::Refused(reason) => SettledDecision::Usage(reason),
            // The server's own refusal to the INITIAL resolve, which the two wait
            // verbs print verbatim. The delegate has no reason to reword it.
            SettledOutcome::ServerRefused(body) => SettledDecision::Failed(body),
            SettledOutcome::Error(reason) => SettledDecision::Failed(reason),
            SettledOutcome::Settled { .. } => match self.await_reply(settled_cursor) {
                Grace::Reply { outcome, info } => SettledDecision::Report {
                    outcome,
                    info: Some(info),
                },
                Grace::Gone => SettledDecision::Report {
                    outcome: Outcome::Gone,
                    info: None,
                },
                Grace::Refused(reason) => SettledDecision::Failed(reason),
                Grace::NewTurn => SettledDecision::WaitAgain,
                Grace::Timeout => SettledDecision::Report {
                    outcome: Outcome::Timeout,
                    info: None,
                },
                Grace::Exhausted => SettledDecision::Report {
                    outcome: if self
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        // The deadline wins inside the grace too: a caller whose
                        // clock ran out does not get a late answer reinterpreted.
                        Outcome::Timeout
                    } else {
                        Outcome::NoResult
                    },
                    info: None,
                },
            },
        }
    }

    /// The grace: a settle is not a reply, so keep asking until one is written.
    ///
    /// The gap between the TUI going quiet and the transcript being committed is
    /// ordinary — the harness clears its spinner first — so a grace that
    /// reported `no_result` the instant the agent went idle would call every
    /// normal turn a failure. How long that gap is, is the harness's row.
    fn await_reply(&self, settled_cursor: &str) -> Grace {
        let grace = self.harness.result_grace;
        let grace_end = match self.deadline {
            Some(deadline) => deadline.min(Instant::now() + grace),
            None => Instant::now() + grace,
        };
        loop {
            if Instant::now() >= grace_end {
                return Grace::Exhausted;
            }
            match self.poll_result() {
                ResultPoll::Fresh { outcome, info } => return Grace::Reply { outcome, info },
                ResultPoll::Gone => return Grace::Gone,
                ResultPoll::Refused(reason) => return Grace::Refused(reason),
                ResultPoll::Timeout => return Grace::Timeout,
                ResultPoll::NotYet => {}
            }
            // The agent took another turn, so the reply that answers THIS round
            // is not the next one to land. Settle again instead. A transport
            // failure here is a FAILURE, not "no new turn" (W4).
            match self.started_another_turn(settled_cursor) {
                TurnCheck::NewTurn => return Grace::NewTurn,
                TurnCheck::NoNewTurn => {}
                TurnCheck::Timeout => return Grace::Timeout,
                TurnCheck::Failed(reason) => return Grace::Refused(reason),
            }
            sleep_bounded(grace_end, RESULT_POLL);
        }
    }

    /// Has a turn started since the cursor that settled?
    ///
    /// Compared the way the settle core compares `--after`, because it is the
    /// same question: has a new turn started since the cursor I waited past? A
    /// higher `working_entries` says yes, and so does a cursor that settled
    /// mid-turn whose `state_seq` has moved since.
    ///
    /// Returns a `TurnCheck`: `NewTurn` ends the grace, `NoNewTurn` keeps
    /// polling, `Timeout` ends the whole await as `Outcome::Timeout`, and
    /// `Failed` ends it as exit 1 with the reason on stderr. Collapsing a
    /// `Fail` into `NoNewTurn` (the behaviour before W4) made an unreachable
    /// server look like a stable "no new turn" and the await spun silently.
    fn started_another_turn(&self, settled_cursor: &str) -> TurnCheck {
        if settled_cursor.is_empty() {
            return TurnCheck::NoNewTurn;
        }
        let record = match agent_record(&self.entry.terminal_id, self.deadline) {
            AgentFetch::Found(record) => record,
            // The agent is gone — not a new turn, but the next `poll_result`
            // will see the gone-ness and return `Grace::Gone`.
            AgentFetch::Missing => return TurnCheck::NoNewTurn,
            AgentFetch::TimedOut => return TurnCheck::Timeout,
            AgentFetch::Failed(reason) => return TurnCheck::Failed(reason),
        };
        let Some(raw) = record
            .get("turn_cursor")
            .and_then(serde_json::Value::as_str)
        else {
            return TurnCheck::NoNewTurn;
        };
        let (Ok(now), Ok(before)) = (Cursor::parse(raw), Cursor::parse(settled_cursor)) else {
            return TurnCheck::NoNewTurn;
        };
        if now.epoch != before.epoch {
            return if now.epoch > before.epoch {
                TurnCheck::NewTurn
            } else {
                TurnCheck::NoNewTurn
            };
        }
        if now.entries > before.entries || (before.working && now.seq > before.seq) {
            TurnCheck::NewTurn
        } else {
            TurnCheck::NoNewTurn
        }
    }

    /// Is the newest reply this round's?
    ///
    /// Three conditions, and the third is the one that matters: a reply written
    /// BEFORE this round's submit belongs to an earlier turn no matter how
    /// finished it is. A reply with no recorded time is never fresh — an
    /// unattributable reply cannot be shown to have come from this turn.
    fn is_fresh(&self, info: &serde_json::Value) -> bool {
        let finished = info
            .get("finished")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let at_ms = info.get("at_ms").and_then(serde_json::Value::as_u64);
        finished && at_ms.is_some_and(|at_ms| at_ms >= self.submitted_at_ms)
    }

    /// Ask the store for the newest reply, and say whether it is this round's.
    fn poll_result(&self) -> ResultPoll {
        let bounded_res: Result<serde_json::Value, BoundedError> = bounded(
            Method::AgentResult(AgentResultParams {
                target: self.entry.terminal_id.clone(),
                max_chars: self.max_chars,
                offset: None,
            }),
            self.deadline,
        );
        let response = match bounded_res {
            Ok(response) => response,
            Err(err) => match await_failure(&err, self.deadline, Instant::now()) {
                AwaitFailure::Timeout => return ResultPoll::Timeout,
                AwaitFailure::NotYet => return ResultPoll::NotYet,
                AwaitFailure::Fail(reason) => return ResultPoll::Refused(reason),
            },
        };
        if let Some(error) = response.get("error") {
            let code = error
                .get("code")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            // `agent_not_found` is the one error here that is not about the
            // store: the agent itself is gone, which is an outcome.
            if code == "agent_not_found" || code == "pane_not_found" {
                return ResultPoll::Gone;
            }
            if not_yet_codes(self.harness).contains(&code) {
                return ResultPoll::NotYet;
            }
            return ResultPoll::Refused(server_error(error));
        }
        match response.pointer("/result/result") {
            Some(info) if self.is_fresh(info) => ResultPoll::Fresh {
                outcome: self.sentinelled(info),
                info: info.clone(),
            },
            // A finished reply from an EARLIER turn is "not yet" rather than an
            // answer: reporting it would attribute the last round's work to this
            // one, which is the confusion `agent result` documents for its own
            // callers.
            Some(_) | None => ResultPoll::NotYet,
        }
    }

    fn sentinelled(&self, info: &serde_json::Value) -> Outcome {
        match info.get("status").and_then(serde_json::Value::as_str) {
            Some("done") => Outcome::Done,
            Some("verdict") => Outcome::Verdict,
            Some("blocked") => Outcome::Blocked,
            _ => Outcome::NoSentinel,
        }
    }
}

// ----------------------------------------------------------------- output

fn emit_submit(entry: &Entry, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "name": entry.name,
                "pane_id": entry.pane_id,
                "workspace_id": entry.workspace_id,
                "worktree": entry.worktree,
                "branch": entry.branch,
                "round": entry.round,
                "turn_cursor": entry.cursor,
            })
        );
    } else {
        println!("delegate {}: submitted round {}", entry.name, entry.round);
    }
}

/// The one object every OUTCOME prints under `--json`.
///
/// Only outcomes. A usage error, a refused cursor and a refused server call print
/// nothing on stdout at all, so a caller piping stdout gets either an outcome it
/// can branch on or an empty stream — never a diagnostic where it expected a
/// shape, and never a shape standing in for a failure.
fn emit_outcome(
    entry: &Entry,
    outcome: &str,
    info: Option<&serde_json::Value>,
    json: bool,
    turn_cursor: &str,
) {
    let text = |key: &str| {
        info.and_then(|info| info.get(key))
            .filter(|value| !value.is_null())
            .cloned()
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "name": entry.name,
                "outcome": outcome,
                "status": text("status"),
                "status_text": text("status_text"),
                "text": text("text"),
                "total_chars": text("total_chars"),
                "next_offset": text("next_offset"),
                "pane_id": entry.pane_id,
                "workspace_id": entry.workspace_id,
                "worktree": entry.worktree,
                "branch": entry.branch,
                "round": entry.round,
                "turn_cursor": turn_cursor,
                "session_id": text("session_id"),
            })
        );
    } else {
        if let Some(text) = info
            .and_then(|info| info.get("text"))
            .and_then(|t| t.as_str())
        {
            println!("{text}");
        }
        let status_text = info
            .and_then(|info| info.get("status_text"))
            .and_then(|t| t.as_str());
        match status_text {
            Some(status_text) => eprintln!("delegate {}: {outcome}: {status_text}", entry.name),
            None => eprintln!("delegate {}: {outcome}", entry.name),
        }
        // `no_result` is the one outcome whose usual cause is a session nobody
        // reported, so it says which channel should have reported one: a
        // supervisor reading this knows whether to look at the opencode plugin
        // or at Claude's `SessionStart` hook.
        if outcome == Outcome::NoResult.as_str() {
            eprintln!(
                "  nothing was written to {} this round, and its session comes from {}",
                entry.harness,
                harness_for(entry).session_source.as_str(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The double-quote byte, named once so the check below does not have to
    /// write a lone quote character literal — which is the very thing it counts.
    const QUOTE: u8 = 0x22;

    #[test]
    fn names_are_safe_as_file_names() {
        for name in ["d1", "a", "A9", "a.b_c-d", "n".repeat(64).as_str()] {
            assert!(validate_name(name).is_ok(), "{name} should be accepted");
        }
        for name in [
            "",
            "../x",
            "a/b",
            ".hidden",
            "-dash",
            "sp ace",
            "n".repeat(65).as_str(),
            "a/b",
        ] {
            assert!(validate_name(name).is_err(), "{name:?} should be refused");
        }
    }

    /// A name is going to be typed into a terminal as part of a sentence, so the
    /// whole unsafe set is refused rather than most of it.
    #[test]
    fn a_brief_path_is_refused_before_it_is_typed() {
        let dir = std::env::temp_dir().join(format!("flk-578-brief-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for unsafe_char in UNSAFE_PATH_CHARS {
            let path = dir.join(format!("b{}.md", unsafe_char));
            std::fs::write(&path, "x\n").unwrap();
            let err = prepare_brief(&path.display().to_string())
                .expect_err("a path that needs escaping is refused");
            assert!(
                err.contains("unsafe to type"),
                "{unsafe_char:?} should be refused as unsafe: {err}"
            );
        }
        let spaced = dir.join("two words");
        std::fs::create_dir_all(&spaced).unwrap();
        let spaced = spaced.join("b.md");
        std::fs::write(&spaced, "x\n").unwrap();
        assert!(prepare_brief(&spaced.display().to_string())
            .expect_err("whitespace is unsafe too")
            .contains("unsafe to type"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The whole point of the brief: the typed text is the sentence, and the
    /// sentence names the ABSOLUTE path rather than whatever the caller wrote.
    #[test]
    fn the_typed_sentence_names_the_absolute_path() {
        let dir = std::env::temp_dir().join(format!("flk-578-sentence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("task.md");
        std::fs::write(&path, "x\n").unwrap();
        let (absolute, sentence) = prepare_brief(&path.display().to_string()).expect("readable");
        assert!(absolute.starts_with('/'), "{absolute} should be absolute");
        assert_eq!(sentence, format!("Read {absolute} and execute it exactly."));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_brief_must_be_a_readable_regular_file() {
        let dir = std::env::temp_dir().join(format!("flk-578-notfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(prepare_brief(&dir.display().to_string())
            .expect_err("a directory is not a brief")
            .contains("not a regular file"));
        assert!(prepare_brief(&dir.join("missing.md").display().to_string()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file that exists and is regular but cannot be OPENED is refused here,
    /// where nothing has been created yet — rather than three requests later,
    /// with a workspace and a checkout already on disk.
    ///
    /// Root reads a mode-000 file, so there is nothing to assert there; the skip
    /// is the honest one rather than a `Permissions::set_mode` that only pretends.
    #[test]
    fn a_brief_that_cannot_be_opened_is_refused_before_anything_is_created() {
        use std::os::unix::fs::PermissionsExt as _;

        // SAFETY-adjacent note: this test only reads its own temp file.
        if std::fs::metadata("/proc/self")
            .map(|_| ())
            .and_then(|_| -> std::io::Result<()> {
                // Effective uid 0 opens a mode-000 file, so the case is
                // unreachable as root.
                let uid = std::fs::read_to_string("/proc/self/status").ok();
                let root = uid
                    .and_then(|status| {
                        status
                            .lines()
                            .find(|line| line.starts_with("Uid:"))
                            .and_then(|line| line.split_whitespace().nth(1).map(str::to_string))
                    })
                    .is_some_and(|uid| uid == "0");
                if root {
                    Err(std::io::Error::other("running as root"))
                } else {
                    Ok(())
                }
            })
            .is_err()
        {
            return;
        }

        let dir = std::env::temp_dir().join(format!("flk-578-unreadable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("task.md");
        std::fs::write(&path, "x\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = prepare_brief(&path.display().to_string())
            .expect_err("a file nobody can open is not a brief");
        assert!(
            err.contains("cannot be opened for reading"),
            "the refusal must name the open, not the stat: {err}"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The reason `--repo` is refused with `--cwd`: there is no checkout to
    /// branch, so accepting the flag would be accepting one that cannot be
    /// honoured.
    #[test]
    fn worktree_flags_need_worktree_mode() {
        let mut flags = StartFlags {
            cwd: Some("/tmp".into()),
            branch: Some("b".into()),
            ..StartFlags::default()
        };
        assert!(validate_worktree_flags(&flags).is_err());
        flags.cwd = None;
        flags.worktree = true;
        assert!(validate_worktree_flags(&flags).is_ok());
    }

    #[test]
    fn placement_is_exactly_one_of_cwd_or_worktree() {
        assert!(validate_placement(&StartFlags::default()).is_err());
        assert!(validate_placement(&StartFlags {
            cwd: Some("/tmp".into()),
            ..StartFlags::default()
        })
        .is_ok());
        assert!(validate_placement(&StartFlags {
            worktree: true,
            ..StartFlags::default()
        })
        .is_ok());
        assert!(validate_placement(&StartFlags {
            cwd: Some("/tmp".into()),
            worktree: true,
            ..StartFlags::default()
        })
        .is_err());
    }

    /// The refusals a caller would hit by typing a flag from the wrong verb.
    #[test]
    fn a_flag_from_another_verb_is_a_usage_error() {
        let args = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let cases: &[(&[&str], &[&str])] = &[
            (&["start", "d1", "--after", "x"], &["start"]),
            (&["wait", "d1", "--await"], &["wait"]),
            (&["send", "d1", "--force"], &["send"]),
            (&["status", "d1", "--timeout", "1"], &["status"]),
            (&["reap", "d1", "--settle", "1"], &["reap"]),
            (&["start", "d1", "--brief"], &["start"]),
        ];
        for (words, expected_in) in cases {
            let err = parse_flags(
                match expected_in[0] {
                    "start" => Verb::Start,
                    "send" => Verb::Send,
                    "wait" => Verb::Wait,
                    "status" => Verb::Status,
                    "reap" => Verb::Reap,
                    other => panic!("no verb {other}"),
                },
                &args(words),
            )
            .expect_err("a flag from another verb is not accepted");
            assert!(
                err.contains("unknown option") || err.contains("missing value"),
                "{words:?}: {err}"
            );
        }
    }

    /// Every flag a usage row documents must be a flag that verb accepts. The
    /// same obligation `cli::help`'s table test enforces, pinned here so a flag
    /// cannot be documented on the delegate's own help and then dropped from its
    /// parser.
    #[test]
    fn every_documented_flag_is_accepted() {
        for (verb, usage) in [
            (Verb::Start, START_USAGE),
            (Verb::Send, SEND_USAGE),
            (Verb::Wait, WAIT_USAGE),
            (Verb::Result, RESULT_USAGE),
            (Verb::Status, STATUS_USAGE),
            (Verb::Reap, REAP_USAGE),
        ] {
            assert!(
                usage.starts_with(&format!("flk delegate {}", verb_word(verb))),
                "{usage}"
            );
            for token in usage.split_whitespace() {
                // The prose after a flag in a usage row ends it with a comma or
                // a full stop, so trim punctuation as well as brackets: the flag
                // is the `--name`, not the sentence around it.
                let flag = token.trim_matches(|c: char| {
                    matches!(c, '[' | ']' | '(' | ')' | ',' | '.' | ':' | ';')
                });
                if !flag.starts_with("--") || flag == "--" {
                    continue;
                }
                assert!(verb.accepts(flag), "{usage} documents {flag}, dropped");
            }
        }
    }

    /// The table, read through the flag: both harnesses resolve, an absent
    /// `--harness` is still opencode, and a refusal names what this build does
    /// drive rather than only rejecting what it does not.
    #[test]
    fn the_harness_table_is_what_the_flag_accepts() {
        for name in ["opencode", "claude"] {
            assert_eq!(
                validate_harness(Some(name)).map(|spec| spec.name),
                Ok(name),
                "{name} is in the table"
            );
        }
        assert_eq!(validate_harness(None).map(|spec| spec.name), Ok("opencode"));
        let refused = validate_harness(Some("codex")).expect_err("not in the table");
        assert!(refused.contains("not supported"), "{refused}");
        assert!(
            refused.contains("opencode|claude"),
            "the refusal lists what exists: {refused}"
        );
    }

    /// The argv each row starts. Both harnesses take `--model M`, so the same
    /// shape covers both today — and the test says so per harness, so a change
    /// to one is a change to a row rather than to the builder.
    #[test]
    fn each_harness_builds_its_own_argv() {
        for (name, expected_with, expected_without) in [
            ("opencode", "opencode --model m1", "opencode"),
            ("claude", "claude --model m1", "claude"),
        ] {
            let spec = validate_harness(Some(name)).expect("in the table");
            assert_eq!((spec.argv)(Some("m1")).join(" "), expected_with);
            assert_eq!((spec.argv)(None).join(" "), expected_without);
        }
    }

    /// Claude's row carries the folder-trust dialog and opencode's carries
    /// none: the refusal is what stops a delegate typing a brief into it, so a
    /// row that lost the marker would be a silent regression.
    #[test]
    fn only_claude_declares_a_startup_dialog() {
        let claude = validate_harness(Some("claude")).expect("in the table");
        let dialog = claude
            .startup_dialog
            .expect("claude asks for folder trust on a fresh worktree");
        assert!(dialog.refusal.contains("#605"), "{}", dialog.refusal);
        assert!(
            !dialog.marker.is_empty() && dialog.marker == dialog.marker.to_lowercase(),
            "the marker is matched against a lowercased screen"
        );
        let opencode = validate_harness(Some("opencode")).expect("in the table");
        assert!(opencode.startup_dialog.is_none());
    }

    /// A quoted character in the file would break the help table's flag scan,
    /// which pairs up double-quote bytes across the module. The rule is a plain
    /// byte count, so it is checked the same way.
    #[test]
    fn the_module_holds_an_even_number_of_quote_bytes() {
        let source = include_str!("delegate.rs");
        assert_eq!(
            source.bytes().filter(|byte| *byte == QUOTE).count() % 2,
            0,
            "delegate.rs must hold an even number of double-quote bytes"
        );
    }

    /// The codes the grace treats as "not yet" include the no-session refusal
    /// (D8), for every harness that reports its session asynchronously — which
    /// is both of them today: opencode's plugin commits it mid-turn and Claude's
    /// `SessionStart` hook reports it from inside the harness, so the ordinary
    /// first poll of a turn has no store to read yet.
    #[test]
    fn the_no_session_refusal_is_one_of_the_not_yet_codes() {
        for harness in &HARNESSES {
            let codes = not_yet_codes(harness);
            for code in [
                NO_SESSION_CODE,
                "no_result",
                "transcript_not_found",
                "transcript_unreadable",
            ] {
                assert!(codes.contains(&code), "{}: {code} is not yet", harness.name);
            }
            assert!(
                !codes.contains(&"permission_denied"),
                "{}: a real refusal must still be an error",
                harness.name
            );
        }
    }

    /// Each store refusal is named once, from the source that emits it, so a
    /// code cannot be quietly dropped by editing a harness row instead of the
    /// table. `agents.rs` refuses `no_result` when the store has no reply,
    /// `transcript_not_found` when a session has no transcript on disk,
    /// `transcript_unreadable` when a store exists and could not be read, and
    /// `no_agent_session` when the pane has reported no session at all.
    #[test]
    fn every_not_yet_code_is_one_agent_result_emits() {
        let source = include_str!("../app/api/agents.rs");
        for code in STORE_NOT_YET_CODES.iter().chain([NO_SESSION_CODE].iter()) {
            assert!(
                source.contains(&format!("\"{code}\"")),
                "agent.result no longer refuses {code}"
            );
        }
    }

    /// A refusal the teardown treats as "already gone" rather than as a failure.
    #[test]
    fn an_absent_checkout_is_a_removal_that_already_happened() {
        for code in [
            "not_linked_worktree",
            "not_git_worktree",
            "workspace_not_found",
        ] {
            assert!(
                ALREADY_GONE_CODES.contains(&code),
                "{code} means the checkout is not there to remove"
            );
        }
        assert!(!ALREADY_GONE_CODES.contains(&"dirty_worktree_requires_force"));
    }
}

#[cfg(test)]
#[path = "delegate_callsite_tests.rs"]
mod callsite_tests;

#[cfg(test)]
#[path = "delegate_final_tests.rs"]
mod final_tests;

#[cfg(test)]
#[path = "delegate_decisions_tests.rs"]
mod decisions_tests;
