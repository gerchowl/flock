use super::*;
use crate::app::App;
use crate::structured::publication::Provider;
use crate::structured::{
    registry::Registry,
    view::Target,
    workers::{
        queue::publication::view_test::Fixture,
        tests::{wait, Case},
        Workers,
    },
};
use std::{sync::Arc, time::Instant};
use tokio::sync::Notify;
const PRODUCER: &str = include_str!("../../../../tests/fixtures/structured-public/claude.py");
async fn fixture(mode: &str) -> (App, Case, Stream) {
    let notify = Arc::new(Notify::new());
    let (mut registry, mut workers) = (Registry::default(), Workers::new(notify.clone()));
    let (mut case, mut launch, factory) = Case::prepare(&mut registry, 929, "unused", vec![]);
    let mut record = registry.remove(&case.id, case.generation).unwrap();
    record.model = Some(mode.into());
    launch.generation = registry.register(record).unwrap();
    let executable = case.fixture.root.path().join("claude");
    let executable_text = std::fs::read_to_string(&executable).unwrap();
    let interpreter = executable_text.lines().next().unwrap();
    let producer = format!("{interpreter}\n{PRODUCER}");
    case.fixture.file("claude", producer.as_bytes(), 0o700);
    let mut journal = queue::Journal::open(case.fixture.root.path(), &case.id).unwrap();
    for (position, id) in ["held-item", "held-next"].into_iter().enumerate() {
        let mut item = queue::tests::item(id);
        item.state = queue::model::State::Held;
        let change = queue::Change::Upsert {
            item: Box::new(item),
            position,
        };
        journal.append(change).unwrap();
    }
    drop(journal);
    case.generation = workers.start(&mut registry, launch, factory).unwrap();
    let live = workers.live(&registry, &case.id).unwrap();
    let clock = Instant::now();
    wait(&notify, || {
        workers.poll(&mut registry, clock, 1000);
        workers.report(&case.id, case.generation).is_some()
    })
    .await;
    workers.shutdown(&mut registry).unwrap();
    let policy = queue::RewritePolicy::new(std::num::NonZeroU64::MAX);
    let id = case.id.clone();
    let generation = case.generation;
    let data = case.fixture.root.path().into();
    let mut recovery = workers
        .recover_queue(&registry, data, &id, generation, policy)
        .unwrap();
    let wake = recovery.waker();
    wait(&wake, || match recovery.poll() {
        crate::structured::workers::queue::recovery::Step::Ready(source) => {
            source.install(&mut workers, &registry).unwrap();
            true
        }
        crate::structured::workers::queue::recovery::Step::Pending => false,
        _ => panic!("queue recovery failed"),
    })
    .await;
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let config = crate::config::Config::default();
    let mut app = App::new(&config, true, None, rx, Default::default());
    app.state.structured_agents = registry;
    app.structured_workers = workers;
    let target = Target {
        agent_id: id,
        generation,
    };
    let permissions = Default::default();
    let mut stream = app.open_structured_view(target, permissions).unwrap();
    stream.live = live;
    (app, case, stream)
}
async fn drain(s: &mut Stream) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while s.next().await.unwrap() {}
    })
    .await
    .expect("subscription producer never completed");
}
fn partials(s: &Stream) -> usize {
    let cards = &s.model.snapshot.transcript;
    cards
        .iter()
        .filter(|c| matches!(&c.content, Content::Markdown(t) if t == "partial"))
        .count()
}
#[tokio::test]
async fn real_producer_replay_live_queue_and_death() {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    s.model.snapshot.admission = Admission::Pending;
    drain(&mut s).await;
    assert!(matches!(s.model.snapshot.admission, Admission::Pending));
    assert_eq!(s.model.snapshot.applied_cursor, 3);
    assert!(matches!(s.model.snapshot.replay, Replay::Ended));
    assert_eq!(s.model.snapshot.queue.len(), 2);
    assert_eq!(s.model.snapshot.queue[0].item_id, "held-item");
    assert_eq!(partials(&s), 1);
    assert!(s.model.snapshot.transcript.iter().any(|c| matches!(&c.content, Content::ToolResult { tool_use_id, .. } if tool_use_id == "fixture-tool")));
}
#[tokio::test]
async fn real_burst_gap_resumes_applied_not_received_cursor() {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-burst-exit").await;
    let gap = tokio::time::timeout(std::time::Duration::from_secs(10), s.live.recv())
        .await
        .unwrap();
    assert!(matches!(gap, live::Item::Gap { through: 3 }));
    s.apply_live(gap);
    assert_eq!(s.model.snapshot.applied_cursor, 0);
    assert!(matches!(
        s.model.snapshot.replay,
        Replay::Lagged { through: 3 }
    ));
    drain(&mut s).await;
    assert_eq!(s.model.snapshot.applied_cursor, 3);
    assert!(s.model.snapshot.transcript.iter().any(|c| c.seq == 1));
    assert_eq!(partials(&s), 1);
}
#[tokio::test]
async fn canonical_refresh_and_retirement_drop_old_prompts_and_cursors() {
    let (mut app, case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    drain(&mut s).await;
    s.model.snapshot.permissions.push(serde_json::from_value(serde_json::json!({"type":"flk_permission_request","request_id":"old","request_seq":4,"state":"open","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"old-tool","input":{}}})).unwrap());
    s.model.snapshot.composer.text = "old draft".into();
    s.model.snapshot.agent.model = None;
    app.refresh_structured_view(&mut s).unwrap();
    let expected_model = Some("s10b-live-lifecycle-exit");
    assert_eq!(s.model.snapshot.agent.model.as_deref(), expected_model);
    let registry = &mut app.state.structured_agents;
    registry.begin_launch(&case.id, case.generation).unwrap();
    assert_eq!(
        app.refresh_structured_view(&mut s),
        Err(crate::api::error_code::ErrorCode::AgentNotFound)
    );
    assert_eq!(s.model.snapshot.applied_cursor, 0);
    assert!(
        s.model.snapshot.transcript.is_empty()
            && s.model.snapshot.permissions.is_empty()
            && s.model.snapshot.queue.is_empty()
            && s.model.snapshot.composer.text.is_empty()
    );
    assert!(matches!(
        s.model.snapshot.admission,
        Admission::Refused(ErrorCode::AgentNotFound)
    ));
    assert!(!s.next().await.unwrap());
}

#[tokio::test]
async fn replacement_queue_is_published_only_when_complete() {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    assert!(s.next().await.unwrap());
    assert!(s.model.snapshot.queue.is_empty());
    let held = Arc::clone(&s.model.snapshot.0);
    assert!(s.next().await.unwrap());
    assert!(Arc::ptr_eq(&held, &s.model.snapshot.0));
    assert!(
        s.model.snapshot.queue.is_empty(),
        "partial replacement leaked"
    );
    assert!(s.next().await.unwrap());
    assert_eq!(s.model.snapshot.queue.len(), 2);
    assert!(held.queue.is_empty());
}

fn poll<T>(future: std::pin::Pin<&mut impl std::future::Future<Output = T>>) -> std::task::Poll<T> {
    future.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}
async fn woke(step: impl std::future::Future<Output = io::Result<bool>>) {
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), step)
            .await
            .expect("queue readiness never arrived")
            .unwrap()
    );
}
async fn lag_recovery(busy: bool, replacement: bool) {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    let mut owner = Fixture::new();
    owner.append(0);
    owner.append(1);
    s.source = owner.source.clone();
    s.queue = Some(s.source.subscribe(None).unwrap());
    s.queue_started = true;
    s.queue_wake = s.queue.as_ref().unwrap().waker();
    s.ended = [true, true, false];
    owner.capture();
    assert!(s.next().await.unwrap());
    assert!(s.next().await.unwrap());
    if !replacement {
        assert!(s.next().await.unwrap());
    }
    for n in 2..7 {
        owner.append(n);
    }
    if busy {
        owner.competing_subscriber(|| {
            let mut step = std::pin::pin!(s.next());
            assert!(
                matches!(poll(step.as_mut()), std::task::Poll::Ready(Ok(true))),
                "lag must discard its spent reader before fallible capture"
            );
        });
        {
            let mut step = std::pin::pin!(s.next());
            owner.competing_subscriber(|| {
                assert!(
                    poll(step.as_mut()).is_pending(),
                    "transient busy must wait for readiness"
                )
            });
            woke(step).await;
        }
        assert!(!s.ended[2]);
        {
            let mut step = std::pin::pin!(s.next());
            assert!(
                poll(step.as_mut()).is_pending(),
                "full capture must not self-wake"
            );
            owner.capture();
            woke(step).await;
        }
    } else {
        assert!(s.next().await.unwrap());

        {
            let mut step = std::pin::pin!(s.next());
            // Either readiness policy may make progress or wait for capture.
            // The final exact FIFO assertion checks loss and duplication.
            if let std::task::Poll::Ready(result) = poll(step.as_mut()) {
                assert!(result.unwrap(), "live queue ended during resubscription");
            }
        }
        owner.capture();
    }
    owner.finish(false);
    drain(&mut s).await;
    assert_eq!(
        s.model
            .snapshot
            .queue
            .iter()
            .map(|v| v.item_id.as_str())
            .collect::<Vec<_>>(),
        (0..7).map(|n| format!("item-{n}")).collect::<Vec<_>>()
    );
    assert!(s.replacement.is_none());
}
#[tokio::test]
async fn queue_lag_resubscribes_without_loss_or_duplicates() {
    lag_recovery(false, false).await;
}
#[tokio::test]
async fn queue_lag_busy_keeps_the_queue_live() {
    lag_recovery(true, false).await;
}
#[tokio::test]
async fn queue_lag_busy_preserves_replacement() {
    lag_recovery(true, true).await;
}
#[tokio::test]
async fn queue_refusal_stays_a_refusal() {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    let mut owner = Fixture::new();
    owner.finish(true);
    s.queue = Some(owner.source.subscribe(None).unwrap());
    s.queue_started = true;
    s.queue_wake = s.queue.as_ref().unwrap().waker();
    for _ in 0..2 {
        let error = s.next().await.unwrap_err();
        assert_eq!(
            error.get_ref().unwrap().downcast_ref::<ErrorCode>(),
            Some(&ErrorCode::InternalError)
        );
        assert!(!s.ended[2]);
    }
}

#[tokio::test]
async fn ended_queue_resumes_after_a_real_competing_subscriber() {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    let mut owner = Fixture::new();
    s.source = owner.source.clone();
    s.queue = Some(s.source.subscribe(None).unwrap());
    s.queue_started = true;
    s.queue_wake = s.queue.as_ref().unwrap().waker();
    s.ended = [true, true, false];
    owner.capture();
    for n in 0..5 {
        owner.append(n);
    }
    owner.finish(false);
    assert!(s.next().await.unwrap());
    assert!(s.queue.is_none(), "overflow reader must be spent");
    {
        let mut step = std::pin::pin!(s.next());
        owner.competing_subscriber(|| assert!(poll(step.as_mut()).is_pending()));
        woke(step).await;
    }
    drain(&mut s).await;
    assert_eq!(s.model.snapshot.queue.len(), 5);
    assert!(s.ended[2]);
}

const WITHIN: std::time::Duration = std::time::Duration::from_secs(10);
pub(super) struct CancelGate {
    entered: tokio::sync::mpsc::UnboundedSender<u8>,
    release: std::sync::mpsc::Receiver<()>,
}
impl CancelGate {
    pub fn pause(&self, stage: u8) {
        self.entered.send(stage).unwrap();
        self.release
            .recv_timeout(WITHIN)
            .expect("cancel test abandoned page producer");
    }
}
async fn cancel_page(stage: u8) {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    let records = (1..=2).map(|seq| crate::structured::transport::StreamRecord {
        seq, known: Some(true), shape: None, received_at_ms: 1,
        origin: crate::structured::transport::RecordOrigin::Claude,
        result_subtype: None, permission_grant_request_id: None, permission_request_seq: None,
        raw: serde_json::json!({"text": if seq == 1 { "x".repeat(history::PAGE_BYTES + 100) } else { "last".into() }}).to_string(),
    }).collect();
    let source = history::tests::Fixture::new(records);
    source.source.finish(false);
    s.history = source.source.clone();
    s.ended = [false, true, true];
    let mut cut = s.history.subscribe(Some(0)).unwrap();
    assert!(matches!(cut.page().unwrap(), history::Item::Progress));
    s.cut = Some(cut);
    let (entered, mut stages) = tokio::sync::mpsc::unbounded_channel();
    let (release, gate) = std::sync::mpsc::channel();
    s.gate = Some(CancelGate {
        entered,
        release: gate,
    });
    let mut step = Box::pin(s.next());
    assert!(poll(step.as_mut()).is_pending());
    assert_eq!(
        tokio::time::timeout(WITHIN, stages.recv()).await.unwrap(),
        Some(0)
    );
    if stage == 1 {
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(WITHIN, stages.recv()).await.unwrap(),
            Some(1)
        );
    }
    drop(step);
    release.send(()).unwrap();
    if stage == 0 {
        assert_eq!(
            tokio::time::timeout(WITHIN, stages.recv()).await.unwrap(),
            Some(1)
        );
        release.send(()).unwrap();
    }
    tokio::time::timeout(WITHIN, s.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        s.model.snapshot.applied_cursor, 1,
        "cancelled page must resume its existing cut"
    );
    drain(&mut s).await;
    assert_eq!(
        s.model
            .snapshot
            .transcript
            .iter()
            .map(|c| c.seq)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        s.cut.as_ref().unwrap().read_bytes(),
        source.file.metadata().unwrap().len()
    );
}
#[tokio::test]
async fn cancellation_before_page_keeps_the_inflight_cut() {
    cancel_page(0).await;
}
#[tokio::test]
async fn cancellation_after_page_keeps_the_unapplied_record() {
    cancel_page(1).await;
}

#[tokio::test]
async fn cancellation_of_readiness_wait_rechecks_the_source() {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    let mut source = history::tests::Fixture::new(vec![]);
    s.history = source.source.clone();
    s.ended = [false, true, true];
    assert!(s.next().await.unwrap());
    let (waiting, ready) = tokio::sync::oneshot::channel();
    s.waiting = Some(waiting);
    let mut step = Box::pin(s.next());
    tokio::time::timeout(WITHIN, async {
        tokio::select! { result = &mut step => panic!("expected readiness wait: {result:?}"), _ = ready => () }
    }).await.unwrap();
    drop(step);
    let record = crate::structured::transport::StreamRecord {
        seq: 1,
        known: Some(true),
        shape: None,
        received_at_ms: 1,
        origin: crate::structured::transport::RecordOrigin::Claude,
        result_subtype: None,
        permission_grant_request_id: None,
        permission_request_seq: None,
        raw: "{}".into(),
    };
    source.append(record);
    source.source.finish(false);
    drain(&mut s).await;
    assert_eq!(s.model.snapshot.applied_cursor, 1);
    assert_eq!(s.model.snapshot.transcript.len(), 1);
}
#[tokio::test]
async fn cancellation_of_busy_resubscribe_preserves_its_cursor() {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    let mut owner = Fixture::new();
    s.source = owner.source.clone();
    s.queue = Some(s.source.subscribe(None).unwrap());
    s.queue_started = true;
    s.queue_wake = s.queue.as_ref().unwrap().waker();
    s.ended = [true, true, false];
    owner.capture();
    for n in 0..5 {
        owner.append(n);
    }
    assert!(s.next().await.unwrap());
    owner.competing_subscriber(|| {
        let mut step = Box::pin(s.next());
        assert!(poll(step.as_mut()).is_pending());
        drop(step);
    });
    assert!(s.queue.is_none());
    {
        let mut step = Box::pin(s.next());
        assert!(poll(step.as_mut()).is_pending());
        owner.capture();
        woke(step).await;
    }
    woke(s.next()).await;
    owner.capture();
    owner.finish(false);
    drain(&mut s).await;
    assert_eq!(
        s.model
            .snapshot
            .queue
            .iter()
            .map(|v| v.item_id.as_str())
            .collect::<Vec<_>>(),
        (0..5).map(|n| format!("item-{n}")).collect::<Vec<_>>()
    );
}
#[tokio::test]
async fn cancelled_pending_page_cannot_hide_a_publication() {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    let mut source = history::tests::Fixture::new(vec![]);
    s.history = source.source.clone();
    s.ended = [false, true, true];
    assert!(s.next().await.unwrap());
    let (entered, mut stages) = tokio::sync::mpsc::unbounded_channel();
    let (release, gate) = std::sync::mpsc::channel();
    s.gate = Some(CancelGate {
        entered,
        release: gate,
    });
    let mut step = Box::pin(s.next());
    assert!(poll(step.as_mut()).is_pending());
    assert_eq!(
        tokio::time::timeout(WITHIN, stages.recv()).await.unwrap(),
        Some(0)
    );
    release.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(WITHIN, stages.recv()).await.unwrap(),
        Some(1)
    );
    drop(step);
    source.append(crate::structured::transport::StreamRecord {
        seq: 1,
        known: Some(true),
        shape: None,
        received_at_ms: 1,
        origin: crate::structured::transport::RecordOrigin::Claude,
        result_subtype: None,
        permission_grant_request_id: None,
        permission_request_seq: None,
        raw: "{}".into(),
    });
    source.source.finish(false);
    release.send(()).unwrap();
    tokio::time::timeout(WITHIN, s.next())
        .await
        .expect("stale Pending page lost the publication wake")
        .unwrap();
    drain(&mut s).await;
    assert_eq!(s.model.snapshot.applied_cursor, 1);
    assert_eq!(s.model.snapshot.transcript.len(), 1);
}
#[tokio::test]
async fn opening_during_book_contention_defers_admission_until_polling() {
    let (_app, _case, s) = fixture("s10b-live-lifecycle-exit").await;
    let mut owner = Fixture::new();
    owner.finish(false);
    let mut opened = None;
    owner.competing_subscriber(|| {
        let mut view = Stream::new(s.model, s.history, s.live, owner.source.clone())
            .expect("opening a view must defer transient book contention");
        view.ended = [true, true, false];
        let mut step = Box::pin(view.next());
        assert!(poll(step.as_mut()).is_pending());
        drop(step);
        opened = Some(view);
    });
    let mut view = opened.unwrap();
    drain(&mut view).await;
    assert!(view.ended[2]);
}

