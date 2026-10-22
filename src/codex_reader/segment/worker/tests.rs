use super::*;
use crate::codex_binding::{self, Decision, Launch, Owner};
use crate::codex_reader::segment::tests::{event, key, limits, owner, Fixture};
use crate::codex_version::{Compatibility, TESTED_VERSION};
use std::{fs, process::Stdio};

const CEILING: Duration = Duration::from_secs(5);
// Only fixtures block for actual producer evidence, with one shared ceiling.
// The production controller API performs no receive or notification wait.
fn wait<'a>(
    worker: &'a mut Worker,
    binding: &mut Binding,
    owners: &mut Ownership,
    clock: impl Fn() -> Instant,
) -> Result<Option<&'a Evidence>, Fault> {
    worker.tick(binding, owners, clock())?;
    if let Some(at) = worker.flight {
        let remaining = WORK_TIMEOUT
            .saturating_sub(clock().saturating_duration_since(at))
            .min(CEILING);
        worker
            .wakes
            .recv_timeout(remaining)
            .expect("reader notification missing or producer disconnected");
    }
    worker.tick(binding, owners, clock())
}
struct Rig {
    fixture: Fixture,
    binding: Binding,
    owners: Ownership,
    worker: Worker,
    now: Instant,
    ceiling: Instant,
}
impl Rig {
    fn new(work_bytes: usize) -> Self {
        let fixture = Fixture::new();
        fixture.append(&event("task_started", "active"));
        let mut binding = Binding::new(owner());
        let mut owners = Ownership::default();
        let k = key(&mut binding);
        let admission = Admission::begin(
            &mut binding,
            &k,
            fixture.validated(&k),
            Mode::Fresh,
            &mut owners,
            Limits {
                work_bytes,
                ..limits()
            },
        )
        .unwrap();
        let now = Instant::now();
        let worker = Worker::new(admission, &mut binding, &mut owners, now).unwrap();
        Self {
            fixture,
            binding,
            owners,
            worker,
            now,
            ceiling: now + CEILING,
        }
    }
    fn complete(&mut self) -> Evidence {
        let end = self.ceiling;
        for _ in 0..10_000 {
            assert!(
                Instant::now() < end,
                "producer wakes never completed a work unit"
            );
            wait(
                &mut self.worker,
                &mut self.binding,
                &mut self.owners,
                || self.now,
            )
            .unwrap();
            if self.worker.flight.is_none() {
                return self
                    .worker
                    .evidence
                    .clone()
                    .expect("completed work evidence");
            }
        }
        panic!("producer wakes never delivered a work result");
    }
    fn admitted(&mut self) -> Evidence {
        let end = self.ceiling;
        for _ in 0..10_000 {
            assert!(Instant::now() < end, "reader never completed admission EOF");
            let evidence = self.complete();
            if self.worker.admitted {
                return evidence;
            }
            assert_eq!(self.binding.decision(&self.owners), Decision::Unknown);
            assert!(evidence.eof_at.is_none());
            self.now = self.worker.next_due();
        }
        panic!("reader never reached admission EOF");
    }
    fn tick(&mut self) -> Result<Option<&Evidence>, Fault> {
        self.worker
            .tick(&mut self.binding, &mut self.owners, self.now)
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.worker.stop(&mut self.binding, &mut self.owners);
        // Closure exit drops the sole sender, proving no owned producer remains.
        let closed = producer_closed(&self.worker.replies);
        if !std::thread::panicking() {
            assert!(closed, "reader did not stop");
        }
    }
}

