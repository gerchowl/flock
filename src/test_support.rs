//! Shared helpers for tests that need to run real programs.
//!
//! Tests reached for absolute FHS paths — `/usr/bin/true`, `/bin/cat` — as a
//! shorthand for "a trivial program that definitely exists". On NixOS neither
//! does: `/bin` holds only `sh`, and there is no `/usr/bin` to speak of, so
//! nine tests failed there on `main` while passing on macOS and on GitHub's
//! ubuntu runners. Same class as the `/bin/bash` fix in #268 — a test asserting
//! about the filesystem layout of the machine it happens to run on.
//!
//! Resolve through `PATH` instead, which is the one mechanism every platform
//! agrees on.

use std::path::PathBuf;

/// Absolute path to `name`, found on `PATH`.
///
/// Panics rather than returning an `Option`: every caller wants a program it
/// can spawn, and a missing `true`/`cat` means the test environment is broken
/// in a way that a confusing spawn error downstream would only obscure.
pub(crate) fn program_path(name: &str) -> PathBuf {
    let Some(path) = std::env::var_os("PATH") else {
        panic!("PATH is unset, cannot resolve `{name}` for a test");
    };
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!("`{name}` not found on PATH — required by this test");
        })
}

/// Same, as a `String`, for the many call sites that hold shell paths as text.
pub(crate) fn program_path_string(name: &str) -> String {
    program_path(name).to_string_lossy().into_owned()
}

/// A program that exits 0 immediately, for tests that need a pane's process to
/// spawn and finish without doing anything. `/usr/bin/true` by another name.
pub(crate) fn no_op_program() -> String {
    program_path_string("true")
}

/// A program that stays up instead of exiting the moment it execs, for tests
/// that need a pane whose process is still there afterwards (#178).
///
/// `sh` with no arguments sits reading the pane's pty, which is the shape a
/// real agent has. Tests that used [`no_op_program`] for this got away with it
/// until flock started refusing a start whose agent was already gone.
pub(crate) fn live_program() -> String {
    program_path_string("sh")
}

/// `PATH` as a `(key, value)` pair, for check scripts.
///
/// `checks::script::run_script` deliberately calls `env_clear()`, so a script
/// it runs has no `PATH` at all. On an FHS distro `/bin/sh` still finds
/// `sleep` or `dd` via its compiled-in fallback (`/bin:/usr/bin`); on NixOS
/// that fallback resolves to nothing. Tests whose scripts call out to real
/// binaries must therefore pass `PATH` explicitly, which is what a real check
/// with an `env` block would do.
pub(crate) fn path_env_pair() -> (String, String) {
    (
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    )
}

// ---------------------------------------------------------------------
// Git fixtures. Real `git`, because the values these tests assert on are
// git's own answers (#351, #402) — a hand-written fixture string only proves
// a matcher matches itself.
//
// Three modules need the same shapes: `worktree`'s classifier tests, the TUI
// kill dialog and fleet sweep, and the socket API. They were three separate
// copies, which is how a fix to one seam quietly stops covering the others —
// the defect #402 exists to correct.
// ---------------------------------------------------------------------

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A temp path unique to this test, derived from the fixture's own name.
///
/// `std::env::temp_dir()`, never a hardcoded `/tmp`: the gate over `tests/`
/// and `#[cfg(test)]` regions treats a fixed path as asserting about the
/// machine, and `/tmp` is a symlink to `/private/tmp` on macOS besides.
pub(crate) fn unique_temp_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("flock-{name}-{}-{nanos}", std::process::id()))
}

/// Run `git` in `repo`, asserting it succeeded.
#[allow(clippy::disallowed_methods)] // Tests exec real git to prime fixtures — TracedCommand polices product code (logging redesign PR-3).
pub(crate) fn run_git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git should be spawnable");
    assert!(
        status.success(),
        "git command failed: git -C {} {}",
        repo.display(),
        args.join(" ")
    );
}

