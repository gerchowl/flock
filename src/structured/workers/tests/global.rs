use super::*;
use crate::api::{
    queue_event::{QueueItemState, QueueState},
    schema::{EventData, EventKind},
    EventHub,
};
use crate::app::{queue_events::Observation, App};
use crate::structured::queue::{
    admission::{Gate, Principal as QueuePrincipal},
    model::{Kind, Producer, State},
    scheduler::PendingCause,
    Change, Journal,
};
use std::{
    io::Write,
    num::NonZeroU64,
    os::unix::net::{UnixListener, UnixStream},
};

pub(crate) struct AppQueue {
    pub(crate) app: App,
    pub(crate) case: Case,
    pub(crate) notify: Arc<Notify>,
    pub(crate) driver: UnixStream,
    pub(crate) hub: EventHub,
    pub(crate) log: std::path::PathBuf,
}
impl AppQueue {
    pub(crate) async fn new(n: u32, seed: impl FnOnce(&Case) -> NonZeroU64) -> Self {
        let (mut registry, workers, notify, _) = owner();
        let (mut case, mut launch, factory) = Case::prepare(&mut registry, n, "queue", vec![]);
        launch.queue_rewrite_bytes = seed(&case);
        let log = case.fixture.root.path().join("event-log.jsonl");
        let hub = EventHub::with_persistence(log.clone());
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            hub.clone(),
        );
        app.state.structured_agents = registry;
        app.structured_workers = workers;
        let listener = UnixListener::bind(case.fixture.root.path().join("queue.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        case.generation = app
            .structured_workers
            .start(&mut app.state.structured_agents, launch, factory)
            .unwrap();
        let mut driver = None;
        wait(&notify, || {
            app.service_structured_workers(Instant::now());
            assert!(
                app.structured_workers
                    .report(&case.id, case.generation)
                    .is_none(),
                "worker died before handshake"
            );
            match listener.accept() {
                Ok((socket, _)) => {
                    driver = Some(socket);
                    true
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
                Err(e) => panic!("fixture handshake: {e}"),
            }
        })
        .await;
        let driver = driver.unwrap();
        driver.set_write_timeout(Some(LIMIT)).unwrap();
        Self {
            app,
            case,
            notify,
            driver,
            hub,
            log,
        }
    }
    pub(crate) async fn until(&mut self, mut condition: impl FnMut(&Self) -> bool) {
        let notify = self.notify.clone();
        wait(&notify, || {
            self.app.service_structured_workers(Instant::now());
            assert!(
                self.app
                    .structured_workers
                    .report(&self.case.id, self.case.generation)
                    .is_none(),
                "worker died before condition"
            );
            condition(self)
        })
        .await;
    }
    pub(crate) fn facts(&self) -> Vec<(u64, QueueItemState)> {
        self.hub
            .events_after(0)
            .into_iter()
            .filter_map(|(seq, e)| match e.data {
                EventData::QueueItemState(fact) => Some((seq, fact)),
                _ => None,
            })
            .collect()
    }
    pub(crate) fn control(&mut self, value: Value) {
        writeln!(self.driver, "{value}").unwrap();
    }
    pub(crate) fn inputs(&self) -> Vec<Value> {
        let path = self
            .case
            .fixture
            .root
            .path()
            .join("sessions")
            .join(&self.case.id)
            .join(format!("{}.jsonl", self.case.session));
        fs::read_to_string(path)
            .unwrap_or_default()
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .map(|line| {
                let record: Value = serde_json::from_str(line).unwrap();
                serde_json::from_str::<Value>(record["raw"].as_str().unwrap()).unwrap()
            })
            .filter(|v| v["type"] == "fixture_input")
            .collect()
    }
    pub(crate) async fn restart(&mut self, before_ready: impl FnOnce(&mut Self)) {
        let socket = self.case.fixture.root.path().join("queue.sock");
        fs::remove_file(&socket).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let launch = Launch {
            agent_id: self.case.id.clone(),
            generation: self.case.generation,
            leases: self.case.leases.clone(),
            state: self.case.fixture.root.path().to_owned(),
            storage: Default::default(),
            mcp: self.case.fixture.mcp(),
            stall_after: STALL,
            queue_rewrite_bytes: max_policy(),
            lineage: None,
        };
        self.case.generation = self
            .app
            .structured_workers
            .start(
                &mut self.app.state.structured_agents,
                launch,
                self.case.factory("queue", vec![]),
            )
            .unwrap();
        before_ready(self);
        let mut driver = None;
        self.until(|_| match listener.accept() {
            Ok((stream, _)) => {
                driver = Some(stream);
                true
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
            Err(e) => panic!("restart handshake: {e}"),
        })
        .await;
        self.driver = driver.unwrap();
        self.driver.set_write_timeout(Some(LIMIT)).unwrap();
    }
    pub(crate) async fn stop(&mut self) {
        self.app
            .structured_workers
            .stop(&self.case.id, self.case.generation);
        let notify = self.notify.clone();
        wait(&notify, || {
            self.app.service_structured_workers(Instant::now());
            self.app
                .structured_workers
                .report(&self.case.id, self.case.generation)
                .is_some()
        })
        .await;
    }
}
pub(crate) fn max_policy() -> NonZeroU64 {
    NonZeroU64::new(u64::MAX).unwrap()
}
pub(crate) fn id(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

#[tokio::test]
async fn queue_global_real_app_admission_publishes_only_lifecycle_and_provider_status() {
    let mut r = AppQueue::new(971, |_| max_policy()).await;
    r.until(|r| {
        matches!(
            r.app.queue_observation(&r.case.id),
            Some(Observation::Status(None, None))
        )
    })
    .await;
    let mut item = crate::structured::queue::tests::item(&id(971));
    item.kind = Kind::Command;
    item.body = "/compact".into();
    r.app
        .structured_workers
        .enqueue(
            &r.app.state.structured_agents,
            &r.case.id,
            r.case.generation,
            super::super::queue::Request {
                item,
                principal: QueuePrincipal::Operator,
                gate: Gate::Send,
            },
        )
        .unwrap();
    r.until(|r| matches!(r.app.queue_observation(&r.case.id), Some(Observation::Status(Some(p),_)) if p.cause == PendingCause::AwaitingResult)).await;
    assert!(
        r.facts().is_empty(),
        "add and submission metadata are not state transitions"
    );
    r.control(json!({"op":"lifecycle","state":"started","uuid":id(971)}));
    r.until(|r| !r.facts().is_empty()).await;
    let facts = r.facts();
    assert_eq!(facts.len(), 1);
    let (global_seq, fact) = &facts[0];
    assert_eq!(
        (fact.from, fact.to),
        (QueueState::Queued, QueueState::Delivered)
    );
    assert_eq!(fact.item_id, id(971));
    assert_eq!(fact.batch_id.as_deref(), Some(id(971).as_str()));
    assert_eq!(fact.producer, Producer::Human);
    assert_eq!(fact.agent, r.case.id);
    assert_eq!(fact.stream, format!("queue/{}", r.case.id));
    assert_ne!(*global_seq, fact.stream_seq);
    let disk = fs::read_to_string(
        r.case
            .fixture
            .root
            .path()
            .join("sessions")
            .join(&r.case.id)
            .join("queue.jsonl"),
    )
    .unwrap();
    assert!(disk
        .lines()
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .any(|v| v["seq"] == fact.stream_seq && v["items"][0]["state"] == "delivered"));
    r.control(json!({"op":"result"}));
    r.until(|r| r.facts().len() == 2).await;
    assert_eq!(
        (r.facts()[1].1.from, r.facts()[1].1.to),
        (QueueState::Delivered, QueueState::Done)
    );
    r.stop().await;
    assert_eq!(r.facts().len(), 2);
    let restarted = EventHub::with_persistence(r.log.clone());
    let replay = restarted
        .persisted_events_after(0)
        .into_iter()
        .map(|(seq, _, e)| (seq, e))
        .collect::<Vec<_>>();
    assert_eq!(replay, r.hub.events_after(0));
    assert!(restarted.persisted_events_after(2).is_empty());
    assert!(replay
        .iter()
        .all(|(_, e)| e.event == EventKind::QueueItemState));
}

#[tokio::test]
async fn queue_global_crash_rehold_keeps_the_transition_seq_before_bootstrap_rewrite() {
    let mut r = AppQueue::new(972, |case| {
        let mut journal = Journal::open(case.fixture.root.path(), &case.id).unwrap();
        let mut item = crate::structured::queue::tests::item(&id(972));
        item.state = State::Delivered;
        item.producer = Producer::Agent("sender-fixture".into());
        item.metadata.insert("awaiting_result".into(), json!(true));
        item.metadata.insert("batch_id".into(), json!(id(972)));
        journal
            .append(Change::Upsert {
                item: Box::new(item),
                position: 0,
            })
            .unwrap();
        max_policy()
    })
    .await;
    r.until(|r| !r.facts().is_empty()).await;
    let facts = r.facts();
    assert_eq!(facts.len(), 1);
    assert_eq!(
        (facts[0].1.from, facts[0].1.to),
        (QueueState::Delivered, QueueState::Held)
    );
    assert_eq!(facts[0].1.stream_seq, 2);
    assert_eq!(
        facts[0].1.producer,
        Producer::Agent("sender-fixture".into())
    );
    assert_eq!(facts[0].1.batch_id.as_deref(), Some(id(972).as_str()));
    r.until(|r| {
        matches!(
            r.app.queue_observation(&r.case.id),
            Some(Observation::Status(..))
        )
    })
    .await;
    r.stop().await;
    let mut journal = Journal::open(r.case.fixture.root.path(), &r.case.id).unwrap();
    assert!(journal.replay(0).unwrap().through >= 3);
    assert!(journal.take_facts().is_empty());
    assert_eq!(
        r.facts().len(),
        1,
        "bootstrap and loading never synthesize a second global fact"
    );
}

#[tokio::test]
async fn queue_global_real_app_exposes_readonly_provider_refusal_without_global_fact() {
    let mut r = AppQueue::new(973, |case| {
        let journal = Journal::open(case.fixture.root.path(), &case.id).unwrap();
        drop(journal);
        let path = case
            .fixture
            .root
            .path()
            .join("sessions")
            .join(&case.id)
            .join("queue.jsonl");
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{{\"kind\":\"future\",\"seq\":1}}").unwrap();
        file.sync_all().unwrap();
        NonZeroU64::new(1).unwrap()
    })
    .await;
    r.until(|r| {
        matches!(
            r.app.queue_observation(&r.case.id),
            Some(Observation::Refused(ErrorCode::QueueJournalCorrupt))
        )
    })
    .await;
    assert!(r.facts().is_empty());
    r.stop().await;
}

#[tokio::test]
async fn queue_global_terminal_transition_survives_full_completion_channel() {
    let mut r = AppQueue::new(974, |_| max_policy()).await;
    r.until(|r| {
        matches!(
            r.app.queue_observation(&r.case.id),
            Some(Observation::Status(None, None))
        )
    })
    .await;
    let mut item = crate::structured::queue::tests::item(&id(974));
    item.kind = Kind::Command;
    item.body = "/compact".into();
    r.app
        .structured_workers
        .enqueue(
            &r.app.state.structured_agents,
            &r.case.id,
            r.case.generation,
            super::super::queue::Request {
                item,
                principal: QueuePrincipal::Operator,
                gate: Gate::Send,
            },
        )
        .unwrap();
    r.until(|r| matches!(r.app.queue_observation(&r.case.id),Some(Observation::Status(Some(p),_)) if p.cause==PendingCause::AwaitingResult)).await;
    r.control(json!({"op":"lifecycle","state":"started","uuid":id(974)}));
    r.until(|r| r.facts().len() == 1).await;
    let path = r
        .case
        .fixture
        .root
        .path()
        .join("sessions")
        .join(&r.case.id)
        .join("queue.jsonl");
    for n in 975..979 {
        r.app
            .structured_workers
            .enqueue(
                &r.app.state.structured_agents,
                &r.case.id,
                r.case.generation,
                super::super::queue::Request {
                    item: crate::structured::queue::tests::item(&id(n)),
                    principal: QueuePrincipal::Operator,
                    gate: Gate::Hold,
                },
            )
            .unwrap();
        let notify = r.notify.clone();
        wait(&notify, || {
            r.app
                .structured_workers
                .poll(&mut r.app.state.structured_agents, Instant::now(), 1000);
            assert!(
                r.app
                    .structured_workers
                    .report(&r.case.id, r.case.generation)
                    .is_none(),
                "producer died while filling completions"
            );
            fs::read_to_string(&path)
                .unwrap()
                .lines()
                .filter_map(|s| serde_json::from_str::<Value>(s).ok())
                .any(|v| v["item"]["item_id"] == id(n))
        })
        .await;
    }
    r.app.structured_workers.stop(&r.case.id, r.case.generation);
    let notify = r.notify.clone();
    wait(&notify, || {
        r.app
            .structured_workers
            .poll(&mut r.app.state.structured_agents, Instant::now(), 1000);
        r.app
            .structured_workers
            .report(&r.case.id, r.case.generation)
            .is_some()
    })
    .await;
    // The producer is finished: two bounded app passes consume four cuts then final facts.
    r.app.service_structured_workers(Instant::now());
    r.app.service_structured_workers(Instant::now());
    let facts = r.facts();
    assert_eq!(facts.len(), 2);
    assert_eq!(
        (facts[1].1.from, facts[1].1.to),
        (QueueState::Delivered, QueueState::Held)
    );
    r.app.service_structured_workers(Instant::now());
    assert_eq!(r.facts().len(), 2, "terminal facts are consumed once");
}

#[tokio::test]
async fn queue_global_retired_registry_row_does_not_erase_a_committed_exit_transition() {
    let mut r = AppQueue::new(979, |_| max_policy()).await;
    r.until(|r| {
        matches!(
            r.app.queue_observation(&r.case.id),
            Some(Observation::Status(None, None))
        )
    })
    .await;
    let mut item = crate::structured::queue::tests::item(&id(979));
    item.kind = Kind::Command;
    item.body = "/compact".into();
    r.app
        .structured_workers
        .enqueue(
            &r.app.state.structured_agents,
            &r.case.id,
            r.case.generation,
            super::super::queue::Request {
                item,
                principal: QueuePrincipal::Operator,
                gate: Gate::Send,
            },
        )
        .unwrap();
    r.until(|r| matches!(r.app.queue_observation(&r.case.id),Some(Observation::Status(Some(p),_)) if p.cause==PendingCause::AwaitingResult)).await;
    r.control(json!({"op":"lifecycle","state":"started","uuid":id(979)}));
    r.until(|r| r.facts().len() == 1).await;
    r.app.structured_workers.stop(&r.case.id, r.case.generation);
    let notify = r.notify.clone();
    wait(&notify, || {
        r.app
            .structured_workers
            .poll(&mut r.app.state.structured_agents, Instant::now(), 1000);
        r.app
            .structured_workers
            .report(&r.case.id, r.case.generation)
            .is_some()
    })
    .await;
    r.app
        .state
        .structured_agents
        .remove(&r.case.id, r.case.generation)
        .unwrap();
    r.app.service_structured_workers(Instant::now());
    r.app.service_structured_workers(Instant::now());
    let facts = r.facts();
    assert_eq!(facts.len(), 2);
    assert_eq!(
        (facts[1].1.from, facts[1].1.to),
        (QueueState::Delivered, QueueState::Held)
    );
    assert!(r.app.queue_observation(&r.case.id).is_none());
}

#[tokio::test]
async fn queue_global_status_getter_never_returns_an_old_generation_observation() {
    let mut r = AppQueue::new(980, |_| max_policy()).await;
    r.until(|r| {
        matches!(
            r.app.queue_observation(&r.case.id),
            Some(Observation::Status(None, None))
        )
    })
    .await;
    r.app.structured_workers.stop(&r.case.id, r.case.generation);
    let notify = r.notify.clone();
    wait(&notify, || {
        r.app
            .structured_workers
            .poll(&mut r.app.state.structured_agents, Instant::now(), 1000);
        r.app
            .structured_workers
            .report(&r.case.id, r.case.generation)
            .is_some()
    })
    .await;
    let generation = r
        .app
        .state
        .structured_agents
        .begin_launch(&r.case.id, r.case.generation)
        .unwrap();
    assert_ne!(generation, r.case.generation);
    assert!(
        r.app.queue_observation(&r.case.id).is_none(),
        "cached old epoch must be fenced even before the next app poll"
    );
    r.app.service_structured_workers(Instant::now());
    assert!(matches!(
        r.app.queue_observation(&r.case.id),
        Some(Observation::Refused(ErrorCode::QueueSourceCold))
    ));
    assert_eq!(ErrorCode::QueueSourceCold.retry().as_str(), "later");
}

async fn gate_pending(n: u32, signal: &str, cause: PendingCause) {
    let mut r = AppQueue::new(n, |_| max_policy()).await;
    r.until(|r| {
        matches!(
            r.app.queue_observation(&r.case.id),
            Some(Observation::Status(None, None))
        )
    })
    .await;
    let mut signal_event = event(signal);
    if signal == "rate_limit_event" {
        signal_event["rate_limit_info"]["status"] = json!("rejected");
    }
    r.control(json!({"op":"event","event":signal_event}));
    r.until(|r| {
        r.app
            .state
            .structured_agents
            .get(&r.case.id)
            .unwrap()
            .1
            .update
            .state
            .attention
            != Attention::None
    })
    .await;
    let mut item = crate::structured::queue::tests::item(&id(n));
    item.kind = Kind::Command;
    item.body = "/compact".into();
    r.app
        .structured_workers
        .enqueue(
            &r.app.state.structured_agents,
            &r.case.id,
            r.case.generation,
            super::super::queue::Request {
                item,
                principal: QueuePrincipal::Operator,
                gate: Gate::Send,
            },
        )
        .unwrap();
    r.until(|r| matches!(r.app.queue_observation(&r.case.id),Some(Observation::Status(Some(p),_)) if p.cause==cause)).await;
    assert!(
        r.facts().is_empty(),
        "an unoffered queued item has no state transition"
    );
    r.stop().await;
}
#[tokio::test]
async fn queue_global_s4_gate_pending_is_reported_without_a_reservation() {
    gate_pending(981, "rate_limit_event", PendingCause::StateNotReady).await;
}
#[tokio::test]
async fn queue_global_s6_gate_pending_is_reported_without_a_reservation() {
    gate_pending(982, "control_request", PendingCause::WaitingOnHuman).await;
}

#[tokio::test]
async fn queue_global_server_shutdown_publishes_committed_final_facts_without_another_tick() {
    let mut r = AppQueue::new(983, |_| max_policy()).await;
    r.until(|r| {
        matches!(
            r.app.queue_observation(&r.case.id),
            Some(Observation::Status(None, None))
        )
    })
    .await;
    let mut item = crate::structured::queue::tests::item(&id(983));
    item.kind = Kind::Command;
    item.body = "/compact".into();
    r.app
        .structured_workers
        .enqueue(
            &r.app.state.structured_agents,
            &r.case.id,
            r.case.generation,
            super::super::queue::Request {
                item,
                principal: QueuePrincipal::Operator,
                gate: Gate::Send,
            },
        )
        .unwrap();
    r.until(|r| matches!(r.app.queue_observation(&r.case.id),Some(Observation::Status(Some(p),_)) if p.cause==PendingCause::AwaitingResult)).await;
    r.control(json!({"op":"lifecycle","state":"started","uuid":id(983)}));
    r.until(|r| r.facts().len() == 1).await;
    r.app.shutdown_structured_workers().unwrap();
    let journal = Journal::open(r.case.fixture.root.path(), &r.case.id).unwrap();
    assert_eq!(
        journal.items()[0].state,
        State::Held,
        "shutdown committed its transition"
    );
    let facts = r.facts();
    assert_eq!(
        facts.len(),
        2,
        "server must drain final facts before relinquishing its global log"
    );
    assert_eq!(
        (facts[1].1.from, facts[1].1.to),
        (QueueState::Delivered, QueueState::Held)
    );
}

#[tokio::test]
async fn queue_global_empty_completed_worker_does_not_require_a_fact_consumer_for_retirement() {
    let (mut registry, mut workers, notify, now) = owner();
    let case = Case::start(&mut registry, &mut workers, 984, "idle", vec![]);
    case.ready(&mut registry, &mut workers, &notify, now).await;
    workers.stop(&case.id, case.generation);
    case.finish(
        &mut registry,
        &mut workers,
        &notify,
        now,
        Some(Outcome::Stopped),
    )
    .await;
    registry.remove(&case.id, case.generation).unwrap();
    workers.poll(&mut registry, now, 1000);
    assert!(
        !workers.entries.contains_key(&case.id),
        "empty notifications must not retain a removed producer"
    );
}
