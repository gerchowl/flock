//! Failure fixtures drive the actual worker dispatch and public summary route.
use super::*;
use std::path::PathBuf;
use std::sync::mpsc;

fn app() -> App {
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(&Default::default(), true, None, rx, Default::default());
    app.state
        .workspaces
        .push(Workspace::test_new("git-failure"));
    app.git_refresh_health.mark_success(Instant::now());
    app.checks_heartbeat_deadline = None;
    app
}

fn summary(app: &mut App) -> serde_json::Value {
    let response = app.handle_api_request(crate::api::schema::Request {
        id: "git-failure".into(),
        method: crate::api::schema::Method::PeersSummary(crate::api::schema::EmptyParams {}),
    });
    serde_json::from_str::<serde_json::Value>(&response).unwrap()["result"]["pollers"]
        ["git_refresh"]
        .clone()
}

fn successful_output(workspace: &Workspace) -> WorkspaceGitRefreshOutput {
    WorkspaceGitRefreshOutput {
        error: None,
        results: vec![WorkspaceGitStatus {
            workspace_id: workspace.id.clone(),
            resolved_identity_cwd: workspace.identity_cwd.clone(),
            branch: Some("refreshed-fixture".into()),
            ahead_behind: None,
            space: None,
        }],
        failures: vec![],
        cache_updates: vec![],
    }
}

/// Joining the producer makes a lost completion an immediate failure, not a hang.
async fn receive(app: &mut App, finished: tokio::sync::oneshot::Receiver<()>) -> AppEvent {
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            event = git_completion(app) => event,
            _ = finished => loop {
                let event = app.event_rx.try_recv().expect("worker finished without an outcome");
                if matches!(event, AppEvent::GitStatusRefreshed { .. }) { break event; }
            },
        }
    })
    .await
    .expect("live worker never completed")
}

fn tracked_spawn(
    finished: tokio::sync::oneshot::Sender<()>,
) -> impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
    move |job| {
        let worker = std::thread::spawn(job);
        std::thread::spawn(move || {
            let _ = worker.join();
            let _ = finished.send(());
        });
        Ok(())
    }
}

#[tokio::test]
async fn git_refresh_worker_panic_is_a_failure_outcome() {
    let mut app = app();
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dispatch_git_refresh(
        Instant::now(),
        || panic!("fixture worker panic"),
        tracked_spawn(tx),
    );
    let event = receive(&mut app, rx).await;
    app.handle_internal_event(event);
    let row = summary(&mut app);
    assert_eq!(row["consecutive_failures"], 1);
    assert_eq!(row["last_error"], "worker_panic");
    assert_eq!(row["status"], "broken");
    assert!(!app.git_refresh_in_flight);
}

#[tokio::test]
async fn git_refresh_worker_spawn_failure_is_reported_without_panicking() {
    let mut app = app();
    let now = Instant::now();
    app.dispatch_git_refresh(
        now,
        || panic!("must not run"),
        |_| Err(std::io::Error::other("private fixture error")),
    );
    let row = summary(&mut app);
    assert_eq!(row["consecutive_failures"], 1);
    assert_eq!(row["last_error"], "worker_spawn");
    assert_eq!(row["status"], "broken");
    assert!(!app.git_refresh_in_flight);
    assert_eq!(
        app.git_refresh_deadline(),
        Some(now + GIT_REMOTE_STATUS_REFRESH_INTERVAL)
    );
    assert!(!row.to_string().contains("private fixture error"));
}