#[test]
fn real_worker_admits_multistep_history_reports_eof_and_keeps_quiet_turns_without_a_lease() {
    let mut rig = Rig::new(13);
    let admitted = rig.admitted();
    assert_eq!(admitted.key, rig.worker.key);
    assert_eq!(admitted.writer, rig.worker.writer);
    assert_eq!(
        admitted.checkpoint.read_offset,
        fs::metadata(&rig.fixture.path).unwrap().len()
    );
    assert_eq!(admitted.turn, Turn::Working);
    assert!(admitted.eof_at.is_some());
    let start = admitted.boundary.clone().unwrap();
    // An end omitted by the writer cannot be inferred from the passage of time.
    for hours in [1, 10, 100] {
        rig.now += Duration::from_secs(hours * 3600);
        rig.complete();
        assert_eq!(
            rig.worker.evidence.as_ref().unwrap().boundary.as_ref(),
            Some(&start)
        );
        for _ in 0..10000 {
            if rig.worker.evidence.as_ref().unwrap().eof_at == Some(rig.now) {
                break;
            }
            rig.now = rig.worker.next_due();
            rig.complete();
        }
        assert_eq!(rig.worker.evidence.as_ref().unwrap().eof_at, Some(rig.now));
        let evidence = rig.worker.evidence.as_ref().unwrap();
        assert_eq!(evidence.turn, Turn::Working);
        assert_eq!(evidence.boundary, Some(start.clone()));
    }
    rig.fixture.append(&event("task_complete", "active"));
    for _ in 0..10000 {
        rig.now = rig.worker.next_due();
        let evidence = rig.complete();
        if evidence.turn == Turn::Idle {
            break;
        }
    }
    assert_eq!(rig.worker.evidence.as_ref().unwrap().turn, Turn::Idle);
    let ended = rig.worker.evidence.as_ref().unwrap().boundary.clone();
    rig.now += Duration::from_secs(360_000);
    rig.complete();
    assert_eq!(rig.worker.evidence.as_ref().unwrap().turn, Turn::Idle);
    assert_eq!(rig.worker.evidence.as_ref().unwrap().boundary, ended);
}

#[test]
fn scheduling_pauses_after_eof_but_continues_prefix_units_at_the_step_interval() {
    let mut rig = Rig::new(7);
    rig.complete();
    assert_eq!(rig.worker.next_due(), rig.now + STEP_INTERVAL);
    assert!(rig.worker.evidence.as_ref().unwrap().eof_at.is_none());
    let progress = rig.worker.evidence.as_ref().unwrap().progress_at;
    rig.tick().unwrap();
    assert!(rig.worker.flight.is_none());
    assert_eq!(rig.worker.evidence.as_ref().unwrap().progress_at, progress);
    rig.now = rig.worker.next_due();
    rig.admitted();
    assert_eq!(rig.worker.next_due(), rig.now + EOF_INTERVAL);
    rig.tick().unwrap();
    assert!(rig.worker.flight.is_none());
}

#[test]
fn source_loss_and_reused_writer_identity_invalidate_cached_evidence() {
    for writer_mismatch in [false, true] {
        let mut rig = Rig::new(1024);
        rig.admitted();
        if writer_mismatch {
            rig.worker.writer.birth += 1;
        } else {
            fs::write(&rig.fixture.path, b"replaced contents\n").unwrap();
        }
        rig.now = rig.worker.next_due();
        let failure = if writer_mismatch {
            rig.tick().unwrap_err()
        } else {
            wait(&mut rig.worker, &mut rig.binding, &mut rig.owners, || {
                rig.now
            })
            .unwrap_err()
        };
        assert_eq!(
            failure,
            if writer_mismatch {
                Fault::Writer
            } else {
                Fault::Source(Error::Record(Reason::SourceDiscontinuity))
            }
        );
        assert_eq!(rig.binding.decision(&rig.owners), Decision::Unknown);
        assert!(rig.worker.evidence.is_none());
        rig.fixture.append(&event("task_complete", "active"));
        assert_eq!(rig.tick().unwrap_err(), failure);
    }
}

