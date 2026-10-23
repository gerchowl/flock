use super::*;
use crate::api::agent_state::{Attention, Phase, Turn};
use crate::structured::{
    permissions::{self, CloseReason, Principal},
    registry::StructuredRuntime,
    runtime::{tests::Fixture, Outcome},
};
use serde_json::{json, Value};
use std::fs;
const LIMIT: Duration = Duration::from_secs(10);
const STALL: Duration = Duration::from_secs(60);

pub(crate) fn event(kind: &str) -> Value {
    include_str!("../../../tests/fixtures/claude-session/events.jsonl")
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .find(|v| v["type"] == kind && (kind != "command_lifecycle" || v["state"] == "started"))
        .unwrap()
}

/// Every observation below starts in a real, owned subprocess and passes through
/// the bounded runtime queue. Test profile construction runs after reservation.
pub(crate) struct Case {
    pub fixture: Arc<Fixture>,
    pub id: String,
    session: String,
    pub generation: u64,
    leases: SessionLeases,
}
impl Case {
    pub fn start(
        registry: &mut Registry,
        workers: &mut Workers,
        n: u32,
        mode: &str,
        events: Vec<Value>,
    ) -> Self {
        let (mut case, launch, factory) = Self::prepare(registry, n, mode, events);
        case.generation = workers.start(registry, launch, factory).unwrap();
        case
    }
    pub(crate) fn prepare(
        registry: &mut Registry,
        n: u32,
        mode: &str,
        events: Vec<Value>,
    ) -> (
        Self,
        Launch,
        impl FnOnce(&Record) -> Result<LaunchProfile, LaunchError> + Send + 'static,
    ) {
        let fixture = Arc::new(Fixture::for_socket());
        let cwd = fixture.root.path().join("project");
        fs::create_dir(&cwd).unwrap();
        let id = format!("agent_worker_{n}");
        let session = format!("00000000-0000-4000-8000-{n:012}");
        let generation = registry
            .register(Record {
                runtime: StructuredRuntime::Structured,
                agent_id: id.clone(),
                name: Some(format!("worker-{n}")),
                harness: "claude".into(),
                session_id: session.clone(),
                cwd,
                model: Some("fixture-worker-model".into()),
            })
            .unwrap();
        let leases = SessionLeases::default();
        let launch = Launch {
            agent_id: id.clone(),
            generation,
            leases: leases.clone(),
            state: fixture.root.path().to_owned(),
            storage: Default::default(),
            mcp: fixture.mcp(),
            stall_after: STALL,
            lineage: None,
            admission: None,
        };
        let f = fixture.clone();
        let authority = leases.clone();
        let mode = mode.to_owned();
        let factory = move |record: &Record| {
            assert!(
                authority.is_reserved(&record.session_id).unwrap(),
                "reserve precedes profile factory"
            );
            let profile = f.profile_at(&mode, "2.1.296", &record.session_id);
            let mut argv: Vec<_> = profile
                .args()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            argv.extend(["--model".into(), record.model.clone().unwrap()]);
            f.file("argv.json", &serde_json::to_vec(&argv).unwrap(), 0o600);
            f.file(
                "case.json",
                &serde_json::to_vec(
                    &json!({"mode":mode,"version":"2.1.296","cwd":record.cwd,"model":record.model}),
                )
                .unwrap(),
                0o600,
            );
            f.file(
                "state-events.jsonl",
                events
                    .iter()
                    .map(|v| format!("{v}\n"))
                    .collect::<String>()
                    .as_bytes(),
                0o600,
            );
            Ok(profile)
        };
        (
            Self {
                fixture,
                id,
                session,
                generation,
                leases,
            },
            launch,
            factory,
        )
    }
    pub(crate) fn assert_shutdown(&self, registry: &Registry, workers: &Workers) {
        assert!(workers.entries[&self.id].thread.is_none(), "worker joined");
        assert!(
            !workers
                .report(&self.id, self.generation)
                .unwrap()
                .authority_retained
        );
        assert!(!self.leases.is_reserved(&self.session).unwrap());
        let view = &registry.get(&self.id).unwrap().1;
        assert_eq!(view.update.state.lifecycle, Lifecycle::Offline);
        assert!(view.update.needs_human.is_empty());
        let stream = fs::read_to_string(
            self.fixture
                .root
                .path()
                .join("sessions")
                .join(&self.id)
                .join(format!("{}.jsonl", self.session)),
        )
        .unwrap();
        let closed = stream.lines().any(|line| {
            let envelope: Value = serde_json::from_str(line).unwrap();
            let raw: Value = serde_json::from_str(envelope["raw"].as_str().unwrap()).unwrap();
            raw["type"] == "flk_permission_request" && raw["state"] == "closed"
        });
        assert!(closed, "permission close survives runtime shutdown on disk");
    }
    pub(crate) fn force_shutdown(
        &self,
        registry: &mut Registry,
        workers: &mut Workers,
    ) -> ShutdownError {
        workers
            .shutdown_until(registry, Duration::ZERO, LIMIT)
            .unwrap_err()
    }
    pub(crate) async fn uncertain(
        registry: &mut Registry,
        workers: &mut Workers,
        n: u32,
        notify: &Arc<Notify>,
    ) -> Self {
        let (case, _, _) = Self::prepare(registry, n, "state-reducer", vec![]);
        let (tx, messages) = mpsc::sync_channel(2);
        drop(tx);
        workers.entries.insert(
            case.id.clone(),
            Worker {
                generation: case.generation,
                messages,
                runtime: None,
                stop: StopRequest::default(),
                report: None,
                finished: false,
                events_closed: false,
                stall_after: STALL,
                thread: Some(std::thread::spawn(|| {})),
            },
        );
        wait(notify, || {
            workers.poll(registry, Instant::now(), 1000);
            workers.report(&case.id, case.generation).is_some()
        })
        .await;
        case
    }
    pub(crate) fn forget_report(&self, workers: &mut Workers) {
        let worker = workers.entries.get_mut(&self.id).unwrap();
        assert!(worker.finished && worker.thread.is_none());
        worker.report = None;
    }
    pub fn submit(&self, workers: &Workers) {
        workers.runtime(&self.id, self.generation).unwrap().try_submit(&json!({"type":"user","message":{"role":"user","content":"fixture turn"},"parent_tool_use_id":null,"session_id":""})).unwrap();
    }
    pub async fn ready(
        &self,
        registry: &mut Registry,
        workers: &mut Workers,
        notify: &Arc<Notify>,
        now: Instant,
    ) {
        wait(notify, || {
            workers.poll(registry, now, 1000);
            assert!(
                workers.report(&self.id, self.generation).is_none(),
                "worker ended before readiness"
            );
            workers.runtime(&self.id, self.generation).is_some()
        })
        .await;
        assert_eq!(
            registry.get(&self.id).unwrap().1.update.state.lifecycle,
            Lifecycle::Live
        );
    }
    pub async fn human(
        &self,
        registry: &mut Registry,
        workers: &mut Workers,
        notify: &Arc<Notify>,
        now: Instant,
    ) {
        self.ready(registry, workers, notify, now).await;
        self.submit(workers);
        wait(notify, || {
            workers.poll(registry, now, 1000);
            assert!(
                workers.report(&self.id, self.generation).is_none(),
                "worker ended before request"
            );
            registry.get(&self.id).unwrap().1.update.state.phase == Phase::NeedsHuman
        })
        .await;
        assert!(self.leases.is_reserved(&self.session).unwrap());
    }
    pub async fn finish(
        &self,
        registry: &mut Registry,
        workers: &mut Workers,
        notify: &Arc<Notify>,
        now: Instant,
        expected: Option<Outcome>,
    ) {
        workers.stop(&self.id, self.generation);
        wait(notify, || {
            workers.poll(registry, now, 1000);
            workers.report(&self.id, self.generation).is_some()
        })
        .await;
        assert!(workers.entries[&self.id].thread.is_none());
        let report = workers.report(&self.id, self.generation).unwrap();
        assert!(!report.authority_retained, "{report:?}");
        if let Some(expected) = expected {
            assert_eq!(report.result.as_ref().unwrap().outcome, expected);
        }
        assert!(!self.leases.is_reserved(&self.session).unwrap());
        let view = &registry.get(&self.id).unwrap().1.update;
        assert!(view.needs_human.is_empty());
        assert_eq!(view.state.lifecycle, Lifecycle::Offline);
        assert_eq!(workers.next_deadline(registry), None);
    }
}