/// git blocks the file transport for submodules by default, so a fixture
/// cloning one from a sibling temp dir has to opt back in.
#[allow(clippy::disallowed_methods)] // Tests exec real git to prime fixtures — TracedCommand polices product code.
pub(crate) fn run_git_over_file_protocol(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "-c",
            "commit.gpgsign=false",
        ])
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git should be spawnable");
    assert!(
        status.success(),
        "git command failed: git -C {} {}",
        repo.display(),
        args.join(" ")
    );
}

/// A repo with one committed file.
pub(crate) fn create_committed_repo(name: &str) -> PathBuf {
    let repo = unique_temp_path(name);
    std::fs::create_dir_all(&repo).unwrap();
    run_git(&repo, &["init", "--quiet"]);
    run_git(&repo, &["config", "user.email", "flock@example.invalid"]);
    run_git(&repo, &["config", "user.name", "Flock Test"]);
    std::fs::write(repo.join("README.md"), "test\n").unwrap();
    run_git(&repo, &["add", "README.md"]);
    run_git(&repo, &["commit", "--quiet", "-m", "initial"]);
    repo
}

/// The #351 fixture: a repo whose committed gitlink is populated, and a linked
/// worktree of it on `branch` that carries that submodule.
///
/// Both the worktree and the submodule are clean — which is the entire reason
/// git's refusal surprises a `dirty` probe, so callers that depend on that
/// premise should assert it rather than inherit it.
///
/// Returns `(submodule repo, superproject, worktree checkout)`; the submodule
/// repo is included because its temp dir is the superproject's `origin` and
/// needs the same cleanup.
pub(crate) fn create_submodule_worktree(name: &str, branch: &str) -> (PathBuf, PathBuf, PathBuf) {
    let sub = create_committed_repo(&format!("{name}-sub"));
    let repo = create_committed_repo(name);
    run_git_over_file_protocol(
        &repo,
        &[
            "submodule",
            "add",
            "--quiet",
            &sub.display().to_string(),
            "sub",
        ],
    );
    run_git(&repo, &["commit", "--quiet", "-m", "add submodule"]);
    let checkout = unique_temp_path(&format!("{name}-checkout"));
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            branch,
            checkout.to_str().unwrap(),
            "HEAD",
        ],
    );
    run_git_over_file_protocol(&checkout, &["submodule", "update", "--init", "--quiet"]);
    (sub, repo, checkout)
}

/// Wait for the one event matching `want`, discarding the rest.
///
/// Predicate-matched on purpose: the kill dialog, the fleet sweep and the
/// socket share the gate event/worker, and a fixture that primes real git
/// generates events for work the test did not ask about. A first-event-wins
/// helper races that; the sweep's rows are the slowest here (a recovered
/// removal is two `git worktree remove` calls), so the deadline is generous
/// rather than tight.
pub(crate) fn wait_for_event(
    app: &mut crate::app::App,
    want: fn(&crate::events::AppEvent) -> bool,
) -> crate::events::AppEvent {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Ok(event) = app.event_rx.try_recv() {
            if want(&event) {
                return event;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for worktree event");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_program_that_exists_on_path() {
        let resolved = program_path("sh");
        assert!(resolved.is_absolute(), "{resolved:?} should be absolute");
        assert!(resolved.is_file(), "{resolved:?} should exist");
        assert!(resolved.ends_with("sh"));
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // Test spawns a real binary to prove it is spawnable — TracedCommand polices product code.
    fn no_op_program_is_spawnable_and_exits_zero() {
        // The property every `/usr/bin/true` call site actually depended on.
        let status = std::process::Command::new(no_op_program())
            .status()
            .expect("the no-op program should spawn");
        assert!(status.success());
    }

    #[test]
    #[should_panic(expected = "not found on PATH")]
    fn missing_program_panics_with_a_useful_message() {
        program_path("flock-definitely-not-a-real-program");
    }
}
