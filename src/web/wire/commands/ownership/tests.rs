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
async fn apply(owner: &Ownership, operation: Operation) -> Result<Admission, ErrorCode> {
    let reply = owner
        .0
        .writer
        .lock()
        .await
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
    assert!(importer.0.writer.lock().await.is_none());
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
    assert!(owner.0.writer.lock().await.is_none());
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
    *owner.0.writer.lock().await = Some(writer);
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
