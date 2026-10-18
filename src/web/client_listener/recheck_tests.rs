use super::*;
use crate::web::wire::tests::TestClock;
use std::sync::atomic::{AtomicUsize, Ordering};
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("listener guard producer stayed silent")
}
struct Fixture {
    _root: crate::test_support::TempDir,
    listener: Listener,
    clock: Arc<TestClock>,
    outcome: Arc<AtomicUsize>,
    calls: tokio::sync::mpsc::UnboundedReceiver<u16>,
}
impl Fixture {
    async fn new() -> Self {
        let root = crate::test_support::TempDir::socket();
        let clock = Arc::new(TestClock::new());
        let outcome = Arc::new(AtomicUsize::new(0));
        let observed = outcome.clone();
        let (sent, calls) = tokio::sync::mpsc::unbounded_channel();
        let probe_fn: Probe = Arc::new(move |port| {
            let result = match observed.load(Ordering::SeqCst) {
                0 => Ok(()),
                1 => Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    InvalidListener::ExposedByServe,
                )),
                _ => Err(io::Error::from_raw_os_error(libc::EIO)),
            };
            sent.send(port).unwrap();
            result
        });
        let listener = Listener::prepare_using(
            Some("127.0.0.1:0".parse().unwrap()),
            Codes::default(),
            root.path().into(),
            None,
            Health::default(),
            clock.clone(),
            probe_fn,
        )
        .await
        .unwrap()
        .unwrap();
        Self {
            _root: root,
            listener,
            clock,
            outcome,
            calls,
        }
    }
    async fn start(&mut self) {
        assert_eq!(
            bounded(self.calls.recv()).await,
            Some(self.listener.local_addr().unwrap().port())
        );
        self.listener.activate();
        bounded(self.clock.deadline(60_000)).await;
    }
    async fn stopped(&mut self, expected: Status) {
        assert!(bounded(self.calls.recv()).await.is_some());
        bounded(async {
            while !self.listener.task.as_ref().unwrap().is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(self.listener.health.status(), Some(expected));
        assert_eq!(
            self.listener.local_addr().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        assert_eq!(
            self.listener.duplicate().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        assert_eq!(self.listener.codes.bound_endpoint(), None);
    }
}
#[tokio::test]
async fn supplied_sixty_second_interval_stops_only_exposed_listener() {
    let mut fixture = Fixture::new().await;
    fixture.start().await;
    fixture.outcome.store(1, Ordering::SeqCst);
    fixture.clock.advance(59_999);
    assert!(matches!(
        fixture.calls.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    fixture.clock.advance(1);
    fixture.stopped(Status::ExposedByServe).await;
    fixture.listener.shutdown().await.unwrap();
}
#[tokio::test]
async fn reload_rechecks_without_waiting_for_the_interval() {
    let mut fixture = Fixture::new().await;
    fixture.start().await;
    fixture.outcome.store(1, Ordering::SeqCst);
    fixture.listener.recheck();
    fixture.stopped(Status::ExposedByServe).await;
    fixture.listener.shutdown().await.unwrap();
}
#[tokio::test]
async fn unsuccessful_recheck_fails_closed_without_stopping_its_owner() {
    let mut fixture = Fixture::new().await;
    fixture.start().await;
    fixture.outcome.store(2, Ordering::SeqCst);
    fixture.listener.recheck();
    fixture.stopped(Status::Failed).await;
    fixture.listener.shutdown().await.unwrap();
}
#[tokio::test]
async fn clean_reload_preserves_listener_and_the_next_periodic_check() {
    let mut fixture = Fixture::new().await;
    fixture.start().await;
    fixture.clock.advance(20_000);
    fixture.listener.recheck();
    assert!(bounded(fixture.calls.recv()).await.is_some());
    bounded(fixture.clock.deadline_count(60_000, 2)).await;
    assert_eq!(fixture.listener.health.status(), Some(Status::Listening));
    assert!(fixture.listener.local_addr().is_ok());
    fixture.outcome.store(1, Ordering::SeqCst);
    fixture.clock.advance(40_000);
    fixture.stopped(Status::ExposedByServe).await;
    fixture.listener.shutdown().await.unwrap();
}
#[test]
fn startup_exposure_refusal_and_health_share_a_typed_reason_and_warning() {
    let health = Health::default();
    let error = validate_funnel(
        std::process::Output {
            status: {
                use std::os::unix::process::ExitStatusExt;
                std::process::ExitStatus::from_raw(0)
            },
            stdout: br#"{"TCP":{"12345":{}}}"#.to_vec(),
            stderr: Vec::new(),
        },
        12345,
    )
    .unwrap_err();
    assert_eq!(
        error.get_ref().unwrap().downcast_ref::<InvalidListener>(),
        Some(&InvalidListener::ExposedByServe)
    );
    let logs = crate::logging::capture_logs(|| publish_probe_failure(&health, &error));
    assert_eq!(health.status(), Some(Status::ExposedByServe));
    assert!(logs.contains("status=ExposedByServe"));
    assert!(logs.contains("client.listener.probe_failed"));
}