pub(crate) async fn wait(notify: &Arc<Notify>, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(LIMIT, async {
        loop {
            if condition() {
                return;
            }
            notify.notified().await;
        }
    })
    .await
    .expect("bounded worker condition never arrived");
}
fn owner() -> (Registry, Workers, Arc<Notify>, Instant) {
    let notify = Arc::new(Notify::new());
    (
        Registry::default(),
        Workers::new(notify.clone()),
        notify,
        Instant::now(),
    )
}

#[tokio::test]
async fn real_stdout_targets_only_its_record_and_book_close_clears_attention_without_result() {
    let (mut registry, mut workers, notify, now) = owner();
    let a = Case::start(
        &mut registry,
        &mut workers,
        1,
        "state-reducer",
        vec![event("command_lifecycle"), event("control_request")],
    );
    let b = Case::start(
        &mut registry,
        &mut workers,
        2,
        "state-reducer",
        vec![event("command_lifecycle")],
    );
    a.human(&mut registry, &mut workers, &notify, now).await;
    assert!(registry.get(&b.id).unwrap().1.update.needs_human.is_empty());
    let before = registry.get(&a.id).unwrap().1.update.clone();
    let id = before.needs_human[0].0.clone();
    let principal = Principal::Token(permissions::auth::fixture("fixture-operator", true));
    workers
        .runtime(&a.id, a.generation)
        .unwrap()
        .close_permissions(&principal, Some(&id), CloseReason::Interrupted)
        .unwrap();
    wait(&notify, || {
        workers.poll(&mut registry, now, 1000);
        assert!(workers.report(&a.id, a.generation).is_none());
        registry.get(&a.id).unwrap().1.update.needs_human.is_empty()
    })
    .await;
    let view = &registry.get(&a.id).unwrap().1.update;
    assert_eq!(
        view.state.turn,
        Turn::Working,
        "book close does not wait for or fabricate a result"
    );
    assert_eq!(
        view.phase_changed,
        Some((Phase::NeedsHuman, Phase::Working))
    );
    assert!(workers
        .runtime(&a.id, a.generation)
        .unwrap()
        .waiting_on_human()
        .unwrap()
        .is_empty());
    a.finish(
        &mut registry,
        &mut workers,
        &notify,
        now,
        Some(Outcome::Stopped),
    )
    .await;
    b.finish(
        &mut registry,
        &mut workers,
        &notify,
        now,
        Some(Outcome::Stopped),
    )
    .await;
}

