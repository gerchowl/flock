//! A repo's owner policy (ADR-0035 §3 and §6.2, slice O0, #1195): the
//! `[owner]` section of `.flk.toml` as committed on the default branch, and
//! the version of it the operator adopted.
//!
//! Where it is read (ADR-0004 decisions 4-5). Every git call goes through the
//! repo's common dir with `--git-dir`, so no worktree is consulted: neither
//! the file a worktree has checked out, nor its uncommitted edits, nor any
//! branch other than the default one. The default branch is the one
//! `origin/HEAD` names (`worktree::detect_default_branch`). Its remote-tracking
//! ref is read when it exists, because that is what was merged upstream as
//! of the last fetch, and the local branch only for a repo with no `origin`
//! copy of it. `policy_version` is the blob id of `.flk.toml` on that ref.
//!
//! Adoption (§6.2). A merged change applies nothing on its own. The policy
//! that applies is the blob the operator adopted last through
//! `owner.policy.adopt`, recorded as a durable `OwnerPolicyAdopted` event and
//! folded here. Until a repo has an adoption, nothing applies.
//!
//! Known limits:
//! - `origin/HEAD` is written at clone time and git never revises it. A repo
//!   whose default branch moved on GitHub reads the old branch until the
//!   operator runs `git remote set-head origin --auto`. `show` names the
//!   branch it read, so the operator can see it.
//! - A merged change is reported by `owner.policy.show` (`pending`), not yet
//!   by an operator notification (#1239).
//! - The repo key is the canonical git common dir, the key
//!   `AgentForked.parent_repo` already uses. The owner binding (O1a) joins on
//!   it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::schema::{EventData, EventEnvelope, EventKind};

/// The policy file, at the repo root of the default branch.
pub(crate) const POLICY_FILE: &str = ".flk.toml";

/// The `[owner]` keys this build reads. They are behaviour that grants
/// nothing (ADR-0035 §3): the wake class table and its windows (§2,
/// `[owner.wake]`) and the standup (§7a, `[owner.standup]`). Every other key
/// in `[owner]` is reported as ignored, never applied. The slices that give
/// a key its meaning add it here; `[[owner.rules]]` waits for O10, which
/// checks its actions at adoption.
pub(crate) const BEHAVIOUR_KEYS: &[&str] = &["standup", "wake"];

/// The adopted version of one repo's policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AdoptedPolicy {
    /// The blob id adopted, or null when the operator adopted a default
    /// branch with no `.flk.toml`.
    pub policy_version: Option<String>,
}

/// What `owner.policy.show` and `owner.policy.adopt` answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct OwnerPolicyReport {
    /// The repo key: its canonical git common dir.
    pub repo: String,
    pub default_branch: String,
    /// The ref the current version was read from.
    pub git_ref: String,
    /// The `.flk.toml` blob id on `git_ref` now, null when there is none.
    pub current: Option<String>,
    /// Absent until the operator first adopts a version for this repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted: Option<AdoptedPolicy>,
    /// The current version differs from the adopted one, so it does not
    /// apply until the operator adopts it.
    pub pending: bool,
    /// The behaviour keys that apply: the adopted version's, or none.
    pub policy: serde_json::Map<String, serde_json::Value>,
    /// `[owner]` keys in the current version that this build does not read.
    pub ignored_keys: Vec<String>,
    /// Why the current version cannot be read (not TOML, `owner` not a
    /// table). The adopted one still applies, and adoption is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_error: Option<String>,
    /// `adopt` only: whether this call recorded a new adoption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_now: Option<bool>,
}

/// Where a repo's policy is read, and its current version there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DefaultBranchPolicy {
    pub common_dir: PathBuf,
    pub default_branch: String,
    pub git_ref: String,
    pub current: Option<String>,
}

impl DefaultBranchPolicy {
    /// The repo key (see the module docs).
    pub(crate) fn repo_key(&self) -> String {
        self.common_dir.to_string_lossy().into_owned()
    }
}

