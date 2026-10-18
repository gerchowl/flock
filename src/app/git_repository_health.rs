//! Per-repository failures preserve healthy refresh progress (ADR-0007, #2034).
use super::{runtime::WorkspaceGitRefreshTarget, App};
use crate::{health::GitRefreshErrorKind, workspace::WorkspaceGitStatus};
use std::{collections::HashMap, path::PathBuf};

/// An owned probe's failure. Targets retain their observed cwd to reject stale completions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepositoryFailure {
    pub cache_key: PathBuf,
    pub targets: Vec<WorkspaceGitRefreshTarget>,
    pub error: GitRefreshErrorKind,
}

impl App {
    /// Apply only to still-current workspace/cwd pairs. Successful repositories
    /// clear their own streak, never another repository's failure or stale flag.
    pub(crate) fn apply_git_repository_outcome(
        &mut self,
        results: &[WorkspaceGitStatus],
        failures: Vec<RepositoryFailure>,
    ) -> bool {
        let live: HashMap<_, _> = self
            .state
            .workspaces
            .iter()
            .map(|ws| {
                (
                    ws.id.clone(),
                    ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes),
                )
            })
            .collect();
        let current = |target: &WorkspaceGitRefreshTarget| {
            live.get(&target.workspace_id).and_then(Option::as_ref)
                == Some(&target.resolved_identity_cwd)
        };
        self.git_repository_failures.retain(|_, (failure, _)| {
            failure.targets.retain(&current);
            !failure.targets.is_empty()
                && !results.iter().any(|result| {
                    failure.targets.iter().any(|target| {
                        target.workspace_id == result.workspace_id
                            && target.resolved_identity_cwd == result.resolved_identity_cwd
                    })
                })
        });
        for mut failure in failures {
            failure.targets.retain(&current);
            if failure.targets.is_empty() {
                continue;
            }
            let count = self
                .git_repository_failures
                .get(&failure.cache_key)
                .map_or(1, |(_, count)| count.saturating_add(1));
            self.git_repository_failures
                .insert(failure.cache_key.clone(), (failure, count));
        }
        let mut changed = false;
        for ws in &mut self.state.workspaces {
            let failed = self.git_repository_failures.values().any(|(failure, _)| {
                failure
                    .targets
                    .iter()
                    .any(|target| target.workspace_id == ws.id)
            });
            // Retained failures were pruned against current workspace/cwd pairs.
            // An old flag cannot outlive the cwd that earned it.
            let stale = failed;
            changed |= stale != ws.git_status_stale;
            ws.git_status_stale = stale;
        }
        changed
    }

    /// Stable order, content-free error codes, and basename-only repository labels.
    pub(crate) fn git_repository_health_rows(
        &self,
    ) -> Vec<crate::api::schema::GitRepositoryHealth> {
        let mut rows: Vec<_> = self
            .git_repository_failures
            .values()
            .filter_map(|(failure, count)| {
                let mut workspace_ids: Vec<_> = failure
                    .targets
                    .iter()
                    .filter(|t| {
                        self.state.workspaces.iter().any(|ws| {
                            ws.id == t.workspace_id
                                && ws
                                    .resolved_identity_cwd_from(
                                        &self.state.terminals,
                                        &self.terminal_runtimes,
                                    )
                                    .as_ref()
                                    == Some(&t.resolved_identity_cwd)
                        })
                    })
                    .map(|t| t.workspace_id.clone())
                    .collect();
                if workspace_ids.is_empty() {
                    return None;
                }
                workspace_ids.sort();
                Some(crate::api::schema::GitRepositoryHealth {
                    repo: failure
                        .cache_key
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .escape_default()
                        .to_string(),
                    workspace_ids,
                    consecutive_failures: *count,
                    last_error: failure.error.as_str().into(),
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            a.repo
                .cmp(&b.repo)
                .then_with(|| a.workspace_ids.cmp(&b.workspace_ids))
        });
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (App, RepositoryFailure) {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Default::default(), true, None, rx, Default::default());
        let mut ws = crate::workspace::Workspace::test_new("repository-health-fixture");
        ws.tabs.clear();
        let failure = RepositoryFailure {
            cache_key: ws.identity_cwd.clone(),
            targets: vec![WorkspaceGitRefreshTarget {
                workspace_id: ws.id.clone(),
                resolved_identity_cwd: ws.identity_cwd.clone(),
            }],
            error: GitRefreshErrorKind::GitExit,
        };
        app.state.workspaces.push(ws);
        (app, failure)
    }

    #[tokio::test]
    async fn git2100_moved_cwd_clears_previously_stale_flag_without_a_result() {
        let (mut app, failure) = fixture();
        assert!(app.apply_git_repository_outcome(&[], vec![failure]));
        assert!(app.state.workspaces[0].git_status_stale);
        let moved = crate::test_support::TempDir::socket();
        app.state.workspaces[0].identity_cwd = moved.path().to_owned();
        assert!(app.apply_git_repository_outcome(&[], vec![]));
        assert!(!app.state.workspaces[0].git_status_stale);
        assert!(app.git_repository_failures.is_empty());
    }

    #[tokio::test]
    async fn git2034_repository_diagnostic_escapes_terminal_control_text() {
        let (mut app, mut failure) = fixture();
        failure
            .cache_key
            .set_file_name("repo\u{9d}52;c;fixture\u{9c}\u{202e}");
        app.apply_git_repository_outcome(&[], vec![failure]);
        let row = app.git_repository_health_rows().remove(0);
        assert!(row.repo.is_ascii());
        assert!(row.repo.contains("\\u{9d}"));
        assert!(row.repo.contains("\\u{202e}"));
    }

    #[tokio::test]
    async fn git2034_moved_workspace_ignores_old_repository_failure() {
        let (mut app, failure) = fixture();
        let moved = crate::test_support::TempDir::socket();
        app.state.workspaces[0].identity_cwd = moved.path().to_owned();
        assert!(!app.apply_git_repository_outcome(&[], vec![failure]));
        assert!(!app.state.workspaces[0].git_status_stale);
        assert!(app.git_repository_failures.is_empty());
    }

    #[tokio::test]
    async fn git2034_removed_workspace_has_no_repository_diagnostic() {
        let (mut app, failure) = fixture();
        assert!(app.apply_git_repository_outcome(&[], vec![failure]));
        assert_eq!(app.git_repository_health_rows().len(), 1);
        app.state.workspaces.clear();
        assert!(app.git_repository_health_rows().is_empty());
    }
}
