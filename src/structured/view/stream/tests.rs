use super::*;
use crate::{
    app::App,
    structured::{
        registry::Registry,
        view::Target,
        workers::{
            tests::{wait, Case},
            Workers,
        },
    },
};
use std::{sync::Arc, time::Instant};
use tokio::sync::Notify;
async fn fixture(mode: &str) -> (App, Case, Stream) {
    let notify = Arc::new(Notify::new());
    let (mut registry, mut workers) = (Registry::default(), Workers::new(notify.clone()));
    let (mut case, mut launch, factory) = Case::prepare(&mut registry, 929, "unused", vec![]);
    let mut record = registry.remove(&case.id, case.generation).unwrap();
    record.model = Some(mode.into());
    launch.generation = registry.register(record).unwrap();
    let executable = case.fixture.root.path().join("claude");
    let interpreter = std::fs::read_to_string(&executable)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    case.fixture.file(
        "claude",
        format!(
            "{interpreter}\n{}",
            include_str!("../../../../tests/fixtures/structured-public/claude.py")
        )
        .as_bytes(),
        0o700,
    );
    let mut journal = queue::Journal::open(case.fixture.root.path(), &case.id).unwrap();
    let mut item = queue::tests::item("held-item");
    item.state = queue::model::State::Held;
    journal
        .append(queue::Change::Upsert {
            item: Box::new(item),
            position: 0,
        })
        .unwrap();
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
    let mut recovery = workers
        .recover_queue(
            &registry,
            case.fixture.root.path().into(),
            &case.id,
            case.generation,
            policy,
        )
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
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        Default::default(),
    );
    app.state.structured_agents = registry;
    app.structured_workers = workers;
    let target = Target {
        agent_id: case.id.clone(),
        generation: case.generation,
    };
    let mut stream = app
        .open_structured_view(target, Default::default())
        .unwrap();
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
    s.model
        .snapshot
        .transcript
        .iter()
        .filter(|c| matches!(&c.content, Content::Markdown(t) if t == "partial"))
        .count()
}
#[tokio::test]
async fn real_producer_replay_live_queue_and_death() {
    let (_app, _case, mut s) = fixture("s10b-live-lifecycle-exit").await;
    drain(&mut s).await;
    assert_eq!(s.model.snapshot.applied_cursor, 3);
    assert!(matches!(s.model.snapshot.replay, Replay::Ended));
    assert_eq!(s.model.snapshot.queue.len(), 1);
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
    assert_eq!(
        s.model.snapshot.agent.model.as_deref(),
        Some("s10b-live-lifecycle-exit")
    );
    app.state
        .structured_agents
        .begin_launch(&case.id, case.generation)
        .unwrap();
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
    assert!(!s.next().await.unwrap());
}
