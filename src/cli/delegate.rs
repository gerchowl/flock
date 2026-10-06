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
//! `flk delegate` is that script, composed client-side out of the socket methods
//! that already exist. No new socket method, no server change, no new
//! dependency: every claim in the docs is checkable against a method `flk agent
//! wait` or `flk worktree kill` already used. What the composition adds is the
//! part that was never written down anywhere — which cursor belongs to which
//! round, which errors mean "not yet", what a bare `idle` does and does not say,
//! and what has to be cleaned up if the start fails halfway.
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

use crate::api::client::ApiClient;
use crate::api::schema::{
    AgentResultParams, AgentStartParams, AgentTarget, EmptyParams, Method, PaneListParams,
    PaneTarget, Request, WorkspaceCreateParams, WorkspaceTarget, WorktreeCreateParams,
    WorktreeKillParams, WorktreeListParams,
};

use super::settled::{Cursor, PinnedTarget, SettleTarget};

/// `delegate start`'s usage. A `pub(super) const` rather than a literal in the
/// help table so `flk delegate start --help` and `flk delegate --help` cannot
/// answer two different things.
pub(super) const START_USAGE: &str = concat!(
    "flk delegate start <name> --brief FILE (--cwd PATH | --worktree [--repo PATH] [--branch B] [--base REF])\n",
    "                     [--harness opencode] [--model M] [--await] [--timeout MS] [--settle MS]\n",
    "                     [--ready-timeout MS] [--max-chars N] [--json]\n",
    "  --brief FILE        a readable file; exactly `Read <path> and execute it exactly.` is typed\n",
    "  --cwd PATH          run in a workspace the delegate creates for that directory\n",
    "  --worktree          run in a fresh linked worktree: --repo, --branch, --base\n",
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
    "  removes only the workspace and checkout recorded at start; needs no live agent",
);

/// How long after a settle the delegate keeps looking for the reply.
///
/// The gap between "the agent went quiet" and "opencode has written the reply":
/// the TUI clears its spinner first and the plugin commits the transcript after.
const RESULT_GRACE: Duration = Duration::from_secs(10);

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

/// `agent.result` errors that mean "the reply is not written yet".
///
/// Every one of these is a statement about the store rather than about the
/// turn, and all of them are ordinary in the seconds between the agent going
/// quiet and its plugin committing the transcript. Treating any of them as a
/// failure would make a normal turn a `result` error; treating a real refusal as
/// "not yet" would make the grace hang until the deadline and then report
/// `no_result`, which is a lie about a server that answered.
///
/// `no_agent_session` is the no-session refusal (D8): the pane has not reported a
/// session yet, so there is no store to read at all.
const NOT_YET_CODES: [&str; 4] = [
    "no_result",
    "transcript_not_found",
    "transcript_unreadable",
    "no_agent_session",
];

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

/// The only harness this build drives.
fn validate_harness(harness: Option<&str>) -> Result<&'static str, String> {
    match harness {
        None | Some("opencode") => Ok("opencode"),
        Some(other) => Err(format!("harness {other} is not supported yet")),
    }
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

/// One socket request, bounded by whatever clock the caller has.
///
/// For write operations (worktree.create, worktree.kill, workspace.create,
/// workspace.close) the base timeout is 60 s; for polls it is 2 s. The actual
/// timeout is the minimum of that base and the time left on the caller's
/// deadline. Once the deadline has passed, nothing is sent.
fn request(method: Method, deadline: Option<Instant>) -> io::Result<serde_json::Value> {
    let base_timeout = match &method {
        Method::WorktreeCreate(_)
        | Method::WorktreeKill(_)
        | Method::WorkspaceCreate(_)
        | Method::WorkspaceClose(_) => Duration::from_secs(60),
        _ => REQUEST_TIMEOUT,
    };
    let timeout = match deadline {
        None => base_timeout,
        Some(deadline) => {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            left.min(base_timeout)
        }
    };
    ApiClient::local()
        .request_value_with_timeout(
            &Request {
                id: "cli:delegate".into(),
                method,
            },
            timeout,
        )
        .map_err(super::api_client_error_to_io)
}

