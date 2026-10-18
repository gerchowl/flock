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

fn empty_output() -> WorkspaceGitRefreshOutput {
    WorkspaceGitRefreshOutput {
        error: None,
        results: vec![],
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
            empty_output()
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
    assert_eq!(app.git_refresh_health.consecutive_failures, 1);
    assert_eq!(app.git_status_cache.len(), 1);
    assert_eq!(
        app.state.workspaces[1].cached_git_ahead_behind,
        Some((3, 1))
    );
    assert_eq!(summary(&mut app)["last_error"], "git_exit");
}