/// Always releases this test's producer, including when a watchdog assertion fails.
struct Release(Option<mpsc::Sender<()>>);
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[tokio::test]
async fn git_refresh_watchdog_counts_once_retains_ownership_and_recovers() {
    let mut app = app();
    app.state.workspaces[0].tabs.clear();
    let output = successful_output(&app.state.workspaces[0]);
    let start = Instant::now();
    let (release_tx, release_rx) = mpsc::channel();
    let release = Release(Some(release_tx));
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dispatch_git_refresh(
        start,
        move || {
            release_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("fixture was never released");
            output
        },
        tracked_spawn(tx),
    );
    let bound = start + Duration::from_secs(crate::health::GIT_REFRESH_MAX_ROUND_SECS);
    assert_eq!(app.git_refresh_deadline(), Some(bound));
    assert_eq!(
        app.next_headless_loop_deadline_with_git_refresh(start, false, false),
        Some(bound)
    );
    app.start_git_status_refresh_if_due(bound);
    assert_eq!(summary(&mut app)["last_error"], "round_stalled");
    assert_eq!(app.git_refresh_health.consecutive_failures, 1);
    assert!(app.git_refresh_in_flight);
    assert_eq!(app.git_refresh_health.in_flight_since, Some(start));
    app.start_git_status_refresh_if_due(bound + Duration::from_secs(1));
    assert_eq!(app.git_refresh_health.consecutive_failures, 1);
    assert_eq!(app.git_refresh_deadline(), None);
    drop(release);
    let event = receive(&mut app, rx).await;
    app.handle_internal_event(event);
    assert!(!app.git_refresh_in_flight);
    assert_eq!(app.git_refresh_health.consecutive_failures, 0);
    assert_eq!(summary(&mut app)["status"], "ok");
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("refreshed-fixture")
    );
}

/// Each nextest test has its own process. Keep PATH scoped until its worker has
/// finished all subprocess work, so the fixture cannot affect another test.
struct GitFixture {
    root: crate::test_support::TempDir,
    path: Option<std::ffi::OsString>,
}
impl GitFixture {
    fn new(script: Option<&str>) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::test_support::TempDir::socket();
        let git = root.path().join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::create_dir_all(git.join("refs/remotes/origin")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git.join("refs/heads/main"),
            "1111111111111111111111111111111111111111\n",
        )
        .unwrap();
        std::fs::write(
            git.join("refs/remotes/origin/main"),
            "2222222222222222222222222222222222222222\n",
        )
        .unwrap();
        std::fs::write(
            git.join("config"),
            "[branch \"main\"]\nremote = origin\nmerge = refs/heads/main\n",
        )
        .unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        if let Some(script) = script {
            let file = bin.join("git");
            std::fs::write(&file, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = std::env::var_os("PATH");
        std::env::set_var("PATH", bin);
        Self { root, path }
    }
    fn cwd(&self) -> PathBuf {
        self.root.path().to_owned()
    }
}
impl Drop for GitFixture {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
    }
}

// App initialization also owns unrelated background tasks on this shared
// channel. Only the git worker's completion is the outcome of these fixtures.
async fn git_completion(app: &mut App) -> AppEvent {
    loop {
        let event = app.event_rx.recv().await.expect("worker channel closed");
        if matches!(event, AppEvent::GitStatusRefreshed { .. }) {
            return event;
        }
    }
}

async fn failed_probe(script: Option<&str>, code: &str) {
    let mut app = app();
    let fixture = GitFixture::new(script);
    app.state.workspaces[0].identity_cwd = fixture.cwd();
    app.state.workspaces[0].tabs.clear();
    let now = Instant::now();
    app.mark_git_status_refresh_due(now);
    app.start_git_status_refresh_if_due(now);
    let event = tokio::time::timeout(Duration::from_secs(30), git_completion(&mut app))
        .await
        .expect("probe stalled");
    app.handle_internal_event(event);
    let row = summary(&mut app);
    assert_eq!(row["status"], "degraded");
    assert_eq!(row["repositories"][0]["consecutive_failures"], 1);
    assert_eq!(
        (
            row["consecutive_failures"].as_u64(),
            row["last_error"].as_str(),
            app.git_status_cache.len()
        ),
        (Some(1), Some(code), 0)
    );
}