#[tokio::test]
async fn explicit_stop_closes_open_request_and_releases_only_after_completion() {
    let (mut registry, mut workers, notify, now) = owner();
    let mut question = event("control_request");
    question["request"]["tool_name"] = "AskUserQuestion".into();
    let a = Case::start(
        &mut registry,
        &mut workers,
        3,
        "state-reducer",
        vec![question],
    );
    a.human(&mut registry, &mut workers, &notify, now).await;
    assert!(!workers.stop(&a.id, a.generation + 1));
    assert!(workers.stop(&a.id, a.generation));
    assert_eq!(
        registry.get(&a.id).unwrap().1.update.state.lifecycle,
        Lifecycle::Live,
        "stop intent is not cleanup proof"
    );
    a.finish(
        &mut registry,
        &mut workers,
        &notify,
        now,
        Some(Outcome::Stopped),
    )
    .await;
    assert!(workers.runtime(&a.id, a.generation).is_none());
    registry.remove(&a.id, a.generation).unwrap();
    workers.poll(&mut registry, now, 1000);
    assert!(
        !workers.entries.contains_key(&a.id),
        "removed agents cannot accumulate completed workers"
    );
}

#[tokio::test]
async fn unexpected_exit_drains_the_last_request_then_retires_its_generation() {
    let (mut registry, mut workers, notify, now) = owner();
    let a = Case::start(
        &mut registry,
        &mut workers,
        4,
        "worker-exit",
        vec![event("command_lifecycle"), event("control_request")],
    );
    a.ready(&mut registry, &mut workers, &notify, now).await;
    a.submit(&workers);
    wait(&notify, || {
        workers.poll(&mut registry, now, 1000);
        workers.report(&a.id, a.generation).is_some()
    })
    .await;
    let stream = fs::read_to_string(
        a.fixture
            .root
            .path()
            .join("sessions")
            .join(&a.id)
            .join(format!("{}.jsonl", a.session)),
    )
    .unwrap();
    let raw: Vec<Value> = stream
        .lines()
        .map(|line| {
            let envelope: Value = serde_json::from_str(line).unwrap();
            serde_json::from_str(envelope["raw"].as_str().unwrap()).unwrap()
        })
        .collect();
    assert!(raw.iter().any(|v| v["type"] == "control_request"));
    assert!(raw
        .iter()
        .any(|v| v["type"] == "flk_permission_request" && v["state"] == "closed"));
    assert!(matches!(
        registry.get(&a.id).unwrap().1.update.state.attention,
        Attention::Blocked {
            reason: crate::api::agent_state::BlockedReason::Crashed,
            ..
        }
    ));
    let report = workers.report(&a.id, a.generation).unwrap();
    assert_eq!(report.result.as_ref().unwrap().outcome, Outcome::Crashed);
    assert!(report.result.as_ref().unwrap().status.unwrap().code() == Some(7));
    a.finish(
        &mut registry,
        &mut workers,
        &notify,
        now,
        Some(Outcome::Crashed),
    )
    .await;
}

#[tokio::test]
async fn preparation_and_spawn_failures_retire_without_losing_the_record() {
    for spawn in [false, true] {
        let (mut registry, mut workers, notify, now) = owner();
        let (mut a, launch, factory) = Case::prepare(&mut registry, 5, "state-reducer", vec![]);
        a.generation = workers
            .start(&mut registry, launch, move |record| {
                if !spawn {
                    return Err(io::Error::other("fixture preparation failed").into());
                }
                let profile = factory(record)?;
                // Version succeeds, then the owned executable vanishes before spawn.
                let path = profile.binary().to_owned();
                let mut case: Value = serde_json::from_slice(
                    &fs::read(path.parent().unwrap().join("case.json")).unwrap(),
                )
                .unwrap();
                case["version"] = "remove".into();
                fs::write(path.parent().unwrap().join("case.json"), case.to_string()).unwrap();
                Ok(profile)
            })
            .unwrap();
        wait(&notify, || {
            workers.poll(&mut registry, now, 1000);
            workers.report(&a.id, a.generation).is_some()
        })
        .await;
        assert_eq!(a.fixture.root.path().join("version.pid").exists(), spawn);
        assert!(!a.fixture.root.path().join("model.pid").exists());
        if spawn {
            assert!(!a.fixture.root.path().join("claude").exists());
        }
        a.finish(&mut registry, &mut workers, &notify, now, None)
            .await;
        let report = workers.report(&a.id, a.generation).unwrap();
        assert!(report.result.is_err(), "{report:?}");
        assert_eq!(registry.get(&a.id).unwrap().0.session_id, a.session);
        assert_eq!(
            registry.get(&a.id).unwrap().1.update.state.attention,
            Attention::None
        );
    }
}

