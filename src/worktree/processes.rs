//! Who is still standing in a checkout flock is about to delete, and what to
//! do about the ones that outlive it (#400).
//!
//! `git worktree remove` unlinks a directory. It says nothing about the
//! processes whose working directory that directory was, and flock's pane
//! teardown only reaches what it can see: a `setsid` child is deliberately out
//! of its process group, so a kill leaves it running against a path that no
//! longer exists. Measured once as 48 orphaned shells at ~16% CPU each for 25
//! hours — ~7.6 cores of a 12-core box, misattributed to other projects for
//! most of a day because nothing in the kill's output ever mentioned them.
//!
//! Two rules make the sweep safe enough to run by default:
//!
//! 1. **The leaf, exactly.** Matching is component-wise against the checkout
//!    itself, never a string prefix and never the shared
//!    `~/.flock/worktrees/<repo>/` parent — a looser glob in the downstream
//!    workaround this replaces once killed a sibling agent's sweep.
//! 2. **After the removal, not before.** Every live worktree has pane shells
//!    sitting in it, so a decision taken before teardown is a decision about
//!    the workspace's own healthy processes. A process still alive *after* the
//!    checkout is gone is an orphan by construction: its cwd is a directory
//!    that does not exist, so there is no work in that tree it could still be
//!    doing.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use crate::platform::{self, Signal};

/// What Linux appends to a `/proc/<pid>/cwd` readlink once the directory has
/// been unlinked. Left in place it defeats every match, which is precisely the
/// case the sweep exists for: by the time anybody looks, the leaf is gone.
const DELETED_SUFFIX: &str = " (deleted)";

/// How long a process gets to honour `SIGTERM` before `SIGKILL`. Long enough
/// for a shell to run its traps, short enough that a kill still feels like one
/// command — the removal's own `git` call routinely takes longer.
const TERMINATE_GRACE: Duration = Duration::from_millis(1500);

/// How long the post-`SIGKILL` confirmation waits before reporting a process
/// as unkillable. `SIGKILL` is not delivered instantly; a process blocked in
/// an uninterruptible syscall survives it entirely, and that is worth saying.
const KILL_CONFIRM_WAIT: Duration = Duration::from_millis(250);

/// Poll interval while waiting for signalled processes to go away.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How long the pane teardown gets to finish before whatever is left counts
/// as an orphan. See [`survivors_after_teardown`].
const TEARDOWN_SETTLE: Duration = Duration::from_millis(250);

/// Ceiling on the ancestry walk that builds the protected set, so a bad
/// `ppid` chain cannot spin.
const MAX_ANCESTRY_DEPTH: usize = 64;

/// A process found standing in a checkout.
///
/// The name is carried alongside the pid for two reasons: a bare pid list is
/// unreadable in a kill's output, and the name is what re-identifies the
/// process after the removal — see [`survivors`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckoutProcess {
    pub pid: u32,
    pub name: Option<String>,
}

/// Strip the marker Linux appends to a `/proc/<pid>/cwd` readlink for an
/// unlinked directory. A path that is not valid UTF-8 is passed through
/// untouched rather than lossily rewritten — a mangled path matches nothing,
/// which would silently drop a real orphan.
fn undeleted(cwd: &Path) -> PathBuf {
    let Some(text) = cwd.to_str() else {
        return cwd.to_path_buf();
    };
    match text.strip_suffix(DELETED_SUFFIX) {
        Some(trimmed) => PathBuf::from(trimmed),
        None => cwd.to_path_buf(),
    }
}

/// Is `cwd` the checkout itself, or something below it?
///
/// [`Path::starts_with`] compares whole components, which is the entire point:
/// a string prefix would make `…/fix-400` match `…/fix-4000`, and the parent
/// directory every worktree of a repo shares would match all of them.
///
/// Both spellings of the checkout are tried because the two sides resolve
/// differently: the kernel reports a fully resolved path, while flock's is
/// whatever the config produced — on macOS a `/tmp` checkout is reported under
/// `/private/tmp`, and a symlinked home does the same on any platform.
fn cwd_is_in_checkout(cwd: &Path, checkout: &Path, canonical_checkout: &Path) -> bool {
    let cwd = undeleted(cwd);
    cwd.starts_with(checkout) || cwd.starts_with(canonical_checkout)
}

