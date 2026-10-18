use super::*;
use crate::api::agent_state::{AgentState, Turn};
use crate::api::error_code::ErrorCode;
use crate::structured::queue::admission::{Admission, Gate, Principal, MAX_BATCH};
use crate::structured::queue::model::{Delivery, Kind, Priority, Producer, State};
use crate::structured::queue::scheduler::{Offer, Pending, PendingCause, Scheduler};
use crate::structured::queue::{Change, Item, Journal};
use crate::structured::state::{Input, SessionState};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};

fn id(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}
fn item(n: u64) -> Item {
    Item {
        item_id: id(n),
        producer: Producer::Human,
        kind: Kind::Prompt,
        state: State::Queued,
        delivery: Delivery::AtIdle,
        priority: Priority::Normal,
        body: format!("input {n}"),
        attachments: vec![],
        metadata: BTreeMap::new(),
    }
}
fn catalogue(kind: &str, subtype: Option<&str>) -> Value {
    include_str!("../../../../tests/fixtures/claude-session/events.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["type"] == kind && subtype.is_none_or(|s| event["subtype"] == s))
        .unwrap()
}
struct QueueRun {
    running: Running,
    scheduler: Option<Scheduler>,
    driver: UnixStream,
    fixture: Fixture,
    reducer: SessionState,
    state: AgentState,
}
impl QueueRun {
    fn new() -> Self {
        Self::mode("queue")
    }
    fn mode(mode: &str) -> Self {
        let fixture = Fixture::for_socket();
        let listener = UnixListener::bind(fixture.root.path().join("queue.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let running = fixture.launch(mode, "2.1.296").unwrap();
        let driver = running.wait(|| match listener.accept() {
            Ok((stream, _)) => Some(stream),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
            Err(e) => panic!("driver accept: {e}"),
        });
        driver
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let admission = Admission::new(
            Journal::open(fixture.root.path(), "fixture-agent").unwrap(),
            MAX_BATCH,
        )
        .unwrap();
        let mut reducer = SessionState::new("fixture-agent".into());
        let state = reducer
            .reduce(Input::Observe, Instant::now(), 100, Duration::from_secs(60))
            .state;
        let mut this = Self {
            running,
            scheduler: Some(Scheduler::new(admission).unwrap()),
            driver,
            fixture,
            reducer,
            state,
        };
        let first = this.raw("system");
        assert_eq!(
            serde_json::from_str::<Value>(&first.record.raw).unwrap()["subtype"],
            "hook_started"
        );
        this.apply(&first);
        let init = this.raw("system");
        assert_eq!(
            serde_json::from_str::<Value>(&init.record.raw).unwrap()["subtype"],
            "init"
        );
        this.apply(&init);
        this
    }
    fn q(&self) -> &Scheduler {
        self.scheduler.as_ref().unwrap()
    }
    fn q_mut(&mut self) -> &mut Scheduler {
        self.scheduler.as_mut().unwrap()
    }
    fn send(&mut self, n: u64, producer: Producer) {
        let origin = if producer == Producer::Human {
            Principal::Operator
        } else {
            Principal::Origin(producer)
        };
        let mut item = item(n);
        item.kind = Kind::Message;
        self.q_mut().enqueue(item, origin, Gate::Send).unwrap();
    }
    fn offer(&mut self) -> Result<Option<String>, crate::structured::queue::admission::Failure> {
        self.offer_at(100)
    }
    fn offer_at(
        &mut self,
        now_ms: u64,
    ) -> Result<Option<String>, crate::structured::queue::admission::Failure> {
        self.outcome_at(now_ms).map(|outcome| match outcome {
            Offer::Submitted { batch_id } => Some(batch_id),
            Offer::Pending(_) | Offer::NoEligibleItems => None,
        })
    }
    fn outcome_at(
        &mut self,
        now_ms: u64,
    ) -> Result<Offer, crate::structured::queue::admission::Failure> {
        self.scheduler
            .as_mut()
            .unwrap()
            .deliver(&self.running.runtime, &self.state, now_ms)
    }
    fn barrier(&mut self, after_count: u64) -> u64 {
        self.control(json!({"op":"barrier", "after_count":after_count}));
        self.received("fixture_barrier")["count"].as_u64().unwrap()
    }
    fn loss(&mut self) {
        self.control(json!({"op":"loss"}));
        let event = self.raw("flk_oversize");
        assert_eq!(event.record.origin, RecordOrigin::Flk);
        assert_eq!(
            serde_json::from_str::<Value>(&event.record.raw).unwrap()["turn_end"],
            "unknown"
        );
        self.apply(&event);
    }
    fn control(&mut self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        self.driver.write_all(&bytes).unwrap();
    }
    fn raw(&self, kind: &str) -> Received {
        let event = self.running.receive();
        assert_eq!(
            serde_json::from_str::<Value>(&event.record.raw).unwrap()["type"],
            kind,
            "unexpected stage: {}",
            event.record.raw
        );
        event
    }
    fn reduce(&mut self, event: &Received) {
        if event.record.origin == RecordOrigin::Claude {
            self.state = self
                .reducer
                .reduce(
                    Input::Event(&event.event),
                    Instant::now(),
                    event.record.received_at_ms.try_into().unwrap(),
                    Duration::from_secs(60),
                )
                .state;
        }
    }
    fn apply(&mut self, event: &Received) {
        self.reduce(event);
        self.q_mut().observe(&event.event).unwrap();
    }
    fn received(&mut self, kind: &str) -> Value {
        let event = self.raw(kind);
        let raw = serde_json::from_str(&event.record.raw).unwrap();
        self.apply(&event);
        raw
    }
    fn input(&mut self) -> Value {
        self.received("fixture_input")
    }
    fn lifecycle(&mut self, state: &str) {
        self.control(json!({"op":"lifecycle","state":state}));
        self.received("command_lifecycle");
    }
    fn result(&mut self) {
        self.control(json!({"op":"result"}));
        self.received("result");
    }
    fn audit(&mut self, state: &str) {
        let event = self.raw("flk_permission_request");
        assert_eq!(event.record.origin, RecordOrigin::Flk);
        assert_eq!(
            serde_json::from_str::<Value>(&event.record.raw).unwrap()["state"],
            state
        );
        self.apply(&event);
    }
}

#[test]
fn actual_stdin_read_stays_queued_until_started_and_all_members_move_atomically() {
    let mut run = QueueRun::new();
    run.send(0, Producer::Human);
    run.send(1, Producer::Human);
    assert_eq!(run.state.turn, Turn::Unknown);
    let batch = run.offer().unwrap().unwrap();
    let read = run.input();
    assert_eq!(read["input"]["uuid"], batch);
    assert_eq!(
        read["input"]["message"]["content"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(run
        .q()
        .journal()
        .items()
        .iter()
        .all(|i| i.state == State::Queued));
    let before = run.q().journal().replay(0).unwrap().through;
    run.lifecycle("started");
    assert_eq!(run.q().journal().replay(0).unwrap().through, before + 1);
    assert!(run
        .q()
        .journal()
        .items()
        .iter()
        .all(|i| i.state == State::Delivered && i.metadata["batch_id"] == batch));
    run.lifecycle("completed");
    assert!(run
        .q()
        .journal()
        .items()
        .iter()
        .all(|i| i.state == State::Done && i.metadata["awaiting_result"] == true));
    let mut continuation = item(2);
    continuation.kind = Kind::Continuation;
    continuation
        .metadata
        .insert("anchor".into(), json!({"item_id":id(0)}));
    run.q_mut()
        .enqueue(
            continuation,
            Principal::Origin(Producer::Agent("sender".into())),
            Gate::Send,
        )
        .unwrap();
    assert!(run.offer().unwrap().is_none());
    run.send(3, Producer::Human);
    run.control(json!({"op":"result"}));
    let result = run.raw("result");
    run.reduce(&result);
    assert_eq!(run.state.turn, Turn::Idle);
    assert!(
        run.offer().unwrap().is_none(),
        "S4 idle and writer idle cannot bypass the reservation"
    );
    run.q_mut().observe(&result.event).unwrap();
    assert!(run.q().journal().items()[..2]
        .iter()
        .all(|i| !i.metadata.contains_key("awaiting_result")));
    assert_eq!(run.offer().unwrap(), Some(id(2)));
    assert_eq!(run.input()["input"]["uuid"], id(2));
    run.lifecycle("started");
    run.result();
    assert_eq!(run.offer().unwrap(), Some(id(3)));
    run.input();
    run.lifecycle("started");
    run.result();
}

#[test]
fn actual_s4_working_event_blocks_even_when_runtime_writer_is_idle() {
    let mut run = QueueRun::new();
    run.send(0, Producer::Human);
    let mut status = catalogue("system", Some("status"));
    status["status"] = json!("requesting");
    run.control(json!({"op":"event","event":status}));
    run.received("system");
    assert_eq!(run.state.turn, Turn::Working);
    let before = run.q().journal().replay(0).unwrap().through;
    assert!(run.offer().unwrap().is_none());
    assert_eq!(run.q().journal().replay(0).unwrap().through, before);
    run.result();
    assert_eq!(run.state.turn, Turn::Idle);
    assert!(run.offer().unwrap().is_some());
    run.input();
    run.lifecycle("started");
    run.result();
}

#[test]
fn actual_s6_open_request_blocks_with_stale_s4_idle_and_cancel_releases_it() {
    let mut run = QueueRun::new();
    run.result();
    run.send(0, Producer::Human);
    let mut permission = catalogue("control_request", None);
    permission["request_id"] = json!("permit");
    run.control(json!({"op":"event","event":permission}));
    let open = run.raw("control_request");
    assert_eq!(
        run.state.turn,
        Turn::Idle,
        "S4 has not consumed the open event yet"
    );
    assert_eq!(run.running.runtime.waiting_on_human().unwrap(), ["permit"]);
    let before = run.q().journal().replay(0).unwrap().through;
    assert!(run.offer().unwrap().is_none());
    assert_eq!(run.q().journal().replay(0).unwrap().through, before);
    run.apply(&open);
    run.audit("open");
    run.control(
        json!({"op":"event","event":{"type":"control_cancel_request","request_id":"permit"}}),
    );
    run.received("control_cancel_request");
    run.audit("closed");
    assert!(run.running.runtime.waiting_on_human().unwrap().is_empty());
    assert_eq!(run.state.turn, Turn::Idle);
    assert!(run.offer().unwrap().is_some());
    run.input();
    run.lifecycle("started");
    run.result();
}

#[test]
fn all_producer_boundaries_and_verbatim_command_reach_real_stdin() {
    let mut run = QueueRun::new();
    let origins = [
        Producer::Human,
        Producer::Human,
        Producer::Agent("sender".into()),
        Producer::Human,
        Producer::Channel {
            binding: "chat".into(),
            user: "user".into(),
        },
        Producer::System("wake".into()),
        Producer::Fork("child".into()),
    ];
    for (n, origin) in origins.iter().enumerate() {
        run.send(n as u64, origin.clone());
    }
    let mut command = item(7);
    command.kind = Kind::Command;
    command.body = "/compact".into();
    run.q_mut()
        .enqueue(command, Principal::Operator, Gate::Send)
        .unwrap();
    let mut slash = item(8);
    slash.kind = Kind::Message;
    slash.body = "/compact".into();
    run.q_mut()
        .enqueue(
            slash,
            Principal::Origin(Producer::Agent("sender".into())),
            Gate::Send,
        )
        .unwrap();
    for members in [
        vec![0, 1],
        vec![2],
        vec![3],
        vec![4],
        vec![5],
        vec![6],
        vec![7],
        vec![8],
    ] {
        let batch_id = run.offer().unwrap().unwrap();
        let observed = run.input();
        assert_eq!(observed["input"]["uuid"], batch_id);
        let blocks = observed["input"]["message"]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), members.len());
        assert_eq!(observed["executed_command"], members == [7]);
        if members == [7] {
            assert_eq!(blocks[0]["text"], "/compact");
        } else {
            for (block, n) in blocks.iter().zip(&members) {
                let producer = if *n == 8 {
                    Producer::Agent("sender".into())
                } else {
                    origins[*n as usize].clone()
                };
                assert!(block["text"].as_str().unwrap().starts_with(&format!(
                    "From {} (item {}):",
                    json!(producer),
                    id(*n)
                )));
            }
        }
        run.lifecycle("started");
        run.result();
        for n in members {
            assert_eq!(run.q().journal().items()[n as usize].state, State::Done);
        }
    }
    assert!(run.offer().unwrap().is_none());
}

#[test]
fn nested_and_unknown_results_and_unrelated_lifecycle_never_release_members() {
    let mut run = QueueRun::new();
    run.send(0, Producer::Human);
    run.send(1, Producer::Human);
    run.offer().unwrap().unwrap();
    run.input();
    run.lifecycle("started");
    run.send(2, Producer::Human);
    run.control(json!({"op":"result","nested":true}));
    run.received("result");
    run.control(json!({"op":"lifecycle","state":"completed","uuid":"unrelated"}));
    run.received("command_lifecycle");
    run.lifecycle("future_state");
    run.control(json!({"op":"result","unknown":true}));
    run.received("result");
    assert!(run.q().journal().items()[..2]
        .iter()
        .all(|i| i.state == State::Delivered));
    assert!(run.offer().unwrap().is_none());
    run.result();
    assert!(run.offer().unwrap().is_some());
    run.input();
    run.lifecycle("started");
    run.result();
}

#[test]
fn no_result_holds_every_pending_member_even_after_lifecycle_completed() {
    for stage in ["written", "started", "completed"] {
        let mut run = QueueRun::new();
        run.send(0, Producer::Human);
        run.send(1, Producer::Human);
        run.offer().unwrap().unwrap();
        run.input();
        if stage != "written" {
            run.lifecycle("started");
        }
        if stage == "completed" {
            run.lifecycle("completed");
        }
        run.control(json!({"op":"exit"}));
        run.running.finish();
        run.q_mut().exited().unwrap();
        assert!(run
            .q()
            .journal()
            .items()
            .iter()
            .all(|i| i.state == State::Held
                && i.metadata["refusal"] == "no_result"
                && !i.metadata.contains_key("awaiting_result")));
        drop(run.scheduler.take());
        let reopened = Journal::open(run.fixture.root.path(), "fixture-agent").unwrap();
        assert!(reopened.items().iter().all(|i| i.state == State::Held));
    }
}

#[test]
fn cancellation_is_terminal_across_result_and_process_exit() {
    for result in [false, true] {
        let mut run = QueueRun::new();
        run.send(0, Producer::Human);
        run.send(1, Producer::Human);
        run.offer().unwrap().unwrap();
        run.input();
        run.lifecycle("cancelled");
        assert!(run
            .q()
            .journal()
            .items()
            .iter()
            .all(|i| i.state == State::Cancelled));
        if result {
            run.result();
        }
        run.control(json!({"op":"exit"}));
        run.running.finish();
        run.q_mut().exited().unwrap();
        assert!(run
            .q()
            .journal()
            .items()
            .iter()
            .all(|i| i.state == State::Cancelled && !i.metadata.contains_key("awaiting_result")));
    }
}

#[test]
fn busy_offer_restores_members_without_repeating_the_gate() {
    let mut run = QueueRun::new();
    let external = json!({"type":"user","uuid":"external","message":{"role":"user","content":[{"type":"text","text":"owned fixture input"}]}});
    run.running.runtime.try_submit(&external).unwrap();
    run.input();
    run.send(0, Producer::Human);
    let before = serde_json::to_value(&run.q().journal().items()[0]).unwrap();
    let error = run.offer().unwrap_err();
    assert_eq!(error.code, ErrorCode::SessionBusy);
    assert_eq!(
        serde_json::to_value(&run.q().journal().items()[0]).unwrap(),
        before
    );
    run.control(json!({"op":"result"}));
    let result = run.raw("result");
    assert_eq!(run.state.turn, Turn::Unknown);
    run.apply(&result);
    assert!(run.offer().unwrap().is_some());
    run.input();
    run.lifecycle("started");
    run.result();
}

#[test]
fn stopped_runtime_refusal_is_typed_and_holds_the_unwritten_input() {
    let mut run = QueueRun::new();
    run.send(0, Producer::Human);
    run.running.runtime.stop();
    run.running.finish();
    let error = run.offer().unwrap_err();
    assert_eq!(error.code, ErrorCode::InputWriteFailed);
    assert_eq!(run.q().journal().items()[0].state, State::Held);
    assert_eq!(
        run.q().journal().items()[0].metadata["refusal"],
        "input_write_failed"
    );
}

#[test]
fn fresh_generation_holds_unresolved_and_unknown_result_flags() {
    let fixture = Fixture::new();
    for flag in [json!(true), json!("future")] {
        let mut journal = Journal::open(fixture.root.path(), "fixture-agent").unwrap();
        let mut pending = item(0);
        pending.state = State::Done;
        pending.metadata.insert("awaiting_result".into(), flag);
        journal
            .append(Change::Upsert {
                item: Box::new(pending),
                position: 0,
            })
            .unwrap();
        let scheduler = Scheduler::new(Admission::new(journal, MAX_BATCH).unwrap()).unwrap();
        assert_eq!(scheduler.journal().items()[0].state, State::Held);
        assert_eq!(
            scheduler.journal().items()[0].metadata["refusal"],
            "session_unconfirmed"
        );
    }
}

#[test]
fn reservation_sync_failure_precedes_any_runtime_offer() {
    let mut run = QueueRun::new();
    drop(run.scheduler.take());
    let mut journal = Journal::open(run.fixture.root.path(), "fixture-agent").unwrap();
    let mut queued = item(0);
    queued
        .metadata
        .insert("gate".into(), json!({"decision":"send"}));
    journal
        .append(Change::Upsert {
            item: Box::new(queued),
            position: 0,
        })
        .unwrap();
    let _peer = crate::structured::queue::tests::fail_writes(&mut journal);
    run.scheduler = Some(Scheduler::new(Admission::new(journal, MAX_BATCH).unwrap()).unwrap());
    let error = run.offer().unwrap_err();
    assert_eq!(error.code, ErrorCode::InputWriteUnknown);
    assert!(run
        .q()
        .journal()
        .items()
        .iter()
        .all(|i| i.state == State::Queued && !i.metadata.contains_key("awaiting_result")));
    let sentinel = json!({"type":"user","uuid":"sentinel","message":{"role":"user","content":[{"type":"text","text":"owned fixture probe"}]}});
    run.running
        .runtime
        .try_submit(&sentinel)
        .expect("failed reservation never occupied stdin");
    let observed = run.raw("fixture_input");
    assert_eq!(
        serde_json::from_str::<Value>(&observed.record.raw).unwrap()["input"]["uuid"],
        "sentinel"
    );
}

#[test]
fn count_bound_and_live_human_editor_skip_reach_real_stdin() {
    let mut run = QueueRun::new();
    run.send(0, Producer::Human);
    let mut edited = item(1);
    edited.metadata.insert(
        "edit_lease".into(),
        json!({"client":"editor","until_ms":200}),
    );
    run.q_mut()
        .enqueue(edited, Principal::Operator, Gate::Send)
        .unwrap();
    for n in 2..11 {
        run.send(n, Producer::Human);
    }
    let first = run.offer().unwrap().unwrap();
    let observed = run.input();
    assert_eq!(
        observed["input"]["message"]["content"]
            .as_array()
            .unwrap()
            .len(),
        8
    );
    assert_eq!(
        run.q()
            .journal()
            .items()
            .iter()
            .filter(|i| i.metadata.get("batch_id") == Some(&json!(first)))
            .map(|i| i.item_id.clone())
            .collect::<Vec<_>>(),
        [0, 2, 3, 4, 5, 6, 7, 8].map(id)
    );
    assert_eq!(run.q().journal().items()[1].state, State::Queued);
    run.lifecycle("started");
    run.result();
    run.offer().unwrap().unwrap();
    assert_eq!(
        run.input()["input"]["message"]["content"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    run.lifecycle("started");
    run.result();
    assert!(run.offer().unwrap().is_none());
    assert_eq!(run.offer_at(200).unwrap(), Some(id(1)));
    assert_eq!(run.input()["input"]["uuid"], id(1));
    run.lifecycle("started");
    run.result();
}

#[test]
fn known_error_results_retire_the_batch_without_claiming_turn_success() {
    for subtype in ["success", "error_during_execution"] {
        let mut run = QueueRun::new();
        run.send(0, Producer::Human);
        run.offer().unwrap().unwrap();
        run.input();
        run.lifecycle("started");
        run.send(1, Producer::Human);
        let mut result = catalogue("result", Some(subtype));
        result["is_error"] = json!(true);
        run.control(json!({"op":"event","event":result}));
        run.received("result");
        let item = &run.q().journal().items()[0];
        assert_eq!(item.state, State::Done);
        assert!(!item.metadata.contains_key("awaiting_result"));
        assert_eq!(run.state.turn, Turn::Idle);
        assert_eq!(run.offer().unwrap(), Some(id(1)));
        run.input();
        run.lifecycle("started");
        run.result();
    }
}

#[test]
fn nonlive_or_unconfirmed_s4_state_never_spends_a_queue_cursor() {
    let mut run = QueueRun::new();
    run.send(0, Producer::Human);
    let ready = run.state.clone();
    let before = run.q().journal().replay(0).unwrap().through;
    run.state.attention = crate::api::agent_state::Attention::Unknown;
    assert!(run.offer().unwrap().is_none());
    run.state = ready.clone();
    run.state.lifecycle = crate::api::agent_state::Lifecycle::Unknown;
    assert!(run.offer().unwrap().is_none());
    assert_eq!(run.q().journal().replay(0).unwrap().through, before);
    run.state = ready;
    run.offer().unwrap().unwrap();
    run.input();
    run.lifecycle("started");
    run.result();
}

fn pending(batch_id: &str, since_ms: u64, cause: PendingCause) -> Offer {
    Offer::Pending(Pending {
        batch_id: Some(batch_id.into()),
        since_ms,
        cause,
    })
}

#[test]
fn queue_silent_reports_stable_pending_without_mutating_the_journal() {
    let mut run = QueueRun::mode("queue-silent");
    run.send(0, Producer::Human);
    assert_eq!(run.offer().unwrap(), Some(id(0)));
    run.send(1, Producer::Human);
    // Positive acknowledgement of the actual read, not a missing-event deadline.
    assert_eq!(run.barrier(1), 1);
    let through = run.q().journal().replay(0).unwrap().through;
    for now_ms in [200, 300] {
        assert_eq!(
            run.outcome_at(now_ms).unwrap(),
            pending(&id(0), 100, PendingCause::AwaitingResult)
        );
    }
    assert_eq!(
        run.q().status(&run.running.runtime),
        Some(Pending {
            batch_id: Some(id(0)),
            since_ms: 100,
            cause: PendingCause::AwaitingResult
        })
    );
    assert_eq!(
        serde_json::to_value(run.outcome_at(400).unwrap()).unwrap(),
        json!({"outcome":"pending", "detail":{"batch_id":id(0), "since_ms":100, "cause":"awaiting_result"}})
    );
    // A socket barrier cannot prove the asynchronous stdin writer has flushed.
    // The typed early returns and unchanged journal below guard repeated offers.
    assert!(matches!(
        run.running.result.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(run.q().journal().replay(0).unwrap().through, through);
    assert!(run
        .q()
        .journal()
        .items()
        .iter()
        .all(|i| i.state == State::Queued));
}

#[test]
fn turn_end_unknown_reports_pending_without_holding_or_resending() {
    let mut run = QueueRun::new();
    run.send(0, Producer::Human);
    run.offer().unwrap().unwrap();
    run.input();
    run.send(1, Producer::Agent("sender".into()));
    run.send(2, Producer::Human);
    run.loss();
    let before = run.q().journal().items().to_vec();
    let through = run.q().journal().replay(0).unwrap().through;
    for now_ms in [200, 300] {
        assert_eq!(
            run.outcome_at(now_ms).unwrap(),
            pending(&id(0), 100, PendingCause::TurnEndUnknown)
        );
        assert_eq!(run.q().journal().items(), before);
    }
    assert_eq!(run.barrier(1), 1);
    assert_eq!(run.q().journal().replay(0).unwrap().through, through);
    assert_eq!(
        run.q().status(&run.running.runtime).unwrap().cause,
        PendingCause::TurnEndUnknown
    );
    run.result();
    assert!(run.q().status(&run.running.runtime).is_none());
    for (n, count) in [(1, 2), (2, 3)] {
        assert_eq!(run.offer().unwrap(), Some(id(n)));
        let read = run.input();
        assert_eq!(read["input"]["uuid"], id(n));
        assert_eq!(read["count"], count);
        run.lifecycle("started");
        run.result();
    }
    assert!(run
        .q()
        .journal()
        .items()
        .iter()
        .all(|i| i.state == State::Done));
}

#[test]
fn turn_end_unknown_refusal_rolls_back_intact_instead_of_holding() {
    let mut run = QueueRun::new();
    let external = json!({"type":"user","uuid":"external","message":{"role":"user","content":[{"type":"text","text":"owned fixture input"}]}});
    run.running.runtime.try_submit(&external).unwrap();
    run.input();
    run.send(0, Producer::Human);
    run.send(1, Producer::Agent("sender".into()));
    run.loss();
    let before = run.q().journal().items().to_vec();
    let offered = run.outcome_at(101);
    assert_eq!(
        run.q().journal().items(),
        before,
        "refused input was never written"
    );
    assert_eq!(
        offered.unwrap(),
        pending(&id(0), 101, PendingCause::TurnEndUnknown)
    );
    let through = run.q().journal().replay(0).unwrap().through;
    assert_eq!(
        run.outcome_at(201).unwrap(),
        pending(&id(0), 101, PendingCause::TurnEndUnknown)
    );
    assert_eq!(run.q().journal().replay(0).unwrap().through, through);
    assert_eq!(run.barrier(1), 1);
    assert_eq!(
        run.q().status(&run.running.runtime).unwrap().cause,
        PendingCause::TurnEndUnknown
    );
    run.result();
    assert!(run.q().status(&run.running.runtime).is_none());
    for n in [0, 1] {
        assert_eq!(run.offer().unwrap(), Some(id(n)));
        assert_eq!(run.input()["input"]["uuid"], id(n));
        run.lifecycle("started");
        run.result();
    }
}

#[test]
fn refused_pending_id_describes_the_attempt_even_when_the_queued_batch_changes() {
    let mut run = QueueRun::new();
    let external = json!({"type":"user","uuid":"external","message":{"role":"user","content":[{"type":"text","text":"owned fixture input"}]}});
    run.running.runtime.try_submit(&external).unwrap();
    run.input();
    run.send(0, Producer::Human);
    run.loss();
    let refused = pending(&id(0), 101, PendingCause::TurnEndUnknown);
    assert_eq!(run.outcome_at(101).unwrap(), refused);
    run.send(1, Producer::Human);
    let before = run.q().journal().items().to_vec();
    let through = run.q().journal().replay(0).unwrap().through;
    for now_ms in [201, 301] {
        assert_eq!(run.outcome_at(now_ms).unwrap(), refused);
        assert_eq!(
            run.q().status(&run.running.runtime),
            Some(Pending {
                batch_id: Some(id(0)),
                since_ms: 101,
                cause: PendingCause::TurnEndUnknown,
            })
        );
    }
    assert_eq!(run.q().journal().items(), before);
    assert_eq!(run.q().journal().replay(0).unwrap().through, through);
    run.result();
    assert!(run.q().status(&run.running.runtime).is_none());
    let batch = run.offer_at(401).unwrap().unwrap();
    assert_ne!(batch, id(0));
    let input = run.input();
    assert_eq!(input["input"]["uuid"], batch);
    assert_eq!(
        input["input"]["message"]["content"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        run.q().status(&run.running.runtime),
        Some(Pending {
            batch_id: Some(batch),
            since_ms: 401,
            cause: PendingCause::AwaitingResult,
        })
    );
    run.result();
}

#[test]
fn cancelled_without_result_reports_pending_until_real_result_or_exit() {
    for (completed, result) in [(true, true), (true, false), (false, true), (false, false)] {
        let mut run = QueueRun::new();
        run.send(0, Producer::Human);
        run.send(1, Producer::Human);
        let batch = run.offer().unwrap().unwrap();
        run.input();
        run.send(2, Producer::Human);
        if completed {
            run.lifecycle("started");
            run.lifecycle("completed");
        }
        run.lifecycle("cancelled");
        let outcome = if completed {
            State::Done
        } else {
            State::Cancelled
        };
        let through = run.q().journal().replay(0).unwrap().through;
        for now_ms in [200, 300] {
            assert_eq!(
                run.outcome_at(now_ms).unwrap(),
                pending(&batch, 100, PendingCause::CancelledAwaitingResult)
            );
        }
        assert_eq!(run.barrier(1), 1);
        assert_eq!(run.q().journal().replay(0).unwrap().through, through);
        assert!(run.q().journal().items()[..2]
            .iter()
            .all(|i| i.state == outcome));
        assert_eq!(
            run.q().status(&run.running.runtime).unwrap().cause,
            PendingCause::CancelledAwaitingResult
        );
        if result {
            run.result();
            assert!(run.q().status(&run.running.runtime).is_none());
            assert_eq!(run.offer().unwrap(), Some(id(2)));
            run.input();
            run.lifecycle("started");
            run.result();
        } else {
            run.control(json!({"op":"exit"}));
            run.running.finish();
            run.q_mut().exited().unwrap();
            assert!(run.q().status(&run.running.runtime).is_none());
            assert!(run.q().journal().items()[..2].iter().all(|i| i.state
                == if completed {
                    State::Held
                } else {
                    State::Cancelled
                }
                && !i.metadata.contains_key("awaiting_result")));
            assert_eq!(run.q().journal().items()[2].state, State::Queued);
        }
    }
}

#[test]
fn newer_journal_status_survives_scheduler_open_and_every_runtime_mutation_is_refused() {
    readonly_runtime("newer");
}

#[test]
fn corrupt_journal_status_survives_scheduler_open_and_every_runtime_mutation_is_refused() {
    readonly_runtime("corrupt");
}

#[test]
fn invalid_header_reports_typed_read_only_scheduler_status() {
    readonly_runtime("header");
}

#[test]
fn legacy_unknown_reports_typed_read_only_scheduler_status() {
    readonly_runtime("legacy");
}

#[test]
fn required_checkpoint_reports_typed_read_only_scheduler_status() {
    readonly_runtime("checkpoint");
}

fn readonly_runtime(mode: &str) {
    let mut run = QueueRun::mode("queue-silent");
    run.send(1, Producer::Human);
    run.offer().unwrap().unwrap();
    assert_eq!(run.barrier(1), 1);
    drop(run.scheduler.take());
    let path = run
        .fixture
        .root
        .path()
        .join("sessions/fixture-agent/queue.jsonl");
    let old = fs::read(&path).unwrap();
    let mut lines = old.split_inclusive(|b| *b == b'\n');
    let mut header: Value = serde_json::from_slice(lines.next().unwrap()).unwrap();
    match mode {
        "newer" => header["format_version"] = json!(999),
        "header" => header["format_version"] = Value::Null,
        "legacy" => {
            header.as_object_mut().unwrap().remove("format_version");
        }
        "corrupt" | "checkpoint" => (),
        _ => panic!("unexpected fixture mode"),
    }
    let mut bytes = serde_json::to_vec(&header).unwrap();
    bytes.push(b'\n');
    bytes.extend(lines.flatten());
    let offset = bytes.len() as u64;
    if mode == "corrupt" {
        bytes.extend(b"{\"seq\":3,\"kind\":\"upsert\",\"item\":42}\ntorn tail");
    } else if matches!(mode, "legacy" | "checkpoint") {
        bytes.extend(b"{\"seq\":3,\"kind\":\"future\"}\n");
    }
    fs::write(&path, &bytes).unwrap();
    let mut journal = Journal::open(run.fixture.root.path(), "fixture-agent").unwrap();
    if mode == "checkpoint" {
        assert!(journal.read_only().is_none());
        assert!(journal.checkpoint().is_err());
    }
    let admission = Admission::new(journal, MAX_BATCH).unwrap();
    let mut scheduler = Scheduler::new(admission).unwrap();
    let status = scheduler.status(&run.running.runtime).unwrap();
    let code = if mode != "newer" {
        ErrorCode::QueueJournalCorrupt
    } else {
        ErrorCode::QueueJournalNewer
    };
    if mode != "newer" {
        let (seq, offset, reason) = match mode {
            "corrupt" => (Some(3), offset, "malformed_known_record"),
            "header" => (None, 0, "invalid_format_version"),
            "legacy" | "checkpoint" => (
                Some(3),
                bytes.len() as u64,
                "unknown_data_prevents_checkpoint",
            ),
            _ => panic!("unexpected fixture mode"),
        };
        assert_eq!(
            status.cause,
            PendingCause::QueueJournalCorrupt(crate::structured::queue::QueueJournalCorrupt {
                seq,
                offset,
                reason
            })
        );
        assert_eq!(
            serde_json::to_value(&status).unwrap()["cause"]["queue_journal_corrupt"]["reason"],
            reason
        );
    } else {
        assert_eq!(
            status.cause,
            PendingCause::QueueJournalNewer {
                found: 999,
                supported: 2
            }
        );
        assert_eq!(
            serde_json::to_value(&status).unwrap()["cause"]["queue_journal_newer"]["found"],
            999
        );
    }
    let event = crate::structured::claude::SessionEvent::parse(
        &catalogue("result", Some("success")).to_string(),
        "fixture",
    );
    let errors = [
        scheduler
            .deliver(&run.running.runtime, &run.state, 100)
            .unwrap_err(),
        scheduler.observe(&event).unwrap_err(),
        scheduler.exited().unwrap_err(),
        scheduler
            .enqueue(item(2), Principal::Operator, Gate::Send)
            .unwrap_err(),
    ];
    for error in errors {
        assert_eq!(error.code, code);
    }
    assert_eq!(scheduler.status(&run.running.runtime).unwrap(), status);
    assert_eq!(fs::read(path).unwrap(), bytes);
}
