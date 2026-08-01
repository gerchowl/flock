//! `flk revert-run <run-id>` — trailer-scan and revert (#175 S3 commit 4).
//!
//! Scans a set of repos (open workspaces + their worktree roots) for
//! commits that carry `Agent-Run: <id>` in their trailer, and creates
//! `revert/<run-id>` branches with a `git revert --no-edit` per commit,
//! newest-first. Never touches `main`. Refuses a repo with a dirty tree.
//!
//! Structured as pure logic + a shell adapter so the unit tests can
//! drive the whole revert flow against fake fixture repos without
//! shelling out. The App-side entry point ([`RevertRun::from_app`]) is
//! thin — build the repo list, call [`RevertRun::run`], collect the
//! per-repo results, emit `RunReverted` events, and return.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::api::schema::RevertRepoResult;

/// One repo to consider for revert.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RevertRepo {
    pub path: PathBuf,
}

/// Immutable inputs for one `revert.run` invocation.
#[derive(Debug, Clone)]
pub(crate) struct RevertRun {
    pub run_id: String,
    pub dry_run: bool,
    /// Unique, sorted, deduplicated list of repos to consider.
    pub repos: Vec<RevertRepo>,
}

impl RevertRun {
    /// Deduplicate + sort the input paths. Callers can pass a mixed list
    /// of workspace checkouts + worktree roots; the same checkout won't
    /// be scanned twice.
    pub(crate) fn new<I>(run_id: String, dry_run: bool, repos: I) -> Self
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let unique: BTreeSet<PathBuf> = repos.into_iter().collect();
        Self {
            run_id,
            dry_run,
            repos: unique.into_iter().map(|path| RevertRepo { path }).collect(),
        }
    }

    /// Execute the revert for every repo, driving git via the caller's
    /// shim. Production callers hand in a `Command::new("git")`-based
    /// closure; tests hand in a canned shim so behavior is auditable
    /// without shelling out.
    pub(crate) fn run_with_git<F>(&self, mut git: F) -> Vec<RevertRepoResult>
    where
        F: FnMut(&Path, &[&str]) -> Result<String, String>,
    {
        let mut out = Vec::with_capacity(self.repos.len());
        for repo in &self.repos {
            out.push(revert_one(&mut git, &repo.path, &self.run_id, self.dry_run));
        }
        out
    }
}

/// Revert one repo. Extracted so tests exercise it without threading a
/// `RevertRun` in each case.
pub(crate) fn revert_one<F>(
    git: &mut F,
    repo_path: &Path,
    run_id: &str,
    dry_run: bool,
) -> RevertRepoResult
where
    F: FnMut(&Path, &[&str]) -> Result<String, String>,
{
    // 1. Refuse dirty trees. `git status --porcelain` is empty when clean.
    match git(repo_path, &["status", "--porcelain"]) {
        Ok(out) if out.trim().is_empty() => {}
        Ok(out) => {
            return RevertRepoResult {
                repo_path: repo_path.display().to_string(),
                revert_branch: String::new(),
                reverted_commits: Vec::new(),
                outcome: "dirty_tree".into(),
                detail: Some(format!(
                    "working tree has uncommitted changes:\n{}",
                    out.trim()
                )),
                pr_command: None,
            }
        }
        Err(err) => {
            return RevertRepoResult {
                repo_path: repo_path.display().to_string(),
                revert_branch: String::new(),
                reverted_commits: Vec::new(),
                outcome: "git_failed".into(),
                detail: Some(format!("git status failed: {err}")),
                pr_command: None,
            }
        }
    }

    // 2. List matching commits. `git log --all --grep=^Agent-Run: <id>$
    //    --extended-regexp --format=%H` returns newest-first.
    let grep = format!("^Agent-Run: {run_id}$");
    let commits = match git(
        repo_path,
        &[
            "log",
            "--all",
            "--extended-regexp",
            "--grep",
            grep.as_str(),
            "--format=%H",
        ],
    ) {
        Ok(out) => out
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect::<Vec<_>>(),
        Err(err) => {
            return RevertRepoResult {
                repo_path: repo_path.display().to_string(),
                revert_branch: String::new(),
                reverted_commits: Vec::new(),
                outcome: "git_failed".into(),
                detail: Some(format!("git log failed: {err}")),
                pr_command: None,
            }
        }
    };
    if commits.is_empty() {
        return RevertRepoResult {
            repo_path: repo_path.display().to_string(),
            revert_branch: String::new(),
            reverted_commits: Vec::new(),
            outcome: "no_matches".into(),
            detail: None,
            pr_command: None,
        };
    }

    let branch_name = revert_branch_name(run_id);
    if dry_run {
        return RevertRepoResult {
            repo_path: repo_path.display().to_string(),
            revert_branch: branch_name,
            reverted_commits: commits,
            outcome: "dry_run".into(),
            detail: Some("dry-run: nothing was written".into()),
            pr_command: None,
        };
    }

    // 3. Create the branch off HEAD (never `main`) and revert on it.
    // `--create` (not `--force-create`) refuses to clobber a branch from a
    // prior attempt; that refusal is reported distinctly so the operator
    // knows to inspect or delete it rather than re-running blindly.
    if let Err(err) = git(repo_path, &["switch", "--create", branch_name.as_str()]) {
        let already_exists = err.contains("already exists");
        return RevertRepoResult {
            repo_path: repo_path.display().to_string(),
            revert_branch: branch_name.clone(),
            reverted_commits: Vec::new(),
            outcome: if already_exists {
                "revert_branch_exists".into()
            } else {
                "git_failed".into()
            },
            detail: Some(if already_exists {
                format!(
                    "branch {branch_name} already exists from an earlier revert of this run; \
                     inspect it, then delete it to retry"
                )
            } else {
                format!("git switch --create failed: {err}")
            }),
            pr_command: None,
        };
    }
    let mut reverted = Vec::with_capacity(commits.len());
    for commit in &commits {
        if let Err(err) = git(repo_path, &["revert", "--no-edit", commit.as_str()]) {
            // A conflicting revert leaves an in-progress sequencer: index
            // full of conflict markers, and every later flk revert-run in
            // this repo refused by the dirty-tree gate with no clue why.
            // Abort so the operator gets the repo back on the revert
            // branch, clean, and say what happened and what to do.
            let aborted = git(repo_path, &["revert", "--abort"]).is_ok();
            let recovery = if aborted {
                "the in-progress revert was aborted, so the repo is clean; \
                 resolve the conflict by hand or revert the remaining commits individually"
            } else {
                "the in-progress revert could NOT be aborted — run `git revert --abort` \
                 in this repo before retrying"
            };
            return RevertRepoResult {
                repo_path: repo_path.display().to_string(),
                revert_branch: branch_name,
                reverted_commits: reverted,
                outcome: "revert_conflicted".into(),
                detail: Some(format!("git revert {commit} failed: {err}; {recovery}")),
                pr_command: None,
            };
        }
        reverted.push(commit.clone());
    }

    let pr_command = format!(
        "gh pr create --title \"revert: {run_id}\" --body \"Reverts {} commit(s) tagged with Agent-Run: {run_id}\" --head {branch_name}",
        reverted.len()
    );
    RevertRepoResult {
        repo_path: repo_path.display().to_string(),
        revert_branch: branch_name,
        reverted_commits: reverted,
        outcome: "ok".into(),
        detail: None,
        pr_command: Some(pr_command),
    }
}

