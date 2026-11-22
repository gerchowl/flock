use super::super::{
    journal::{Admission, Operation},
    Command,
};
use super::*;
use serde_json::json;
use std::time::Duration;

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("ownership producer stayed silent")
}
fn fixture() -> (crate::structured::launch::SessionConfigDir, Ownership) {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let owner = Ownership::new(root.path().to_owned(), Arc::new(Notify::new()));
    (root, owner)
}
fn claim(id: &str) -> Operation {
    Operation::Claim {
        namespace: "local".into(),
        principal: "fixture-principal".into(),
        command: Command {
            id: id.into(),
            verb: "agent.decide".into(),
            args: json!({}),
            resume_only: false,
            read_only: false,
        },
        now: 100,
    }
}
async fn install(owner: &Ownership, writer: Writer) {
    *owner.0.pending.lock() = writer.pending();
    let mut slot = owner.0.writer.lock().await;
    slot.phase = owner.0.phase.load(Ordering::Acquire) & !1;
    slot.writer = Some(writer);
}
async fn apply(owner: &Ownership, operation: Operation) -> Result<Admission, ErrorCode> {
    let reply = owner
        .0
        .writer
        .lock()
        .await
        .writer
        .as_ref()
        .unwrap()
        .submit(operation)?;
    within(reply).await.unwrap()
}
#[tokio::test]
async fn commit_prepares_without_lease_then_replays_after_exporter_freeze() {
    let (root, exporter) = fixture();
    exporter.acquire().await.unwrap();
    assert!(matches!(
        apply(&exporter, claim("before-commit")).await.unwrap(),
        Admission::Claimed(100)
    ));
    let first = apply(
        &exporter,
        Operation::Finish {
            key: (
                "local".into(),
                "fixture-principal".into(),
                "before-commit".into(),
            ),
            admitted_ms: 100,
            result: Ok(json!({"accepted":true})),
        },
    )
    .await
    .unwrap();
    let importer = Ownership::new(root.path().to_owned(), Arc::new(Notify::new()));
    assert_eq!(importer.gate().load(Ordering::Acquire) & 1, 0);
    assert!(importer.0.writer.lock().await.writer.is_none());
    exporter.freeze().await.unwrap();
    importer.acquire().await.unwrap();
    assert_eq!(importer.gate().load(Ordering::Acquire) & 1, 1);
    let replay = apply(&importer, claim("before-commit")).await.unwrap();
    assert!(matches!((first, replay), (Admission::Replayed(a), Admission::Replayed(b)) if a == b));
    // Keeping the old owner alive and shutting it down cannot release the importer's lease.
    exporter.shutdown().await.unwrap();
    assert!(matches!(
        Writer::open(root.path().to_owned(), Arc::new(Notify::new())).await,
        Err(ErrorCode::SessionBusy)
    ));
    importer.shutdown().await.unwrap();
}
#[tokio::test]
async fn rollback_replays_before_opening_and_shutdown_cannot_be_undone_by_recovery() {
    let (_root, owner) = fixture();
    owner.acquire().await.unwrap();
    apply(&owner, claim("retained")).await.unwrap();
    owner.freeze().await.unwrap();
    within(owner.reopen()).await.unwrap().unwrap();
    assert!(matches!(
        apply(&owner, claim("retained")).await.unwrap(),
        Admission::Pending(100)
    ));
    owner.freeze().await.unwrap();
    let recovery = owner.reopen();
    owner.shutdown().await.unwrap();
    let _ = within(recovery).await.unwrap();
    assert_eq!(owner.gate().load(Ordering::Acquire) & 1, 0);
    assert!(owner.0.writer.lock().await.writer.is_none());
    assert_eq!(owner.acquire().await.unwrap_err(), ErrorCode::SessionBusy);
}
#[tokio::test]
async fn shutdown_drains_accepted_write_and_releases_lease_with_handles_alive() {
    let (root, owner) = fixture();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let mut entered = Some(entered);
    let writer = Writer::test_open(
        root.path().to_owned(),
        owner.0.wake.clone(),
        Box::new(move || {
            if let Some(entered) = entered.take() {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5))
                    .expect("test did not release sync");
            }
        }),
    )
    .await;
    let reply = writer.submit(claim("drained")).unwrap();
    install(&owner, writer).await;
    within(ready).await.unwrap();
    let shutdown = tokio::spawn({
        let owner = owner.clone();
        async move { owner.shutdown().await }
    });
    release.send(()).unwrap();
    within(shutdown).await.unwrap().unwrap();
    assert!(matches!(
        within(reply).await.unwrap().unwrap(),
        Admission::Claimed(100)
    ));
    let replacement = Ownership::new(root.path().to_owned(), Arc::new(Notify::new()));
    replacement.acquire().await.unwrap();
    assert!(matches!(
        apply(&replacement, claim("drained")).await.unwrap(),
        Admission::Pending(100)
    ));
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_inflight_replay_and_fences_its_late_completion() {
    let (root, owner) = fixture();
    owner.acquire().await.unwrap();
    owner.freeze().await.unwrap();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    *owner.0.open_barrier.lock() = Some((entered, wait));
    let recovery = owner.reopen();
    within(started).await.unwrap();
    let mut shutdown = Box::pin(owner.shutdown());
    assert!(matches!(
        futures_util::poll!(shutdown.as_mut()),
        std::task::Poll::Pending
    ));
    assert_eq!(owner.gate().load(Ordering::Acquire) & 1, 0);
    release.send(()).unwrap();
    within(shutdown).await.unwrap();
    assert_eq!(
        within(recovery).await.unwrap().unwrap_err(),
        ErrorCode::SessionBusy
    );
    assert_eq!(owner.gate().load(Ordering::Acquire) & 1, 0);
    let replacement = Ownership::new(root.path().to_owned(), Arc::new(Notify::new()));
    replacement.acquire().await.unwrap();
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_during_leased_replay_is_bounded_and_fences_late_completion() {
    let (root, owner) = fixture();
    let (started, release) = Writer::test_stall_replay(root.path().to_owned());
    let opening = tokio::spawn({
        let owner = owner.clone();
        async move { owner.acquire().await }
    });
    within(started).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), owner.shutdown())
        .await
        .expect("shutdown waited on replay instead of detaching")
        .unwrap();
    assert_eq!(owner.gate().load(Ordering::Acquire) & 1, 0);
    assert!(matches!(
        Writer::open(root.path().to_owned(), Arc::new(Notify::new())).await,
        Err(ErrorCode::SessionBusy)
    ));
    release.send(()).unwrap();
    assert_eq!(within(opening).await.unwrap(), Err(ErrorCode::SessionBusy));
    let replacement = Ownership::new(root.path().to_owned(), Arc::new(Notify::new()));
    replacement.acquire().await.unwrap();
    replacement.shutdown().await.unwrap();
}

