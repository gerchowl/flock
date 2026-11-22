//! Background jobs from a pane's process tree (#1022, #663 A): the
//! `background` axis for harnesses whose screen does not count them.
//!
//! What a job looks like was read off live panes with `ps`. Claude runs a
//! `run_in_background` call in a shell it spawns (`zsh -c …`). Codex runs a
//! background terminal on its own pty, so the command (often exec'd by its
//! shell, so named `sleep` or `cargo`) sits in a session of its own. Both
//! keep their MCP servers as direct, non-shell children in the pane's
//! session. So a job is a child of the harness that is a shell or that left
//! the pane's session, or a process still in the session whose parent left
//! the tree (`cmd &` after its shell exited). Everything below a job is part
//! of that job, and a login shell's own children are the user's.

use std::collections::{HashMap, HashSet};

const MAX_DEPTH: usize = 64;

/// One process of the pane: pid, parent pid (`None` once gone), short
/// command name and session id.
pub(crate) struct ProcessRow {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub session: Option<i32>,
}

/// Jobs in the pane rooted at `root` (the pane's child process). `None` when
/// the root is gone or the platform has no process table.
pub(crate) fn pane_background_jobs(root: u32) -> Option<u32> {
    use crate::platform::{all_process_ids, process_parent_id, session_processes};
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for pid in all_process_ids() {
        if let Some(parent) = process_parent_id(pid) {
            children.entry(parent).or_default().push(pid);
        }
    }
    let mut tree = HashSet::new();
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        if tree.insert(pid) {
            queue.extend(children.get(&pid).into_iter().flatten().copied());
        }
    }
    let rows: Vec<ProcessRow> = tree
        .into_iter()
        .chain(session_processes(root))
        .collect::<HashSet<u32>>()
        .into_iter()
        .map(|pid| ProcessRow {
            pid,
            parent: process_parent_id(pid),
            name: crate::platform::process_name(pid).unwrap_or_default(),
            session: crate::platform::process_session_id(pid),
        })
        .collect();
    count_jobs(root, &rows)
}

/// [`pane_background_jobs`] over an injected process table.
pub(crate) fn count_jobs(root: u32, rows: &[ProcessRow]) -> Option<u32> {
    let by_pid: HashMap<u32, &ProcessRow> = rows.iter().map(|row| (row.pid, row)).collect();
    let root_session = by_pid.get(&root)?.session?;
    let is_shell = |pid: u32| by_pid.get(&pid).is_some_and(|row| is_shell(&row.name));
    let in_session = |pid: u32| {
        by_pid
            .get(&pid)
            .is_some_and(|row| row.session == Some(root_session))
    };
    // The harness: the pane root, or a non-shell in the pane's session whose
    // line up to the root runs through no shell but the root.
    let is_harness = |mut pid: u32| {
        for _ in 0..MAX_DEPTH {
            if pid == root {
                return true;
            }
            if is_shell(pid) || !in_session(pid) {
                return false;
            }
            match by_pid.get(&pid).and_then(|row| row.parent) {
                Some(up) if by_pid.contains_key(&up) => pid = up,
                _ => return false,
            }
        }
        false
    };
    let jobs = rows.iter().filter(|row| match row.parent {
        _ if row.pid == root => false,
        None => false,
        Some(up) if !by_pid.contains_key(&up) => in_session(row.pid),
        Some(up) => !is_shell(up) && is_harness(up) && (is_shell(row.pid) || !in_session(row.pid)),
    });
    Some(jobs.count() as u32)
}

fn is_shell(name: &str) -> bool {
    let name = name.rsplit('/').next().unwrap_or(name);
    matches!(
        name.trim_start_matches('-'),
        "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh" | "nu"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(pid, parent, name, session)`.
    fn rows(table: &[(u32, u32, &str, i32)]) -> Vec<ProcessRow> {
        table
            .iter()
            .map(|&(pid, parent, name, session)| ProcessRow {
                pid,
                parent: Some(parent),
                name: name.to_string(),
                session: Some(session),
            })
            .collect()
    }

    /// The shapes read with `ps` off live panes (#1022): Codex under a login
    /// shell with its MCP servers and a background terminal, and Claude as
    /// the pane root with two `run_in_background` shells, a `caffeinate` and
    /// an MCP server.
    #[test]
    fn tool_shells_pty_jobs_and_orphans_are_jobs_and_harness_children_are_not() {
        let mut codex = rows(&[
            (20, 10, "-zsh", 20),
            (30, 20, "codex", 20),
            (31, 30, "node", 20),
            (32, 30, "s1-mcp", 20),
            (33, 30, "flk", 20),
        ]);
        assert_eq!(count_jobs(20, &codex), Some(0));

        codex.extend(rows(&[(40, 30, "sleep", 40), (41, 40, "sh", 40)]));
        assert_eq!(
            count_jobs(20, &codex),
            Some(1),
            "a background terminal on its own pty, children included"
        );
        codex.extend(rows(&[
            (50, 30, "zsh", 20),
            (51, 50, "cargo", 20),
            (52, 51, "sh", 20),
        ]));
        assert_eq!(
            count_jobs(20, &codex),
            Some(2),
            "a tool shell, its build's own shells are part of it"
        );
        codex.extend(rows(&[(60, 1, "sleep", 20), (61, 60, "sh", 20)]));
        assert_eq!(
            count_jobs(20, &codex),
            Some(3),
            "a `cmd &` orphan stays in the session"
        );

        let claude = rows(&[
            (94, 91, "claude", 94),
            (95, 94, "zsh", 94),
            (96, 95, "sleep", 94),
            (97, 94, "zsh", 94),
            (98, 97, "sleep", 94),
            (99, 94, "caffeinate", 94),
            (100, 94, "tg-mcp", 94),
        ]);
        assert_eq!(count_jobs(94, &claude), Some(2));
    }

    #[test]
    fn a_plain_shell_pane_a_vanished_parent_and_a_gone_root_are_not_jobs() {
        let shell_pane = rows(&[
            (20, 10, "zsh", 20),
            (21, 20, "vim", 20),
            (22, 20, "bash", 20),
        ]);
        assert_eq!(count_jobs(20, &shell_pane), Some(0));
        let mut racing = rows(&[(20, 10, "codex", 20), (21, 20, "zsh", 20)]);
        racing[1].parent = None;
        assert_eq!(count_jobs(20, &racing), Some(0));
        assert_eq!(count_jobs(20, &racing[1..]), None, "the root is gone");
    }

    /// The live path: a session leader standing in for the harness (it
    /// `exec`s into a non-shell) with one tool shell still running.
    #[test]
    fn a_live_session_with_a_running_tool_shell_counts_one_job() {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "/bin/sh -c 'sleep 30; :' & exec sleep 30"]);
        // SAFETY: `setsid` is async-signal-safe, and it makes the child the
        // leader of its own session, as a pane's child is.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut harness = crate::process::TracedCommand::from_command(command, "test")
            .spawn_traced()
            .unwrap();
        let root = harness.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = pane_background_jobs(root);
        while seen != Some(1) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            seen = pane_background_jobs(root);
        }
        let jobs = crate::platform::session_processes(root);
        crate::platform::signal_processes(&jobs, crate::platform::Signal::Kill);
        let _ = harness.wait();
        assert_eq!(seen, Some(1));
        assert_eq!(pane_background_jobs(root), None, "a gone root is unknown");
    }
}
