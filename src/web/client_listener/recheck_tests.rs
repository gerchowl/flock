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
        assert!(self.listener.export_descriptor().unwrap().is_none());
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

#[tokio::test]
async fn export_snapshot_survives_stop_and_stopped_export_is_disabled() {
    let mut fixture = Fixture::new().await;
    fixture.start().await;
    let descriptor = fixture.listener.export_descriptor().unwrap().unwrap();
    fixture.outcome.store(1, Ordering::SeqCst);
    fixture.listener.recheck();
    fixture.stopped(Status::ExposedByServe).await;
    let transferred = crate::platform::client_listener::take(&mut vec![{
        use std::os::fd::IntoRawFd;
        descriptor.into_raw_fd()
    }])
    .unwrap();
    assert!(transferred.local_addr().unwrap().ip().is_loopback());
    assert!(fixture.listener.export_descriptor().unwrap().is_none());
    fixture.listener.shutdown().await.unwrap();
}
#[tokio::test]
async fn activation_address_failure_clears_endpoint_socket_and_health() {
    let mut fixture = Fixture::new().await;
    fixture
        .listener
        .codes
        .set_endpoint(fixture.listener.local_addr().unwrap())
        .unwrap();
    fixture
        .listener
        .activate_with_addr(|_| Err(io::Error::from_raw_os_error(libc::EIO)));
    let error = bounded(fixture.listener.task.take().unwrap())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EIO));
    assert_eq!(fixture.listener.health.status(), Some(Status::Failed));
    assert_eq!(fixture.listener.codes.bound_endpoint(), None);
    assert!(fixture.listener.export_descriptor().unwrap().is_none());
    fixture.listener.shutdown().await.unwrap();
}

#[test]
fn activation_address_failure_never_publishes_listening_and_has_its_own_event() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let logs = crate::logging::capture_logs(|| {
        runtime.block_on(async {
            let mut fixture = Fixture::new().await;
            let health = fixture.listener.health.clone();
            let sequence = Arc::new(Mutex::new(Vec::new()));
            let observed = sequence.clone();
            fixture.listener.activate_with_addr(move |_| {
                observed.lock().unwrap().push(health.status());
                Err(io::Error::from_raw_os_error(libc::EIO))
            });
            let error = bounded(fixture.listener.task.take().unwrap())
                .await
                .unwrap()
                .unwrap_err();
            sequence
                .lock()
                .unwrap()
                .push(fixture.listener.health.status());
            assert_eq!(*sequence.lock().unwrap(), vec![None, Some(Status::Failed)]);
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
            fixture.listener.shutdown().await.unwrap();
        })
    });
    assert!(logs.contains("client.listener.address_failed"), "{logs}");
    assert!(!logs.contains("client.listener.probe_failed"), "{logs}");
}