#[tokio::test]
async fn supplied_clock_drives_stall_and_settling_without_poll_timers() {
    for settle in [false, true] {
        let (mut registry, mut workers, notify, now) = owner();
        let events = if settle {
            vec![event("command_lifecycle"), event("result")]
        } else {
            vec![event("command_lifecycle")]
        };
        let a = Case::start(&mut registry, &mut workers, 6, "state-reducer", events);
        a.ready(&mut registry, &mut workers, &notify, now).await;
        a.submit(&workers);
        wait(&notify, || {
            workers.poll(&mut registry, now, 1000);
            assert!(workers.report(&a.id, a.generation).is_none());
            registry.get(&a.id).unwrap().1.update.state.phase
                == if settle {
                    Phase::Settling
                } else {
                    Phase::Working
                }
        })
        .await;
        let due = workers.next_deadline(&registry).unwrap();
        assert_eq!(
            due,
            now + if settle {
                Duration::from_millis(crate::api::agent_state::SETTLE_MS)
            } else {
                STALL
            }
        );
        assert!(!workers.poll(&mut registry, due - Duration::from_nanos(1), 1000));
        assert!(workers.poll(&mut registry, due, 1000));
        let view = &registry.get(&a.id).unwrap().1.update;
        if settle {
            assert_eq!(view.state.phase, Phase::Unknown);
            assert_eq!(view.state.turn, Turn::Idle);
            assert_eq!(view.phase_changed, Some((Phase::Settling, Phase::Unknown)));
        } else {
            assert!(view.stalled_since_ms.is_some());
        }
        assert_eq!(workers.next_deadline(&registry), None);
        a.finish(
            &mut registry,
            &mut workers,
            &notify,
            due,
            Some(Outcome::Stopped),
        )
        .await;
    }
}

#[tokio::test]
async fn a_blocked_preparation_never_blocks_another_agent_and_stop_is_latched() {
    let (mut registry, mut workers, notify, now) = owner();
    let (mut a, launch, factory) = Case::prepare(&mut registry, 7, "state-reducer", vec![]);
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release_tx, release) = mpsc::sync_channel(1);
    a.generation = workers
        .start(&mut registry, launch, move |record| {
            entered_tx.send(()).unwrap();
            release.recv_timeout(LIMIT).expect("bounded profile gate");
            factory(record)
        })
        .unwrap();
    tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
    assert!(a.leases.is_reserved(&a.session).unwrap());
    let b = Case::start(
        &mut registry,
        &mut workers,
        8,
        "state-reducer",
        vec![event("control_request")],
    );
    b.human(&mut registry, &mut workers, &notify, now).await;
    assert!(workers.runtime(&a.id, a.generation).is_none());
    assert!(workers.stop(&a.id, a.generation));
    release_tx.send(()).unwrap();
    a.finish(&mut registry, &mut workers, &notify, now, None)
        .await;
    assert!(workers.report(&a.id, a.generation).unwrap().result.is_err());
    assert!(
        !a.fixture.root.path().join("model.pid").exists(),
        "cancel before spawn"
    );
    b.finish(
        &mut registry,
        &mut workers,
        &notify,
        now,
        Some(Outcome::Stopped),
    )
    .await;
}

#[tokio::test]
async fn a_replacement_fences_late_worker_records_and_completion() {
    let (mut registry, mut workers, notify, now) = owner();
    let a = Case::start(
        &mut registry,
        &mut workers,
        9,
        "state-reducer",
        vec![event("control_request")],
    );
    a.human(&mut registry, &mut workers, &notify, now).await;
    let record = registry.remove(&a.id, a.generation).unwrap();
    let current = registry.register(record).unwrap();
    wait(&notify, || {
        workers.poll(&mut registry, now, 1000);
        workers.report(&a.id, a.generation).is_some()
    })
    .await;
    let (_, view, generation) = registry.get(&a.id).unwrap();
    assert_eq!(generation, current);
    assert_eq!(view.update.state.lifecycle, Lifecycle::Offline);
    assert_eq!(view.update.state.turn, Turn::Unknown);
    assert!(view.update.needs_human.is_empty());
    assert!(view.update.phase_changed.is_none());
    assert!(!a.leases.is_reserved(&a.session).unwrap());
    assert!(
        !workers
            .report(&a.id, a.generation)
            .unwrap()
            .authority_retained
    );
}