#[tokio::test]
async fn git_refresh_nonzero_probe_is_not_cached_as_success() {
    failed_probe(Some("exit 7"), "git_exit").await;
}
#[tokio::test]
async fn git_refresh_io_probe_is_not_cached_as_success() {
    failed_probe(None, "git_io").await;
}
#[tokio::test]
async fn git_refresh_invalid_count_is_not_cached_as_success() {
    failed_probe(Some("printf 'not counts'"), "git_output").await;
}
#[tokio::test]
async fn git_refresh_extra_counts_are_not_cached_as_success() {
    failed_probe(Some("printf '1 2 3'"), "git_output").await;
}
#[tokio::test]
async fn git_refresh_invalid_utf8_is_not_cached_as_success() {
    failed_probe(Some("printf '\\377'"), "git_output").await;
}

#[tokio::test]
async fn git_refresh_timeout_is_counted_and_never_cached() {
    let mut app = app();
    let fixture = crate::workspace::git::test_support::HungGit::new();
    crate::workspace::git::test_support::write_fake_tracked_repo(fixture.cwd());
    app.state.workspaces[0].identity_cwd = fixture.cwd().to_owned();
    app.state.workspaces[0].tabs.clear();
    let now = Instant::now();
    app.mark_git_status_refresh_due(now);
    app.start_git_status_refresh_if_due(now);
    let controller = fixture.started();
    let event = tokio::time::timeout(Duration::from_secs(30), git_completion(&mut app)).await;
    drop(controller);
    let event = event.expect("git child remained in flight beyond its bound");
    app.handle_internal_event(event);
    let row = summary(&mut app);
    assert_eq!(row["status"], "degraded");
    assert_eq!(
        (
            row["consecutive_failures"].as_u64(),
            row["last_error"].as_str(),
            app.git_status_cache.len()
        ),
        (Some(1), Some("timeout"), 0)
    );
}

#[tokio::test]
async fn git_refresh_watchdog_error_completion_counts_the_round_only_once() {
    let mut app = app();
    let start = Instant::now();
    let (release_tx, release_rx) = mpsc::channel();
    let release = Release(Some(release_tx));
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dispatch_git_refresh(
        start,
        move || {
            release_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("fixture never released");
            WorkspaceGitRefreshOutput {
                error: Some(GitRefreshErrorKind::Timeout),
                ..Default::default()
            }
        },
        tracked_spawn(tx),
    );
    app.start_git_status_refresh_if_due(start + Duration::from_secs(60));
    assert_eq!(app.git_refresh_health.consecutive_failures, 1);
    drop(release);
    let event = receive(&mut app, rx).await;
    app.handle_internal_event(event);
    assert_eq!(app.git_refresh_health.consecutive_failures, 1);
    assert_eq!(summary(&mut app)["last_error"], "timeout");
    assert!(!app.git_refresh_in_flight);
}

#[tokio::test]
async fn git_refresh_failed_repository_does_not_starve_healthy_repositories() {
    let mut app = app();
    let fixture = GitFixture::new(Some("if [ -f \"$2/fail\" ]; then exit 7; fi\nprintf '3 1'"));
    std::fs::write(fixture.cwd().join("fail"), "").unwrap();
    let healthy = crate::test_support::TempDir::socket();
    crate::workspace::git::test_support::write_fake_tracked_repo(healthy.path());
    app.state.workspaces[0].identity_cwd = fixture.cwd();
    app.state.workspaces[0].tabs.clear();
    let mut second = Workspace::test_new("healthy-git");
    second.identity_cwd = healthy.path().to_owned();
    second.tabs.clear();
    app.state.workspaces.push(second);
    let now = Instant::now();
    app.mark_git_status_refresh_due(now);
    app.start_git_status_refresh_if_due(now);
    let event = tokio::time::timeout(Duration::from_secs(30), git_completion(&mut app))
        .await
        .expect("probe did not complete");
    app.handle_internal_event(event);
    assert_eq!(app.git_refresh_health.consecutive_failures, 0);
    assert_eq!(app.git_status_cache.len(), 1);
    assert_eq!(
        app.state.workspaces[1].cached_git_ahead_behind,
        Some((3, 1))
    );
    assert_eq!(summary(&mut app)["last_error"], "git_exit");
}