/// Is this path specific enough to sweep?
///
/// A backstop, not the main defence — callers only ever pass a resolved linked
/// worktree checkout. It refuses the paths where a mistake is unrecoverable:
/// a relative path, the filesystem root, a single top-level directory, and the
/// home directory itself. `home` is a parameter rather than an environment
/// read so the rule can be stated in a test without depending on the machine
/// running it.
fn is_sweepable_checkout(path: &Path, home: Option<&Path>) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if home.is_some_and(|home| path == home) {
        return false;
    }
    path.components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count()
        >= 2
}

/// [`is_sweepable_checkout`] against the real home directory.
fn is_sweepable_checkout_here(path: &Path) -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    is_sweepable_checkout(path, home.as_deref())
}

/// flock's own process and everything that launched it, plus `caller_pid` and
/// everything that launched THAT.
///
/// `flk worktree kill` run from inside the checkout has its own cwd in the
/// leaf, and so does the shell it was typed into. Sweeping those would kill
/// the command partway through answering and take the operator's terminal with
/// it — a sweep that eats its own caller is not a fix.
pub(crate) fn protected_pids(caller_pid: Option<u32>) -> BTreeSet<u32> {
    let mut protected = BTreeSet::new();
    for start in [Some(std::process::id()), caller_pid].into_iter().flatten() {
        if start <= 1 {
            continue;
        }
        let mut pid = start;
        for _ in 0..MAX_ANCESTRY_DEPTH {
            if !protected.insert(pid) {
                // Already walked from here — same ancestry, or a cycle.
                break;
            }
            match platform::process_parent_id(pid) {
                Some(parent) if parent > 1 => pid = parent,
                _ => break,
            }
        }
    }
    protected
}

/// Every process whose working directory is inside `checkout`, minus the
/// protected set. Sorted by pid so the reported list is stable.
pub(crate) fn processes_in_checkout(
    checkout: &Path,
    protected: &BTreeSet<u32>,
) -> Vec<CheckoutProcess> {
    if !is_sweepable_checkout_here(checkout) {
        return Vec::new();
    }
    let canonical = crate::worktree::canonical_or_original(checkout);
    let mut found: Vec<CheckoutProcess> = platform::all_process_ids()
        .into_iter()
        .filter(|pid| !protected.contains(pid))
        .filter(|pid| {
            platform::process_cwd(*pid)
                .is_some_and(|cwd| cwd_is_in_checkout(&cwd, checkout, &canonical))
        })
        .map(|pid| CheckoutProcess {
            pid,
            name: platform::process_name(pid),
        })
        .collect();
    found.sort_by_key(|process| process.pid);
    found
}

/// Which of `snapshot` is still the same process, given ways to ask whether a
/// pid is alive and what it is called.
///
/// Existence alone is not enough. A pid freed while the checkout was being
/// removed can be handed to something unrelated before anybody looks, and
/// signalling that one is exactly the sibling-kill this sweep must never do.
/// Re-reading the cwd would be the stronger check but is not available: the
/// directory it would be compared against has just been deleted.
fn survivors_of(
    snapshot: &[CheckoutProcess],
    alive: impl Fn(u32) -> bool,
    name_of: impl Fn(u32) -> Option<String>,
) -> Vec<CheckoutProcess> {
    snapshot
        .iter()
        .filter(|process| alive(process.pid) && name_of(process.pid) == process.name)
        .cloned()
        .collect()
}

/// [`survivors_of`] against the live process table.
fn survivors(snapshot: &[CheckoutProcess]) -> Vec<CheckoutProcess> {
    survivors_of(snapshot, platform::process_exists, platform::process_name)
}