/// The branch a revert lands on. Kept a function so tests can pin it.
pub(crate) fn revert_branch_name(run_id: &str) -> String {
    // Sanitize slashes and other suspect chars — a branch that contains
    // `..` or leading dashes would trip git's ref validation.
    let safe: String = run_id
        .chars()
        .map(|ch| match ch {
            '/' | ' ' | '\t' | '\n' => '-',
            _ => ch,
        })
        .collect();
    format!("revert/{safe}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake git shim: records every invocation and returns canned
    /// responses keyed by the argv prefix.
    struct Fake<'a> {
        clean: bool,
        commits: &'a [&'a str],
        fail_on: Option<&'a str>,
        switch_error: Option<&'a str>,
        calls: Vec<(PathBuf, Vec<String>)>,
    }

    impl<'a> Fake<'a> {
        fn new(clean: bool, commits: &'a [&'a str]) -> Self {
            Self {
                clean,
                commits,
                fail_on: None,
                switch_error: None,
                calls: Vec::new(),
            }
        }

        fn shim<'b>(
            &'b mut self,
        ) -> impl FnMut(&Path, &[&str]) -> Result<String, String> + use<'b, 'a> {
            move |path: &Path, args: &[&str]| -> Result<String, String> {
                self.calls.push((
                    path.to_path_buf(),
                    args.iter().map(|s| s.to_string()).collect(),
                ));
                if let Some(fail) = self.fail_on {
                    // `--abort` recovery must still succeed even when the
                    // revert itself is the simulated failure.
                    let is_abort = args == ["revert", "--abort"];
                    if args.contains(&fail) && !is_abort {
                        return Err(format!("simulated failure on {fail}"));
                    }
                }
                match args[0] {
                    "status" => Ok(if self.clean {
                        String::new()
                    } else {
                        " M src/foo.rs".into()
                    }),
                    "log" => Ok(self.commits.join("\n")),
                    "switch" => match self.switch_error {
                        Some(err) => Err(err.to_string()),
                        None => Ok(String::new()),
                    },
                    "revert" => Ok(String::new()),
                    other => panic!("unexpected git call: {other} {args:?}"),
                }
            }
        }
    }

    #[test]
    fn conflicting_revert_aborts_and_tells_the_operator() {
        // US-8: a conflicting revert must not leave the repo wedged with a
        // half-applied sequencer — every later revert-run would then hit
        // the dirty-tree refusal with no clue why.
        let mut fake = Fake::new(true, &["aaa111", "bbb222"]);
        fake.fail_on = Some("aaa111");
        let out = revert_one(&mut fake.shim(), Path::new("/tmp/repo"), "r-9", false);
        assert_eq!(out.outcome, "revert_conflicted");
        assert!(out.reverted_commits.is_empty());
        let detail = out.detail.unwrap_or_default();
        assert!(detail.contains("aborted"), "{detail}");
        assert!(
            fake.calls
                .iter()
                .any(|(_, args)| args == &["revert", "--abort"]),
            "abort must be attempted: {:?}",
            fake.calls
        );
        assert!(out.pr_command.is_none(), "no PR for a failed revert");
    }

    #[test]
    fn existing_revert_branch_is_reported_distinctly() {
        // `switch --create` refuses to clobber a prior attempt; the
        // operator needs to know that is what happened.
        let mut fake = Fake::new(true, &["aaa111"]);
        fake.switch_error = Some("fatal: a branch named 'revert/r-9' already exists");
        let out = revert_one(&mut fake.shim(), Path::new("/tmp/repo"), "r-9", false);
        assert_eq!(out.outcome, "revert_branch_exists");
        assert!(
            out.detail.unwrap_or_default().contains("already exists"),
            "operator must be told the branch survives from an earlier attempt"
        );
    }

    #[test]
    fn dirty_tree_refuses_without_touching_git() {
        let mut fake = Fake::new(false, &[]);
        let out = revert_one(&mut fake.shim(), Path::new("/tmp/repo"), "r-1", false);
        assert_eq!(out.outcome, "dirty_tree");
        assert!(out.reverted_commits.is_empty());
        assert!(out.revert_branch.is_empty());
        // Only status was called (never log/switch/revert).
        assert_eq!(fake.calls.len(), 1, "{:?}", fake.calls);
        assert_eq!(fake.calls[0].1[0], "status");
    }

    #[test]
    fn no_matches_returns_no_matches_and_creates_no_branch() {
        let mut fake = Fake::new(true, &[]);
        let out = revert_one(&mut fake.shim(), Path::new("/tmp/repo"), "r-2", false);
        assert_eq!(out.outcome, "no_matches");
        assert!(out.reverted_commits.is_empty());
        assert!(out.revert_branch.is_empty());
    }

    #[test]
    fn dry_run_lists_matches_but_never_writes_anything() {
        let mut fake = Fake::new(true, &["cafebabe", "deadbeef"]);
        let out = revert_one(&mut fake.shim(), Path::new("/tmp/repo"), "r-3", true);
        assert_eq!(out.outcome, "dry_run");
        assert_eq!(out.reverted_commits, vec!["cafebabe", "deadbeef"]);
        assert_eq!(out.revert_branch, "revert/r-3");
        // Only status + log — no switch/revert.
        let kinds: Vec<&String> = fake.calls.iter().map(|c| &c.1[0]).collect();
        assert!(kinds.iter().all(|k| *k == "status" || *k == "log"));
    }

    #[test]
    fn happy_path_creates_branch_and_reverts_newest_first() {
        let mut fake = Fake::new(true, &["c1_new", "c2_mid", "c3_old"]);
        let out = revert_one(&mut fake.shim(), Path::new("/tmp/repo"), "r-4", false);
        assert_eq!(out.outcome, "ok");
        assert_eq!(out.revert_branch, "revert/r-4");
        assert_eq!(out.reverted_commits, vec!["c1_new", "c2_mid", "c3_old"]);
        assert!(out
            .pr_command
            .as_deref()
            .unwrap()
            .contains("--head revert/r-4"));
        // Branch is created BEFORE any revert.
        let switch_idx = fake
            .calls
            .iter()
            .position(|c| c.1[0] == "switch")
            .expect("switch called");
        let first_revert = fake
            .calls
            .iter()
            .position(|c| c.1[0] == "revert")
            .expect("revert called");
        assert!(switch_idx < first_revert, "branch first, revert after");
    }

    #[test]
    fn revert_run_deduplicates_repo_list() {
        let run = RevertRun::new(
            "r".into(),
            true,
            vec!["/a".into(), "/b".into(), "/a".into()],
        );
        assert_eq!(run.repos.len(), 2);
        assert_eq!(run.repos[0].path, Path::new("/a"));
        assert_eq!(run.repos[1].path, Path::new("/b"));
    }

    #[test]
    fn revert_run_over_two_repos_only_reverts_the_one_with_matches() {
        // First repo: two matches. Second repo: none. Both scanned.
        let mut fake = Fake::new(true, &["c1", "c2"]);
        let repo_a = PathBuf::from("/tmp/a");
        let out_a = revert_one(&mut fake.shim(), &repo_a, "r", false);
        assert_eq!(out_a.outcome, "ok");
        assert_eq!(out_a.reverted_commits.len(), 2);

        let mut fake_b = Fake::new(true, &[]);
        let out_b = revert_one(&mut fake_b.shim(), Path::new("/tmp/b"), "r", false);
        assert_eq!(out_b.outcome, "no_matches");
    }

    #[test]
    fn revert_branch_name_sanitizes_run_id() {
        assert_eq!(revert_branch_name("fork:abc"), "revert/fork:abc");
        assert_eq!(revert_branch_name("has spaces"), "revert/has-spaces");
        assert_eq!(revert_branch_name("with/slash"), "revert/with-slash");
    }
}
