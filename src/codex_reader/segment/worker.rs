//! Supervised reader work and capability-checked delivery (ADR-0028 §1a).
//! Runtime authority/consumer wiring remains #1664. One work unit per flight;
//! the controller supplies monotonic time and schedules `next_due()` wakeups.
use super::*;
use crate::codex_binding::Writer;
use crate::codex_rollout::{Boundary, Turn};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError},
    Arc,
};
use std::time::Duration;

/// Continuation cadence, not a wait for Codex events. With 1 MiB work units,
/// a 1 GiB pass takes at least 10.24 s plus I/O/controller scheduling time.
pub(crate) const STEP_INTERVAL: Duration = Duration::from_millis(10);
/// Idle between complete verification passes; never inserted between chunks.
pub(crate) const EOF_INTERVAL: Duration = Duration::from_millis(250);
/// One dispatch-to-delivery budget, including any notification wait.
pub(crate) const WORK_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard process-wide limit, including abandoned threads blocked in kernel I/O.
const MAX_READERS: usize = 128;
static LIVE: AtomicUsize = AtomicUsize::new(0);
struct Permit;
impl Permit {
    fn take() -> Result<Self, Fault> {
        LIVE.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_READERS).then_some(n + 1)
        })
        .map(|_| Self)
        .map_err(|_| Fault::Capacity)
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Source health failures, distinct from socket/CLI error codes. A failed
/// current-launch source cannot restore authority or select fallback by retrying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    Capacity,
    Busy,
    Spawn,
    Disconnected,
    Stalled,
    Writer,
    Revoked,
    Stopped,
    Source(Error),
}
/// Delivered evidence. EOF/progress times use the controller's supplied clock;
/// boundary age is diagnostic and never expires a quiet working/idle turn.
#[derive(Clone, Debug)]
pub(crate) struct Evidence {
    pub key: Key,
    pub writer: Writer,
    pub checkpoint: Checkpoint,
    pub turn: Turn,
    pub reason: Option<Reason>,
    pub boundary: Option<Boundary>,
    pub progress_at: Instant,
    pub eof_at: Option<Instant>,
}
enum Work {
    Admission(Box<Admission>),
    Segment(Box<Segment>),
}
impl Work {
    fn reader(&self) -> Result<&Reader, Fault> {
        match self {
            Self::Admission(a) => a.reader.as_ref().ok_or(Fault::Stopped),
            Self::Segment(s) => Ok(&s.reader),
        }
    }
    fn step(self, now: Instant) -> Result<(Self, bool), Fault> {
        let result = match self {
            Self::Admission(mut a) => a.advance(now).map(|ready| match ready {
                Some(segment) => (Self::Segment(Box::new(segment)), true),
                None => (Self::Admission(a), false),
            }),
            Self::Segment(mut s) => s.poll(now).and_then(|update| {
                if let Some(reason) = s
                    .observe(now)
                    .reason
                    .filter(|r| *r != Reason::PartialRecord)
                {
                    return Err(Error::Record(reason));
                }
                Ok((Self::Segment(s), update.eof_at.is_some()))
            }),
        };
        result.map_err(Fault::Source)
    }
}
struct Job {
    work: Work,
    now: Instant,
}
type Reply = Result<(Work, bool), Fault>;