#[test]
fn local_writer_exit_is_observed_before_delivery_of_queued_results() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(
        crate::process::TracedCommand::new("/bin/sh", "rollout-worker-test")
            .args(["-c", "read reply"])
            .stdin(Stdio::piped())
            .spawn_traced()
            .unwrap(),
    );
    let writer = Writer {
        pid: child.0.id(),
        birth: crate::platform::process_start_time(child.0.id()).unwrap(),
    };
    let f = Fixture::new();
    f.append(&event("task_started", "active"));
    let mut binding = Binding::new(owner());
    let mut owners = Ownership::default();
    let k = key(&mut binding);
    let candidate = codex_binding::validate(
        &k,
        &Launch {
            owner: k.owner.clone(),
            generation: k.launch,
            home: f.root.path().to_owned(),
            writer,
        },
        Some(&f.path),
        &[writer],
        &Compatibility {
            metadata_version: None,
            writer_version: Some(TESTED_VERSION.into()),
            probe_failure: None,
        },
        4096,
    )
    .unwrap();
    let admission = Admission::begin(
        &mut binding,
        &k,
        Ok(candidate),
        Mode::Fresh,
        &mut owners,
        limits(),
    )
    .unwrap();
    let now = Instant::now();
    let mut worker = Worker::new(admission, &mut binding, &mut owners, now).unwrap();
    wait(&mut worker, &mut binding, &mut owners, || now).unwrap();
    assert_eq!(binding.decision(&owners), Decision::Bound);
    worker
        .tick(&mut binding, &mut owners, now + EOF_INTERVAL)
        .unwrap();
    assert!(worker.flight.is_some());
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    assert_eq!(
        worker
            .tick(&mut binding, &mut owners, now + EOF_INTERVAL)
            .unwrap_err(),
        Fault::Writer
    );
    assert_eq!(binding.decision(&owners), Decision::Unknown);
    worker.stop(&mut binding, &mut owners);
    drain_closed(&worker.replies);
}

fn producer_closed(rx: &Receiver<Reply>) -> bool {
    let end = Instant::now() + CEILING;
    for _ in 0..2 {
        match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            Err(mpsc::RecvTimeoutError::Disconnected) => return true,
            Err(mpsc::RecvTimeoutError::Timeout) => return false,
            Ok(_) => {}
        }
    }
    false
}
fn drain_closed(rx: &Receiver<Reply>) {
    assert!(producer_closed(rx), "reader producer failed to terminate");
}

struct Silent {
    release: SyncSender<()>,
    done: Receiver<()>,
}
impl Silent {
    fn install(rig: &mut Rig) -> Self {
        rig.worker.requests.take();
        drain_closed(&rig.worker.replies);
        let permit = Permit::take().unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let (wake_tx, wakes) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(CEILING);
            drop((permit, rx, result_tx, wake_tx));
            let _ = done_tx.send(());
        });
        ready_rx
            .recv_timeout(CEILING)
            .expect("silent producer never started");
        rig.worker.requests = Some(tx);
        rig.worker.replies = result_rx;
        rig.worker.wakes = wakes;
        Self { release, done }
    }
}
impl Drop for Silent {
    fn drop(&mut self) {
        let _ = self.release.try_send(());
        let stopped = self.done.recv_timeout(CEILING).is_ok();
        if !std::thread::panicking() {
            assert!(stopped, "silent producer failed to stop");
        }
    }
}

#[test]
fn alive_but_silent_reader_times_out_without_renewing_cached_idle_or_working() {
    for ended in [false, true] {
        let mut rig = Rig::new(1024);
        if ended {
            rig.fixture.append(&event("task_complete", "active"));
        }
        rig.admitted();
        assert_eq!(
            rig.worker.evidence.as_ref().unwrap().turn,
            if ended { Turn::Idle } else { Turn::Working }
        );
        let silent = Silent::install(&mut rig);
        rig.now = rig.worker.next_due();
        rig.tick().unwrap();
        let dispatched = rig.worker.flight.unwrap();
        rig.now = dispatched + WORK_TIMEOUT;
        assert_eq!(rig.tick().unwrap_err(), Fault::Stalled);
        assert_eq!(rig.binding.decision(&rig.owners), Decision::Unknown);
        assert!(rig.worker.evidence.is_none());
        assert!(rig.worker.requests.is_none());
        // Exiting the failed producer later must not heal a sticky failure.
        drop(silent);
        assert_eq!(rig.tick().unwrap_err(), Fault::Stalled);
    }
}