#[tokio::test]
async fn missing_worker_completion_retains_uncertain_authority_and_refuses_restart() {
    let (mut registry, mut workers, notify, now) = owner();
    let (a, launch, _) = Case::prepare(&mut registry, 10, "state-reducer", vec![]);
    let (tx, messages) = mpsc::sync_channel(2);
    let producer = std::thread::spawn(move || drop(tx));
    producer.join().unwrap();
    workers
        .pending
        .insert(a.id.clone(), Some("fixture-parent".into()));
    workers.entries.insert(
        a.id.clone(),
        Worker {
            generation: a.generation,
            messages,
            runtime: None,
            stop: StopRequest::default(),
            report: None,
            finished: false,
            events_closed: false,
            stall_after: STALL,
            thread: Some(std::thread::spawn(|| {})),
        },
    );
    wait(&notify, || {
        workers.poll(&mut registry, now, 1000);
        workers.report(&a.id, a.generation).is_some()
    })
    .await;
    assert!(
        !workers.pending.contains_key(&a.id),
        "disconnected launch must free its pending slot"
    );
    assert_eq!(workers.counts(&a.leases, "fixture-parent"), Ok((0, 0)));
    assert!(workers.entries[&a.id].thread.is_none());
    let report = workers.report(&a.id, a.generation).unwrap();
    assert!(report.authority_retained);
    assert!(report.result.is_err());
    assert_eq!(
        registry.get(&a.id).unwrap().1.update.state.lifecycle,
        Lifecycle::Unknown
    );
    assert_eq!(
        workers.start(&mut registry, launch, |_| unreachable!()),
        Err(ErrorCode::SessionBusy)
    );
    assert_eq!(workers.next_deadline(&registry), None);
    let error = workers.shutdown(&mut registry).unwrap_err();
    assert!(error
        .0
        .contains(&(a.id.clone(), ErrorCode::StructuredWorkerCleanupFailed)));
}

#[tokio::test]
async fn terminal_completion_cannot_overtake_the_queued_success_result() {
    let (mut registry, mut workers, notify, now) = owner();
    let a = Case::start(
        &mut registry,
        &mut workers,
        12,
        "worker-exit-ok",
        vec![event("command_lifecycle"), event("result")],
    );
    a.ready(&mut registry, &mut workers, &notify, now).await;
    a.submit(&workers);
    wait(&notify, || {
        workers.poll(&mut registry, now, 1000);
        workers.report(&a.id, a.generation).is_some()
    })
    .await;
    let update = &registry.get(&a.id).unwrap().1.update;
    assert_eq!(update.state.turn, Turn::Idle);
    assert_eq!(update.state.attention, Attention::None);
    let report = workers.report(&a.id, a.generation).unwrap();
    assert!(!report.result.as_ref().unwrap().input_inflight);
    assert!(report.result.as_ref().unwrap().status.unwrap().success());
    a.finish(
        &mut registry,
        &mut workers,
        &notify,
        now,
        Some(Outcome::Exited),
    )
    .await;
}

