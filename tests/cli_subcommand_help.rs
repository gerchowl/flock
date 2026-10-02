//! E2E (#455): asking any `flk` subcommand for help answers with usage and
//! allocates nothing.
//!
//! Driven through the compiled binary, not the parser, because the thing this
//! protects is a binary: `flk worktree create --help` used to answer "unknown
//! option", and the same command with the flag dropped ran `git worktree add`
//! and left a branch behind. `flk agent fork --help` was worse — it took the
//! flag as an agent target and went looking for an agent named `--help`.
//!
//! No server is needed and none is started: every one of these invocations is
//! answered before a socket is opened. `FLOCK_SOCKET_PATH` points at a path
//! that cannot exist so that anything which *does* get past the help check
//! fails quickly, and for an unmistakably different reason — which is what
//! makes "exit 0 with usage" evidence that the verb's parser never ran rather
//! than evidence that the verb happened to fail.

// Integration tests drive the compiled binary through raw Command; the
// TracedCommand funnel polices flock's own subprocesses, not the harness's.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::process::{Command, Output};

/// A socket path that cannot exist, so a request that escapes the help check
/// has nowhere to go.
fn absent_socket() -> PathBuf {
    let mut socket = std::env::temp_dir();
    socket.push("flock-455-no-such-server.sock");
    socket
}

fn flk(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env("FLOCK_SOCKET_PATH", absent_socket())
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_ENV")
        .output()
        .expect("flk should run")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

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
];

#[test]
fn help_on_an_allocating_verb_prints_usage_and_exits_zero() {
    for verb in ALLOCATING_VERBS {
        for flag in ["--help", "-h"] {
            let mut args = verb.to_vec();
            args.push(flag);
            let output = flk(&args);
            assert_eq!(
                output.status.code(),
                Some(0),
                "flk {} {flag} must exit 0: {}",
                verb.join(" "),
                stderr_of(&output)
            );
            let stderr = stderr_of(&output);
            assert!(
                stderr.starts_with(&format!("usage: flk {} ", verb[0])),
                "flk {} {flag} must print its own usage: {stderr}",
                verb.join(" ")
            );
        }
    }
}

/// The reported trap, verbatim. Before this the flag was an agent target and
/// the answer was an error about an agent that did not exist — which is how a
/// question about the CLI came to be answered as a failed fork.
#[test]
fn agent_fork_help_is_not_answered_as_a_missing_agent() {
    let output = flk(&["agent", "fork", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let stderr = stderr_of(&output);
    assert!(stderr.contains("flk agent fork <target>"), "{stderr}");
    assert!(!stderr.contains("agent_not_found"), "{stderr}");
}

/// "Anywhere in the argument list": a flag asked for after the values it would
/// modify is still a request for usage, not a value and not an unknown option.
#[test]
fn help_after_the_flags_it_would_modify_is_still_help() {
    let output = flk(&["worktree", "kill", "--path", "/nonexistent", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert!(stderr_of(&output).contains("flk worktree kill"));

    let output = flk(&["agent", "fork", "w1", "--label", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
}

/// The shape that started this: probing `flk worktree create` allocated a git
/// worktree and a branch. Asking how it works must leave a repository exactly
/// as it found it — the same worktrees, the same branches, no new checkout on
/// disk.
#[test]
fn asking_for_help_leaves_a_repository_untouched() {
    let repo = TempRepo::new();

    for verb in ALLOCATING_VERBS {
        for flag in ["--help", "-h"] {
            let mut args = verb.to_vec();
            args.push(flag);
            let output = Command::new(env!("CARGO_BIN_EXE_flk"))
                .args(&args)
                .current_dir(&repo.path)
                .env("FLOCK_SOCKET_PATH", absent_socket())
                .env_remove("FLOCK_CLIENT_SOCKET_PATH")
                .env_remove("FLOCK_ENV")
                .output()
                .expect("flk should run");
            assert_eq!(
                output.status.code(),
                Some(0),
                "flk {} {flag}: {}",
                verb.join(" "),
                stderr_of(&output)
            );
        }
    }

    assert_eq!(repo.worktrees().len(), 1, "only the repository itself");
    assert_eq!(repo.branches(), vec!["main".to_string()]);
    assert!(
        !repo.path.join(".git/worktrees").exists(),
        "a help request must not create a linked worktree checkout"
    );
}

/// A throwaway repository in the temp dir, named after this test's own fixture
/// rather than anything on the machine it runs on.
struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("flock-455-help-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp repo");

        let repo = Self { path };
        // `-c init.defaultBranch` and the other overrides are what
        // `tests/support` does: a developer's global git config must not decide
        // what this fixture looks like.
        repo.git(&[
            "-c",
            "init.defaultBranch=main",
            "-c",
            "user.name=flock test",
            "-c",
            "user.email=test@flock.invalid",
            "init",
        ]);
        repo.git(&[
            "-c",
            "user.name=flock test",
            "-c",
            "user.email=test@flock.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        repo
    }

    fn git(&self, args: &[&str]) -> Output {
        let output = Command::new("git")
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
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