#[test]
fn dead_or_disconnected_reader_cannot_leave_cached_evidence_authoritative() {
    for admitted in [false, true] {
        let mut rig = Rig::new(1024);
        if admitted {
            rig.admitted();
        }
        rig.worker.requests.take();
        drain_closed(&rig.worker.replies);
        assert_eq!(rig.tick().unwrap_err(), Fault::Disconnected);
        assert_eq!(
            rig.binding.decision(&rig.owners),
            if admitted {
                Decision::Unknown
            } else {
                Decision::BindingValidationFailed(Failure::SourceLost)
            }
        );
        assert!(rig.worker.evidence.is_none());
    }
}

#[test]
fn backpressure_shutdown_and_abandoned_thread_capacity_are_bounded() {
    let mut rig = Rig::new(1024);
    rig.admitted();
    let silent = Silent::install(&mut rig);
    let live = LIVE.load(Ordering::Acquire);
    let mut permits = Vec::new();
    for _ in live..MAX_READERS {
        permits.push(Permit::take().unwrap());
    }
    assert!(matches!(Permit::take(), Err(Fault::Capacity)));
    rig.now = rig.worker.next_due();
    rig.tick().unwrap();
    // A second tick cannot overwrite/enqueue another owned work item.
    rig.tick().unwrap();
    assert!(rig.worker.work.is_none());
    assert!(matches!(
        rig.worker
            .handoff(&mut rig.binding, &mut rig.owners, rig.now),
        Err(Fault::Busy)
    ));
    assert_eq!(rig.binding.decision(&rig.owners), Decision::Bound);
    rig.worker.stop(&mut rig.binding, &mut rig.owners);
    assert!(rig.worker.requests.is_none());
    assert_eq!(rig.binding.decision(&rig.owners), Decision::Unknown);
    // Shutdown cannot release a permit held by a still-live blocked producer.
    assert_eq!(LIVE.load(Ordering::Acquire), MAX_READERS);
    let k = key(&mut rig.binding);
    let a = Admission::begin(
        &mut rig.binding,
        &k,
        rig.fixture.validated(&k),
        Mode::Fresh,
        &mut rig.owners,
        limits(),
    )
    .unwrap();
    assert!(matches!(
        Worker::new(a, &mut rig.binding, &mut rig.owners, rig.now),
        Err(Fault::Capacity)
    ));
    assert_eq!(rig.binding.decision(&rig.owners), Decision::Unknown);

    assert!(matches!(Permit::take(), Err(Fault::Capacity)));
    drop(silent);
    let recovered = Permit::take().expect("permit was not released by actual producer exit");
    drop((recovered, permits));
}

#[test]
fn stale_result_retirement_and_duplicate_live_ownership_cannot_restore_authority() {
    for retire in [false, true] {
        let mut rig = Rig::new(1024);
        rig.tick().unwrap();
        if retire {
            rig.binding.retire(&mut rig.owners).unwrap();
        }
        let current = key(&mut rig.binding);
        assert_eq!(rig.tick().unwrap_err(), Fault::Revoked);
        assert!(rig.binding.pending(&current));
        let a = Admission::begin(
            &mut rig.binding,
            &current,
            rig.fixture.validated(&current),
            Mode::Fresh,
            &mut rig.owners,
            limits(),
        )
        .unwrap();
        let mut worker = Worker::new(a, &mut rig.binding, &mut rig.owners, rig.now).unwrap();
        wait(&mut worker, &mut rig.binding, &mut rig.owners, || rig.now).unwrap();
        assert_eq!(rig.binding.decision(&rig.owners), Decision::Bound);
        assert_eq!(rig.tick().unwrap_err(), Fault::Revoked);
        worker.stop(&mut rig.binding, &mut rig.owners);
        drain_closed(&worker.replies);
    }
    let mut rig = Rig::new(1024);
    rig.admitted();
    let destination = Owner {
        node: "destination.example".into(),
        pane: "reused-label".into(),
    };
    let mut other = Binding::new(destination.clone());
    let k = other.begin(&destination, 0, "child".into()).unwrap();
    assert!(matches!(
        other.finish(&k, rig.fixture.validated(&k), &mut rig.owners),
        Err(Failure::DuplicateOwner)
    ));
    assert_eq!(rig.tick().unwrap_err(), Fault::Revoked);
    assert_eq!(rig.binding.decision(&rig.owners), Decision::Unknown);
    assert_eq!(
        other.decision(&rig.owners),
        Decision::BindingValidationFailed(Failure::DuplicateOwner)
    );
}