#[tokio::test]
async fn shutdown_deadline_kills_the_owned_child_and_joins_before_reporting() {
    use std::os::unix::process::ExitStatusExt;
    let (mut registry, mut workers, notify, now) = owner();
    let a = Case::start(
        &mut registry,
        &mut workers,
        31,
        "worker-stubborn",
        vec![event("control_request")],
    );
    a.human(&mut registry, &mut workers, &notify, now).await;
    let pid: u32 = fs::read_to_string(a.fixture.root.path().join("model.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let identity = super::super::supervision::ProcessIdentity::capture(pid).unwrap();
    let descendant: u32 = fs::read_to_string(a.fixture.root.path().join("grandchild.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let descendant = super::super::supervision::ProcessIdentity::capture(descendant).unwrap();
    let error = workers
        .shutdown_until(&mut registry, Duration::ZERO, LIMIT)
        .unwrap_err();
    assert!(error
        .0
        .contains(&(a.id.clone(), ErrorCode::StructuredWorkerTimeout)));
    assert!(
        !identity.alive(),
        "deadline must not leave the harness alive"
    );
    a.assert_shutdown(&registry, &workers);
    assert!(!descendant.alive(), "detached descendant must also be gone");
    assert!(!a.leases.is_reserved(&a.session).unwrap());
    let exit = workers
        .report(&a.id, a.generation)
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert_eq!(exit.status.unwrap().signal(), Some(9));
    assert_eq!(
        registry.get(&a.id).unwrap().1.update.state.lifecycle,
        Lifecycle::Offline
    );
    assert!(registry.get(&a.id).unwrap().1.update.needs_human.is_empty());
}

#[tokio::test]
async fn fake_harness_preparation_panic_has_a_distinct_typed_failure_and_is_joined() {
    let (mut registry, mut workers, notify, now) = owner();
    let (mut a, launch, factory) = Case::prepare(&mut registry, 32, "state-reducer", vec![]);
    a.generation = workers
        .start(&mut registry, launch, move |record| {
            let profile = factory(record)?;
            let mut harness =
                crate::process::TracedCommand::new(profile.binary(), "fixture-worker-panic");
            harness.arg("--version").env_clear();
            profile.configure_environment(|key, value| {
                if key != "CLAUDE_CODE_OAUTH_TOKEN" {
                    harness.env(key, value);
                }
            });
            let output = harness.output_traced_with_timeout(LIMIT).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            // Inject failure in the existing factory seam after the fake harness ran.
            panic!("fake harness adapter panicked");
        })
        .unwrap();
    wait(&notify, || {
        workers.poll(&mut registry, now, 1000);
        workers.report(&a.id, a.generation).is_some()
    })
    .await;
    assert_eq!(
        workers
            .report(&a.id, a.generation)
            .unwrap()
            .result
            .as_ref()
            .unwrap_err()
            .0,
        ErrorCode::StructuredWorkerPanicked
    );
    assert!(workers.entries[&a.id].thread.is_none());
    assert!(!a.leases.is_reserved(&a.session).unwrap());
    let pid: u32 = fs::read_to_string(a.fixture.root.path().join("version.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(crate::platform::process_start_time(pid).is_none());
    assert!(!a.fixture.root.path().join("model.pid").exists());
    assert_eq!(
        registry.get(&a.id).unwrap().1.update.state.lifecycle,
        Lifecycle::Offline
    );
}

#[test]
fn drop_after_failed_shutdown_does_not_wait_a_second_time() {
    let (mut registry, mut workers, _, _) = owner();
    let (_case, launch, factory) = Case::prepare(&mut registry, 44, "state-reducer", vec![]);
    let (entered_tx, entered) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    workers
        .start(&mut registry, launch, move |record| {
            entered_tx.send(()).unwrap();
            // The test owns the sender and releases it even when the deadline fails.
            let _ = released.recv();
            factory(record)
        })
        .unwrap();
    entered.recv_timeout(LIMIT).unwrap();
    let error = workers
        .shutdown_until(&mut registry, Duration::ZERO, Duration::ZERO)
        .unwrap_err();
    assert_eq!(error.0[0].1, ErrorCode::StructuredWorkerTimeout);
    // Retain the owning handle so the fixture can join after releasing preparation.
    let worker = workers
        .entries
        .values_mut()
        .next()
        .unwrap()
        .thread
        .take()
        .unwrap();
    let (finished, completion) = mpsc::sync_channel(1);
    let dropper = std::thread::spawn(move || {
        drop(workers);
        finished.send(()).unwrap();
    });
    let result = completion.recv_timeout(LIMIT);
    release.send(()).unwrap();
    worker.join().unwrap();
    dropper.join().unwrap();
    result.expect("Drop must not repeat the full teardown after its deadline");
}

#[test]
fn restored_unknown_refuses_before_profile_preparation_and_preserves_epoch() {
    let mut registry = Registry::default();
    let mut workers = Workers::new(Arc::new(Notify::new()));
    let (case, launch, _) = Case::prepare(&mut registry, 901, "state-reducer", vec![]);
    registry.restore_handoff_unknown(std::slice::from_ref(&case.id));
    let (_, projection, generation) = registry.get(&case.id).unwrap();
    let before = format!("{:?}", projection.update);
    let error = workers
        .start(&mut registry, launch, |_| {
            panic!("Unknown must refuse before preparation")
        })
        .unwrap_err();
    assert_eq!(
        error,
        crate::api::error_code::ErrorCode::StructuredRecoveryRequired
    );
    let (_, after, epoch) = registry.get(&case.id).unwrap();
    assert_eq!(epoch, generation);
    assert_eq!(format!("{:?}", after.update), before);
    assert!(workers.runtime(&case.id, epoch).is_none());
    assert!(!case.leases.is_reserved(&case.session).unwrap());
}

#[tokio::test]
async fn registry_worker_launch_uses_configured_blob_threshold() {
    let (mut registry, mut workers, notify, now) = owner();
    let config: crate::config::Config =
        toml::from_str("[sessions]\nblob_min_bytes = 64\n").unwrap();
    let block = json!({"type":"thinking", "thinking":"x".repeat(256)});
    let bytes = serde_json::to_vec(&block).unwrap();
    assert!(bytes.len() > config.sessions.blob_min_bytes);
    assert!(bytes.len() < 2048);
    let message = json!({"type":"assistant", "message":{"role":"assistant", "content":[block]}});
    let (mut case, mut launch, factory) = Case::prepare(
        &mut registry,
        1919,
        "worker-exit-ok",
        vec![message, event("result")],
    );
    launch.storage = config.sessions.storage();
    case.generation = workers.start(&mut registry, launch, factory).unwrap();
    case.ready(&mut registry, &mut workers, &notify, now).await;
    case.submit(&workers);
    wait(&notify, || {
        workers.poll(&mut registry, now, 1000);
        workers.report(&case.id, case.generation).is_some()
    })
    .await;
    let report = workers.report(&case.id, case.generation).unwrap();
    assert_eq!(report.result.as_ref().unwrap().outcome, Outcome::Exited);
    assert!(!report.authority_retained);
    assert!(workers.entries[&case.id].thread.is_none());
    let stream = fs::read_to_string(
        case.fixture
            .root
            .path()
            .join("sessions")
            .join(&case.id)
            .join(format!("{}.jsonl", case.session)),
    )
    .unwrap();
    let stored = stream
        .lines()
        .map(|line| {
            let envelope: Value = serde_json::from_str(line).unwrap();
            serde_json::from_str::<Value>(envelope["raw"].as_str().unwrap()).unwrap()
        })
        .find(|value| value["type"] == "assistant")
        .expect("worker persisted assistant");
    let reference: crate::structured::blobs::BlobRef =
        serde_json::from_value(stored["message"]["content"][0].clone())
            .expect("configured threshold must project this block");
    let store = crate::structured::blobs::BlobStore::open(case.fixture.root.path()).unwrap();
    assert_eq!(reference.size, bytes.len() as u64);
    assert_eq!(store.get(&reference.blob).unwrap(), bytes);
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), block);
}

#[tokio::test]
async fn registry_stream_owner_preserves_history_without_competing_with_state_sink() {
    use crate::structured::history::Item;
    let (mut registry, mut workers, notify, now) = owner();
    let case = Case::start(
        &mut registry,
        &mut workers,
        91,
        "state-reducer",
        vec![event("command_lifecycle"), event("control_request")],
    );
    case.human(&mut registry, &mut workers, &notify, now).await;
    let history = workers
        .history(&registry, &case.id)
        .expect("registry runtime owns publication")
        .clone();
    let mut cut = history.subscribe(None).unwrap();
    let mut open = false;
    let replay_deadline = Instant::now() + LIMIT;
    loop {
        assert!(
            Instant::now() < replay_deadline,
            "owned replay must reach the barrier"
        );
        match cut.page().unwrap() {
            Item::Record(record) => {
                let value: Value = serde_json::from_str(&record.raw).unwrap();
                open |= value["type"] == "flk_permission_request" && value["state"] == "open";
            }
            Item::Synced(_) => break,
            Item::Pending | Item::Progress => {}
            item => panic!("{item:?}"),
        }
    }
    assert!(open);
    assert!(!registry
        .get(&case.id)
        .unwrap()
        .1
        .update
        .needs_human
        .is_empty());
    assert!(workers.stop(&case.id, case.generation));
    case.finish(
        &mut registry,
        &mut workers,
        &notify,
        now,
        Some(Outcome::Stopped),
    )
    .await;
    assert!(workers.runtime(&case.id, case.generation).is_none());
    let history = workers
        .history(&registry, &case.id)
        .expect("history outlives runtime");
    let mut cut = history.subscribe(None).unwrap();
    let mut closed = false;
    let deadline = Instant::now() + LIMIT;
    loop {
        assert!(
            Instant::now() < deadline,
            "stopped stream must terminate or fail visibly"
        );
        match cut.page() {
            Ok(Item::Record(record)) => {
                let value: Value = serde_json::from_str(&record.raw).unwrap();
                closed |= value["type"] == "flk_permission_request" && value["state"] == "closed";
            }
            Ok(Item::End) | Err(_) => break,
            Ok(_) => {}
        }
    }
    assert!(
        closed,
        "cancelled notifications cannot erase durable permission closes"
    );
    assert!(registry
        .get(&case.id)
        .unwrap()
        .1
        .update
        .needs_human
        .is_empty());
    let mut cold = Registry::restore(vec![registry.get(&case.id).unwrap().0.clone()]).unwrap();
    let generation = cold.get(&case.id).unwrap().2;
    let mut cold_owner = Workers::new(Arc::new(Notify::new()));
    assert!(cold_owner.runtime(&case.id, generation).is_none());
    let path = case
        .fixture
        .root
        .path()
        .join("sessions")
        .join(&case.id)
        .join(format!("{}.jsonl", case.session));
    let retained = crate::structured::history::History::open_retained(&path).unwrap();
    let mut other = cold.get(&case.id).unwrap().0.clone();
    other.agent_id = "other-history-agent".into();
    other.session_id = "other-history-session".into();
    other.name = None;
    let other_generation = cold.register(other).unwrap();
    assert_eq!(
        cold_owner.install_retained(
            &cold,
            "other-history-agent",
            other_generation,
            retained.clone()
        ),
        Err(ErrorCode::AgentNotFound)
    );

    assert_eq!(
        cold_owner.install_retained(&cold, &case.id, generation + 1, retained.clone()),
        Err(ErrorCode::AgentNotFound)
    );
    cold_owner
        .install_retained(&cold, &case.id, generation, retained.clone())
        .unwrap();
    assert_eq!(
        cold_owner.install_retained(&cold, &case.id, generation, retained),
        Err(ErrorCode::SessionBusy)
    );
    let mut restored = cold_owner
        .history(&cold, &case.id)
        .unwrap()
        .subscribe(None)
        .unwrap();
    assert!(matches!(restored.page().unwrap(), Item::Record(_)));
    registry.remove(&case.id, case.generation).unwrap();
    assert!(workers.history(&registry, &case.id).is_none());
}

fn limited_launch(mut launch: Launch, leases: &SessionLeases) -> Launch {
    launch.leases = leases.clone();
    launch.lineage = Some(("fixture-parent".into(), 0));
    launch.admission = Some(Admission {
        config: crate::spawn::FleetConfig {
            max_concurrent_agents: 1,
            ..Default::default()
        },
        pty: crate::spawn::SpawnCensus {
            live_agent_started: 0,
            parent_live_children: 0,
            child_depth: 1,
        },
        paused: false,
        verb: crate::spawn::SpawnVerb::Fork,
    });
    launch
}

/// A producer held before lease acquisition cannot leave an uncounted admission gap.
#[tokio::test]
async fn owner_pending_reservation_blocks_concurrent_start_and_failure_frees_it() {
    let (mut registry, mut workers, notify, now) = owner();
    let leases = SessionLeases::default();
    let (first, launch, _) = Case::prepare(&mut registry, 120, "state-reducer", vec![]);
    let (release, barrier) = mpsc::channel();
    workers.before_reserve = Some(barrier);
    let generation = workers
        .start(&mut registry, limited_launch(launch, &leases), |_| {
            Err(io::Error::other("fixture preparation failure").into())
        })
        .unwrap();
    assert_eq!(workers.counts(&leases, "fixture-parent"), Ok((1, 1)));
    assert!(!leases.is_reserved(&first.session).unwrap());
    workers.poll(&mut registry, now + Duration::from_secs(3600), 1000);
    assert_eq!(
        workers.counts(&leases, "fixture-parent"),
        Ok((1, 1)),
        "a clock cannot release pending admission"
    );
    let (_, second, _) = Case::prepare(&mut registry, 121, "state-reducer", vec![]);
    let prepared = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = prepared.clone();
    let refused = workers.start(&mut registry, limited_launch(second, &leases), move |_| {
        observed.store(true, std::sync::atomic::Ordering::SeqCst);
        Err(io::Error::other("unadmitted preparation").into())
    });
    release.send(()).unwrap();
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        workers.report(&first.id, generation).is_some()
    })
    .await;
    let remaining = workers.counts(&leases, "fixture-parent");
    let cleanup = workers.shutdown(&mut registry);
    assert_eq!(refused, Err(ErrorCode::AtAgentCapacity));
    assert!(!prepared.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        remaining,
        Ok((0, 0)),
        "failed preparation releases reservation"
    );
    cleanup.unwrap();
    let (_, third, _) = Case::prepare(&mut registry, 122, "state-reducer", vec![]);
    workers
        .start(&mut registry, limited_launch(third, &leases), |_| {
            Err(io::Error::other("fixture preparation failure").into())
        })
        .unwrap();
    workers.shutdown(&mut registry).unwrap();
}

#[test]
fn owner_counts_deduplicate_pending_and_leased_identity() {
    let (_, mut workers, _, _) = owner();
    let leases = SessionLeases::default();
    workers
        .pending
        .insert("fixture-child".into(), Some("fixture-parent".into()));
    let lease = leases
        .reserve(
            "fixture-child",
            "00000000-0000-4000-8000-000000000123",
            Some(("fixture-parent", 0)),
        )
        .unwrap();
    assert_eq!(workers.counts(&leases, "fixture-parent"), Ok((1, 1)));
    drop(lease);
    assert_eq!(workers.counts(&leases, "fixture-parent"), Ok((1, 1)));
}

#[tokio::test]
async fn owner_failed_lease_acquisition_releases_pending_reservation() {
    let (mut registry, mut workers, notify, _) = owner();
    let leases = SessionLeases::default();
    let (case, launch, _) = Case::prepare(&mut registry, 124, "state-reducer", vec![]);
    let existing = leases
        .reserve("other-fixture-agent", &case.session, None)
        .unwrap();
    let generation = workers
        .start(&mut registry, limited_launch(launch, &leases), |_| {
            panic!("a duplicate lease must never prepare a profile")
        })
        .unwrap();
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        workers.report(&case.id, generation).is_some()
    })
    .await;
    let pending = workers.pending.contains_key(&case.id);
    drop(existing);
    let _ = workers.shutdown(&mut registry);
    assert!(!pending, "lease failure must release its owner reservation");
}

