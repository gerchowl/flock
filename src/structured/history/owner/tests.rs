use super::*;
use crate::structured::{
    history::Item,
    launch::SessionConfigDir,
    registry::{Record, StructuredRuntime},
};
use serde_json::json;
use std::{fs, io::Write, time::Duration};

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("producer failed to make progress")
}
struct Fixture {
    root: SessionConfigDir,
    registry: Registry,
    workers: Workers,
    client: Client,
    owner: Owner,
    generation: u64,
}
impl Fixture {
    fn new() -> Self {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let mut registry = Registry::default();
        let generation = registry
            .register(Record {
                runtime: StructuredRuntime::Structured,
                agent_id: "fixture-agent".into(),
                name: None,
                harness: "claude".into(),
                session_id: "history".into(),
                cwd: root.path().to_owned(),
                model: None,
            })
            .unwrap();
        let wake = Arc::new(Notify::new());
        let workers = Workers::new(wake.clone());
        let (client, owner) = channel(wake);
        let fixture = Self {
            root,
            registry,
            workers,
            client,
            owner,
            generation,
        };
        fixture.write("fixture-agent");
        fixture
    }
    fn write(&self, agent: &str) -> std::path::PathBuf {
        let sessions = self.root.path().join("sessions");
        crate::platform::structured::ensure_private_dir(&sessions).unwrap();
        let dir = sessions.join(agent);
        crate::platform::structured::ensure_private_dir(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let mut file = crate::platform::structured::create_private_file(&path).unwrap();
        writeln!(file,"{}", json!({"seq":1,"known":true,"received_at_ms":1,"origin":"claude","result_subtype":null,"raw":"{\"type\":\"assistant\",\"message\":{}}"})).unwrap();
        file.sync_all().unwrap();
        path
    }
    fn service(&mut self) {
        self.owner
            .service(&self.registry, &mut self.workers, self.root.path());
    }
    async fn locate(&mut self) -> Located {
        let (tx, rx) = oneshot::channel();
        self.client
            .tx
            .try_send(Request::Lookup("fixture-agent".into(), tx))
            .unwrap();
        self.service();
        within(rx).await.unwrap().unwrap()
    }
    async fn install(&mut self, source: History, generation: u64) -> io::Result<History> {
        let (tx, rx) = oneshot::channel();
        self.client
            .tx
            .try_send(Request::Install(
                "fixture-agent".into(),
                generation,
                source,
                tx,
            ))
            .unwrap();
        self.service();
        within(rx).await.unwrap()
    }
    async fn lookup(&mut self, id: &str) -> io::Result<Option<History>> {
        let client = self.client.clone();
        let future = client.lookup(id.into());
        tokio::pin!(future);
        within(async {
            loop {
                tokio::select! {
                    result = &mut future => return result,
                    _ = client.wake.notified() => self.service(),
                }
            }
        })
        .await
    }
}
#[tokio::test]
async fn consumer_recovers_real_jsonl_then_uses_the_installed_canonical_source() {
    let mut fixture = Fixture::new();
    let source = fixture.lookup("fixture-agent").await.unwrap().unwrap();
    let mut cut = source.subscribe(None).unwrap();
    assert!(matches!(cut.page().unwrap(), Item::Record(record) if record.seq==1));
    let hint = cut.checkpoint().unwrap();
    assert!(matches!(cut.page().unwrap(), Item::Synced(1)));
    let again = fixture.lookup("fixture-agent").await.unwrap().unwrap();
    assert!(matches!(
        again.resume(1, &hint).unwrap().page().unwrap(),
        Item::Synced(1)
    ));
    assert!(fixture.lookup("missing-agent").await.unwrap().is_none());
}
#[tokio::test]
async fn concurrent_recovery_returns_the_winners_source_without_replacing_it() {
    let mut fixture = Fixture::new();
    let first = fixture.locate().await;
    let second = fixture.locate().await;
    let load = |located| match located {
        Located::Retained(path, generation) => (History::open_retained(&path).unwrap(), generation),
        _ => panic!("recovery expected"),
    };
    let (first, generation) = load(first);
    let (second, _) = load(second);
    let canonical = fixture.install(first, generation).await.unwrap();
    let hint = canonical.subscribe(Some(1)).unwrap().checkpoint().unwrap();
    let winner = fixture.install(second, generation).await.unwrap();
    assert!(
        winner.resume(1, &hint).is_ok(),
        "must return the same publication authority"
    );
}
#[tokio::test]
async fn generation_fences_lookup_and_install_even_when_a_new_canonical_source_exists() {
    let mut fixture = Fixture::new();
    let Located::Retained(path, generation) = fixture.locate().await else {
        panic!("retained")
    };
    let stale = History::open_retained(&path).unwrap();
    fixture.install(stale.clone(), generation).await.unwrap();
    fixture
        .registry
        .begin_launch("fixture-agent", generation)
        .unwrap();
    let Located::Retained(_, current) = fixture.locate().await else {
        panic!("old generation was exposed")
    };
    fixture
        .install(History::open_retained(&path).unwrap(), current)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .install(stale, generation)
            .await
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::Interrupted
    );
    assert_ne!(current, fixture.generation);
}
#[tokio::test]
async fn namespace_is_checked_before_returning_an_existing_canonical_source() {
    let mut fixture = Fixture::new();
    let canonical = fixture.lookup("fixture-agent").await.unwrap().unwrap();
    let hint = canonical.subscribe(Some(1)).unwrap().checkpoint().unwrap();
    let foreign = History::open_retained(&fixture.write("another-agent")).unwrap();
    assert_eq!(
        fixture
            .install(foreign, fixture.generation)
            .await
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::Interrupted
    );
    assert!(fixture
        .workers
        .history(&fixture.registry, "fixture-agent")
        .unwrap()
        .resume(1, &hint)
        .is_ok());
}
#[tokio::test]
async fn full_or_closed_owner_mailbox_fails_without_an_unbounded_send_queue() {
    let (client, owner) = channel(Arc::new(Notify::new()));
    for _ in 0..REQUESTS {
        let (tx, _rx) = oneshot::channel();
        client
            .tx
            .try_send(Request::Lookup("missing-agent".into(), tx))
            .unwrap();
    }
    assert_eq!(
        within(client.lookup("missing-agent".into()))
            .await
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    drop(owner);
    assert_eq!(
        within(client.lookup("missing-agent".into()))
            .await
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
}
#[tokio::test]
async fn cancelled_lookup_is_not_installed_and_owner_loss_wakes_the_consumer() {
    let mut fixture = Fixture::new();
    let source = History::open_retained(
        &fixture
            .root
            .path()
            .join("sessions/fixture-agent/history.jsonl"),
    )
    .unwrap();
    let (tx, rx) = oneshot::channel();
    fixture
        .client
        .tx
        .try_send(Request::Install(
            "fixture-agent".into(),
            fixture.generation,
            source,
            tx,
        ))
        .unwrap();
    drop(rx);
    fixture.service();
    assert!(fixture
        .workers
        .history(&fixture.registry, "fixture-agent")
        .is_none());
    let (tx, rx) = oneshot::channel();
    let client = fixture.client.clone();
    let task = tokio::spawn(async move {
        client
            .request(Request::Lookup("fixture-agent".into(), tx), rx)
            .await
    });
    within(fixture.client.wake.notified()).await;
    drop(fixture.owner);
    assert_eq!(
        within(task).await.unwrap().err().unwrap().kind(),
        io::ErrorKind::BrokenPipe
    );
}
#[tokio::test]
async fn cancellation_keeps_io_capacity_until_the_blocking_producer_returns() {
    let budget = Budget(Arc::new(Semaphore::new(1)));
    let (ready, started) = oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let task = tokio::spawn({
        let budget = budget.clone();
        async move {
            budget
                .run(move || {
                    ready.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                })
                .await
        }
    });
    within(started).await.unwrap();
    task.abort();
    assert!(within(task).await.unwrap_err().is_cancelled());
    let result = budget.run(|| 7).await;
    release.send(()).unwrap();
    let permit = within(budget.0.clone().acquire_owned()).await.unwrap();
    drop(permit);
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(within(budget.run(|| 9)).await.unwrap(), 9);
}
#[tokio::test]
async fn storage_failure_does_not_install_a_source_and_worker_panic_releases_capacity() {
    let mut fixture = Fixture::new();
    fs::remove_file(
        fixture
            .root
            .path()
            .join("sessions/fixture-agent/history.jsonl"),
    )
    .unwrap();
    assert_eq!(
        fixture.lookup("fixture-agent").await.err().unwrap().kind(),
        io::ErrorKind::NotFound
    );
    assert!(fixture
        .workers
        .history(&fixture.registry, "fixture-agent")
        .is_none());
    let budget = Budget(Arc::new(Semaphore::new(1)));
    assert!(within(budget.run(|| panic!("fixture worker failed")))
        .await
        .is_err());
    assert_eq!(within(budget.run(|| 11)).await.unwrap(), 11);
}

#[tokio::test]
async fn recovery_work_runs_off_the_async_executor() {
    let budget = Budget(Arc::new(Semaphore::new(1)));
    let owner = std::thread::current().id();
    let producer = within(budget.run(|| std::thread::current().id()))
        .await
        .unwrap();
    assert_ne!(owner, producer);
}
#[test]
fn cancelled_queued_recovery_keeps_its_io_slot_until_the_job_runs() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let (ready, started) = oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                ready.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            within(started).await.unwrap();
            let budget = Budget(Arc::new(Semaphore::new(1)));
            let mut queued = Box::pin(budget.run(|| 7));
            let pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(queued.as_mut(), cx).is_pending())
            })
            .await;
            drop(queued);
            assert!(pending);
            let refused = budget.run(|| 9).await;
            release.send(()).unwrap();
            within(blocker).await.unwrap();
            let permit = within(budget.0.clone().acquire_owned()).await.unwrap();
            drop(permit);
            assert_eq!(refused.err().unwrap().kind(), io::ErrorKind::WouldBlock);
            assert_eq!(within(budget.run(|| 11)).await.unwrap(), 11);
        });
}