#[tokio::test]
async fn driver_coalesces_facts_and_fences_a_retired_generation() {
    use crate::structured::view::driver::{Driver, Facts};
    let (_app, _case, stream) = fixture("s10b-live-lifecycle-exit").await;
    let wake = Arc::new(Notify::new());
    let driver = Driver::new(stream, wake.clone());
    let old = driver.frame().unwrap();
    for name in ["intermediate", "latest"] {
        let mut agent = old.agent.clone();
        agent.name = Some(name.into());
        driver.refresh(Facts {
            target: old.target.clone(),
            agent,
        });
    }
    wait(&wake, || {
        driver
            .frame()
            .is_ok_and(|f| f.agent.name.as_deref() == Some("latest"))
    })
    .await;
    assert_ne!(old.agent.name.as_deref(), Some("latest"));
    let mut retired = old.target.clone();
    retired.generation += 1;
    driver.refresh(Facts {
        target: retired,
        agent: old.agent.clone(),
    });
    wait(&wake, || {
        matches!(driver.frame(), Err(ErrorCode::AgentNotFound))
    })
    .await;
    assert_eq!(old.target.generation, driver.target.generation);
}

#[tokio::test]
async fn driver_preserves_a_terminal_source_refusal() {
    use crate::structured::view::driver::Driver;
    let (_app, _case, mut stream) = fixture("s10b-live-lifecycle-exit").await;
    let mut owner = Fixture::new();
    owner.finish(true);
    stream.source = owner.source.clone();
    stream.queue = None;
    stream.queue_started = false;
    stream.queue_wake = stream.source.waker();
    let wake = Arc::new(Notify::new());
    let driver = Driver::new(stream, wake.clone());
    wait(&wake, || {
        matches!(driver.frame(), Err(ErrorCode::InternalError))
    })
    .await;
}