#[test]
fn compaction_handoff_stays_owned_and_destination_requires_its_own_local_claim() {
    let mut rig = Rig::new(1024);
    rig.admitted();
    let before = rig.worker.evidence.as_ref().unwrap().boundary.clone();
    let handoff = rig
        .worker
        .handoff(&mut rig.binding, &mut rig.owners, rig.now)
        .unwrap();
    drain_closed(&rig.worker.replies);
    let k = key(&mut rig.binding);
    let a = Admission::begin(
        &mut rig.binding,
        &k,
        rig.fixture.validated(&k),
        Mode::Compact(handoff),
        &mut rig.owners,
        limits(),
    )
    .unwrap();
    let mut compact = Worker::new(a, &mut rig.binding, &mut rig.owners, rig.now).unwrap();
    wait(&mut compact, &mut rig.binding, &mut rig.owners, || rig.now).unwrap();
    assert_eq!(compact.evidence.as_ref().unwrap().boundary, before);
    let handoff = compact
        .handoff(&mut rig.binding, &mut rig.owners, rig.now)
        .unwrap();
    drain_closed(&compact.replies);
    rig.binding.retire(&mut rig.owners).unwrap();
    let destination = Owner {
        node: "destination.example".into(),
        pane: "fixture".into(),
    };
    let mut dest = Binding::new(destination.clone());
    let k = dest.begin(&destination, 0, "child".into()).unwrap();
    assert!(matches!(
        Admission::begin(
            &mut dest,
            &k,
            rig.fixture.validated(&k),
            Mode::Compact(handoff),
            &mut rig.owners,
            limits()
        ),
        Err(Error::Compaction)
    ));
    assert_eq!(
        dest.decision(&rig.owners),
        Decision::BindingValidationFailed(Failure::Metadata)
    );
    assert_eq!(
        rig.binding.decision(&rig.owners),
        Decision::AwaitingFirstTurn
    );
}

#[test]
fn copied_history_needs_destination_home_writer_validation_and_a_new_boundary() {
    let mut rig = Rig::new(1024);
    rig.admitted();
    let mut capture = Capture::new(
        fs::File::open(&rig.fixture.path).unwrap(),
        rig.fixture.path.clone(),
        limits(),
    )
    .unwrap();
    let snapshot = capture.step(rig.now).unwrap().unwrap();
    rig.binding.retire(&mut rig.owners).unwrap();
    assert_eq!(rig.tick().unwrap_err(), Fault::Revoked);
    let f = Fixture::new();
    fs::copy(&rig.fixture.path, &f.path).unwrap();
    f.append(&event("task_started", "destination"));
    let destination = Owner {
        node: "destination.example".into(),
        pane: "reused-label".into(),
    };
    let mut dest = Binding::new(destination.clone());
    let k = dest.begin(&destination, 0, "child".into()).unwrap();
    let writer = Writer {
        pid: std::process::id(),
        birth: crate::platform::process_start_time(std::process::id()).unwrap(),
    };
    let compatibility = Compatibility {
        metadata_version: None,
        writer_version: Some(TESTED_VERSION.into()),
        probe_failure: None,
    };
    assert!(matches!(
        codex_binding::validate(
            &k,
            &Launch {
                owner: destination,
                generation: 0,
                home: rig.fixture.root.path().to_owned(),
                writer
            },
            Some(&f.path),
            &[writer],
            &compatibility,
            4096
        ),
        Err(Failure::Path)
    ));
    let a = Admission::begin(
        &mut dest,
        &k,
        f.validated(&k),
        Mode::Resume {
            snapshot,
            copied: true,
        },
        &mut rig.owners,
        limits(),
    )
    .unwrap();
    let mut worker = Worker::new(a, &mut dest, &mut rig.owners, rig.now).unwrap();
    wait(&mut worker, &mut dest, &mut rig.owners, || rig.now).unwrap();
    assert_eq!(
        worker
            .evidence
            .as_ref()
            .unwrap()
            .boundary
            .as_ref()
            .unwrap()
            .turn_id,
        "destination"
    );
    assert_eq!(dest.decision(&rig.owners), Decision::Bound);
    assert_eq!(rig.tick().unwrap_err(), Fault::Revoked);
    assert_eq!(
        rig.binding.decision(&rig.owners),
        Decision::AwaitingFirstTurn
    );
    worker.stop(&mut dest, &mut rig.owners);
    drain_closed(&worker.replies);
}