#[tokio::test]
async fn invalid_stored_namespaces_never_reach_filesystem_recovery() {
    let mut fixture = Fixture::new();
    for (id, session) in [
        ("../escaped", "other-history"),
        ("unsafe-session", "../other-history"),
        ("queue-session", "queue"),
    ] {
        fixture
            .registry
            .register(Record {
                runtime: StructuredRuntime::Structured,
                agent_id: id.into(),
                session_id: session.into(),
                name: None,
                harness: "claude".into(),
                cwd: fixture.root.path().to_owned(),
                model: None,
            })
            .unwrap();
        assert!(fixture.lookup(id).await.unwrap().is_none());
        assert!(fixture.workers.history(&fixture.registry, id).is_none());
    }
}

#[tokio::test]
async fn preparation_defers_cold_recovery_then_returns_the_live_canonical_source() {
    use crate::structured::workers::tests::Case;
    let mut fixture = Fixture::new();
    let wake = fixture.client.wake.clone();
    let case = Case::start(
        &mut fixture.registry,
        &mut fixture.workers,
        430,
        "state-reducer",
        vec![],
    );
    let refused = fixture.lookup(&case.id).await;
    let now = std::time::Instant::now();
    case.ready(&mut fixture.registry, &mut fixture.workers, &wake, now)
        .await;
    let source = fixture.lookup(&case.id).await.unwrap().unwrap();
    let same = Arc::ptr_eq(
        &source.0,
        &fixture
            .workers
            .history(&fixture.registry, &case.id)
            .unwrap()
            .0,
    );
    case.finish(
        &mut fixture.registry,
        &mut fixture.workers,
        &wake,
        now,
        None,
    )
    .await;
    assert_eq!(refused.err().unwrap().kind(), io::ErrorKind::WouldBlock);
    assert!(same);
}
#[tokio::test]
async fn unconfirmed_cleanup_never_licenses_cold_recovery() {
    use crate::structured::workers::tests::Case;
    let mut fixture = Fixture::new();
    let wake = fixture.client.wake.clone();
    let case = Case::uncertain(&mut fixture.registry, &mut fixture.workers, 431, &wake).await;
    assert!(
        fixture
            .workers
            .report(&case.id, case.generation)
            .unwrap()
            .authority_retained
    );
    assert_eq!(
        fixture.lookup(&case.id).await.err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
}
