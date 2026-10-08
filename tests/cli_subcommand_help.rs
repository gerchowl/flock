//! E2E (#455): asking any `flk` subcommand for help answers with usage and
//! allocates nothing.
//!
//! Driven through the compiled binary, not the parser, because the thing this
//! protects is a binary: `flk worktree create --help` used to answer "unknown
//! option", and the same command with the flag dropped ran `git worktree add`
//! and left a branch behind. `flk agent fork --help` was worse — it took the
//! flag as an agent target and went looking for an agent named `--help`.
//!
//! The allocation assertions run against a STAND-IN SERVER that really performs
//! `git worktree add`, not against an absent socket. That distinction is the
//! whole test: with the socket pointed at a path that cannot exist, nothing can
//! be allocated whether or not `--help` is honoured, so "the repo is untouched"
//! would be true for reasons that have nothing to do with the fix. Here the
//! allocating path is one command away from working, and
//! `the_stand_in_really_allocates` is the control that proves it does.

// Integration tests drive the compiled binary through raw Command; the
// TracedCommand funnel polices flock's own subprocesses, not the harness's.
#![allow(clippy::disallowed_methods)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Every verb that allocates something, so a regression in any one of them
/// shows up here rather than in someone's repository.
const ALLOCATING_VERBS: &[&[&str]] = &[
    &["worktree", "create"],
    &["worktree", "open"],
    &["worktree", "kill"],
    &["agent", "fork"],
    &["agent", "start"],
    &["agent", "resume"],
    &["workspace", "create"],
    &["tab", "create"],
    &["pane", "split"],
    &["delegate", "start"],
    &["delegate", "send"],
    &["delegate", "reap"],
];

/// Identity and config overrides for every git call this file makes.
///
/// They are on EVERY call, not just `init`, and that is the whole point: a
/// developer's machine will infer an author from the local username and
/// hostname, so a missing `-c user.email` here looks fine locally and fails on
/// a CI runner with no configured identity ("Author identity unknown"). This
/// suite's rule is that a fixture must not depend on whose machine it runs on.
const GIT_IDENTITY: &[&str] = &[
    "-c",
    "init.defaultBranch=main",
    "-c",
    "user.name=flock test",
    "-c",
    "user.email=test@flock.invalid",
    "-c",
    "commit.gpgsign=false",
];

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn help_on_an_allocating_verb_prints_usage_and_exits_zero() {
    let env = Env::new();
    for verb in ALLOCATING_VERBS {
        for flag in ["--help", "-h"] {
            let mut args = verb.to_vec();
            args.push(flag);
            let output = env.flk(&args);
            assert_eq!(
                output.status.code(),
                Some(0),
                "flk {} {flag} must exit 0: {}",
                verb.join(" "),
                stderr_of(&output)
            );
            let stdout = stdout_of(&output);
            // stdout, like `flk --help`: `… --help > usage.txt` has to write
            // something, and this is the request that was honoured.
            assert!(
                stdout.starts_with(&format!("usage: flk {} ", verb[0])),
                "flk {} {flag} must print its own usage on stdout: {stdout}{}",
                verb.join(" "),
                stderr_of(&output)
            );
        }
    }
}