async fn warm_opening(failed: bool) {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    let pending = History::default();
    s.history = pending.clone();
    s.ended = [false, true, true];
    let (waiting, entered) = tokio::sync::oneshot::channel();
    s.waiting = Some(waiting);
    let mut step = Box::pin(s.next());
    tokio::time::timeout(WITHIN, async {
        tokio::select! {
            result = &mut step => panic!("warm opening must wait for readiness: {result:?}"),
            result = entered => result.unwrap(),
        }
    })
    .await
    .expect("warm opening never reached its readiness wait");
    drop(step);
    if failed {
        pending.finish(true);
        let error = tokio::time::timeout(WITHIN, s.next())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
    } else {
        let source =
            history::tests::Fixture::new(vec![crate::structured::transport::StreamRecord {
                seq: 1,
                known: Some(false),
                shape: None,
                received_at_ms: 1,
                origin: crate::structured::transport::RecordOrigin::Claude,
                result_subtype: None,
                permission_grant_request_id: None,
                permission_request_seq: None,
                raw: "{\"future_field\":\"ready\"}".into(),
            }]);
        pending
            .initialize(&source.file, 1, source.file.metadata().unwrap().len())
            .unwrap();
        pending.finish(false);
        drain(&mut s).await;
        assert_eq!(s.model.snapshot.applied_cursor, 1);
        assert_eq!(
            s.model
                .snapshot
                .transcript
                .iter()
                .filter(|c| c.seq == 1)
                .count(),
            1
        );
    }
}
#[tokio::test]
async fn warm_history_opening_survives_cancellation_and_initialization() {
    warm_opening(false).await;
}
#[tokio::test]
async fn warm_history_opening_reports_a_producer_failure() {
    warm_opening(true).await;
}

#[tokio::test]
async fn cancelled_opening_cannot_hide_initialization_before_its_next_poll() {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    let history = History::default();
    s.history = history.clone();
    s.ended = [false, true, true];
    let source = history::tests::Fixture::new(vec![]);
    let (entered, mut stages) = tokio::sync::mpsc::unbounded_channel();
    let (release, gate) = std::sync::mpsc::channel();
    s.gate = Some(CancelGate {
        entered,
        release: gate,
    });
    let mut step = Box::pin(s.next());
    tokio::time::timeout(WITHIN, async {
        tokio::select! {
            result = &mut step => panic!("opening terminated before the gate: {result:?}"),
            stage = stages.recv() => assert_eq!(stage, Some(2)),
        }
    })
    .await
    .unwrap();
    drop(step);
    history.initialize(&source.file, 0, 0).unwrap();
    history.finish(false);
    release.send(()).unwrap();
    drain(&mut s).await;
    assert_eq!(s.ended, [true; 3]);
}