/// Owns one bounded request queue, result queue and coalesced wake. It never
/// joins a potentially blocked reader. A detached reader retains its Permit
/// until actual exit, so repeated restarts cannot create unlimited threads.
pub(crate) struct Worker {
    key: Key,
    writer: Writer,
    requests: Option<SyncSender<Job>>,
    replies: Receiver<Reply>,
    wakes: Receiver<()>,
    cancel: Arc<AtomicBool>,
    work: Option<Work>,
    flight: Option<Instant>,
    due: Instant,
    admitted: bool,
    evidence: Option<Evidence>,
    fault: Option<Fault>,
}
impl Worker {
    /// Start from the real validated candidate, never a thread ID/copied reducer.
    /// Preparation performed no full-history read; admission I/O runs here.
    pub fn new(
        admission: Admission,
        binding: &mut Binding,
        owners: &mut Ownership,
        now: Instant,
    ) -> Result<Self, Fault> {
        let key = admission.key.clone();
        if !binding.pending(&key) {
            return Err(Fault::Revoked);
        }
        let result = Self::spawn(admission, now);
        if result.is_err() && binding.pending(&key) {
            let _ = binding.finish(&key, Err(Failure::SourceLost), owners);
        }
        result
    }
    fn spawn(admission: Admission, now: Instant) -> Result<Self, Fault> {
        let key = admission.key.clone();
        let writer = admission
            .reader
            .as_ref()
            .ok_or(Fault::Stopped)?
            .validated
            .writer;
        let permit = Permit::take()?;
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let (result_tx, replies) = mpsc::sync_channel(1);
        let (wake_tx, wakes) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let stopped = cancel.clone();
        std::thread::Builder::new()
            .name("codex-rollout".into())
            .spawn(move || {
                let _permit = permit;
                while let Ok(job) = rx.recv() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let result = job.work.step(job.now);
                    if stopped.load(Ordering::Acquire) || result_tx.try_send(result).is_err() {
                        break;
                    }
                    match wake_tx.try_send(()) {
                        Ok(()) | Err(mpsc::TrySendError::Full(())) => {}
                        Err(mpsc::TrySendError::Disconnected(())) => break,
                    }
                }
            })
            .map_err(|_| Fault::Spawn)?;
        Ok(Self {
            key,
            writer,
            requests: Some(tx),
            replies,
            wakes,
            cancel,
            work: Some(Work::Admission(Box::new(admission))),
            flight: None,
            due: now,
            admitted: false,
            evidence: None,
            fault: None,
        })
    }
    /// The controller must wake at this time and while work is in flight. A
    /// notification is only a hint; delivery always rechecks health/capability.
    pub fn next_due(&self) -> Instant {
        self.flight.map_or(self.due, |at| at + WORK_TIMEOUT)
    }
    /// Nonblocking delivery and dispatch. Empty progress never manufactures EOF
    /// or updates a lifecycle boundary. Quiet model output is not reader failure.
    pub fn tick(
        &mut self,
        binding: &mut Binding,
        owners: &mut Ownership,
        now: Instant,
    ) -> Result<Option<&Evidence>, Fault> {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }
        let result = self.drive(binding, owners, now);
        if let Err(fault) = result {
            return Err(self.fail(fault, binding, owners));
        }
        Ok(self.evidence.as_ref())
    }
    fn current(&self, binding: &Binding, owners: &Ownership) -> bool {
        if self.admitted {
            binding.accepts(&self.key, owners)
        } else {
            binding.pending(&self.key)
        }
    }
    fn drive(
        &mut self,
        binding: &mut Binding,
        owners: &mut Ownership,
        now: Instant,
    ) -> Result<(), Fault> {
        if !self.current(binding, owners) {
            return Err(Fault::Revoked);
        }
        self.writer_live()?;
        if self
            .flight
            .is_some_and(|at| now.saturating_duration_since(at) >= WORK_TIMEOUT)
        {
            return Err(Fault::Stalled);
        }
        match self.replies.try_recv() {
            Ok(reply) => {
                let (mut work, eof) = reply?;
                self.writer_live()?;
                if work.reader()?.validated.key() != &self.key
                    || work.reader()?.validated.writer != self.writer
                {
                    return Err(Fault::Revoked);
                }
                match self.replies.try_recv() {
                    Err(TryRecvError::Empty) => {}
                    _ => return Err(Fault::Disconnected),
                }
                work = match work {
                    Work::Segment(mut segment) => {
                        if !self.admitted {
                            segment.reader.validated = binding
                                .finish(&self.key, Ok(segment.reader.validated), owners)
                                .map_err(|e| Fault::Source(Error::Binding(e)))?;
                            self.admitted = true;
                        }
                        Work::Segment(segment)
                    }
                    other => other,
                };
                let observation = match &work {
                    Work::Segment(s) => Some(s.observe(now)),
                    _ => None,
                };
                self.evidence = Some(Evidence {
                    key: self.key.clone(),
                    writer: self.writer,
                    checkpoint: work.reader()?.scan.cursor.clone(),
                    turn: observation.as_ref().map_or(Turn::Unknown, |o| o.turn),
                    reason: observation.as_ref().and_then(|o| o.reason),
                    boundary: observation.and_then(|o| o.boundary.cloned()),
                    progress_at: now,
                    eof_at: if eof {
                        Some(now)
                    } else {
                        self.evidence.as_ref().and_then(|e| e.eof_at)
                    },
                });
                self.work = Some(work);
                self.flight = None;
                self.due = now + if eof { EOF_INTERVAL } else { STEP_INTERVAL };
            }
            Err(TryRecvError::Disconnected) => return Err(Fault::Disconnected),
            Err(TryRecvError::Empty) => {}
        }
        if self.flight.is_none() && now >= self.due {
            let work = self.work.take().ok_or(Fault::Stopped)?;
            self.requests
                .as_ref()
                .ok_or(Fault::Stopped)?
                .try_send(Job { work, now })
                .map_err(|e| match e {
                    mpsc::TrySendError::Full(_) => Fault::Capacity,
                    mpsc::TrySendError::Disconnected(_) => Fault::Disconnected,
                })?;
            self.flight = Some(now);
        }
        Ok(())
    }
    fn writer_live(&self) -> Result<(), Fault> {
        if crate::platform::process_start_time(self.writer.pid) != Some(self.writer.birth) {
            return Err(Fault::Writer);
        }
        Ok(())
    }
    /// Optional bounded notification wait, sharing the original dispatch budget.
    /// Supply the same monotonic clock as tick; no deadline is reset by a wake.
    pub fn wait(
        &mut self,
        binding: &mut Binding,
        owners: &mut Ownership,
        clock: impl Fn() -> Instant,
    ) -> Result<Option<&Evidence>, Fault> {
        self.tick(binding, owners, clock())?;
        if let Some(at) = self.flight {
            let remaining = WORK_TIMEOUT.saturating_sub(clock().saturating_duration_since(at));
            match self.wakes.recv_timeout(remaining) {
                Ok(()) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(self.fail(Fault::Stalled, binding, owners))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.fail(Fault::Disconnected, binding, owners))
                }
            }
        }
        self.tick(binding, owners, clock())
    }
    fn fail(&mut self, fault: Fault, binding: &mut Binding, owners: &mut Ownership) -> Fault {
        if fault == Fault::Writer {
            binding.writer_gone(self.writer, owners);
        }
        if binding.pending(&self.key) {
            let reason = if fault == Fault::Writer {
                Failure::Writer
            } else {
                Failure::SourceLost
            };
            let _ = binding.finish(&self.key, Err(reason), owners);
        } else {
            binding.lost(&self.key);
        }
        self.cancel.store(true, Ordering::Release);
        self.requests.take();
        self.work = None;
        self.evidence = None;
        self.fault = Some(fault.clone());
        fault
    }
    /// Transfer only an idle owned reducer under its still-current capability.
    /// Busy is a refusal, not a cancellation. Admission will reverify continuity.
    pub fn handoff(
        &mut self,
        binding: &mut Binding,
        owners: &mut Ownership,
        now: Instant,
    ) -> Result<Handoff, Fault> {
        self.tick(binding, owners, now)?;
        if !self.admitted || self.flight.is_some() {
            return Err(Fault::Busy);
        }
        let Work::Segment(segment) = self.work.take().ok_or(Fault::Stopped)? else {
            return Err(self.fail(Fault::Revoked, binding, owners));
        };
        let handoff = (*segment)
            .handoff(binding, owners, now)
            .map_err(Fault::Source);
        self.stop(binding, owners);
        handoff
    }
    /// Cancel on the controller before dropping/moving the pane; retirement of
    /// the source is a separate known intent, never inferred from this shutdown.
    pub fn stop(&mut self, binding: &mut Binding, owners: &mut Ownership) {
        self.fail(Fault::Stopped, binding, owners);
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.requests.take();
    }
}

#[cfg(test)]
mod tests;
