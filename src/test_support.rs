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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// How long a test waits for a file some *other* process owes it.
///
/// Deliberately generous, because the thing being waited on is routinely a
/// `/bin/sh -lc` — a fork, an exec, and a login profile — and on a loaded
/// machine those are seconds, not milliseconds. Waiting longer is not the
/// interesting half; the interesting half is that these waits are for a real
/// out-of-process effect and have no other signal to synchronize on.
const FILE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// How far apart two reads have to be before agreeing counts as "finished".
const FILE_SETTLE_GAP: Duration = Duration::from_millis(50);

/// Wait until `path` exists and whatever is writing it has finished, then
/// return its content.
///
/// "Finished" means two reads [`FILE_SETTLE_GAP`] apart that agree and are not
/// empty. "Readable" is not "complete": a redirect truncates before it writes,
/// and `cp` fills a file in pieces, so stopping at the first successful read
/// hands back `""` or a prefix for a command that ran perfectly well. That is
/// the flake this replaces — a `wait_for_file` whose 2 s budget was also too
/// short for the `/bin/sh -lc` it was waiting on (#444).
///
/// The assertions stay with the caller on purpose. An earlier version of this
/// took the caller's assertion as its readiness predicate, which made every
/// assertion downstream of it provably true; deciding *when the file is done*
/// is all a wait can know, and *what it should say* is the caller's business.
pub(crate) fn wait_for_file_stable(path: &Path) -> String {
    let deadline = Instant::now() + FILE_WAIT_TIMEOUT;
    let mut last_seen: Option<String> = None;
    while Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(path) {
            if !content.is_empty() && last_seen.as_deref() == Some(content.as_str()) {
                return content;
            }
            last_seen = Some(content);
        }
        std::thread::sleep(FILE_SETTLE_GAP);
    }
    panic!(
        "timed out waiting for {} to settle, last saw {:?}",
        path.display(),
        last_seen.unwrap_or_default()
    );
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
    // The submodule CLONE carries none of its superproject's config, so a
    // commit made inside it resolves its author from the machine's global git
    // config: present on a laptop, absent on a CI runner. Declared here so any
    // test that commits inside the submodule is hermetic rather than
    // accidentally machine-dependent (#262's lesson).
    let submodule = checkout.join("sub");
    run_git(
        &submodule,
        &["config", "user.email", "flock@example.invalid"],
    );
    run_git(&submodule, &["config", "user.name", "Flock Test"]);
    (sub, repo, checkout)
}

/// Wait for the one event matching `want`, discarding the rest.
///
/// Predicate-matched on purpose: the kill dialog, the fleet sweep and the
/// socket share the gate event/worker, and a fixture that primes real git
/// generates events for work the test did not ask about. A first-event-wins
/// helper races that; the sweep's rows are the slowest here (a recovered
/// removal is two `git worktree remove` calls), so they wait
/// [`DEFAULT_EVENT_WAIT`] rather than a tight bound.
///
/// `timeout` is a bound, checked before every poll: a deadline that only
/// applied while the queue was empty could be beaten by an event arriving
/// late, and an endless stream of unrelated events would never end the wait
/// at all (#539). Callers that need the *patient* bound ask for it by name;
/// callers that need a tight one say so and get it.
///
/// A timeout names what actually arrived, because "timed out waiting for an
/// event" and "the event never came" are different bugs and only one of them
/// is what a bare timeout says.
pub(crate) fn wait_for_event(
    app: &mut crate::app::App,
    want: fn(&crate::events::AppEvent) -> bool,
    timeout: std::time::Duration,
) -> crate::events::AppEvent {
    const MAX_NAMED: usize = 8;
    let mut named: Vec<String> = Vec::new();
    let mut skipped = 0usize;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() >= deadline {
            let arrived = if skipped == 0 {
                "nothing arrived at all".to_string()
            } else {
                format!(
                    "{skipped} event(s) arrived but none matched, first: {}",
                    named.join(", ")
                )
            };
            panic!("timed out waiting for a matching event; {arrived}");
        }
        if let Ok(event) = app.event_rx.try_recv() {
            if want(&event) {
                return event;
            }
            skipped += 1;
            if named.len() < MAX_NAMED {
                named.push(describe_event(&event));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The patient bound: a recovered worktree removal is two `git worktree
/// remove` calls, and the sweep's rows are the slowest workers in the suite.
pub(crate) const DEFAULT_EVENT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// One-line identity of a discarded event, for the timeout diagnostic.
/// Truncated because a `SystemStats` or a pane snapshot in `Debug` is a
/// paragraph, and a diagnostic nobody can read is no diagnostic.
fn describe_event(event: &crate::events::AppEvent) -> String {
    const MAX_CHARS: usize = 96;
    let mut text = format!("{event:?}");
    if text.chars().count() > MAX_CHARS {
        text = text.chars().take(MAX_CHARS).collect::<String>() + "...";
    }
    text
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