async fn git2034_partial_round(app: &mut App) {
    let now = Instant::now();
    app.mark_git_status_refresh_due(now);
    app.start_git_status_refresh_if_due(now);
    let event = tokio::time::timeout(Duration::from_secs(30), git_completion(app))
        .await
        .expect("producer never completed");
    app.handle_internal_event(event);
}

#[tokio::test]
async fn git2034_repository_failure_isolated_and_recovery_clears_stale() {
    let mut app = app();
    app.state.app_client_attached = true;
    let fixture = GitFixture::new(Some("if [ -f \"$2/fail\" ]; then exit 7; fi\nprintf '3 1'"));
    std::fs::write(fixture.cwd().join("fail"), "").unwrap();
    let healthy = crate::test_support::TempDir::socket();
    crate::workspace::git::test_support::write_fake_tracked_repo(healthy.path());
    app.state.workspaces[0].identity_cwd = fixture.cwd();
    app.state.workspaces[0].tabs.clear();
    app.state.workspaces[0].cached_git_branch = Some("last-known".into());
    let mut second = Workspace::test_new("healthy-git");
    second.identity_cwd = healthy.path().to_owned();
    second.tabs.clear();
    app.state.workspaces.push(second);
    for round in 1..=8 {
        std::fs::write(
            healthy.path().join(".git/refs/heads/main"),
            format!("{:040x}\n", 100 + round),
        )
        .unwrap();
        git2034_partial_round(&mut app).await;
        assert_eq!(
            app.git_refresh_health.consecutive_failures, 0,
            "repository failure became worker failure"
        );
        let row = summary(&mut app);
        assert_eq!(row["status"], "degraded");
        assert_eq!(row["repositories"][0]["consecutive_failures"], round);
        assert_eq!(row["repositories"][0]["last_error"], "git_exit");
        assert_eq!(
            row["repositories"][0]["repo"],
            fixture.cwd().file_name().unwrap().to_str().unwrap()
        );
        assert_eq!(
            row["repositories"][0]["workspace_ids"][0],
            app.state.workspaces[0].id
        );
        assert_eq!(
            app.state.workspaces[1].cached_git_ahead_behind,
            Some((3, 1))
        );
        assert!(!crate::ui::grammar::local_member_label(
            &app.state,
            &app.state.workspaces[1],
            &app.terminal_runtimes
        )
        .contains("[git stale]"));
        assert!(crate::ui::grammar::local_member_label(
            &app.state,
            &app.state.workspaces[0],
            &app.terminal_runtimes
        )
        .contains("[git stale]"));
        assert!(crate::ui::grammar::leader_label(
            &app.state,
            &app.state.workspaces[0],
            &app.terminal_runtimes
        )
        .contains("[git stale]"));
    }
    std::fs::remove_file(fixture.cwd().join("fail")).unwrap();
    git2034_partial_round(&mut app).await;
    assert_eq!(summary(&mut app)["status"], "ok");
    assert!(summary(&mut app).get("repositories").is_none());
    assert!(!crate::ui::grammar::local_member_label(
        &app.state,
        &app.state.workspaces[0],
        &app.terminal_runtimes
    )
    .contains("[git stale]"));
}

#[tokio::test]
async fn git2034_all_repositories_failing_does_not_stamp_success() {
    let mut app = app();
    app.state.app_client_attached = true;
    let fixture = GitFixture::new(Some("exit 7"));
    app.state.workspaces[0].identity_cwd = fixture.cwd();
    app.state.workspaces[0].tabs.clear();
    let previous_success = Instant::now() - Duration::from_secs(10_000);
    app.git_refresh_health.last_success = Some(previous_success);
    for count in 1..=4 {
        git2034_partial_round(&mut app).await;
        assert_eq!(app.git_refresh_health.last_success, Some(previous_success));
        assert_eq!(app.git_refresh_health.consecutive_failures, count);
        let row = summary(&mut app);
        assert_eq!(row["status"], "broken");
        assert_eq!(row["last_error"], "git_exit");
        assert_eq!(row["repositories"][0]["consecutive_failures"], count);
    }
}