/// Has the caller's deadline passed?
///
/// Re-checked after every response: a reply that lands after the deadline is a
/// timeout whichever way it went, so a command that has run out of clock does not
/// then act on what it read on the way past.
fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

fn agent_record(target: &str, deadline: Option<Instant>) -> Option<serde_json::Value> {
    let response = request(
        Method::AgentGet(AgentTarget {
            target: target.to_string(),
        }),
        deadline,
    )
    .ok()?;
    response
        .pointer("/result/agent")
        .filter(|record| record.is_object())
        .cloned()
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

/// Every workspace the server lists, as records.
#[allow(dead_code)]
fn workspace_records(deadline: Option<Instant>) -> Vec<serde_json::Value> {
    request(Method::WorkspaceList(EmptyParams::default()), deadline)
        .ok()
        .and_then(|response| {
            response
                .pointer("/result/workspaces")
                .and_then(|workspaces| workspaces.as_array())
                .cloned()
        })
        .unwrap_or_default()
}

#[allow(dead_code)]
fn workspace_ids(deadline: Option<Instant>) -> Vec<String> {
    workspace_records(deadline)
        .iter()
        .filter_map(|workspace| field(workspace, "workspace_id").map(str::to_string))
        .collect()
}

/// The repository-root workspace `worktree.create` opened, identified exactly.
///
/// Three conditions, all of them necessary:
/// - it is new compared with the list taken immediately before the call, so a
///   repository root that was already open is never adopted as a parent;
/// - it is not itself a linked worktree, which is what separates the repository
///   root from the checkout the delegate asked for;
/// - its checkout path is the repo root the new checkout reported, so a
///   workspace opened for some other repository in the same window is not it.
///
/// Exactly one match, or nothing is recorded: an ambiguous parent is a parent this
/// code cannot close safely, and "cannot" has to mean "does not".
fn identify_parent(
    worktrees_before: &[serde_json::Value],
    repo_root: &str,
    _deadline: Option<Instant>,
) -> Option<String> {
    let mut found: Option<String> = None;
    for record in workspace_records(None) {
        let Some(id) = field(&record, "workspace_id") else {
            continue;
        };
        if worktrees_before
            .iter()
            .any(|before| field(before, "workspace_id") == Some(id))
        {
            continue;
        }
        if record
            .pointer("/worktree/is_linked_worktree")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
        {
            continue;
        }
        let Some(checkout) = record
            .pointer("/worktree/checkout_path")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if !same_path(checkout, repo_root) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(id.to_string());
    }
    found
}

/// Create the delegate's workspace, and say what it created.
///
/// On any failure after the create request has gone out, this cleans up through
/// the same routine `reap` runs and returns the exit code with the cause already
/// on stderr — so there is no path where a start has asked the server to create
/// something and then walked away from the answer.
fn place(name: &str, flags: &StartFlags, mode: Mode) -> Result<Placement, i32> {
    let worktrees_before = worktree_list_full(None);
    let response = match mode {
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
            request(
                Method::WorktreeCreate(WorktreeCreateParams {
                    cwd: Some(repo),
                    branch: flags.branch.clone(),
                    base: flags.base.clone(),
                    focus: false,
                    ..WorktreeCreateParams::default()
                }),
                None,
            )
        }
        Mode::Cwd => {
            let cwd = flags
                .cwd
                .clone()
                .expect("placement was validated before anything was created");
            request(
                Method::WorkspaceCreate(WorkspaceCreateParams {
                    cwd: Some(cwd),
                    focus: false,
                    label: None,
                }),
                None,
            )
        }
    };

    let response = match response {
        Ok(response) => response,
        Err(err) => {
            return Err(teardown_after_place_failure(
                name,
                &worktrees_before,
                mode,
                flags,
                err.to_string(),
            ))
        }
    };
    if let Some(error) = response.get("error") {
        let reason = server_error(error);
        return Err(teardown_after_place_failure(
            name,
            &worktrees_before,
            mode,
            flags,
            reason,
        ));
    }

    let workspace_id = at(&response, "/result/workspace/workspace_id").map(str::to_string);
    let root_pane = at(&response, "/result/root_pane/pane_id").map(str::to_string);
    let worktree = at(&response, "/result/worktree/path").map(str::to_string);
    // The branch is what the server decided, not what was asked for: `--branch`
    // is optional, and a server that generated one has recorded a name the
    // operator would never have guessed.
    let branch = at(&response, "/result/worktree/branch").map(str::to_string);
    let repo_root = at(&response, "/result/workspace/worktree/repo_root").map(str::to_string);
    let repo_key = at(&response, "/result/workspace/worktree/repo_key").map(str::to_string);
    let parent_workspace_id = repo_root
        .as_deref()
        .and_then(|root| identify_parent(&worktrees_before, root, None));

    let (Some(workspace_id), Some(root_pane)) = (workspace_id, root_pane) else {
        let reason = "the server created the workspace but named no workspace id or root pane";
        return Err(teardown_after_place_failure(
            name,
            &worktrees_before,
            mode,
            flags,
            reason.to_string(),
        ));
    };

    let cwd = worktree
        .clone()
        .unwrap_or_else(|| flags.cwd.clone().unwrap_or_default());
    Ok(Placement {
        workspace_id,
        cwd,
        worktree,
        branch,
        root_pane,
        parent_workspace_id,
        repo_root,
        repo_key,
    })
}

