use super::*;
use crate::structured::queue::{
    admission::{Gate, Principal as QueuePrincipal},
    model::{Kind, State},
    scheduler::PendingCause,
    Change, Journal,
};
use crate::structured::workers::queue as owner_queue;
use std::io::Write;
use std::num::NonZeroU64;
use std::os::unix::net::{UnixListener, UnixStream};

fn id(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}
fn request(n: u32) -> owner_queue::Request {
    let mut item = crate::structured::queue::tests::item(&id(n));
    item.kind = Kind::Command;
    item.body = "/compact".into();
    owner_queue::Request {
        item,
        principal: QueuePrincipal::Operator,
        gate: Gate::Send,
    }
}
struct Run {
    registry: Registry,
    workers: Workers,
    notify: Arc<Notify>,
    case: Case,
    driver: UnixStream,
    updates: Vec<owner_queue::Update>,
    threshold: NonZeroU64,
}
impl Run {
    async fn new(n: u32, mode: &str, seed: impl FnOnce(&Case) -> NonZeroU64) -> Self {
        Self::with_opening(n, mode, seed, None).await
    }
    async fn with_opening(
        n: u32,
        mode: &str,
        seed: impl FnOnce(&Case) -> NonZeroU64,
        opening: Option<(String, crate::spawn::prompt::SpawnPrompt)>,
    ) -> Self {
        let (mut registry, mut workers, notify, _) = owner();
        let (mut case, mut launch, factory) = Case::prepare(&mut registry, n, mode, vec![]);
        launch.opening_turn = opening;
        let threshold = seed(&case);
        launch.queue_rewrite_bytes = threshold;
        let listener = UnixListener::bind(case.fixture.root.path().join("queue.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        case.generation = workers.start(&mut registry, launch, factory).unwrap();
        let mut updates = Vec::new();
        let mut driver = None;
        wait(&notify, || {
            workers.poll(&mut registry, Instant::now(), 1000);
            while let Some(update) = workers.queue_update(&registry, &case.id, case.generation) {
                updates.push(update);
            }
            assert!(
                workers.report(&case.id, case.generation).is_none(),
                "producer ended before socket handshake"
            );
            match listener.accept() {
                Ok((stream, _)) => {
                    driver = Some(stream);
                    true
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
                Err(e) => panic!("queue socket stage: {e}"),
            }
        })
        .await;
        let driver = driver.unwrap();
        driver.set_write_timeout(Some(LIMIT)).unwrap();
        let mut run = Self {
            registry,
            workers,
            notify,
            case,
            driver,
            updates,
            threshold,
        };
        run.until(|r| r.raws().iter().any(|v| v["subtype"] == "init"))
            .await;
        run
    }
    fn pump(&mut self) {
        self.workers.poll(&mut self.registry, Instant::now(), 1000);
        while let Some(update) =
            self.workers
                .queue_update(&self.registry, &self.case.id, self.case.generation)
        {
            self.updates.push(update);
        }
    }
    async fn until(&mut self, mut condition: impl FnMut(&Self) -> bool) {
        let notify = self.notify.clone();
        wait(&notify, || {
            self.pump();
            assert!(
                self.workers
                    .report(&self.case.id, self.case.generation)
                    .is_none(),
                "producer ended before expected queue stage"
            );
            condition(self)
        })
        .await;
    }
    fn raws(&self) -> Vec<Value> {
        let path = self
            .case
            .fixture
            .root
            .path()
            .join("sessions")
            .join(&self.case.id)
            .join(format!("{}.jsonl", self.case.session));
        match fs::read_to_string(path) {
            Ok(text) => text
                .split_inclusive('\n')
                .filter(|s| s.ends_with('\n'))
                .map(|line| {
                    let envelope: Value = serde_json::from_str(line).unwrap();
                    serde_json::from_str(envelope["raw"].as_str().unwrap()).unwrap()
                })
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
            Err(e) => panic!("durable session stream: {e}"),
        }
    }
    fn inputs(&self) -> Vec<Value> {
        self.raws()
            .into_iter()
            .filter(|v| v["type"] == "fixture_input")
            .collect()
    }
    fn control(&mut self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        self.driver.write_all(&bytes).unwrap();
    }
    async fn admit(&mut self, n: u32) {
        self.workers
            .enqueue(
                &self.registry,
                &self.case.id,
                self.case.generation,
                request(n),
            )
            .unwrap();
        self.until(|r| {
            r.updates
                .iter()
                .any(|u| u.admitted.as_ref().is_some_and(|(key, _)| key == &id(n)))
        })
        .await;
    }
    fn admission(&self, n: u32) -> &Result<(), crate::structured::queue::admission::Failure> {
        &self
            .updates
            .iter()
            .rev()
            .find_map(|u| u.admitted.as_ref().filter(|(key, _)| key == &id(n)))
            .unwrap()
            .1
    }
    fn latest_item(&self, n: u32) -> Option<&Value> {
        self.updates
            .iter()
            .rev()
            .filter_map(|u| u.cut.as_ref())
            .flat_map(|c| c.records.iter().rev())
            .find_map(|r| {
                if r["item"]["item_id"] == id(n) {
                    Some(&r["item"])
                } else {
                    r.get("items")
                        .and_then(Value::as_array)
                        .and_then(|items| items.iter().find(|i| i["item_id"] == id(n)))
                }
            })
    }
    fn pending(&self) -> Option<&crate::structured::queue::scheduler::Pending> {
        self.updates.last().and_then(|u| u.pending.as_ref())
    }
    async fn ended(&mut self) {
        let notify = self.notify.clone();
        wait(&notify, || {
            self.pump();
            self.workers
                .report(&self.case.id, self.case.generation)
                .is_some()
        })
        .await;
        self.pump();
        assert!(
            !self
                .workers
                .report(&self.case.id, self.case.generation)
                .unwrap()
                .authority_retained
        );
        assert!(self.workers.entries[&self.case.id].thread.is_none());
    }
    async fn stop(&mut self) {
        self.workers.stop(&self.case.id, self.case.generation);
        self.ended().await;
    }
    async fn restart(&mut self) {
        let old = self.case.generation;
        let launch = Launch {
            opening_turn: None,
            agent_id: self.case.id.clone(),
            generation: old,
            leases: self.case.leases.clone(),
            state: self.case.fixture.root.path().to_owned(),
            storage: Default::default(),
            mcp: self.case.fixture.mcp(),
            admission: None,
            stall_after: STALL,
            queue_rewrite_bytes: self.threshold,
            lineage: None,
        };
        // The original driver listener was dropped; rebind its owned socket path.
        fs::remove_file(self.case.fixture.root.path().join("queue.sock")).unwrap();
        let listener =
            UnixListener::bind(self.case.fixture.root.path().join("queue.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        self.case.generation = self
            .workers
            .start(
                &mut self.registry,
                launch,
                self.case.factory("queue", vec![]),
            )
            .unwrap();
        assert!(self.case.generation > old);
        assert_eq!(
            self.workers
                .enqueue(&self.registry, &self.case.id, old, request(999))
                .unwrap_err(),
            ErrorCode::AgentNotFound
        );
        assert!(self
            .workers
            .queue_update(&self.registry, &self.case.id, old)
            .is_none());
        self.updates.clear();
        let mut driver = None;
        let notify = self.notify.clone();
        wait(&notify, || {
            self.pump();
            assert!(self
                .workers
                .report(&self.case.id, self.case.generation)
                .is_none());
            match listener.accept() {
                Ok((stream, _)) => {
                    driver = Some(stream);
                    true
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
                Err(e) => panic!("restart socket: {e}"),
            }
        })
        .await;
        self.driver = driver.unwrap();
        self.driver.set_write_timeout(Some(LIMIT)).unwrap();
    }
}
fn default_policy(_: &Case) -> NonZeroU64 {
    crate::config::Config::default()
        .sessions
        .queue_rewrite_bytes
}
fn path(case: &Case) -> std::path::PathBuf {
    case.fixture
        .root
        .path()
        .join("sessions")
        .join(&case.id)
        .join("queue.jsonl")
}
fn unknown(case: &Case) -> NonZeroU64 {
    drop(Journal::open(case.fixture.root.path(), &case.id).unwrap());
    use std::fs::OpenOptions;
    let mut f = OpenOptions::new().append(true).open(path(case)).unwrap();
    writeln!(f, "{{\"seq\":1,\"kind\":\"future\"}}").unwrap();
    f.sync_all().unwrap();
    NonZeroU64::new(f.metadata().unwrap().len()).unwrap()
}

#[tokio::test]
async fn worker_queue_delivers_fifo_on_real_result_and_keeps_a_single_writer() {
    let mut r = Run::new(201, "queue", default_policy).await;
    let e = Journal::open(r.case.fixture.root.path(), &r.case.id)
        .err()
        .unwrap();
    assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
    let raw = r
        .workers
        .runtime(&r.case.id, r.case.generation)
        .unwrap()
        .try_submit(&json!({"type":"user"}))
        .unwrap_err();
    assert_eq!(raw.to_string(), ErrorCode::QueueModeUnsupported.as_str());
    r.admit(1).await;
    assert!(r.admission(1).is_ok());
    r.until(|r| !r.inputs().is_empty()).await;
    assert_eq!(r.inputs()[0]["input"]["uuid"], id(1));
    r.control(json!({"op":"lifecycle","state":"started"}));
    r.until(|r| r.registry.get(&r.case.id).unwrap().1.update.state.turn == Turn::Working)
        .await;
    r.admit(2).await;
    assert!(r.admission(2).is_ok());
    assert_eq!(r.pending().unwrap().cause, PendingCause::AwaitingResult);
    assert_eq!(
        r.pending().unwrap().batch_id.as_deref(),
        Some(id(1).as_str())
    );
    assert!(r.latest_item(2).unwrap().get("awaiting_result").is_none());
    r.control(json!({"op":"result"}));
    r.until(|r| {
        r.inputs().len() >= 2
            && r.latest_item(1)
                .is_some_and(|v| v["state"] == "done" || v["state"] == "cancelled")
    })
    .await;
    assert_eq!(
        r.inputs()
            .iter()
            .map(|v| v["input"]["uuid"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        [id(1), id(2)]
    );
    assert_eq!(r.latest_item(1).unwrap()["state"], "done");
    r.control(json!({"op":"lifecycle","state":"started"}));
    r.until(|r| r.registry.get(&r.case.id).unwrap().1.update.state.turn == Turn::Working)
        .await;
    r.control(json!({"op":"result"}));
    r.until(|r| r.latest_item(2).is_some_and(|v| v["state"] == "done"))
        .await;
    r.control(json!({"op":"exit"}));
    r.ended().await;
    let j = Journal::open(r.case.fixture.root.path(), &r.case.id).unwrap();
    assert!(j.items().iter().all(|i| i.state == State::Done));
}

#[tokio::test]
async fn worker_queue_cancelled_and_unknown_boundaries_stay_pending_until_result() {
    for (n, op, cause, state) in [
        (
            202,
            "lifecycle",
            PendingCause::CancelledAwaitingResult,
            "cancelled",
        ),
        (203, "loss", PendingCause::TurnEndUnknown, "queued"),
    ] {
        let mut r = Run::new(n, "queue", default_policy).await;
        r.admit(1).await;
        r.until(|r| !r.inputs().is_empty()).await;
        r.admit(2).await;
        r.control(json!({"op":op,"state":"cancelled"}));
        r.until(|r| r.pending().is_some_and(|p| p.cause == cause))
            .await;
        assert_eq!(r.latest_item(1).unwrap()["state"], state);
        assert_eq!(r.latest_item(1).unwrap()["awaiting_result"], true);
        assert!(r.latest_item(2).unwrap().get("awaiting_result").is_none());
        r.control(json!({"op":"result"}));
        r.until(|r| {
            r.inputs().len() >= 2
                && r.latest_item(1)
                    .is_some_and(|v| v["state"] == "done" || v["state"] == "cancelled")
        })
        .await;
        assert_eq!(r.inputs()[1]["input"]["uuid"], id(2));
        r.stop().await;
    }
}

#[tokio::test]
async fn worker_queue_silent_turn_reports_stable_reservation_without_a_second_offer() {
    let mut r = Run::new(204, "queue-silent", default_policy).await;
    r.admit(1).await;
    r.control(json!({"op":"barrier","after_count":1}));
    r.until(|r| r.raws().iter().any(|v| v["type"] == "fixture_barrier"))
        .await;
    r.admit(2).await;
    assert!(matches!(
        r.updates
            .iter()
            .find(|u| u.admitted.as_ref().is_some_and(|(key, _)| key == &id(2)))
            .unwrap()
            .offer,
        Ok(crate::structured::queue::scheduler::Offer::Pending(_))
    ));
    let pending = r.pending().unwrap().clone();
    assert_eq!(pending.cause, PendingCause::AwaitingResult);
    assert_eq!(pending.batch_id.as_deref(), Some(id(1).as_str()));
    let mut held = request(3);
    held.gate = Gate::Hold;
    r.workers
        .enqueue(&r.registry, &r.case.id, r.case.generation, held)
        .unwrap();
    r.until(|r| {
        r.updates
            .iter()
            .any(|u| u.admitted.as_ref().is_some_and(|(key, _)| key == &id(3)))
    })
    .await;
    assert_eq!(r.pending(), Some(&pending));
    assert!(r.latest_item(2).unwrap().get("awaiting_result").is_none());
    r.stop().await;
    assert_eq!(r.latest_item(1).unwrap()["state"], "held");
    assert_eq!(r.latest_item(1).unwrap()["refusal"], "no_result");
    r.restart().await;
    r.until(|r| !r.inputs().is_empty()).await;
    assert_eq!(r.inputs()[0]["input"]["uuid"], id(2));
    r.stop().await;
}

#[tokio::test]
async fn worker_queue_recovery_holds_unconfirmed_items_and_never_resends_them() {
    let mut r = Run::new(205, "queue", |case| {
        use crate::structured::queue::admission::{Admission, MAX_BATCH};
        let j = Journal::open(case.fixture.root.path(), &case.id).unwrap();
        let mut admission = Admission::new(j, MAX_BATCH).unwrap();
        for n in [1, 2] {
            let request = request(n);
            admission
                .enqueue(request.item, request.principal, request.gate)
                .unwrap();
        }
        let mut prior = admission.journal().items()[0].clone();
        prior.metadata.insert("awaiting_result".into(), json!(true));
        admission.replace(vec![prior]).unwrap();
        default_policy(case)
    })
    .await;
    r.until(|r| !r.inputs().is_empty()).await;
    assert_eq!(r.inputs()[0]["input"]["uuid"], id(2));
    let held = r.latest_item(1).unwrap();
    assert_eq!(held["state"], "held");
    assert_eq!(held["refusal"], "session_unconfirmed");
    r.stop().await;
}

#[tokio::test]
async fn worker_queue_production_checkpoint_returns_the_committed_empty_cut() {
    let mut r = Run::new(206, "queue", |case| {
        let mut j = Journal::open(case.fixture.root.path(), &case.id).unwrap();
        let header = j.bytes().unwrap();
        j.append(Change::Upsert {
            item: Box::new(request(1).item),
            position: 0,
        })
        .unwrap();
        j.append(Change::Remove { item_id: id(1) }).unwrap();
        NonZeroU64::new(header + 1).unwrap()
    })
    .await;
    r.until(|r| {
        r.updates.iter().any(|u| {
            u.cut.as_ref().is_some_and(|c| {
                c.gap
                    .as_ref()
                    .is_some_and(|g| g.to == 2 && g.snapshot_to == 2)
            })
        })
    })
    .await;
    let cut = r
        .updates
        .iter()
        .find_map(|u| {
            u.cut
                .as_ref()
                .filter(|c| c.gap.as_ref().is_some_and(|g| g.to == 2))
        })
        .unwrap();
    assert_eq!(cut.through, 2);
    assert!(cut.records.is_empty());
    let bytes = fs::read_to_string(path(&r.case)).unwrap();
    assert!(bytes
        .lines()
        .any(|s| serde_json::from_str::<Value>(s).unwrap()["high_water"] == 2));
    r.stop().await;
}

#[tokio::test]
async fn worker_queue_unknown_checkpoint_refusal_latches_and_is_rederived_on_reopen() {
    let mut r = Run::new(207, "queue", unknown).await;
    r.admit(1).await;
    assert!(r.admission(1).is_ok());
    assert_eq!(
        r.pending().unwrap().cause,
        PendingCause::QueueJournalCorrupt(crate::structured::queue::QueueJournalCorrupt {
            seq: Some(2),
            offset: fs::metadata(path(&r.case)).unwrap().len(),
            reason: "unknown_data_prevents_checkpoint"
        })
    );
    let bytes = fs::read(path(&r.case)).unwrap();
    r.admit(2).await;
    assert_eq!(
        r.admission(2).as_ref().unwrap_err().code,
        ErrorCode::QueueJournalCorrupt
    );
    assert_eq!(fs::read(path(&r.case)).unwrap(), bytes);
    assert!(r.latest_item(1).unwrap().get("awaiting_result").is_none());
    r.stop().await;
    assert_eq!(fs::read(path(&r.case)).unwrap(), bytes);
    r.restart().await;
    r.until(|r| {
        r.pending()
            .is_some_and(|p| matches!(p.cause, PendingCause::QueueJournalCorrupt(_)))
    })
    .await;
    r.admit(3).await;
    assert_eq!(
        r.admission(3).as_ref().unwrap_err().code,
        ErrorCode::QueueJournalCorrupt
    );
    assert_eq!(fs::read(path(&r.case)).unwrap(), bytes);
    r.stop().await;
}

#[tokio::test]
async fn worker_queue_torn_tail_counts_toward_reopen_threshold_without_repair() {
    let mut r = Run::new(208, "queue", |case| {
        let threshold = unknown(case);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(path(case))
            .unwrap();
        f.write_all(b"{torn").unwrap();
        f.sync_all().unwrap();
        threshold
    })
    .await;
    let bytes = fs::read(path(&r.case)).unwrap();
    assert!(bytes.ends_with(b"{torn"));
    r.until(|r| {
        r.pending()
            .is_some_and(|p| matches!(p.cause, PendingCause::QueueJournalCorrupt(_)))
    })
    .await;
    r.admit(1).await;
    assert_eq!(
        r.admission(1).as_ref().unwrap_err().code,
        ErrorCode::QueueJournalCorrupt
    );
    r.stop().await;
    assert_eq!(fs::read(path(&r.case)).unwrap(), bytes);
}

// Keep registry polling paused while the pump consumes an entire turn's burst.
// This reproduces the stale pre-turn Idle gate without a scheduling race.
async fn pump_burst(r: &mut Run, events: Vec<Value>, exit: bool) {
    r.control(json!({"op":"burst","events":events,"exit":exit}));
    let notify = r.notify.clone();
    wait(&notify, || {
        while let Some(update) = r
            .workers
            .queue_update(&r.registry, &r.case.id, r.case.generation)
        {
            r.updates.push(update);
        }
        r.latest_item(1).is_some_and(|v| v["state"] == "done")
    })
    .await;
}

#[tokio::test]
async fn worker_queue_result_requires_s4_covering_its_attention_event() {
    let mut r = Run::new(220, "queue", default_policy).await;
    r.control(json!({"op":"result"}));
    r.until(|r| r.registry.get(&r.case.id).unwrap().1.update.state.turn == Turn::Idle)
        .await;
    r.admit(1).await;
    r.until(|r| !r.inputs().is_empty()).await;
    r.admit(2).await;
    let mut started = event("command_lifecycle");
    started["command_uuid"] = json!(id(1));
    let mut limit = event("rate_limit_event");
    limit["rate_limit_info"]["status"] = json!("rejected");
    let result = event("result");
    pump_burst(&mut r, vec![started, limit, result], false).await;
    assert!(
        r.latest_item(2).unwrap().get("awaiting_result").is_none(),
        "next input crossed a result before its S4 projection"
    );
    let stale = owner_queue::Snapshot {
        state: r.registry.get(&r.case.id).unwrap().1.update.state.clone(),
        through: r.workers.entries[&r.case.id].queue_seq,
    };
    let notify = r.notify.clone();
    wait(&notify, || {
        r.workers
            .runtime(&r.case.id, r.case.generation)
            .unwrap()
            .queue_state(stale.clone())
    })
    .await;
    r.workers
        .enqueue(&r.registry, &r.case.id, r.case.generation, request(3))
        .unwrap();
    wait(&notify, || {
        while let Some(update) = r
            .workers
            .queue_update(&r.registry, &r.case.id, r.case.generation)
        {
            r.updates.push(update);
        }
        r.updates
            .iter()
            .any(|u| u.admitted.as_ref().is_some_and(|(key, _)| key == &id(3)))
    })
    .await;
    assert!(
        r.latest_item(2).unwrap().get("awaiting_result").is_none(),
        "stale Idle snapshot bypassed result sequence coverage"
    );
    r.until(|r| {
        matches!(
            r.registry.get(&r.case.id).unwrap().1.update.state.attention,
            Attention::Blocked {
                reason: crate::api::agent_state::BlockedReason::ProviderLimit,
                ..
            }
        )
    })
    .await;
    assert_eq!(r.inputs().len(), 1);
    r.control(json!({"op":"event","event":event("rate_limit_event")}));
    r.until(|r| r.inputs().len() == 2).await;
    r.stop().await;
}

#[tokio::test]
async fn worker_queue_result_followed_by_exit_never_offers_the_next_item() {
    let mut r = Run::new(221, "queue", default_policy).await;
    r.control(json!({"op":"result"}));
    r.until(|r| r.registry.get(&r.case.id).unwrap().1.update.state.turn == Turn::Idle)
        .await;
    r.admit(1).await;
    r.until(|r| !r.inputs().is_empty()).await;
    r.admit(2).await;
    pump_burst(&mut r, vec![event("result")], true).await;
    assert!(
        r.latest_item(2).unwrap().get("awaiting_result").is_none(),
        "next input offered before the exit reached S4"
    );
    let notify = r.notify.clone();
    wait(&notify, || {
        let worker = r.workers.entries.get_mut(&r.case.id).unwrap();
        match worker.messages.try_recv() {
            Ok(Message::End(report)) => {
                worker.report = Some(report);
                true
            }
            Ok(Message::Ready(_)) => panic!("duplicate readiness during exit"),
            Ok(Message::Leased) => panic!("duplicate lease acquisition during exit"),
            Err(mpsc::TryRecvError::Empty) => false,
            Err(error) => panic!("producer lost its exit report: {error}"),
        }
    })
    .await;
    r.ended().await;
    assert!(r
        .updates
        .iter()
        .filter_map(|u| u.cut.as_ref())
        .flat_map(|c| &c.records)
        .all(|v| !v
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| items
                .iter()
                .any(|i| i["item_id"] == id(2) && i["awaiting_result"] == true))));
}

#[tokio::test]
async fn worker_queue_needs_human_turn_keeps_the_next_item_queued() {
    let mut r = Run::new(222, "queue", default_policy).await;
    r.admit(1).await;
    r.until(|r| !r.inputs().is_empty()).await;
    r.control(json!({"op":"lifecycle","state":"started"}));
    r.until(|r| r.registry.get(&r.case.id).unwrap().1.update.state.turn == Turn::Working)
        .await;
    r.admit(2).await;
    r.control(json!({"op":"event","event":event("control_request")}));
    r.until(|r| {
        matches!(
            r.registry.get(&r.case.id).unwrap().1.update.state.attention,
            Attention::NeedsHuman { .. }
        )
    })
    .await;
    assert!(r.latest_item(2).unwrap().get("awaiting_result").is_none());
    assert_eq!(r.inputs().len(), 1);
    r.stop().await;
}

#[tokio::test]
async fn worker_queue_readonly_error_does_not_repeat_a_completion_per_event() {
    let mut r = Run::new(223, "queue", |case| {
        let _ = unknown(case);
        NonZeroU64::new(1).unwrap()
    })
    .await;
    // Persisted init is not a completion barrier. A refused owner request
    // establishes the latched baseline, including any initial error detail.
    r.admit(1).await;
    assert_eq!(
        r.admission(1).as_ref().unwrap_err().code,
        ErrorCode::QueueJournalCorrupt
    );
    r.updates.clear();
    let mut events = vec![event("rate_limit_event"); 12];
    events.push(event("control_request"));
    r.control(json!({"op":"burst","events":events}));
    r.until(|r| {
        matches!(
            r.registry.get(&r.case.id).unwrap().1.update.state.attention,
            Attention::NeedsHuman { .. }
        )
    })
    .await;
    // The registry observes an event before the pump flushes its completion.
    // A second acknowledged request fences all preceding event publication.
    r.admit(2).await;
    assert_eq!(
        r.admission(2).as_ref().unwrap_err().code,
        ErrorCode::QueueJournalCorrupt
    );
    assert_eq!(
        r.updates.len(),
        1,
        "only the explicit admission may emit a completion after the baseline"
    );
    assert!(r.inputs().is_empty());
    r.stop().await;
}

#[tokio::test]
async fn worker_queue_restart_refuses_to_discard_undrained_completions() {
    let mut r = Run::new(224, "queue", default_policy).await;
    r.admit(1).await;
    r.until(|r| !r.inputs().is_empty()).await;
    r.workers.stop(&r.case.id, r.case.generation);
    let notify = r.notify.clone();
    wait(&notify, || {
        r.workers.poll(&mut r.registry, Instant::now(), 1000);
        r.workers.report(&r.case.id, r.case.generation).is_some()
    })
    .await;
    let launch = Launch {
        opening_turn: None,
        agent_id: r.case.id.clone(),
        generation: r.case.generation,
        leases: r.case.leases.clone(),
        state: r.case.fixture.root.path().to_owned(),
        storage: Default::default(),
        mcp: r.case.fixture.mcp(),
        admission: None,
        stall_after: STALL,
        queue_rewrite_bytes: r.threshold,
        lineage: None,
    };
    assert_eq!(
        r.workers
            .start(&mut r.registry, launch, r.case.factory("queue", vec![])),
        Err(ErrorCode::SessionBusy)
    );
    r.pump();
    r.restart().await;
    r.stop().await;
}

#[tokio::test]
async fn mcp_opening_is_durable_before_ready_and_delivered_once_with_attested_origin() {
    use crate::structured::queue::model::Producer;
    let prompt = crate::spawn::prompt::SpawnPrompt::compose("/compact\nclaimed operator").unwrap();
    let framed = prompt.as_argv_element().to_owned();
    let mut r = Run::with_opening(
        240,
        "queue",
        default_policy,
        Some(("attested-starter".into(), prompt)),
    )
    .await;
    let admitted: Vec<_> = r
        .updates
        .iter()
        .filter_map(|u| u.cut.as_ref())
        .flat_map(|cut| &cut.records)
        .filter_map(|v| v.get("item"))
        .filter(|v| v["kind"] == "message")
        .collect();
    assert!(
        !admitted.is_empty(),
        "Ready must expose a committed opening item"
    );
    let item_id = admitted[0]["item_id"].as_str().unwrap().to_owned();
    r.until(|r| !r.inputs().is_empty()).await;
    assert_eq!(r.inputs()[0]["input"]["uuid"], item_id);
    assert!(r.inputs()[0]["input"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains(&framed));
    assert_eq!(r.inputs()[0]["executed_command"], false);
    r.control(json!({"op":"result"}));
    r.until(|r| r.raws().iter().any(|v| v["type"] == "result"))
        .await;
    r.stop().await;
    let journal = Journal::open(r.case.fixture.root.path(), &r.case.id).unwrap();
    assert_eq!(journal.items().len(), 1);
    assert_eq!(
        journal.items()[0].producer,
        Producer::Agent("attested-starter".into())
    );
    assert_eq!(journal.items()[0].kind, Kind::Message);
    assert_eq!(journal.items()[0].body, framed);
    assert_eq!(r.inputs().len(), 1);
}

#[tokio::test]
async fn mcp_opening_refusal_is_typed_and_leaves_no_running_worker() {
    let (mut registry, mut workers, notify, _) = owner();
    let (mut case, mut launch, factory) = Case::prepare(&mut registry, 241, "queue", vec![]);
    launch.opening_turn = Some((
        "attested-starter".into(),
        crate::spawn::prompt::SpawnPrompt::compose("task").unwrap(),
    ));
    drop(Journal::open(case.fixture.root.path(), &case.id).unwrap());
    let mut header: Value =
        serde_json::from_str(fs::read_to_string(path(&case)).unwrap().trim()).unwrap();
    header["format_version"] = json!(crate::structured::queue::FORMAT_VERSION + 1);
    let bytes = format!("{}\n", header);
    fs::write(path(&case), &bytes).unwrap();
    case.generation = workers.start(&mut registry, launch, factory).unwrap();
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        workers.report(&case.id, case.generation).is_some()
    })
    .await;
    assert_eq!(
        workers
            .report(&case.id, case.generation)
            .unwrap()
            .result
            .as_ref()
            .unwrap_err()
            .0,
        ErrorCode::QueueJournalNewer
    );
    assert!(workers.runtime(&case.id, case.generation).is_none());
    assert!(
        workers.entries[&case.id].runtime.is_none(),
        "no Ready was published"
    );
    assert!(workers.entries[&case.id].thread.is_none());
    assert!(!case.leases.is_reserved(&case.session).unwrap());
    assert!(!case.fixture.root.path().join("model.pid").exists());
    assert_eq!(fs::read(path(&case)).unwrap(), bytes.as_bytes());
}

#[tokio::test]
async fn cli_start_admits_no_opening_item_or_input() {
    let mut r = Run::new(242, "queue", default_policy).await;
    r.control(json!({"op":"barrier","after_count":0}));
    r.until(|r| r.raws().iter().any(|v| v["type"] == "fixture_barrier"))
        .await;
    assert!(r.inputs().is_empty());
    assert!(r
        .updates
        .iter()
        .filter_map(|u| u.cut.as_ref())
        .all(|cut| cut.records.is_empty()));
    r.stop().await;
    assert!(Journal::open(r.case.fixture.root.path(), &r.case.id)
        .unwrap()
        .items()
        .is_empty());
}

#[tokio::test]
async fn failed_spawn_settles_opening_and_resume_never_delivers_it() {
    let (mut registry, mut workers, notify, _) = owner();
    let (mut case, mut launch, factory) = Case::prepare(&mut registry, 243, "queue", vec![]);
    let binary = fs::read(case.fixture.root.path().join("claude")).unwrap();
    launch.opening_turn = Some((
        "attested-starter".into(),
        crate::spawn::prompt::SpawnPrompt::compose("stale opening").unwrap(),
    ));
    let f = case.fixture.clone();
    case.generation = workers
        .start(&mut registry, launch, move |record| {
            let profile = factory(record)?;
            let p = f.root.path().join("case.json");
            let mut value: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
            value["version"] = json!("remove");
            fs::write(p, serde_json::to_vec(&value).unwrap()).unwrap();
            Ok(profile)
        })
        .unwrap();
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        workers.report(&case.id, case.generation).is_some()
    })
    .await;
    assert!(workers
        .report(&case.id, case.generation)
        .unwrap()
        .result
        .is_err());
    assert!(workers.runtime(&case.id, case.generation).is_none());
    let journal = Journal::open(case.fixture.root.path(), &case.id).unwrap();
    assert_eq!(journal.items().len(), 1);
    assert_eq!(
        journal.items()[0].state,
        State::Failed,
        "failed start left an eligible opening"
    );
    assert_eq!(journal.items()[0].metadata["refusal"], "agent_start_failed");
    drop(journal);
    assert!(!case.leases.is_reserved(&case.session).unwrap());
    let listener = UnixListener::bind(case.fixture.root.path().join("queue.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let threshold = default_policy(&case);
    case.fixture.file("claude", &binary, 0o700);
    let launch = Launch {
        opening_turn: None,
        agent_id: case.id.clone(),
        generation: case.generation,
        leases: case.leases.clone(),
        state: case.fixture.root.path().to_owned(),
        storage: Default::default(),
        mcp: case.fixture.mcp(),
        admission: None,
        stall_after: STALL,
        queue_rewrite_bytes: threshold,
        lineage: None,
    };
    case.generation = workers
        .start(&mut registry, launch, case.factory("queue", vec![]))
        .unwrap();
    let mut driver = None;
    let mut updates = vec![];
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        while let Some(update) = workers.queue_update(&registry, &case.id, case.generation) {
            updates.push(update);
        }
        assert!(workers.report(&case.id, case.generation).is_none());
        match listener.accept() {
            Ok((stream, _)) => {
                driver = Some(stream);
                true
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
            Err(e) => panic!("resume handshake: {e}"),
        }
    })
    .await;
    let driver = driver.unwrap();
    driver.set_write_timeout(Some(LIMIT)).unwrap();
    let mut r = Run {
        registry,
        workers,
        notify,
        case,
        driver,
        updates,
        threshold,
    };
    r.control(json!({"op":"barrier","after_count":0}));
    r.until(|r| r.raws().iter().any(|v| v["type"] == "fixture_barrier"))
        .await;
    assert!(
        r.inputs().is_empty(),
        "resume delivered a failed launch's opening"
    );
    r.stop().await;
}

#[tokio::test]
async fn oversized_opening_admission_refuses_without_mutation_or_ready() {
    let (mut registry, mut workers, notify, _) = owner();
    let (mut case, mut launch, factory) = Case::prepare(&mut registry, 244, "queue", vec![]);
    // The valid empty journal can open; this refusal must come from admission.
    let (owner, _) =
        owner_queue::Service::open(case.fixture.root.path(), &case.id, default_policy(&case))
            .unwrap();
    drop(owner);
    let before = fs::read(path(&case)).unwrap();
    launch.opening_turn = Some((
        "a".repeat(2 * 1024 * 1024),
        crate::spawn::prompt::SpawnPrompt::compose("bounded task").unwrap(),
    ));
    case.generation = workers.start(&mut registry, launch, factory).unwrap();
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        workers.report(&case.id, case.generation).is_some()
    })
    .await;
    assert_eq!(
        workers
            .report(&case.id, case.generation)
            .unwrap()
            .result
            .as_ref()
            .unwrap_err()
            .0,
        ErrorCode::InvalidRequest
    );
    assert!(
        workers.entries[&case.id].runtime.is_none(),
        "refusal published Ready"
    );
    assert!(workers.entries[&case.id].thread.is_none());
    assert!(!case.leases.is_reserved(&case.session).unwrap());
    assert!(!case.fixture.root.path().join("model.pid").exists());
    assert_eq!(fs::read(path(&case)).unwrap(), before);
}

#[test]
fn reopening_refuses_an_unresolved_opening_before_any_delivery() {
    use crate::structured::queue::admission::Admission;
    let (mut registry, _, _, _) = owner();
    let (case, _, _) = Case::prepare(&mut registry, 246, "queue", vec![]);
    let mut admission = Admission::new(
        Journal::open(case.fixture.root.path(), &case.id).unwrap(),
        8,
    )
    .unwrap();
    let mut input = request(246).item;
    input.kind = Kind::Message;
    input.metadata.insert("opening_turn".into(), json!(true));
    admission
        .enqueue(
            input,
            QueuePrincipal::Origin(crate::structured::queue::model::Producer::Agent(
                "attested-starter".into(),
            )),
            Gate::Send,
        )
        .unwrap();
    drop(admission);
    let (service, _) =
        owner_queue::Service::open(case.fixture.root.path(), &case.id, default_policy(&case))
            .unwrap();
    drop(service);
    let journal = Journal::open(case.fixture.root.path(), &case.id).unwrap();
    assert_eq!(journal.items()[0].state, State::Failed);
    assert_eq!(journal.items()[0].metadata["refusal"], "agent_start_failed");
}

#[tokio::test]
async fn older_input_cannot_disarm_opening_settlement_or_hide_its_final_cut() {
    use crate::structured::queue::admission::Admission;
    let mut r = Run::with_opening(
        247,
        "queue",
        |case| {
            let mut admission = Admission::new(
                Journal::open(case.fixture.root.path(), &case.id).unwrap(),
                8,
            )
            .unwrap();
            admission
                .enqueue(request(247).item, QueuePrincipal::Operator, Gate::Send)
                .unwrap();
            drop(admission);
            default_policy(case)
        },
        Some((
            "attested-starter".into(),
            crate::spawn::prompt::SpawnPrompt::compose("unaccepted opening").unwrap(),
        )),
    )
    .await;
    r.until(|r| !r.inputs().is_empty()).await;
    assert_eq!(r.inputs()[0]["input"]["uuid"], id(247));
    r.stop().await;
    let journal = Journal::open(r.case.fixture.root.path(), &r.case.id).unwrap();
    let opening = journal
        .items()
        .iter()
        .find(|item| item.kind == Kind::Message)
        .unwrap();
    assert_eq!(opening.state, State::Failed);
    assert!(
        r.updates
            .iter()
            .filter_map(|u| u.cut.as_ref())
            .flat_map(|c| &c.records)
            .any(|record| record["kind"] == "fail"
                && record["item_id"] == opening.item_id
                && record["reason"] == "agent_start_failed"),
        "terminal cut omitted opening settlement"
    );
}

#[test]
fn opening_recovery_preserves_opaque_future_items_and_bytes() {
    let (mut registry, _, _, _) = owner();
    let (case, _, _) = Case::prepare(&mut registry, 248, "queue", vec![]);
    let mut journal = Journal::open(case.fixture.root.path(), &case.id).unwrap();
    let mut input = request(248).item;
    input.kind = Kind::Unknown(json!("future_opening_kind"));
    input.metadata.insert("opening_turn".into(), json!(true));
    journal
        .append(crate::structured::queue::Change::Upsert {
            item: Box::new(input),
            position: 0,
        })
        .unwrap();
    drop(journal);
    let before = fs::read(path(&case)).unwrap();
    let (service, _) =
        owner_queue::Service::open(case.fixture.root.path(), &case.id, default_policy(&case))
            .unwrap();
    drop(service);
    assert_eq!(fs::read(path(&case)).unwrap(), before);
}

#[tokio::test]
async fn worker_queue_needs_human_and_result_burst_rejects_stale_idle() {
    let mut r = Run::new(230, "queue", default_policy).await;
    r.control(json!({"op":"result"}));
    r.until(|r| r.registry.get(&r.case.id).unwrap().1.update.state.turn == Turn::Idle)
        .await;
    r.admit(1).await;
    r.until(|r| !r.inputs().is_empty()).await;
    r.admit(2).await;
    let stale = owner_queue::Snapshot {
        state: r.registry.get(&r.case.id).unwrap().1.update.state.clone(),
        through: r.workers.entries[&r.case.id].queue_seq,
    };
    let mut started = event("command_lifecycle");
    started["command_uuid"] = json!(id(1));
    pump_burst(
        &mut r,
        vec![started, event("control_request"), event("result")],
        false,
    )
    .await;
    assert!(r.latest_item(2).unwrap().get("awaiting_result").is_none());
    let notify = r.notify.clone();
    wait(&notify, || {
        r.workers
            .runtime(&r.case.id, r.case.generation)
            .unwrap()
            .queue_state(stale.clone())
    })
    .await;
    r.workers
        .enqueue(&r.registry, &r.case.id, r.case.generation, request(3))
        .unwrap();
    wait(&notify, || {
        while let Some(update) = r
            .workers
            .queue_update(&r.registry, &r.case.id, r.case.generation)
        {
            r.updates.push(update);
        }
        r.updates
            .iter()
            .any(|u| u.admitted.as_ref().is_some_and(|(key, _)| key == &id(3)))
    })
    .await;
    assert!(
        r.latest_item(2).unwrap().get("awaiting_result").is_none(),
        "NeedsHuman/result crossed the stale Idle gate"
    );
    r.stop().await;
}

#[tokio::test]
async fn worker_queue_cold_subscription_is_typed_and_generation_fenced() {
    let (mut registry, workers, _, _) = owner();
    let (case, _, _) = Case::prepare(&mut registry, 231, "queue", vec![]);
    let mut journal = Journal::open(case.fixture.root.path(), &case.id).unwrap();
    journal
        .append(Change::Upsert {
            item: Box::new(request(1).item),
            position: 0,
        })
        .unwrap();
    drop(journal);
    let before = fs::read(path(&case)).unwrap();
    let refusal = workers
        .queue_source(&registry, &case.id, case.generation)
        .err()
        .unwrap();
    assert_eq!(refusal.as_str(), "queue_source_cold");
    assert_eq!(refusal.retry(), crate::cli::exit::Retry::Later);
    assert!(matches!(
        workers.queue_source(&registry, &case.id, case.generation + 1),
        Err(ErrorCode::AgentNotFound)
    ));
    assert_eq!(fs::read(path(&case)).unwrap(), before);
}

#[tokio::test]
async fn worker_queue_admission_is_published_by_its_live_owner_and_retained_after_exit() {
    use crate::structured::{
        publication::{Provider, Subscription},
        workers::queue::publication::Page,
    };
    let mut r = Run::new(232, "queue", default_policy).await;
    let source = r
        .workers
        .queue_source(&r.registry, &r.case.id, r.case.generation)
        .unwrap();
    let mut reader = source.subscribe(None).unwrap();
    let wake = reader.waker();
    let mut observed = vec![];
    wait(&wake, || {
        r.pump();
        loop {
            match reader.page() {
                Page::Pending => return false,
                Page::Synced(n) => {
                    observed.push(n);
                    return true;
                }
                Page::Refused(e) => panic!("bootstrap refused {e:?}"),
                _ => (),
            }
        }
    })
    .await;
    r.admit(1).await;
    wait(&wake, || {
        r.pump();
        loop {
            match reader.page() {
                Page::Record(v)
                    if v["item"]["item_id"] == id(1)
                        || v["items"]
                            .as_array()
                            .is_some_and(|items| items.iter().any(|i| i["item_id"] == id(1))) =>
                {
                    return true
                }
                Page::Pending => return false,
                Page::Refused(e) => panic!("publication refused {e:?}"),
                _ => (),
            }
        }
    })
    .await;
    r.stop().await;
    let mut terminal = source.subscribe(None).unwrap();
    let mut saw_snapshot = false;
    let mut ended = false;
    for _ in 0..100 {
        match terminal.page() {
            Page::Gap(_) => saw_snapshot = true,
            Page::End => {
                ended = true;
                break;
            }
            Page::Refused(e) => panic!("terminal refused {e:?}"),
            _ => (),
        }
    }
    assert!(saw_snapshot && ended);
    assert!(!observed.is_empty());
}