/// Lease acquisition transfers accounting before preparation completes.
#[tokio::test]
async fn owner_lease_acquisition_retires_pending_without_double_counting() {
    let (mut registry, mut workers, notify, _) = owner();
    let leases = SessionLeases::default();
    let (case, launch, _) = Case::prepare(&mut registry, 125, "state-reducer", vec![]);
    let (entered, seen) = mpsc::sync_channel(1);
    let (release, barrier) = mpsc::sync_channel(1);
    let generation = workers
        .start(&mut registry, limited_launch(launch, &leases), move |_| {
            entered.send(()).unwrap();
            barrier
                .recv_timeout(LIMIT)
                .map_err(|_| io::Error::other("profile barrier failed"))?;
            Err(io::Error::other("fixture preparation failure").into())
        })
        .unwrap();
    seen.recv_timeout(LIMIT)
        .expect("preparation must acquire its lease first");
    let count_before = workers.counts(&leases, "fixture-parent");
    workers.poll(&mut registry, Instant::now(), 1000);
    let pending = workers.pending.contains_key(&case.id);
    let count_after = workers.counts(&leases, "fixture-parent");
    release.send(()).unwrap();
    wait(&notify, || {
        workers.poll(&mut registry, Instant::now(), 1000);
        workers.report(&case.id, generation).is_some()
    })
    .await;
    workers.shutdown(&mut registry).unwrap();
    assert!(
        !pending,
        "the lease replaces, rather than supplements, pending ownership"
    );
    assert_eq!(count_before, Ok((1, 1)));
    assert_eq!(count_after, Ok((1, 1)));
}

#[tokio::test]
async fn finished_without_report_allows_retained_recovery() {
    let wake = Arc::new(Notify::new());
    let mut registry = Registry::default();
    let mut workers = Workers::new(wake.clone());
    let case = Case::uncertain(&mut registry, &mut workers, 432, &wake).await;
    let worker = workers.entries.get_mut(&case.id).unwrap();
    assert!(worker.finished && worker.thread.is_none());
    worker.report = None;
    assert!(workers.history_recovery_allowed(&case.id));
}