/// The reported trap, verbatim. Before this the flag was an agent target and
/// the answer was an error about an agent that did not exist — which is how a
/// question about the CLI came to be answered as a failed fork.
#[test]
fn agent_fork_help_is_not_answered_as_a_missing_agent() {
    let env = Env::new();
    let output = env.flk(&["agent", "fork", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_of(&output);
    assert!(stdout.contains("flk agent fork <target>"), "{stdout}");
    assert!(!stdout.contains("agent_not_found"), "{stdout}");
    assert!(!stderr_of(&output).contains("agent_not_found"));
}

/// "Anywhere in the argument list": a flag asked for after the values it would
/// modify is still a request for usage, not a value and not an unknown option.
#[test]
fn help_after_the_flags_it_would_modify_is_still_help() {
    let env = Env::new();
    let output = env.flk(&["worktree", "kill", "--path", "/nonexistent", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert!(stdout_of(&output).contains("flk worktree kill"));

    let output = env.flk(&["agent", "fork", "w1", "--label", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
}

/// The shape that started this: probing `flk worktree create` allocated a git
/// worktree and a branch. Asking how it works must leave a repository exactly
/// as it found it — the same worktrees, the same branches, no new checkout, and
/// nothing even sent to the server that would have allocated one.
#[test]
fn asking_for_help_never_reaches_the_allocating_parser() {
    let env = Env::new();

    for verb in ALLOCATING_VERBS {
        for flag in ["--help", "-h"] {
            let mut args = verb.to_vec();
            args.push(flag);
            let output = env.flk(&args);
            assert_eq!(
                output.status.code(),
                Some(0),
                "flk {} {flag}: {}",
                verb.join(" "),
                stderr_of(&output)
            );
        }
    }

    assert_eq!(
        env.server.requests(),
        0,
        "asking for help must not reach the server at all"
    );
    assert_eq!(env.repo.worktrees().len(), 1, "only the repository itself");
    assert_eq!(env.repo.branches(), vec!["main".to_string()]);
    assert!(
        !env.repo.path.join(".git/worktrees").exists(),
        "a help request must not create a linked worktree checkout"
    );
}

/// The control for the test above, and the reason it is not vacuous.
///
/// If this ever stops allocating, `asking_for_help_never_reaches_the_allocating_parser`
/// has stopped being evidence of anything: a fixture that cannot detect the
/// allocation cannot be used to assert its absence. So this runs the bare verb
/// — the exact command the issue reported as the trap — and requires the
/// stand-in to do the allocating.
#[test]
fn the_stand_in_really_allocates() {
    let env = Env::new();

    let output = env.flk(&["worktree", "create"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the control must be a successful create: {}",
        stderr_of(&output)
    );

    assert_eq!(env.server.requests(), 1, "the server was asked");
    assert_eq!(
        env.repo.worktrees().len(),
        2,
        "the stand-in must really have run `git worktree add`, or the negative \
         assertion in the test above proves nothing"
    );
    assert!(
        env.repo
            .branches()
            .contains(&"worktree/stand-in".to_string()),
        "a worktree without a branch is not what this fixture claims to detect: {:?}",
        env.repo.branches()
    );
}

impl Drop for StandInServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// One test's world: a throwaway git repository, a stand-in flock server whose
/// socket the CLI is pointed at, and a sandboxed HOME so the developer's real
/// config is not in the picture.
struct Env {
    repo: TempRepo,
    server: StandInServer,
    home: PathBuf,
}

impl Env {
    fn new() -> Self {
        let repo = TempRepo::new();
        let home = repo.base.join("home");
        std::fs::create_dir_all(&home).expect("sandbox home");
        let server = StandInServer::start(repo.path.clone());
        Self { repo, server, home }
    }

    fn flk(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_flk"))
            .args(args)
            .current_dir(&self.repo.path)
            .env("FLOCK_SOCKET_PATH", &self.server.socket)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.home)
            .env_remove("FLOCK_CLIENT_SOCKET_PATH")
            .env_remove("FLOCK_ENV")
            .env_remove("FLOCK_SESSION")
            .output()
            .expect("flk should run")
    }
}

/// A stand-in for the flock server that performs the allocation `worktree.create`
/// would, so a test can tell "the CLI never asked" from "there was nothing to
/// ask".
struct StandInServer {
    socket: PathBuf,
    requests: Arc<AtomicUsize>,
}

impl StandInServer {
    fn start(repo: PathBuf) -> Self {
        // Short name: a unix socket path is length-limited, and this runs in the
        // harness's temp dir. Unique per test, so concurrent shards cannot
        // collide or read each other's request counts.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let socket = std::env::temp_dir().join(format!("f455-{}-{nanos}.sock", std::process::id()));
        let listener = UnixListener::bind(&socket).expect("bind stand-in socket");

        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut line = String::new();
                {
                    let mut reader = BufReader::new(match stream.try_clone() {
                        Ok(clone) => clone,
                        Err(_) => continue,
                    });
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let response = if line.contains("worktree.create") {
                    allocate_worktree(&repo);
                    r#"{"id":"stand-in","result":{"type":"worktree_created"}}"#
                } else {
                    r#"{"id":"stand-in","error":{"code":"not_implemented","message":"stand-in"}}"#
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(b"\n");
                let _ = stream.flush();
            }
        });

        Self { socket, requests }
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// What the real `worktree.create` does to a repository: a linked checkout and
/// a branch. Run from the stand-in so the allocation is real rather than
/// asserted about.
fn allocate_worktree(repo: &Path) {
    let checkout = repo.parent().unwrap_or(repo).join("worktree-stand-in");
    let status = Command::new("git")
        .args(GIT_IDENTITY)
        .args(["worktree", "add", "-b", "worktree/stand-in"])
        .arg(&checkout)
        .current_dir(repo)
        .output()
        .expect("git should run");
    assert!(
        status.status.success(),
        "the stand-in could not allocate: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

/// A throwaway repository in the temp dir, named after this file's own fixture
/// rather than anything on the machine it runs on.
struct TempRepo {
    base: PathBuf,
    path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!("flock-455-{}-{nanos}", std::process::id()));
        let path = base.join("repo");
        std::fs::create_dir_all(&path).expect("temp repo");

        let repo = Self { base, path };
        repo.git(&["init"]);
        repo.git(&["commit", "--allow-empty", "-m", "initial"]);
        repo
    }

    fn git(&self, args: &[&str]) -> Output {
        let output = Command::new("git")
            .args(GIT_IDENTITY)
            .args(args)
            .current_dir(&self.path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            stderr_of(&output)
        );
        output
    }

    /// `git worktree list --porcelain` as bare paths, main checkout included.
    fn worktrees(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.git(&["worktree", "list", "--porcelain"]).stdout)
            .lines()
            .filter_map(|line| line.strip_prefix("worktree ").map(str::to_string))
            .collect()
    }

    fn branches(&self) -> Vec<String> {
        String::from_utf8_lossy(
            &self
                .git(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])
                .stdout,
        )
        .lines()
        .map(str::to_string)
        .collect()
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[test]
fn agent_history_help_and_invalid_arguments() {
    let env = Env::new();
    for flag in ["--help", "-h"] {
        let output = env.flk(&["agent", "history", flag]);
        assert_eq!(output.status.code(), Some(0));
        assert!(stdout_of(&output).contains("--detail reply|collapsed|full"));
        assert!(stdout_of(&output).contains("--cursor N"));
        assert!(stdout_of(&output).contains("--limit N"));
    }
    for args in [
        vec!["agent", "history"],
        vec!["agent", "history", "reviewer", "--detail", "invalid"],
        vec!["agent", "history", "reviewer", "--cursor", "-1"],
        vec!["agent", "history", "reviewer", "--limit", "4294967296"],
        vec!["agent", "history", "reviewer", "--limit"],
        vec!["agent", "history", "reviewer", "--unknown"],
    ] {
        let output = env.flk(&args);
        assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
        assert!(!stderr_of(&output).is_empty());
    }
}
