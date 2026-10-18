use std::path::{Path, PathBuf};

use super::probe;
use crate::health::GitRefreshErrorKind as Error;
use crate::workspace::WorkspaceGitStatusSnapshot;
use std::time::Instant;

use super::{
    config::{read_branch_config, upstream_full_ref},
    discovery::{
        canonicalize_best_effort_path, git_ref_storage_is_reftable, git_rev_parse_verify,
        git_space_metadata, git_symbolic_head_full, git_worktree_info, read_ref_oid,
        GitWorktreeInfo,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusCacheEntry {
    pub fingerprint: GitStatusFingerprint,
    pub snapshot: WorkspaceGitStatusSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusFingerprint {
    pub git_dir: PathBuf,
    pub git_common_dir: PathBuf,
    pub head: GitHeadIdentity,
    pub upstream: Option<GitUpstreamIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHeadIdentity {
    Branch {
        full_ref: String,
        short_name: String,
        oid: Option<String>,
    },
    Detached {
        oid: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitUpstreamIdentity {
    pub remote: String,
    pub merge_ref: String,
    pub full_ref: String,
    pub oid: Option<String>,
}

pub fn git_status_cache_key(cwd: &Path) -> Option<PathBuf> {
    git_worktree_info(cwd).map(|info| canonicalize_best_effort_path(&info.repo_root))
}

/// Optional-ref absence is remembered only within one owned refresh round.
/// Linked checkouts share ref names through their common directory, but HEAD
/// belongs to one worktree. Errors and positive OIDs are never memoized here.
/// A ref created during the round is observed on the next round.
#[derive(Default)]
pub(crate) struct GitRefreshRound {
    absent: std::collections::HashSet<(PathBuf, String)>,
}

impl GitRefreshRound {
    fn revision(
        &mut self,
        info: &GitWorktreeInfo,
        revision: &str,
        deadline: Instant,
    ) -> Result<Option<String>, Error> {
        let directory = if revision == "HEAD" {
            &info.git_dir
        } else {
            &info.git_common_dir
        };
        let key = (
            canonicalize_best_effort_path(directory),
            revision.to_owned(),
        );
        if self.absent.contains(&key) {
            return Ok(None);
        }
        let oid = git_rev_parse_verify(&info.repo_root, revision, deadline)?;
        if oid.is_none() {
            self.absent.insert(key);
        }
        Ok(oid)
    }
}

#[cfg(test)]
pub(crate) fn git_status_snapshot_for_cwd(
    cwd: &Path,
    cached: Option<&GitStatusCacheEntry>,
    deadline: Instant,
) -> Result<(WorkspaceGitStatusSnapshot, Option<GitStatusCacheEntry>), Error> {
    git_status_snapshot_in_round(cwd, cached, deadline, &mut GitRefreshRound::default())
}

/// Read one snapshot without caching a failed command. Every git subprocess
/// shares the round deadline as well as its own ten-second ceiling.
pub(crate) fn git_status_snapshot_in_round(
    cwd: &Path,
    cached: Option<&GitStatusCacheEntry>,
    deadline: Instant,
    round: &mut GitRefreshRound,
) -> Result<(WorkspaceGitStatusSnapshot, Option<GitStatusCacheEntry>), Error> {
    let space = git_space_metadata(cwd);
    let Some(fingerprint) = git_status_fingerprint_in_round(cwd, deadline, round)? else {
        return Ok((
            WorkspaceGitStatusSnapshot {
                branch: super::discovery::git_branch_with_deadline(cwd, deadline)?,
                ahead_behind: None,
                space,
            },
            None,
        ));
    };
    let branch = fingerprint.branch_name().map(str::to_string);

    if let Some(cached) = cached.filter(|entry| entry.fingerprint == fingerprint) {
        let snapshot = WorkspaceGitStatusSnapshot {
            branch,
            ahead_behind: cached.snapshot.ahead_behind,
            space,
        };
        return Ok((
            snapshot.clone(),
            Some(GitStatusCacheEntry {
                fingerprint,
                snapshot,
            }),
        ));
    }

    let ahead_behind = fingerprint
        .head_oid()
        .zip(fingerprint.upstream_oid())
        .map(|(head_oid, upstream_oid)| {
            git_ahead_behind_between(cwd, head_oid, upstream_oid, deadline)
        })
        .transpose()?;
    let snapshot = WorkspaceGitStatusSnapshot {
        branch,
        ahead_behind,
        space,
    };
    Ok((
        snapshot.clone(),
        Some(GitStatusCacheEntry {
            fingerprint,
            snapshot,
        }),
    ))
}

#[cfg(test)]
pub(super) fn git_status_fingerprint(
    cwd: &Path,
    deadline: Instant,
) -> Result<Option<GitStatusFingerprint>, Error> {
    git_status_fingerprint_in_round(cwd, deadline, &mut GitRefreshRound::default())
}

fn git_status_fingerprint_in_round(
    cwd: &Path,
    deadline: Instant,
    round: &mut GitRefreshRound,
) -> Result<Option<GitStatusFingerprint>, Error> {
    if Instant::now() >= deadline {
        return Err(Error::Timeout);
    }
    let Some(info) = git_worktree_info(cwd) else {
        return Ok(None);
    };
    let Some(head) = read_head_identity(&info, deadline, round)? else {
        return Ok(None);
    };
    let upstream = match &head {
        GitHeadIdentity::Branch { short_name, .. } => {
            read_upstream_identity(&info, short_name, deadline, round)?
        }
        GitHeadIdentity::Detached { .. } => None,
    };

    Ok(Some(GitStatusFingerprint {
        git_dir: canonicalize_best_effort_path(&info.git_dir),
        git_common_dir: canonicalize_best_effort_path(&info.git_common_dir),
        head,
        upstream,
    }))
}

impl GitStatusFingerprint {
    fn branch_name(&self) -> Option<&str> {
        match &self.head {
            GitHeadIdentity::Branch { short_name, .. } => Some(short_name.as_str()),
            GitHeadIdentity::Detached { .. } => None,
        }
    }

    fn head_oid(&self) -> Option<&str> {
        match &self.head {
            GitHeadIdentity::Branch { oid, .. } => oid.as_deref(),
            GitHeadIdentity::Detached { oid } => Some(oid.as_str()),
        }
    }

    fn upstream_oid(&self) -> Option<&str> {
        self.upstream
            .as_ref()
            .and_then(|upstream| upstream.oid.as_deref())
    }
}

fn read_head_identity(
    info: &GitWorktreeInfo,
    deadline: Instant,
    round: &mut GitRefreshRound,
) -> Result<Option<GitHeadIdentity>, Error> {
    if !git_ref_storage_is_reftable(&info.git_common_dir) {
        return Ok(read_head_identity_from_files(info));
    }
    if let Some(full_ref) = git_symbolic_head_full(&info.repo_root, deadline)? {
        let Some(short_name) = full_ref.strip_prefix("refs/heads/").map(str::to_owned) else {
            return Ok(None);
        };
        let oid = round.revision(info, &full_ref, deadline)?;
        return Ok(Some(GitHeadIdentity::Branch {
            full_ref,
            short_name,
            oid,
        }));
    }
    Ok(round
        .revision(info, "HEAD", deadline)?
        .map(|oid| GitHeadIdentity::Detached { oid }))
}

fn read_head_identity_from_files(info: &GitWorktreeInfo) -> Option<GitHeadIdentity> {
    let head = std::fs::read_to_string(info.git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(full_ref) = head.strip_prefix("ref: ") {
        let short_name = full_ref.strip_prefix("refs/heads/")?.to_string();
        let oid = read_ref_oid(&info.git_common_dir, full_ref);
        return Some(GitHeadIdentity::Branch {
            full_ref: full_ref.to_string(),
            short_name,
            oid,
        });
    }

    (!head.is_empty()).then(|| GitHeadIdentity::Detached {
        oid: head.to_string(),
    })
}

fn read_upstream_identity(
    info: &GitWorktreeInfo,
    branch: &str,
    deadline: Instant,
    round: &mut GitRefreshRound,
) -> Result<Option<GitUpstreamIdentity>, Error> {
    let Some(config) = read_branch_config(info, branch) else {
        return Ok(None);
    };
    let Some(full_ref) = upstream_full_ref(&config) else {
        return Ok(None);
    };
    let oid = if git_ref_storage_is_reftable(&info.git_common_dir) {
        round.revision(info, &full_ref, deadline)?
    } else {
        read_ref_oid(&info.git_common_dir, &full_ref)
    };
    Ok(Some(GitUpstreamIdentity {
        remote: config.remote,
        merge_ref: config.merge_ref,
        full_ref,
        oid,
    }))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test-only helper: TracedCommand polices product code (logging redesign PR-3).
pub(crate) fn git_ahead_behind(cwd: &Path) -> Option<(usize, usize)> {
    super::discovery::git_repo_root(cwd)?;

    let output = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .arg("-C")
        .arg(cwd)
        .args(["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    parse_git_ahead_behind_output(&stdout)
}

fn git_ahead_behind_between(
    cwd: &Path,
    head_oid: &str,
    upstream_oid: &str,
    deadline: Instant,
) -> Result<(usize, usize), Error> {
    let range = format!("{head_oid}...{upstream_oid}");
    let stdout = probe::text(
        cwd,
        &["rev-list", "--left-right", "--count", &range],
        None,
        deadline,
    )?
    .ok_or(Error::GitOutput)?;
    parse_git_ahead_behind_output(&stdout).ok_or(Error::GitOutput)
}

fn parse_git_ahead_behind_output(stdout: &str) -> Option<(usize, usize)> {
    let mut parts = stdout.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((ahead, behind))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Tests exec real git to prime fixtures — TracedCommand polices product code (logging redesign PR-3).
mod tests {
    use super::*;
    fn deadline() -> Instant {
        Instant::now() + std::time::Duration::from_secs(60)
    }
    use crate::workspace::git::test_support::{run_git, temp_test_dir, write_fake_tracked_repo};

    #[test]
    fn git_status_cache_key_ignores_invalid_git_marker() {
        let base = temp_test_dir("invalid-git-root");
        let cwd = base.join("workspace");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        assert_eq!(git_status_cache_key(&cwd), None);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_status_reuses_cached_ahead_behind_when_fingerprint_matches() {
        let root = temp_test_dir("cache-hit");
        write_fake_tracked_repo(&root);
        let fingerprint = git_status_fingerprint(&root, deadline()).unwrap().unwrap();
        let cached = GitStatusCacheEntry {
            fingerprint,
            snapshot: WorkspaceGitStatusSnapshot {
                branch: Some("main".into()),
                ahead_behind: Some((2, 1)),
                space: git_space_metadata(&root),
            },
        };

        let (snapshot, update) =
            git_status_snapshot_for_cwd(&root, Some(&cached), deadline()).unwrap();

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.ahead_behind, Some((2, 1)));
        assert_eq!(update.unwrap().snapshot.ahead_behind, Some((2, 1)));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_status_does_not_reuse_cache_when_branch_changes_at_same_oid() {
        let root = temp_test_dir("branch-switch");
        write_fake_tracked_repo(&root);
        let fingerprint = git_status_fingerprint(&root, deadline()).unwrap().unwrap();
        let cached = GitStatusCacheEntry {
            fingerprint,
            snapshot: WorkspaceGitStatusSnapshot {
                branch: Some("main".into()),
                ahead_behind: Some((4, 0)),
                space: git_space_metadata(&root),
            },
        };
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(
            root.join(".git/refs/heads/feature"),
            "1111111111111111111111111111111111111111\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".git/config"),
            "[branch \"feature\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .unwrap();

        let fingerprint = git_status_fingerprint(&root, deadline()).unwrap().unwrap();
        assert_eq!(fingerprint.branch_name(), Some("feature"));
        // The changed fingerprint must run git rather than reuse the cached
        // counts. Fixture OIDs are deliberately absent from the object store.
        assert_eq!(
            git_status_snapshot_for_cwd(&root, Some(&cached), deadline()),
            Err(Error::GitExit)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_status_clears_ahead_behind_when_upstream_is_unset() {
        let root = temp_test_dir("upstream-unset");
        write_fake_tracked_repo(&root);
        let fingerprint = git_status_fingerprint(&root, deadline()).unwrap().unwrap();
        let cached = GitStatusCacheEntry {
            fingerprint,
            snapshot: WorkspaceGitStatusSnapshot {
                branch: Some("main".into()),
                ahead_behind: Some((0, 3)),
                space: git_space_metadata(&root),
            },
        };
        std::fs::write(root.join(".git/config"), "").unwrap();

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, Some(&cached), deadline()).unwrap();

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.ahead_behind, None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_status_fingerprint_reads_packed_refs() {
        let root = temp_test_dir("packed-refs");
        write_fake_tracked_repo(&root);
        std::fs::remove_file(root.join(".git/refs/remotes/origin/main")).unwrap();
        std::fs::write(
            root.join(".git/packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted\n2222222222222222222222222222222222222222 refs/remotes/origin/main\n",
        )
        .unwrap();

        let fingerprint = git_status_fingerprint(&root, deadline()).unwrap().unwrap();

        assert_eq!(
            fingerprint.upstream.unwrap().oid.as_deref(),
            Some("2222222222222222222222222222222222222222")
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_status_cache_key_is_per_linked_worktree_checkout() {
        let base = temp_test_dir("linked-worktree-keys");
        let common_dir = base.join("repo/.git");
        let worktree_one = base.join("one");
        let worktree_two = base.join("two");
        let git_dir_one = common_dir.join("worktrees/one");
        let git_dir_two = common_dir.join("worktrees/two");
        std::fs::create_dir_all(&git_dir_one).unwrap();
        std::fs::create_dir_all(&git_dir_two).unwrap();
        std::fs::create_dir_all(&worktree_one).unwrap();
        std::fs::create_dir_all(&worktree_two).unwrap();
        std::fs::write(
            worktree_one.join(".git"),
            format!("gitdir: {}\n", git_dir_one.display()),
        )
        .unwrap();
        std::fs::write(
            worktree_two.join(".git"),
            format!("gitdir: {}\n", git_dir_two.display()),
        )
        .unwrap();
        std::fs::write(git_dir_one.join("HEAD"), "ref: refs/heads/one\n").unwrap();
        std::fs::write(git_dir_two.join("HEAD"), "ref: refs/heads/two\n").unwrap();

        assert_ne!(
            git_status_cache_key(&worktree_one),
            git_status_cache_key(&worktree_two)
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    fn branch_error_fixture(detached: bool) {
        use crate::workspace::git::test_support::CountingGit;
        let root = crate::test_support::TempDir::socket();
        run_git(
            root.path(),
            &["init", "-q", "--ref-format=reftable", "-b", "main"],
        );
        run_git(
            root.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        );
        if detached {
            run_git(root.path(), &["checkout", "--detach", "-q"]);
        } else {
            run_git(root.path(), &["symbolic-ref", "HEAD", "refs/tags/fixture"]);
        }
        let fault = if detached {
            "if [ \"$3\" = symbolic-ref ]; then exit 128; fi"
        } else {
            "if [ \"$3\" = symbolic-ref ] && [ \"$5\" = --short ]; then exit 128; fi"
        };
        let counter = CountingGit::scripted(fault);
        assert_eq!(
            git_status_snapshot_for_cwd(root.path(), None, deadline()),
            Err(Error::GitExit)
        );
        drop(counter);
        let (snapshot, cache) = git_status_snapshot_for_cwd(root.path(), None, deadline()).unwrap();
        assert_eq!(
            snapshot.branch.as_deref(),
            if detached { None } else { Some("fixture") }
        );
        assert_eq!(cache.is_some(), detached);
    }

    #[test]
    fn git2100_symbolic_head_outside_heads_preserves_typed_fallback_error() {
        branch_error_fixture(false);
    }

    #[test]
    fn git2100_detached_head_preserves_typed_probe_error() {
        branch_error_fixture(true);
    }

    #[test]
    fn git2034_non_branch_reftable_head_keeps_legacy_branch_fallback() {
        let root = crate::test_support::TempDir::socket();
        run_git(
            root.path(),
            &["init", "-q", "--ref-format=reftable", "-b", "main"],
        );
        run_git(
            root.path(),
            &["symbolic-ref", "HEAD", "refs/tags/unborn-fixture"],
        );
        let expected = super::super::discovery::git_branch(root.path());
        assert_eq!(expected.as_deref(), Some("unborn-fixture"));
        let (snapshot, cache) = git_status_snapshot_for_cwd(root.path(), None, deadline()).unwrap();
        assert_eq!(snapshot.branch, expected);
        assert!(snapshot.ahead_behind.is_none());
        assert!(cache.is_none());
    }

    #[test]
    fn git_status_fingerprint_reads_reftable_branch_identity() {
        let root = temp_test_dir("reftable-fingerprint");
        let root_arg = root.to_string_lossy().to_string();
        let output = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(["init", "--ref-format=reftable", "-b", "main", &root_arg])
            .output()
            .unwrap();
        if !output.status.success() {
            std::fs::remove_dir_all(root).unwrap();
            return;
        }
        run_git(&root, &["config", "user.email", "flock@example.invalid"]);
        run_git(&root, &["config", "user.name", "Flock Test"]);
        run_git(&root, &["commit", "--allow-empty", "-m", "initial"]);

        let fingerprint = git_status_fingerprint(&root, deadline()).unwrap().unwrap();

        assert_eq!(
            fingerprint.head,
            GitHeadIdentity::Branch {
                full_ref: "refs/heads/main".into(),
                short_name: "main".into(),
                oid: git_rev_parse_verify(&root, "HEAD", deadline()).unwrap(),
            }
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_status_recomputes_ahead_behind_when_head_moves() {
        let base = temp_test_dir("head-moves");
        let remote = base.join("remote.git");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let remote_arg = remote.to_string_lossy().to_string();
        run_git(&base, &["init", "--bare", &remote_arg]);
        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.email", "flock@example.invalid"]);
        run_git(&repo, &["config", "user.name", "Flock Test"]);
        run_git(&repo, &["commit", "--allow-empty", "-m", "initial"]);
        run_git(&repo, &["branch", "-M", "main"]);
        run_git(&repo, &["remote", "add", "origin", &remote_arg]);
        run_git(&repo, &["push", "-u", "origin", "main"]);

        let (initial, cache_entry) = git_status_snapshot_for_cwd(&repo, None, deadline()).unwrap();
        assert_eq!(initial.ahead_behind, Some((0, 0)));
        run_git(&repo, &["commit", "--allow-empty", "-m", "ahead"]);

        let (updated, _) =
            git_status_snapshot_for_cwd(&repo, cache_entry.as_ref(), deadline()).unwrap();

        assert_eq!(updated.branch.as_deref(), Some("main"));
        assert_eq!(updated.ahead_behind, Some((1, 0)));

        std::fs::remove_dir_all(base).unwrap();
    }
}