/// Run git on the common dir alone, so no worktree is ever consulted.
fn git(common_dir: &Path, args: &[&str]) -> Result<String, String> {
    let git_dir = format!("--git-dir={}", common_dir.display());
    let full: Vec<&str> = std::iter::once(git_dir.as_str())
        .chain(args.iter().copied())
        .collect();
    crate::worktree::run_command_capture("git", &full, None).map_err(Into::into)
}

/// Find `repo`'s default branch and the `.flk.toml` blob committed on it.
pub(crate) fn locate(repo: &Path) -> Result<DefaultBranchPolicy, String> {
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let common = crate::worktree::run_command_capture("git", &args, Some(repo))
        .map_err(|err| format!("{} is not inside a git repo: {err}", repo.display()))?;
    let common_dir = std::fs::canonicalize(&common).map_err(|err| format!("{common}: {err}"))?;
    let default_branch = crate::worktree::detect_default_branch(repo).ok_or_else(|| {
        format!(
            "{} has no default branch: origin/HEAD is unset and there is no main or master; run `git remote set-head origin --auto`",
            repo.display()
        )
    })?;
    let remote = format!("refs/remotes/origin/{default_branch}");
    let git_ref = if git(&common_dir, &["rev-parse", "--verify", "--quiet", &remote]).is_ok() {
        remote
    } else {
        format!("refs/heads/{default_branch}")
    };
    let listing = git(&common_dir, &["ls-tree", &git_ref, "--", POLICY_FILE])
        .map_err(|err| format!("cannot read {git_ref}: {err}"))?;
    let current = match listing.split_whitespace().collect::<Vec<_>>()[..] {
        [] => None,
        [_, "blob", id, path] if path == POLICY_FILE => Some(id.to_string()),
        _ => {
            return Err(format!(
                "{POLICY_FILE} on {git_ref} is not a file: {listing}"
            ))
        }
    };
    Ok(DefaultBranchPolicy {
        common_dir,
        default_branch,
        git_ref,
        current,
    })
}

/// The `[owner]` behaviour keys in blob `version`, and the `[owner]` keys
/// this build ignores. No version means no file: nothing applies.
pub(crate) fn behaviour(
    common_dir: &Path,
    version: Option<&str>,
) -> Result<(serde_json::Map<String, serde_json::Value>, Vec<String>), String> {
    let mut policy = serde_json::Map::new();
    let mut ignored = Vec::new();
    let Some(version) = version else {
        return Ok((policy, ignored));
    };
    let text = git(common_dir, &["cat-file", "blob", version])
        .map_err(|err| format!("cannot read {POLICY_FILE} blob {version}: {err}"))?;
    let table: toml::Table = toml::from_str(&text)
        .map_err(|err| format!("{POLICY_FILE} blob {version} is not valid TOML: {err}"))?;
    let owner = match table.get("owner") {
        None => return Ok((policy, ignored)),
        Some(toml::Value::Table(owner)) => owner,
        Some(_) => {
            return Err(format!(
                "{POLICY_FILE} blob {version}: `owner` is not a table"
            ))
        }
    };
    for (key, value) in owner {
        if BEHAVIOUR_KEYS.contains(&key.as_str()) {
            let value = serde_json::to_value(value).map_err(|err| err.to_string())?;
            policy.insert(key.clone(), value);
        } else {
            ignored.push(key.clone());
        }
    }
    Ok((policy, ignored))
}

/// The adopted version per repo key, folded from `OwnerPolicyAdopted`.
#[derive(Debug, Default)]
pub(crate) struct OwnerPolicyIndex {
    adopted: BTreeMap<String, AdoptedPolicy>,
}

impl OwnerPolicyIndex {
    pub(crate) fn apply(&mut self, data: &EventData) {
        if let EventData::OwnerPolicyAdopted {
            repo,
            policy_version,
            ..
        } = data
        {
            self.adopted.insert(
                repo.clone(),
                AdoptedPolicy {
                    policy_version: policy_version.clone(),
                },
            );
        }
    }