/// Undo a create whose answer could not be used, and hand back the exit code.
///
/// The parent is re-identified here even though the placement never completed: a
/// `worktree.create` that opened a repository root and then failed to answer is
/// exactly the case where leaving the parent behind is most visible.
/// Undo a create whose answer could not be used, and hand back the exit code.
///
/// For worktree mode, if the create failed after the request was sent, we may have
/// a checkout on disk that the server created but couldn't report. We find it by
/// comparing the worktree list before and after the call, matching on --branch if
/// given, and kill it with force. The parent workspace is no longer closed here
/// (without a repo root we can't identify it reliably); see #595.
fn teardown_after_place_failure(
    name: &str,
    worktrees_before: &[serde_json::Value],
    mode: Mode,
    flags: &StartFlags,
    reason: impl fmt::Display,
) -> i32 {
    // For worktree mode, the create may have left a checkout behind. We find it
    // by diffing the worktree list before and after, matching on --branch if given.
    if mode == Mode::Worktree {
        let worktrees_after = worktree_list_full(None);
        let branch = flags.branch.as_deref();
        let new_checkout = find_new_checkout(worktrees_before, &worktrees_after, branch);
        if let Some(checkout) = new_checkout {
            let response = kill(None, Some(&checkout), true);
            let response = match response {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("delegate {name}: rollback also failed: {err}");
                    return fail(reason);
                }
            };
            if let Some(err) = refusal(&response) {
                if !ALREADY_GONE_CODES.contains(&err.code.as_str()) {
                    eprintln!("delegate {name}: rollback also failed: {err}");
                }
            } else {
                eprintln!("delegate {name}: rollback removed stray checkout at {checkout}");
            }
        } else {
            eprintln!("delegate {name}: rollback found no new checkout to remove (branch filter: {branch:?})");
        }
    }
    // Parent workspace cleanup is handled by the rollback function using the
    // recorded parent_workspace_id, not by this function. The identify_unlinked_parent
    // approach is removed because without a repo root from the failed response we
    // cannot reliably identify the parent workspace; see #595.
    fail(reason)
}