#[tokio::test]
async fn git2100_starvation_is_visible_and_running_clears_it() {
    let mut app = app();
    app.state.app_client_attached = true;
    app.state.workspaces[0].tabs.clear();
    let ws = &app.state.workspaces[0];
    let jobs = vec![WorkspaceGitRefreshJob {
        cache_key: ws.identity_cwd.clone(),
        status_cwd: ws.identity_cwd.clone(),
        cached: None,
        targets: vec![WorkspaceGitRefreshTarget {
            workspace_id: ws.id.clone(),
            resolved_identity_cwd: ws.identity_cwd.clone(),
        }],
    }];
    let mut schedule = GitRefreshSchedule::default();
    for round in 1..=3 {
        let deadline = Instant::now();
        refresh_git_jobs(
            jobs.clone(),
            &mut schedule,
            deadline,
            || deadline,
            |_, _| panic!("expired budget must not probe"),
        );
        *app.git_refresh_schedule.lock() = schedule.clone();
        let row = summary(&mut app);
        if round < 3 {
            assert!(row["repositories"].is_null());
        } else {
            assert_eq!(row["status"], "degraded");
            assert_eq!(row["reason"], "starved");
            assert_eq!(row["repositories"][0]["status"], "starved");
            assert_eq!(row["repositories"][0]["consecutive_not_started"], 3);
            assert_eq!(row["repositories"][0]["consecutive_failures"], 0);
            // Mesh-v5 readers that predate starvation still accept these rows.
            #[derive(serde::Deserialize)]
            struct LegacyRepository {
                repo: String,
                workspace_ids: Vec<String>,
                consecutive_failures: u32,
                last_error: String,
            }
            let legacy: LegacyRepository =
                serde_json::from_value(row["repositories"][0].clone()).unwrap();
            assert!(!legacy.repo.is_empty());
            assert_eq!(legacy.workspace_ids.len(), 1);
            assert_eq!(legacy.consecutive_failures, 0);
            assert!(legacy.last_error.is_empty());
            assert!(row["last_error"].is_null());
        }
        assert!(!app.state.workspaces[0].git_status_stale);
    }
    let start = Instant::now();
    let output = refresh_git_jobs(
        jobs,
        &mut schedule,
        start + Duration::from_secs(1),
        || start,
        |_, _| Err(GitRefreshErrorKind::GitExit),
    );
    // Admission clears starvation even if this repository's probe then fails.
    *app.git_refresh_schedule.lock() = schedule;
    app.handle_internal_event(AppEvent::GitStatusRefreshed {
        results: output.results,
        failures: output.failures,
        cache_updates: output.cache_updates,
        error: output.error,
    });
    let row = summary(&mut app);
    assert_eq!(row["status"], "degraded");
    assert_ne!(row["reason"], "starved");
    assert_eq!(row["repositories"][0]["status"], "failed");
    assert_eq!(row["last_error"], "git_exit");
    assert!(app.state.workspaces[0].git_status_stale);
}

#[tokio::test]
async fn git2100_removed_or_moved_repository_has_no_starvation_diagnostic() {
    let mut app = app();
    app.state.workspaces[0].tabs.clear();
    let ws = &app.state.workspaces[0];
    app.git_refresh_schedule.lock().missed.insert(
        ws.identity_cwd.clone(),
        (
            vec![WorkspaceGitRefreshTarget {
                workspace_id: ws.id.clone(),
                resolved_identity_cwd: ws.identity_cwd.clone(),
            }],
            3,
        ),
    );
    assert_eq!(app.git_repository_health_rows().len(), 1);
    app.state.workspaces[0].identity_cwd.push("moved");
    assert!(app.git_repository_health_rows().is_empty());
    app.state.workspaces.clear();
    assert!(app.git_repository_health_rows().is_empty());
}