/// Who outlived the teardown, once the teardown has had its moment.
///
/// Closing the workspace drops the panes' PTYs and their shells get `SIGHUP`,
/// which they act on in milliseconds — but not in zero. Asking the instant the
/// workspace closes would count flock's own healthy teardown as a leak and
/// report every ordinary kill as having orphaned its panes. The wait ends the
/// moment the last one is gone, so it only costs anything when something
/// really did survive.
pub(crate) fn survivors_after_teardown(snapshot: &[CheckoutProcess]) -> Vec<CheckoutProcess> {
    wait_for_exit(snapshot, TEARDOWN_SETTLE)
}

/// Comma-joined pids, for a log field. Shaped here rather than at the call
/// site so a tracing macro never has to reach for a raw `?`/`%` formatter.
pub(crate) fn pid_list(processes: &[CheckoutProcess]) -> String {
    processes
        .iter()
        .map(|process| process.pid.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// `SIGTERM`, a grace period, then `SIGKILL` for whatever ignored it. Returns
/// the processes still alive at the end — nothing flock can do about those,
/// but naming them beats the silence #400 measured.
pub(crate) fn terminate(processes: &[CheckoutProcess]) -> Vec<CheckoutProcess> {
    if processes.is_empty() {
        return Vec::new();
    }

    let pids: Vec<u32> = processes.iter().map(|process| process.pid).collect();
    platform::signal_processes(&pids, Signal::Terminate);
    let remaining = wait_for_exit(processes, TERMINATE_GRACE);
    if remaining.is_empty() {
        return Vec::new();
    }

    let pids: Vec<u32> = remaining.iter().map(|process| process.pid).collect();
    platform::signal_processes(&pids, Signal::Kill);
    wait_for_exit(&remaining, KILL_CONFIRM_WAIT)
}

/// Poll until every process in `processes` is gone or `budget` runs out.
fn wait_for_exit(processes: &[CheckoutProcess], budget: Duration) -> Vec<CheckoutProcess> {
    let deadline = Instant::now() + budget;
    loop {
        let remaining = survivors(processes);
        if remaining.is_empty() || Instant::now() >= deadline {
            return remaining;
        }
        std::thread::sleep(EXIT_POLL_INTERVAL);
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Spawns a real child to prove the sweep finds and ends one; TracedCommand polices product code.
mod tests {
    use super::*;

    fn checkout(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flock-sweep-{name}-{}", std::process::id()))
    }

    #[test]
    fn deleted_suffix_is_stripped_so_a_removed_leaf_still_matches() {
        let leaf = Path::new("/w/repo/fix-400");
        let cwd = Path::new("/w/repo/fix-400 (deleted)");
        assert!(cwd_is_in_checkout(cwd, leaf, leaf));
    }

    #[test]
    fn the_checkout_itself_and_anything_below_it_match() {
        let leaf = Path::new("/w/repo/fix-400");
        assert!(cwd_is_in_checkout(leaf, leaf, leaf));
        assert!(cwd_is_in_checkout(
            Path::new("/w/repo/fix-400/src"),
            leaf,
            leaf
        ));
        assert!(cwd_is_in_checkout(
            Path::new("/w/repo/fix-400/src/ui (deleted)"),
            leaf,
            leaf
        ));
    }

    /// The whole safety argument in one test: the shared parent every worktree
    /// of a repo lives under must never match, and neither must a sibling whose
    /// name merely starts with the same characters.
    #[test]
    fn neither_the_shared_parent_nor_a_string_prefix_sibling_matches() {
        let leaf = Path::new("/w/repo/fix-400");
        assert!(!cwd_is_in_checkout(Path::new("/w/repo"), leaf, leaf));
        assert!(!cwd_is_in_checkout(
            Path::new("/w/repo/fix-4000"),
            leaf,
            leaf
        ));
        assert!(!cwd_is_in_checkout(
            Path::new("/w/repo/fix-4000/src"),
            leaf,
            leaf
        ));
        assert!(!cwd_is_in_checkout(
            Path::new("/w/other/fix-400"),
            leaf,
            leaf
        ));
    }

    #[test]
    fn the_canonical_spelling_matches_when_the_configured_one_does_not() {
        let configured = Path::new("/tmp/w/fix-400");
        let canonical = Path::new("/private/tmp/w/fix-400");
        assert!(cwd_is_in_checkout(
            Path::new("/private/tmp/w/fix-400/src"),
            configured,
            canonical
        ));
    }

    #[test]
    fn unsweepable_paths_are_refused() {
        let home = Path::new("/home/agent");
        assert!(!is_sweepable_checkout(Path::new("relative/path"), None));
        assert!(!is_sweepable_checkout(Path::new("/"), None));
        assert!(!is_sweepable_checkout(Path::new("/home"), None));
        assert!(!is_sweepable_checkout(home, Some(home)));
        assert!(is_sweepable_checkout(
            Path::new("/home/agent/w/fix-400"),
            Some(home)
        ));
    }

    /// A pid that came back alive under a different name is a recycled pid,
    /// not a survivor. Signalling it would be the sibling-kill the sweep is
    /// built to avoid.
    #[test]
    fn a_recycled_pid_is_not_a_survivor() {
        let snapshot = vec![
            CheckoutProcess {
                pid: 10,
                name: Some("zsh".into()),
            },
            CheckoutProcess {
                pid: 11,
                name: Some("claude".into()),
            },
            CheckoutProcess {
                pid: 12,
                name: Some("cargo".into()),
            },
        ];
        let survivors = survivors_of(
            &snapshot,
            |pid| pid != 12,
            |pid| match pid {
                10 => Some("zsh".into()),
                // Same pid, different process: the kernel handed it on.
                11 => Some("sshd".into()),
                _ => None,
            },
        );
        assert_eq!(
            survivors,
            vec![CheckoutProcess {
                pid: 10,
                name: Some("zsh".into())
            }]
        );
    }

    #[test]
    fn flock_never_sweeps_itself() {
        assert!(protected_pids(None).contains(&std::process::id()));
    }

    /// Park a process in a directory and hand back its pid.
    ///
    /// Deliberately a GRANDCHILD: the shell that starts it exits immediately,
    /// so the loop is reparented to init exactly as #400's `setsid` spinners
    /// were. A direct child would also be a zombie after `SIGTERM` until the
    /// test reaped it, and a zombie still answers `kill(pid, 0)` — the sweep
    /// would report it as having outlived `SIGKILL`.
    fn park_process_in(dir: &Path) -> u32 {
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            // The redirect matters: a background job inheriting the pipe keeps
            // it open, and reading the parent's output would never finish.
            .arg("while :; do sleep 1; done >/dev/null 2>&1 & echo $!")
            .current_dir(dir)
            .output()
            .expect("park a process in the checkout");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("the parked pid")
    }

    /// End to end against the real process table: a process parked in the
    /// checkout is found, is not confused with one parked next door, and is
    /// gone after the sweep. Everything above this test is a rule about paths;
    /// this is the one that proves the platform layer answers the question at
    /// all — on Linux through `/proc`, on Darwin through libproc.
    #[test]
    fn a_real_process_in_the_checkout_is_found_and_ended() {
        let leaf = checkout("found");
        let sibling = checkout("found-sibling");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();

        let inside = park_process_in(&leaf);
        let outside = park_process_in(&sibling);

        let protected = protected_pids(None);
        let found = processes_in_checkout(&leaf, &protected);
        assert!(
            found.iter().any(|process| process.pid == inside),
            "the process parked in the checkout must be found: {found:?}"
        );
        assert!(
            !found.iter().any(|process| process.pid == outside),
            "a process parked in a sibling directory must not be: {found:?}"
        );

        // The real sequence: remove the directory first, then sweep whatever
        // outlived it — which is what a `(deleted)` cwd looks like on Linux.
        std::fs::remove_dir_all(&leaf).unwrap();
        let orphans = survivors_after_teardown(&found);
        assert!(
            orphans.iter().any(|process| process.pid == inside),
            "the parked process outlived the directory: {orphans:?}"
        );
        assert!(
            terminate(&orphans).is_empty(),
            "a parked shell must not outlive TERM then KILL"
        );

        assert!(
            platform::process_exists(outside),
            "the process next door must still be running"
        );
        platform::signal_processes(&[outside], Signal::Kill);
        let _ = std::fs::remove_dir_all(&sibling);
    }
}