#[test]
fn failed_worker_admission_preserves_the_launch_trust_scope() {
    for trusted in [false, true] {
        let mut rig = Rig::new(1024);
        if trusted {
            rig.admitted();
        }
        let current = key(&mut rig.binding);
        rig.fixture.append(b"malformed complete record\n");
        let a = Admission::begin(
            &mut rig.binding,
            &current,
            rig.fixture.validated(&current),
            Mode::Fresh,
            &mut rig.owners,
            limits(),
        )
        .unwrap();
        let mut worker = Worker::new(a, &mut rig.binding, &mut rig.owners, rig.now).unwrap();
        assert_eq!(
            wait(&mut worker, &mut rig.binding, &mut rig.owners, || rig.now).unwrap_err(),
            Fault::Source(Error::Record(Reason::MalformedRecord))
        );
        assert_eq!(
            rig.binding.decision(&rig.owners),
            if trusted {
                Decision::Unknown
            } else {
                Decision::BindingValidationFailed(Failure::SourceLost)
            }
        );
        drain_closed(&worker.replies);
    }
}

#[test]
fn late_tick_accepts_queued_work_before_checking_the_outstanding_deadline() {
    for work_bytes in [7, 1024] {
        let mut rig = Rig::new(work_bytes);
        rig.tick().unwrap();
        // Real worker publication precedes the delayed controller clock sample.
        rig.worker
            .wakes
            .recv_timeout(CEILING)
            .expect("worker never published its result");
        rig.now += WORK_TIMEOUT + Duration::from_secs(1);
        let progress_at = rig
            .tick()
            .expect("ready work was mistaken for a stalled reader")
            .unwrap()
            .progress_at;
        assert_eq!(progress_at, rig.now);
        assert!(rig.worker.flight.is_none());
        assert!(rig.worker.fault.is_none());
        rig.now = rig.worker.next_due();
        rig.admitted();
        let boundary = rig.worker.evidence.as_ref().unwrap().boundary.clone();
        rig.now = rig.worker.next_due();
        rig.tick().unwrap();
        rig.worker
            .wakes
            .recv_timeout(CEILING)
            .expect("bound reader never completed its unit");
        rig.now += WORK_TIMEOUT + Duration::from_secs(1);
        assert_eq!(rig.tick().unwrap().unwrap().boundary, boundary);
        assert_eq!(rig.binding.decision(&rig.owners), Decision::Bound);
    }
}

#[test]
fn wake_poll_returns_without_waiting_for_a_live_silent_producer() {
    let mut rig = Rig::new(1024);
    rig.admitted();
    let (producer, wake_rx) = mpsc::sync_channel(1);
    rig.worker.wakes = wake_rx;
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let polled = rig.worker.poll_wake();
        result_tx
            .send(polled)
            .expect("wake poll consumer disappeared");
        drop(rig);
        stopped_tx
            .send(())
            .expect("wake poll cleanup consumer disappeared");
    });
    let result = result_rx.recv_timeout(CEILING);
    // Also unblock an injected blocking regression before asserting failure.
    drop(producer);
    stopped_rx
        .recv_timeout(CEILING)
        .expect("wake poll producer failed to stop");
    assert!(!result.expect("wake poll blocked on a live silent producer"));
}