fn open_hooks() -> &'static parking_lot::Mutex<std::collections::HashMap<PathBuf, OpenBarrier>> {
    static HOOKS: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<PathBuf, OpenBarrier>>,
    > = std::sync::OnceLock::new();
    HOOKS.get_or_init(Default::default)
}
type ReplayEvents = tokio::sync::mpsc::UnboundedSender<(u64, u64)>;
fn replay_events() -> &'static parking_lot::Mutex<std::collections::HashMap<PathBuf, ReplayEvents>>
{
    static EVENTS: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<PathBuf, ReplayEvents>>,
    > = std::sync::OnceLock::new();
    EVENTS.get_or_init(Default::default)
}
pub(super) fn replay_observed(data: &std::path::Path, progress: (u64, u64)) {
    if let Some(events) = replay_events().lock().get(data) {
        let _ = events.send(progress);
    }
}
pub(super) fn take_open_barrier(data: &std::path::Path) -> Option<OpenBarrier> {
    open_hooks().lock().remove(data)
}
impl Ownership {
    pub(in crate::web::wire) fn test_watch_replay(
        data: PathBuf,
    ) -> tokio::sync::mpsc::UnboundedReceiver<(u64, u64)> {
        let (send, events) = tokio::sync::mpsc::unbounded_channel();
        replay_events().lock().insert(data, send);
        events
    }
    pub(in crate::web::wire) fn test_step_replay(
        data: PathBuf,
    ) -> (
        tokio::sync::mpsc::UnboundedReceiver<u64>,
        std::sync::mpsc::Sender<()>,
    ) {
        Writer::test_step_replay(data)
    }
    pub(in crate::web::wire) fn test_stall_replay(
        data: PathBuf,
    ) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        Writer::test_stall_replay(data)
    }
    pub(in crate::web::wire) fn test_stall_next_open(
        data: PathBuf,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        open_hooks().lock().insert(data, (entered, wait));
        (started, release)
    }
}

async fn stalled_drain(shutdown: bool) {
    let (root, owner) = fixture();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let mut entered = Some(entered);
    let writer = Writer::test_open(
        root.path().to_owned(),
        owner.0.wake.clone(),
        Box::new(move || {
            if let Some(entered) = entered.take() {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5))
                    .expect("test did not release stalled writer");
            }
        }),
    )
    .await;
    let mut reply = writer.submit(claim("abandoned")).unwrap();
    install(&owner, writer).await;
    within(ready).await.unwrap();
    let mut caller = tokio::spawn({
        let owner = owner.clone();
        async move {
            if shutdown {
                owner.shutdown().await
            } else {
                owner.freeze().await
            }
        }
    });
    let result = tokio::time::timeout(Duration::from_secs(2), &mut caller).await;
    let bounded = result.is_ok();
    if let Ok(result) = &result {
        assert!(result.as_ref().unwrap().is_ok());
    }
    assert_eq!(owner.0.pending.lock().load(Ordering::Acquire), 1);
    assert!(matches!(
        reply.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    let closed = owner.gate().load(Ordering::Acquire) & 1 == 0;
    let competing = Writer::open(root.path().to_owned(), Arc::new(Notify::new())).await;
    assert!(
        matches!(competing, Err(ErrorCode::SessionBusy)),
        "stalled writer must retain its exclusive lease"
    );
    release.send(()).unwrap();
    if result.is_err() {
        within(caller).await.unwrap().unwrap();
    }
    within(reply).await.unwrap().unwrap();
    owner.shutdown().await.unwrap();
    let replacement = Ownership::new(root.path().to_owned(), Arc::new(Notify::new()));
    replacement.acquire().await.unwrap();
    replacement.shutdown().await.unwrap();
    assert!(closed);
    assert!(bounded, "caller hung on the stalled journal writer");
}
fn check_stalled_drain(shutdown: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let log = crate::logging::capture_logs(|| runtime.block_on(stalled_drain(shutdown)));
    assert!(log.contains("pending_writes=1"), "{log}");
    assert!(log.contains("drain"), "{log}");
}
#[test]
fn stalled_shutdown_returns_with_lease_fenced() {
    check_stalled_drain(true);
}
#[test]
fn stalled_freeze_returns_with_lease_fenced() {
    check_stalled_drain(false);
}