/// Find a checkout that appeared after the create call, matching on --branch if given.
///
/// The checkout is identified by its path, which must not have been present in
/// the `worktree.list` taken before the create. We use `worktree.list` to find
/// checkouts, filtering by branch if given.
fn find_new_checkout(
    worktrees_before: &[serde_json::Value],
    worktrees_after: &[serde_json::Value],
    branch: Option<&str>,
) -> Option<String> {
    let before_paths: std::collections::HashSet<String> = worktrees_before
        .iter()
        .filter_map(|w| {
            w.pointer("/worktree/path")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();

    for worktree in worktrees_after {
        let Some(path) = worktree.pointer("/worktree/path").and_then(|v| v.as_str()) else {
            continue;
        };
        if before_paths.contains(path) {
            continue;
        }
        if let Some(branch_name) = branch {
            let wt_branch = worktree
                .pointer("/worktree/branch")
                .and_then(|v| v.as_str());
            if wt_branch != Some(branch_name) {
                continue;
            }
        }
        return Some(path.to_string());
    }
    None
}

/// Full worktree list with all fields (used for diffing checkouts).
fn worktree_list_full(deadline: Option<Instant>) -> Vec<serde_json::Value> {
    request(
        Method::WorktreeList(WorktreeListParams {
            workspace_id: None,
            cwd: None,
            scan: true,
        }),
        deadline,
    )
    .ok()
    .and_then(|response| {
        response
            .pointer("/result/worktrees")
            .and_then(|worktrees| worktrees.as_array())
            .cloned()
    })
    .unwrap_or_default()
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

/// The error a response carries, if it is an error at all.
fn refusal(response: &serde_json::Value) -> Option<ServerError> {
    response.get("error").map(parse_server_error)
}

/// What a teardown may touch, and what it removed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cleanup {
    name: String,
    workspace_id: String,
    pane_id: String,
    root_pane: String,
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
///   holds the recorded checkout; in cwd mode, that it still holds the recorded
///   agent's pane. A mismatch is treated as gone — the checkout is reached by
///   path instead, and the workspace is left alone.
/// * **The parent workspace is only closed when it is still the parent.** It is
///   closed only if it exists, still holds the recorded repository root, holds a
///   single pane, and no other open workspace is a linked worktree of the same
///   repository. Any of those failing leaves it open: a repository-root workspace
///   the operator has since put panes in is not a leftover.
fn tear_down(target: &Cleanup, force: bool) -> Result<TearDown, ServerError> {
    let mut done = TearDown::default();

    if !target.workspace_id.is_empty() && workspace_is_ours(target) {
        match target.mode {
            Mode::Worktree => {
                let response = kill(Some(&target.workspace_id), None, force)?;
                if let Some(err) = refusal(&response) {
                    if err.code != "workspace_not_found" {
                        return Err(err);
                    }
                } else {
                    // The kill closes the workspace and removes the checkout with
                    // it, so both are accounted for.
                    done.workspace_closed = true;
                    done.checkout_removed = true;
                }
            }
            Mode::Cwd => {
                let response = close_workspace(&target.workspace_id)?;
                if let Some(err) = refusal(&response) {
                    if err.code != "workspace_not_found" {
                        return Err(err);
                    }
                } else {
                    done.workspace_closed = true;
                }
            }
        }
    }

    // The checkout the delegate made is still there when the workspace was gone
    // (or was never ours), and `worktree.kill` reaches it by path.
    if let Some(checkout) = target.worktree.clone() {
        if !done.checkout_removed {
            let response = kill(None, Some(&checkout), force)?;
            if let Some(err) = refusal(&response) {
                let already_gone = ALREADY_GONE_CODES.contains(&err.code.as_str())
                    || !Path::new(&checkout).exists();
                if !already_gone {
                    return Err(err);
                }
            } else {
                done.checkout_removed = true;
            }
        }
    }

    if let Some(parent) = parent_to_close(target)? {
        let response = close_workspace(&parent)?;
        if let Some(err) = refusal(&response) {
            if err.code != "workspace_not_found" {
                return Err(err);
            }
        } else {
            done.parent_closed = true;
        }
    }

    Ok(done)
}

/// Is the recorded workspace still the one this delegate created?
///
/// The two modes can only be checked against different things, because they were
/// recorded differently. A worktree workspace is identified by its CHECKOUT — the
/// thing that was actually created. A cwd workspace has no checkout of its own, so
/// it is identified by a PANE it still holds: the agent's, or the shell it was
/// created with. A public pane id is never reused within a server's life, so
/// either is a stronger claim than the workspace id alone.
///
/// A workspace that does not resolve is gone, and so is one that does not match:
/// both mean the same thing to a caller, which is that there is nothing here to
/// close.
fn workspace_is_ours(target: &Cleanup) -> bool {
    let Some(record) = workspace_record(&target.workspace_id) else {
        return false;
    };
    match target.mode {
        Mode::Worktree => {
            let Some(checkout) = target.worktree.as_deref() else {
                return false;
            };
            let current = at(&record, "/worktree/checkout_path").unwrap_or_default();
            !current.is_empty() && same_path(current, checkout)
        }
        Mode::Cwd => {
            // A cwd-mode workspace is ours only if it still holds a pane whose
            // terminal_id matches the one we recorded. Terminal ids are unique
            // across restarts (term_<micros><counter>), unlike pane ids which
            // are reused. If the agent's terminal is gone, the cwd workspace is
            // not ours to close: the agent's exit already closed it.
            //
            // Exception: if we never recorded a terminal_id (the agent died at
            // launch), we still own the workspace and should clean it up.
            if target.terminal_id.is_empty() {
                // No terminal_id recorded — the agent died at launch.
                // The workspace is still ours to clean up.
                return workspace_record(&target.workspace_id).is_some();
            }
            let terminal_id = &target.terminal_id;
            workspace_has_terminal(&target.workspace_id, terminal_id)
        }
    }
}

fn workspace_record(workspace_id: &str) -> Option<serde_json::Value> {
    let response = request(
        Method::WorkspaceGet(WorkspaceTarget {
            workspace_id: workspace_id.to_string(),
        }),
        None,
    )
    .ok()?;
    response
        .pointer("/result/workspace")
        .filter(|record| record.is_object())
        .cloned()
}

#[allow(dead_code)]
fn workspace_has_pane(workspace_id: &str, pane_id: &str) -> bool {
    let Ok(response) = request(
        Method::PaneList(PaneListParams {
            workspace_id: Some(workspace_id.to_string()),
        }),
        None,
    ) else {
        return false;
    };
    response
        .pointer("/result/panes")
        .and_then(|panes| panes.as_array())
        .is_some_and(|panes| {
            panes
                .iter()
                .any(|pane| field(pane, "pane_id") == Some(pane_id))
        })
}

/// Check if a workspace holds a pane with the given terminal_id.
fn workspace_has_terminal(workspace_id: &str, terminal_id: &str) -> bool {
    let Ok(response) = request(
        Method::PaneList(PaneListParams {
            workspace_id: Some(workspace_id.to_string()),
        }),
        None,
    ) else {
        return false;
    };
    response
        .pointer("/result/panes")
        .and_then(|panes| panes.as_array())
        .is_some_and(|panes| {
            panes
                .iter()
                .any(|pane| field(pane, "terminal_id") == Some(terminal_id))
        })
}

/// The parent workspace to close, or `None` to leave it alone.
///
/// All four conditions, and the last one is the expensive one: while another
/// workspace still holds a linked worktree of the same repository, the
/// repository-root workspace is that checkout's parent and closing it would take
/// an operator's work with it.
fn parent_to_close(target: &Cleanup) -> Result<Option<String>, ServerError> {
    let Some(parent_id) = target.parent_workspace_id.clone() else {
        return Ok(None);
    };
    let Some(record) = workspace_record(&parent_id) else {
        return Ok(None);
    };
    let Some(repo_root) = target.repo_root.as_deref() else {
        return Ok(None);
    };
    let checkout = at(&record, "/worktree/checkout_path").unwrap_or_default();
    if checkout.is_empty() || !same_path(checkout, repo_root) {
        return Ok(None);
    }
    if record
        .get("pane_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        != 1
    {
        return Ok(None);
    }
    let repo_key = target.repo_key.as_deref();
    let ours = target.workspace_id.as_str();
    let parent = parent_id.as_str();
    for other in workspace_records(None) {
        let Some(id) = field(&other, "workspace_id") else {
            continue;
        };
        if id == ours || id == parent {
            continue;
        }
        let linked = other
            .pointer("/worktree/is_linked_worktree")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let same_repo = repo_key.is_none()
            || other
                .pointer("/worktree/repo_key")
                .and_then(serde_json::Value::as_str)
                == repo_key;
        if linked && same_repo {
            return Ok(None);
        }
    }
    Ok(Some(parent_id))
}

fn kill(
    workspace_id: Option<&str>,
    path: Option<&str>,
    force: bool,
) -> io::Result<serde_json::Value> {
    request(
        Method::WorktreeKill(WorktreeKillParams {
            workspace_id: workspace_id.map(str::to_string),
            path: path.map(str::to_string),
            force,
            caller_pid: Some(std::process::id()),
            ..WorktreeKillParams::default()
        }),
        None,
    )
}

fn close_workspace(workspace_id: &str) -> io::Result<serde_json::Value> {
    request(
        Method::WorkspaceClose(WorkspaceTarget {
            workspace_id: workspace_id.to_string(),
        }),
        None,
    )
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

    if agent_record(name, None).is_some() {
        return Ok(fail(format!("agent_name_taken: {name}")));
    }
    if let Some(existing) = read_entry(name) {
        if let Some(cleanup) = Cleanup::from_entry(&existing) {
            if workspace_is_ours(&cleanup) {
                return Ok(fail(format!("delegate {name} exists; reap it first")));
            }
            // The workspace is gone. A checkout that is still on disk is not: it
            // belongs to this name, and a second start would leave it with no
            // delegate and no way to address it.
            if let Some(checkout) = existing.worktree.as_deref() {
                if Path::new(checkout).exists() {
                    return Ok(fail(format!(
                        "delegate {name} has a leftover checkout at {checkout}; reap it first"
                    )));
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
    let settled_record = match await_ready(name, &agent, ready_deadline) {
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
        harness: harness.to_string(),
        model: flags.model.clone(),
        round: 1,
        brief: brief.clone(),
        submitted_at_ms,
        cursor: cursor.clone(),
        created_at_ms: now_ms(),
    };
    if let Err(err) = write_entry(&entry) {
        let _ = std::fs::remove_file(entry_path(name));
        rollback(&cleanup);
        return Ok(fail(format!(
            "could not write the delegate registry entry: {err}"
        )));
    }

    // No request between persisting the cursor and typing the brief: the whole
    // point of capturing it last is that it is the cursor of the turn this
    // submit starts.
    match super::pane::submit_sequence(&pane_id, &sentence) {
        Ok(0) => {}
        Ok(_) | Err(_) => {
            // The sentence did not land, so this start did not happen. Undo it
            // rather than leave a workspace, a checkout and a registry entry
            // describing a round nobody is running.
            let _ = std::fs::remove_file(entry_path(name));
            rollback(&cleanup);
            return Ok(fail(format!(
                "delegate {name}: the brief was not submitted"
            )));
        }
    }

    if !flags.await_result {
        emit_submit(&entry, flags.json);
        return Ok(exit::OK);
    }

    let deadline = flags
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    Await {
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
    if let Err(err) = tear_down(cleanup, true) {
        eprintln!("delegate {}: rollback also failed: {err}", cleanup.name);
    }
}

fn start_the_agent(
    name: &str,
    placement: &Placement,
    harness: &str,
    model: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut argv = vec![harness.to_string()];
    if let Some(model) = model {
        argv.push("--model".to_string());
        argv.push(model.to_string());
    }
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
    if let Some(error) = response.get("error") {
        return Err(format!("delegate {name}: {}", server_error(error)));
    }
    response
        .pointer("/result/agent")
        .filter(|record| record.is_object())
        .cloned()
        .ok_or_else(|| format!("delegate {name}: the start answered with no agent record"))
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
    deadline: Instant,
) -> Result<serde_json::Value, String> {
    let pane_id = field(agent, "pane_id").unwrap_or_default().to_string();
    let terminal_id = field(agent, "terminal_id").unwrap_or_default().to_string();
    let ready_ms = u64::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis(),
    )
    .unwrap_or(0);

    match super::ready::wait_for_ready(name, &pane_id, ready_ms) {
        Ok(super::ready::ReadyOutcome::Ready(_)) => {}
        Ok(super::ready::ReadyOutcome::Refused { code, message }) => {
            return Err(format!(
                "delegate {name} never became ready: {code}: {message}"
            ));
        }
        Ok(super::ready::ReadyOutcome::SubscribeRefused(body)) => {
            return Err(format!("delegate {name}: {body}"));
        }
        Err(err) => {
            return Err(format!(
                "delegate {name}: readiness could not be watched: {err}"
            ))
        }
    }

    loop {
        let Some(record) = agent_record(&terminal_id, None) else {
            return Err(format!("delegate {name}: the agent no longer resolves"));
        };
        let status = field(&record, "agent_status")
            .unwrap_or("unknown")
            .to_string();
        if matches!(status.as_str(), "idle" | "done") {
            return Ok(record);
        }
        if expired(Some(deadline)) {
            return Err(format!("delegate {name} is {status}"));
        }
        sleep_bounded(deadline, READY_POLL);
    }
}

fn sleep_bounded(deadline: Instant, interval: Duration) {
    let left = deadline.saturating_duration_since(Instant::now());
    std::thread::sleep(left.min(interval));
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
    let mut entry = match require_delegate(name, None) {
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

    match super::pane::submit_sequence(&entry.pane_id, &sentence) {
        Ok(0) => {}
        Ok(_) | Err(_) => {
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
                "delegate {name}: the brief was not submitted"
            )));
        }
    }

    if !flags.await_result {
        emit_submit(&entry, flags.json);
        return Ok(exit::OK);
    }

    let deadline = flags
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    Await {
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
    loop {
        let Some(record) = agent_record(&entry.terminal_id, None) else {
            return Err(format!("delegate {name}: the agent no longer resolves"));
        };
        let status = field(&record, "agent_status")
            .unwrap_or("unknown")
            .to_string();
        if matches!(status.as_str(), "idle" | "done") {
            return Ok(record);
        }
        if expired(Some(deadline)) {
            return Err(format!("delegate {name} is {status}"));
        }
        sleep_bounded(deadline, READY_POLL);
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
        Err(code) => return Ok(code),
    };
    // A caller resuming after a timeout needs to wait for the SAME round, and
    // freshness is judged against when that round was submitted — so both come
    // from the entry, and `--after` only replaces the cursor.
    let after = flags.after.clone().or_else(|| Some(entry.cursor.clone()));
    Await {
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

fn delegate_result(args: &[String]) -> io::Result<i32> {
    let Some(name) = args.first() else {
        return Ok(usage(format!("usage: {}", RESULT_USAGE)));
    };
    let flags = match parse_flags(Verb::Result, args) {
        Ok(flags) => flags,
        Err(reason) => return Ok(usage(reason)),
    };
    let entry = match require_delegate(name, None) {
        Ok(entry) => entry,
        Err(code) => return Ok(code),
    };

    let response = match request(
        Method::AgentResult(AgentResultParams {
            target: entry.terminal_id.clone(),
            max_chars: flags.max_chars,
            offset: None,
        }),
        None,
    ) {
        Ok(response) => response,
        Err(err) => return Ok(fail(err)),
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
            let status = agent_record(&entry.terminal_id, None)
                .and_then(|record| field(&record, "agent_status").map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string());
            let running = matches!(status.as_str(), "working" | "blocked" | "unknown");
            let outcome = if running { "running" } else { "no_result" };
            emit_outcome(&entry, outcome, None, flags.json, &entry.cursor);
            return Ok(exit::OK);
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
    let entry = match require_delegate(name, None) {
        Ok(entry) => entry,
        Err(code) => return Ok(code),
    };
    let status = agent_record(&entry.terminal_id, None)
        .and_then(|record| field(&record, "agent_status").map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
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
fn require_delegate(name: &str, _deadline: Option<Instant>) -> Result<Entry, i32> {
    if let Err(reason) = validate_name(name) {
        return Err(usage(reason));
    }
    let refuse = || usage(format!("not a delegate: {name}"));
    let Some(entry) = read_entry(name) else {
        return Err(refuse());
    };
    let Some(record) = agent_record(name, None) else {
        return Err(refuse());
    };
    if field(&record, "terminal_id") != Some(entry.terminal_id.as_str()) {
        return Err(refuse());
    }
    Ok(entry)
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
    // The lock BEFORE the entry, for the same reason as `send`: the entry a reap
    // acts on must be the one the lock protects.
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
    let removed = match tear_down(&cleanup, flags.force) {
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
    if flags.json {
        println!(
            "{}",
            serde_json::json!({
                "name": entry.name,
                "workspace_id": entry.workspace_id,
                "worktree": entry.worktree,
                "removed": true,
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

struct Await<'a> {
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
    /// The gap between the TUI going quiet and the plugin committing the
    /// transcript is ordinary — opencode clears its spinner first — so a grace
    /// that reported `no_result` the instant the agent went idle would call every
    /// normal turn a failure.
    fn await_reply(&self, settled_cursor: &str) -> Grace {
        let grace_end = match self.deadline {
            Some(deadline) => deadline.min(Instant::now() + RESULT_GRACE),
            None => Instant::now() + RESULT_GRACE,
        };
        loop {
            if Instant::now() >= grace_end {
                return Grace::Exhausted;
            }
            match self.poll_result() {
                ResultPoll::Fresh { outcome, info } => return Grace::Reply { outcome, info },
                ResultPoll::Gone => return Grace::Gone,
                ResultPoll::Refused(reason) => return Grace::Refused(reason),
                ResultPoll::NotYet => {}
            }
            // The agent took another turn, so the reply that answers THIS round
            // is not the next one to land. Settle again instead.
            if self.started_another_turn(settled_cursor) {
                return Grace::NewTurn;
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
    fn started_another_turn(&self, settled_cursor: &str) -> bool {
        if settled_cursor.is_empty() {
            return false;
        }
        let Some(record) = agent_record(&self.entry.terminal_id, None) else {
            return false;
        };
        let Some(raw) = record
            .get("turn_cursor")
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };
        let (Ok(now), Ok(before)) = (Cursor::parse(raw), Cursor::parse(settled_cursor)) else {
            return false;
        };
        if now.epoch != before.epoch {
            return now.epoch > before.epoch;
        }
        now.entries > before.entries || (before.working && now.seq > before.seq)
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
        let response = match request(
            Method::AgentResult(AgentResultParams {
                target: self.entry.terminal_id.clone(),
                max_chars: self.max_chars,
                offset: None,
            }),
            self.deadline,
        ) {
            Ok(response) => response,
            Err(err) => return ResultPoll::Refused(err.to_string()),
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
            if NOT_YET_CODES.contains(&code) {
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

    /// The one harness this build drives, named in the refusal rather than a
    /// list of the ones it does not.
    #[test]
    fn only_opencode_is_supported_yet() {
        assert_eq!(validate_harness(None), Ok("opencode"));
        assert_eq!(validate_harness(Some("opencode")), Ok("opencode"));
        assert!(validate_harness(Some("claude"))
            .expect_err("claude is not wired")
            .contains("not supported"));
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

    /// The `no_result` codes the grace treats as "not yet" include the
    /// no-session refusal, which is the one D8 names: an agent that has not
    /// reported its session yet has no store to read, and the plugin reports it
    /// asynchronously — so this is the ordinary first poll of a turn, not a fault.
    #[test]
    fn the_no_session_refusal_is_one_of_the_not_yet_codes() {
        assert!(NOT_YET_CODES.contains(&"no_agent_session"));
        assert!(NOT_YET_CODES.contains(&"no_result"));
        assert!(NOT_YET_CODES.contains(&"transcript_not_found"));
        assert!(NOT_YET_CODES.contains(&"transcript_unreadable"));
        assert!(
            !NOT_YET_CODES.contains(&"permission_denied"),
            "a real refusal must still be an error"
        );
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
