//! `revert.run` handler (#175 phase 5 / S3 commit 4, ops).
//!
//! Scans every repo flock knows about — open workspaces' checkout paths
//! plus every worktree_space's `checkout_path` — for commits carrying
//! the requested `Agent-Run:` trailer, and reverts them onto a fresh
//! `revert/<run-id>` branch. Never touches `main`. Refuses a repo with
//! a dirty tree (reports and continues with other repos).
//!
//! Structured as a thin adapter over `crate::revert`: build the repo
//! list, hand it the git shim, collect results, emit `RunReverted` per
//! commit, encode the reply. Git is invoked through
//! `std::process::Command` because there is no App-scoped tracer for
//! ad-hoc git subshells (the check runner has one but its API expects
//! the check config).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::api::schema::{EventData, EventEnvelope, EventKind, ResponseResult, RevertRunParams};
use crate::app::App;
use crate::process::TracedCommand;
use crate::revert::{revert_branch_name, RevertRun};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_revert_run(&mut self, id: String, params: RevertRunParams) -> String {
        let run_id = params.run_id.trim().to_string();
        if run_id.is_empty() {
            return encode_error(id, "invalid_request", "run_id is required");
        }
        // Belt + braces against a shell-meta run_id sneaking past the
        // grep pattern — `git log --grep` is safe, but a run_id that
        // includes `$` or backticks would still land in the reply for
        // an operator to paste. Reject anything not in a conservative
        // charset.
        if !run_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':' | '.'))
        {
            return encode_error(id, "invalid_request", "run_id must be [A-Za-z0-9_.:-]+");
        }

        let repos = collect_known_repos(self);
        let dry_run = params.dry_run;
        let run = RevertRun::new(run_id.clone(), dry_run, repos.clone());
        let mut git = git_shim();
        let results = run.run_with_git(&mut git);
        // #175 S3 commit 4: durable RunReverted per reverted commit so
        // an audit can walk the log back to the exact revert history
        // without needing git. Dry-run also records — the audit trail
        // should show the intent even without side effects.
        for result in &results {
            for commit in &result.reverted_commits {
                self.event_hub.push(EventEnvelope {
                    event: EventKind::RunReverted,
                    data: EventData::RunReverted {
                        run_id: run.run_id.clone(),
                        repo_path: result.repo_path.clone(),
                        reverted_commit: commit.clone(),
                        revert_branch: if result.revert_branch.is_empty() {
                            revert_branch_name(&run.run_id)
                        } else {
                            result.revert_branch.clone()
                        },
                        dry_run: run.dry_run,
                    },
                });
            }
        }

        encode_success(
            id,
            ResponseResult::RunReverted {
                run_id,
                dry_run,
                results,
            },
        )
    }
}

/// Collect the set of repos flock knows about right now: every open
/// workspace's `worktree_space.checkout_path` (which includes the
/// main-worktree case, since main workspaces get a non-linked
/// membership too when they sit inside a git repo).
///
/// Deduplicated by canonicalized path so a repo referenced by both a
/// parent workspace and one of its linked-worktree children is scanned
/// once.
fn collect_known_repos(app: &App) -> Vec<PathBuf> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for ws in &app.state.workspaces {
        if let Some(space) = ws.worktree_space.as_ref() {
            let path = space.checkout_path.clone();
            let canon = std::fs::canonicalize(&path).unwrap_or(path);
            seen.insert(canon);
        }
    }
    seen.into_iter().collect()
}

/// Default git shim: spawn `git -C <repo> <args...>` via TracedCommand
/// and return stdout (with stderr merged into the error on non-zero
/// exit). Every subprocess flock runs goes through this funnel so the
/// audit log answers "what did flock actually run?" (see `crate::process`).
fn git_shim() -> impl FnMut(&Path, &[&str]) -> Result<String, String> {
    move |repo_path: &Path, args: &[&str]| -> Result<String, String> {
        let mut cmd = TracedCommand::new("git", "revert");
        cmd.arg("-C").arg(repo_path).args(args);
        let output = cmd
            .output_traced()
            .map_err(|err| format!("git spawn failed: {err}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if stderr.is_empty() {
                format!("git exited {}", output.status)
            } else {
                stderr
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