    pub(crate) fn get(&self, repo: &str) -> Option<&AdoptedPolicy> {
        self.adopted.get(repo)
    }

    /// One `OwnerPolicyAdopted` per repo, so the adoption outlives rotation.
    pub(crate) fn reissue_events(&self) -> Vec<EventEnvelope> {
        self.adopted
            .iter()
            .map(|(repo, adopted)| EventEnvelope {
                event: EventKind::OwnerPolicyAdopted,
                data: EventData::OwnerPolicyAdopted {
                    repo: repo.clone(),
                    policy_version: adopted.policy_version.clone(),
                    reissued: true,
                },
            })
            .collect()
    }
}

/// The report for a repo: what is current on the default branch, what is
/// adopted, and the behaviour keys that apply.
pub(crate) fn report(
    located: &DefaultBranchPolicy,
    adopted: Option<&AdoptedPolicy>,
) -> Result<OwnerPolicyReport, String> {
    let (ignored_keys, current_error) =
        match behaviour(&located.common_dir, located.current.as_deref()) {
            Ok((_, ignored)) => (ignored, None),
            Err(err) => (Vec::new(), Some(err)),
        };
    let (policy, _) = match adopted {
        Some(adopted) => behaviour(&located.common_dir, adopted.policy_version.as_deref())?,
        None => Default::default(),
    };
    let pending = adopted.map_or(located.current.is_some(), |adopted| {
        adopted.policy_version != located.current
    });
    Ok(OwnerPolicyReport {
        repo: located.repo_key(),
        default_branch: located.default_branch.clone(),
        git_ref: located.git_ref.clone(),
        current: located.current.clone(),
        adopted: adopted.cloned(),
        pending,
        policy,
        ignored_keys,
        current_error,
        adopted_now: None,
    })
}

#[cfg(test)]
pub(crate) mod test_fixture {
    use std::path::{Path, PathBuf};

    use crate::test_support::{create_committed_repo, run_git, unique_temp_path};

    /// An upstream repo and an operator clone of it, whose `origin/HEAD`
    /// names the upstream's default branch.
    pub(crate) struct Fixture {
        pub upstream: PathBuf,
        pub clone: PathBuf,
    }

    impl Fixture {
        pub(crate) fn new(name: &str, policy: &str) -> Self {
            let upstream = create_committed_repo(&format!("{name}-upstream"));
            commit(&upstream, policy, "policy v1");
            let clone = unique_temp_path(&format!("{name}-clone"));
            run_git(
                upstream.parent().expect("temp dir"),
                &["clone", "--quiet", path(&upstream), path(&clone)],
            );
            run_git(&clone, &["config", "user.email", "flock@example.invalid"]);
            run_git(&clone, &["config", "user.name", "Flock Test"]);
            Self { upstream, clone }
        }

        /// Merge a new policy on the upstream default branch, then fetch it.
        pub(crate) fn merge(&self, policy: &str) {
            commit(&self.upstream, policy, "policy merged");
            run_git(&self.clone, &["fetch", "--quiet", "origin"]);
        }

        /// The blob id of the upstream default branch's `.flk.toml`.
        pub(crate) fn upstream_blob(&self) -> String {
            super::locate(&self.upstream)
                .expect("upstream policy")
                .current
                .expect("upstream has a policy")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.upstream);
            let _ = std::fs::remove_dir_all(&self.clone);
        }
    }

    pub(crate) fn commit(repo: &Path, policy: &str, message: &str) {
        std::fs::write(repo.join(super::POLICY_FILE), policy).expect("write policy");
        run_git(repo, &["add", super::POLICY_FILE]);
        run_git(repo, &["commit", "--quiet", "-m", message]);
    }

    pub(crate) fn path(path: &Path) -> &str {
        path.to_str().expect("utf-8 temp path")
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixture::{commit, path, Fixture};
    use super::*;
    use crate::test_support::{run_git, unique_temp_path};

    const V1: &str = "[owner]\nmax_delegates = 9\n\n[owner.standup]\ntime = \"09:00\"\n\n[worktrees]\nprotected_branches = [\"x\"]\n";
    const EDITED: &str = "[owner.standup]\ntime = \"03:00\"\n";

    /// ADR-0004 decision 4 / ADR-0035 §3: neither a worktree's edit, nor a
    /// commit on a PR branch, nor an unpushed commit on the local default
    /// branch, is the policy. Only what landed on the default branch is.
    #[test]
    fn a_policy_edited_in_a_worktree_or_on_a_pr_branch_is_not_read() {
        let fixture = Fixture::new("owner-policy-not-read", V1);
        let v1 = fixture.upstream_blob();
        assert_eq!(
            locate(&fixture.clone).unwrap().current.as_deref(),
            Some(v1.as_str())
        );

        // An uncommitted edit in the operator's own checkout.
        std::fs::write(fixture.clone.join(POLICY_FILE), EDITED).unwrap();
        // A linked worktree on a PR branch: edited, committed and pushed.
        let worktree = unique_temp_path("owner-policy-not-read-wt");
        run_git(
            &fixture.clone,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "pr/edit",
                path(&worktree),
            ],
        );
        commit(&worktree, EDITED, "edit policy on a PR branch");
        run_git(&worktree, &["push", "--quiet", "origin", "pr/edit"]);
        // Then an edit left uncommitted in the worktree on top of that.
        std::fs::write(worktree.join(POLICY_FILE), "[owner.wake]\nx = 1\n").unwrap();

        for from in [&fixture.clone, &worktree] {
            let located = locate(from).unwrap();
            assert_eq!(
                located.current.as_deref(),
                Some(v1.as_str()),
                "from {from:?}"
            );
            assert_eq!(
                located.repo_key(),
                locate(&fixture.clone).unwrap().repo_key()
            );
            assert!(
                located.git_ref.starts_with("refs/remotes/origin/"),
                "{located:?}"
            );
        }

        // An unpushed commit on the local default branch is not merged.
        run_git(&fixture.clone, &["checkout", "--quiet", "--", POLICY_FILE]);
        commit(&fixture.clone, EDITED, "local only");
        assert_eq!(
            locate(&worktree).unwrap().current.as_deref(),
            Some(v1.as_str())
        );

        let _ = std::fs::remove_dir_all(&worktree);
    }

    #[test]
    fn only_the_owner_behaviour_keys_are_read() {
        let fixture = Fixture::new("owner-policy-keys", V1);
        let located = locate(&fixture.clone).unwrap();
        let (policy, ignored) = behaviour(&located.common_dir, located.current.as_deref()).unwrap();
        assert_eq!(
            serde_json::Value::Object(policy),
            serde_json::json!({"standup": {"time": "09:00"}})
        );
        assert_eq!(ignored, ["max_delegates"]);
        assert_eq!(
            behaviour(&located.common_dir, None).unwrap(),
            Default::default()
        );
    }

    #[test]
    fn a_repo_without_a_policy_file_reports_none() {
        let fixture = Fixture::new("owner-policy-none", V1);
        run_git(&fixture.upstream, &["rm", "--quiet", POLICY_FILE]);
        run_git(
            &fixture.upstream,
            &["commit", "--quiet", "-m", "drop policy"],
        );
        run_git(&fixture.clone, &["fetch", "--quiet", "origin"]);
        assert_eq!(locate(&fixture.clone).unwrap().current, None);
    }

    #[test]
    fn a_path_outside_any_repo_is_unreadable() {
        let dir = unique_temp_path("owner-policy-no-repo");
        std::fs::create_dir_all(&dir).unwrap();
        let err = locate(&dir).unwrap_err();
        assert!(err.contains("not inside a git repo"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
