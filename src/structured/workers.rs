//! Owned workers for ADR-0036 S3a-3; no public start/admission route until #1843.
//!
//! Lease locks, preparation and supervision run off the app loop. Each launch has
//! three bounded owner messages (lease/ready/end) and the runtime's bounded stream queue.
//! Only `Received::state_input` adapts stream records, including book closes.
//! Polling drains four records per agent and rearms the shared wake when full.
//! Exact S4 deadlines share the app scheduler; silence never kills a process.
//! Shutdown and pre-handoff stop, drain and join workers with bounded deadlines.
//! Handoff stops sessions; explicit resume uses the stored session id.
//! Unconfirmed cleanup retains lease
//! authority and Unknown lifecycle, requiring explicit recovery rather than restart.
#![deny(missing_docs)]
use super::{
    launch::{LaunchError, LaunchProfile, SessionMcp},
    lease::SessionLeases,
    registry::{Record, Registry},
    runtime::{self, Exit, Runtime},
    state::Input,
    supervision::StopRequest,
    transport::StreamLocation,
};
use crate::api::{agent_state::Lifecycle, error_code::ErrorCode};
use std::{
    collections::BTreeMap,
    io,
    path::PathBuf,
    sync::{mpsc, Arc},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tokio::sync::Notify;
const BATCH: usize = 4;
mod admission;
pub(crate) use admission::Admission;

/// Internal launch policy, supplied by authenticated admission in #1843.
pub struct Launch {
    /// Stable registered identity.
    pub agent_id: String,
    /// Current registry epoch; replaced atomically by a fresh launch epoch.
    pub generation: u64,
    /// Shared native-session authority.
    pub leases: SessionLeases,
    /// Durable stream/supervision directory.
    pub state: PathBuf,
    /// Caller-supplied `[sessions]` storage policy, captured for this launch.
    pub storage: super::blobs::StorageConfig,
    /// Pinned flock executable and owning server socket for this session.
    pub mcp: SessionMcp,
    /// Caller-supplied sessions stall policy, with no guessed default.
    pub stall_after: Duration,
    /// Optional authenticated delegation lineage.
    pub lineage: Option<(String, u32)>,
    /// Agent-caller policy; None is the already-attested operator exemption.
    pub(crate) admission: Option<Admission>,
}
/// Completion available only after preceding stream records have been reduced.
#[derive(Debug)]
pub struct Report {
    /// Actual process outcome or typed preparation/worker failure.
    pub result: Result<Exit, (ErrorCode, String)>,
    /// Missing cleanup evidence conservatively retains authority.
    pub authority_retained: bool,
}
enum Message {
    Leased,
    Ready(Runtime),
    End(Report),
}
struct Worker {
    generation: u64,
    messages: mpsc::Receiver<Message>,
    runtime: Option<Runtime>,
    stop: StopRequest,
    report: Option<Report>,
    finished: bool,
    events_closed: bool,
    stall_after: Duration,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    fn join_finished(&mut self) {
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            && self
                .thread
                .take()
                .is_some_and(|thread| thread.join().is_err())
        {
            self.report = Some(Report {
                result: Err((
                    ErrorCode::StructuredWorkerPanicked,
                    "structured worker panicked".into(),
                )),
                authority_retained: true,
            });
        }
    }
    fn stop(&self) {
        self.stop.request();
        if let Some(runtime) = &self.runtime {
            runtime.stop();
        }
    }
    fn reduce(
        &self,
        registry: &mut Registry,
        id: &str,
        input: Input<'_>,
        now: Instant,
        unix_ms: u64,
    ) -> bool {
        registry
            .apply(id, self.generation, input, now, unix_ms, self.stall_after)
            .is_ok()
    }
}
/// Runtime ownership outside pure AppState. Polling never waits for a join.
/// Explicit shutdown is bounded and must run before handoff or server exit.
/// Completed reports are bounded to the latest launch per agent. Removed records
/// discard confirmed completions, while uncertain ownership remains recoverable.
pub struct Workers {
    entries: BTreeMap<String, Worker>,
    history: BTreeMap<String, (u64, super::history::History)>,
    pending: BTreeMap<String, Option<String>>,
    #[cfg(test)]
    before_reserve: Option<mpsc::Receiver<()>>,
    notify: Arc<Notify>,
    // Coalesced wake for teardown while the async server loop is quiescent.
    wake: mpsc::SyncSender<()>,
    wakes: mpsc::Receiver<()>,
    // One teardown per admitted worker set, including failed attempts.
    shutdown_result: Option<Result<(), ShutdownError>>,
}
impl Workers {
    /// Share the app's event wake, including while no client is connected.
    pub fn new(notify: Arc<Notify>) -> Self {
        let (wake, wakes) = mpsc::sync_channel(1);
        Self {
            wake,
            wakes,
            shutdown_result: None,
            entries: BTreeMap::new(),
            history: BTreeMap::new(),
            pending: BTreeMap::new(),
            #[cfg(test)]
            before_reserve: None,
            notify,
        }
    }
    /// Reserve before preparation, then bind the registered cwd/model before spawn.
    /// Preparation must be bounded by its owner. Stop during preparation is latched
    /// and checked on both sides of the factory. Live/unconfirmed workers refuse
    /// replacement. Failure may advance the observation epoch but never the record.
    pub fn start(
        &mut self,
        registry: &mut Registry,
        launch: Launch,
        prepare: impl FnOnce(&Record) -> Result<LaunchProfile, LaunchError> + Send + 'static,
    ) -> Result<u64, ErrorCode> {
        let Launch {
            agent_id,
            generation: current,
            leases,
            state,
            storage,
            mcp,
            stall_after,
            lineage,
            admission,
        } = launch;
        if let Some(previous) = self.entries.get_mut(&agent_id) {
            previous.join_finished();
            if !previous.finished
                || previous.thread.is_some()
                || previous
                    .report
                    .as_ref()
                    .is_some_and(|r| r.authority_retained)
            {
                return Err(ErrorCode::SessionBusy);
            }
        }
        let record = registry
            .get(&agent_id)
            .filter(|(_, _, g)| *g == current)
            .map(|(r, _, _)| r.clone())
            .ok_or(ErrorCode::AgentNotFound)?;
        if registry
            .get(&agent_id)
            .is_some_and(|(_, p, _)| p.update.state.lifecycle == Lifecycle::Unknown)
        {
            return Err(ErrorCode::StructuredRecoveryRequired);
        }
        self.reserve_pending(
            &agent_id,
            lineage.as_ref().map(|(id, _)| id.as_str()),
            &leases,
            admission.as_ref(),
        )?;
        let generation = registry.begin_launch(&agent_id, current).map_err(|error| {
            self.pending.remove(&agent_id);
            match error {
                super::registry::RegistryError::RecoveryRequired => {
                    ErrorCode::StructuredRecoveryRequired
                }
                _ => ErrorCode::AgentStartFailed,
            }
        })?;
        let (tx, messages) = mpsc::sync_channel(3);
        let stop = StopRequest::default();
        let cancelled = stop.clone();
        let notify = self.notify.clone();
        let wake = self.wake.clone();
        #[cfg(test)]
        let before_reserve = self.before_reserve.take();
        let thread = std::thread::Builder::new()
            .name("structured-launch".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(test)]
                    if let Some(barrier) = before_reserve {
                        barrier.recv_timeout(Duration::from_secs(10)).map_err(|_| {
                            (
                                ErrorCode::AgentStartFailed,
                                "reservation barrier failed".into(),
                            )
                        })?;
                    }
                    let lease = leases
                        .reserve(
                            &record.agent_id,
                            &record.session_id,
                            lineage.as_ref().map(|(id, depth)| (id.as_str(), *depth)),
                        )
                        .map_err(|code| (code, code.as_str().to_owned()))?;
                    let _ = tx.try_send(Message::Leased);
                    notify.notify_one();
                    let _ = wake.try_send(());
                    let prepare = || -> Result<_, LaunchError> {
                        if cancelled.requested() {
                            return Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "structured launch stopped",
                            )
                            .into());
                        }
                        let profile =
                            prepare(&record)?.for_agent(&record.cwd, record.model.as_deref())?;
                        if cancelled.requested() {
                            return Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "structured launch stopped",
                            )
                            .into());
                        }
                        Ok(profile)
                    };
                    let profile = prepare().map_err(|error| (error.code(), error.to_string()))?;
                    let location = StreamLocation {
                        data_dir: state.clone(),
                        storage,
                        agent: record.agent_id.clone(),
                        session: record.session_id.clone(),
                    };
                    let event_notify = notify.clone();
                    let event_wake = wake.clone();
                    runtime::run_controlled(
                        &state,
                        (profile, mcp),
                        lease,
                        location,
                        |runtime| {
                            if cancelled.requested() {
                                runtime.stop();
                            }
                            let _ = tx.try_send(Message::Ready(runtime));
                            notify.notify_one();
                            let _ = wake.try_send(());
                        },
                        move || {
                            event_notify.notify_one();
                            let _ = event_wake.try_send(());
                        },
                        cancelled.clone(),
                    )
                    .map_err(|error| (error.code(), error.to_string()))
                }))
                .unwrap_or_else(|_| {
                    Err((
                        ErrorCode::StructuredWorkerPanicked,
                        "structured launch worker panicked".into(),
                    ))
                });
                let authority_retained = leases.is_reserved(&record.session_id).unwrap_or(true);
                let _ = tx.try_send(Message::End(Report {
                    result,
                    authority_retained,
                }));
                notify.notify_one();
                let _ = wake.try_send(());
            })
            .map_err(|_| {
                self.pending.remove(&agent_id);
                ErrorCode::AgentStartFailed
            })?;
        self.shutdown_result = None;
        self.entries.insert(
            agent_id,
            Worker {
                generation,
                messages,
                runtime: None,
                stop,
                report: None,
                finished: false,
                events_closed: false,
                stall_after,
                thread: Some(thread),
            },
        );
        Ok(generation)
    }
    /// Borrow this launch's nonblocking commands; no handle exists during preparation.
    pub fn runtime(&self, id: &str, generation: u64) -> Option<&Runtime> {
        self.entries
            .get(id)
            .filter(|w| w.generation == generation && !w.finished)
            .and_then(|w| w.runtime.as_ref())
    }
    /// Registry-fenced stored stream, retained after runtime exit. This handle
    /// lives with runtime ownership, never inside the pure registry/AppState.
    pub fn history(&self, registry: &Registry, id: &str) -> Option<&super::history::History> {
        let (generation, source) = self.history.get(id)?;
        let (_, _, current) = registry.get(id)?;
        (*generation == current).then_some(source)
    }
    /// Install a closed-file source recovered on an I/O worker. A changed epoch,
    /// active producer or already installed source refuses stale recovery.
    pub fn install_retained(
        &mut self,
        registry: &Registry,
        id: &str,
        generation: u64,
        source: super::history::History,
    ) -> Result<(), ErrorCode> {
        if registry.get(id).is_none_or(|(record, _, g)| {
            g != generation || !source.belongs_to(&record.agent_id, &record.session_id)
        }) {
            return Err(ErrorCode::AgentNotFound);
        }
        if self.history(registry, id).is_some() || self.entries.get(id).is_some_and(|w| !w.finished)
        {
            return Err(ErrorCode::SessionBusy);
        }
        self.history.insert(id.into(), (generation, source));
        Ok(())
    }
    /// Latch cancellation without blocking on preparation, process or I/O cleanup.
    pub fn stop(&self, id: &str, generation: u64) -> bool {
        let Some(worker) = self.entries.get(id).filter(|w| w.generation == generation) else {
            return false;
        };
        worker.stop();
        true
    }
    /// Inspect terminal evidence only after queued stream facts have been projected.
    pub fn report(&self, id: &str, generation: u64) -> Option<&Report> {
        self.entries
            .get(id)
            .filter(|w| w.generation == generation && w.finished)
            .and_then(|w| w.report.as_ref())
    }
    /// Drain bounded batches and due observations. Old generations are cancelled
    /// and fenced by the pure registry; terminal reports cannot overtake stdout.
    pub fn poll(&mut self, registry: &mut Registry, now: Instant, unix_ms: u64) -> bool {
        let mut changed = false;
        for (id, worker) in &mut self.entries {
            if worker.finished {
                continue;
            }
            if registry
                .get(id)
                .is_none_or(|(_, _, g)| g != worker.generation)
            {
                worker.stop();
            }
            for _ in 0..3 {
                if worker.report.is_some() {
                    break;
                }
                match worker.messages.try_recv() {
                    Ok(Message::Leased) => {
                        self.pending.remove(id);
                    }
                    Ok(Message::Ready(runtime)) => {
                        self.history
                            .insert(id.clone(), (worker.generation, runtime.history.clone()));
                        if worker.stop.requested() {
                            runtime.stop();
                        }
                        worker.runtime = Some(runtime);
                        changed |= worker.reduce(
                            registry,
                            id,
                            Input::Lifecycle(Lifecycle::Live),
                            now,
                            unix_ms,
                        );
                    }
                    Ok(Message::End(report)) => {
                        self.pending.remove(id);
                        worker.report = Some(report);
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.pending.remove(id);
                        worker.stop();
                        worker.report = Some(Report {
                            result: Err((
                                ErrorCode::AgentStartFailed,
                                "structured worker ended without completion".into(),
                            )),
                            authority_retained: true,
                        });
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            for index in 0..BATCH {
                let Some(runtime) = &worker.runtime else {
                    break;
                };
                match runtime.try_receive() {
                    Ok(received) => {
                        if let Some(input) = received.state_input() {
                            changed |= worker.reduce(
                                registry,
                                id,
                                input,
                                now,
                                received.record.received_at_ms.min(u128::from(u64::MAX)) as u64,
                            );
                        }
                        if index + 1 == BATCH {
                            self.notify.notify_one();
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        worker.events_closed = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            if registry.get(id).is_some_and(|(_, view, g)| {
                g == worker.generation && view.update.observe_at.is_some_and(|at| at <= now)
            }) {
                changed |= worker.reduce(registry, id, Input::Observe, now, unix_ms);
            }
            if let Some(report) = worker
                .report
                .as_ref()
                .filter(|_| worker.events_closed || worker.runtime.is_none())
            {
                if !report.authority_retained {
                    changed |= worker.reduce(registry, id, Input::ProcessExited, now, unix_ms);
                }
                changed |= worker.reduce(
                    registry,
                    id,
                    Input::Lifecycle(if report.authority_retained {
                        Lifecycle::Unknown
                    } else {
                        Lifecycle::Offline
                    }),
                    now,
                    unix_ms,
                );
                worker.join_finished();
                if worker.thread.is_some() {
                    self.notify.notify_one();
                    continue;
                }
                worker.finished = true;
                worker.runtime = None;
                changed = true;
            }
        }
        self.history.retain(|id, (generation, _)| {
            registry.get(id).is_some_and(|(_, _, g)| g == *generation)
        });
        // Removed records need no completed handle; uncertain ownership must survive.
        self.entries.retain(|id, worker| {
            worker.join_finished();
            worker.thread.is_some()
                || !worker.finished
                || registry.get(id).is_some()
                || worker
                    .report
                    .as_ref()
                    .is_some_and(|report| report.authority_retained)
        });
        changed
    }
    /// Stop all sessions before shutdown or handoff. Drain durable book closes,
    /// release confirmed leases, then join. Offline records resume only explicitly.
    /// After the grace deadline kill confirmed child identities and allow a bounded
    /// cleanup interval. Diagnostics are cached and reported without vetoing handoff.
    /// Uncertain records cross handoff as Unknown; this call never retries teardown.
    pub fn shutdown(&mut self, registry: &mut Registry) -> Result<(), ShutdownError> {
        self.shutdown_until(registry, Duration::from_secs(10), Duration::from_secs(10))
    }
    fn shutdown_until(
        &mut self,
        registry: &mut Registry,
        grace: Duration,
        cleanup: Duration,
    ) -> Result<(), ShutdownError> {
        if let Some(result) = &self.shutdown_result {
            return result.clone();
        }
        let result = self.shutdown_once(registry, grace, cleanup);
        for (id, worker) in &self.entries {
            if !worker.finished || worker.report.as_ref().is_some_and(|r| r.authority_retained) {
                worker.reduce(
                    registry,
                    id,
                    Input::Lifecycle(Lifecycle::Unknown),
                    Instant::now(),
                    crate::app::notifications::now_ms(),
                );
            }
        }
        self.shutdown_result = Some(result.clone());
        result
    }
    fn shutdown_once(
        &mut self,
        registry: &mut Registry,
        grace: Duration,
        cleanup: Duration,
    ) -> Result<(), ShutdownError> {
        for worker in self.entries.values() {
            worker.stop();
        }
        let mut deadline = Instant::now() + grace;
        let mut failures = Vec::new();
        let mut forced = false;
        loop {
            self.poll(
                registry,
                Instant::now(),
                crate::app::notifications::now_ms(),
            );
            if self
                .entries
                .values()
                .all(|w| w.thread.is_none() && w.finished)
            {
                for (id, worker) in &self.entries {
                    if worker.report.as_ref().is_some_and(|r| r.authority_retained) {
                        worker.stop.kill();
                        failures.push((id.clone(), ErrorCode::StructuredWorkerCleanupFailed));
                    }
                }
                return if failures.is_empty() {
                    Ok(())
                } else {
                    Err(ShutdownError(failures))
                };
            }
            let now = Instant::now();
            if now >= deadline {
                if forced {
                    return Err(ShutdownError(failures));
                }
                for (id, worker) in &self.entries {
                    if worker.thread.is_some() || !worker.finished {
                        failures.push((id.clone(), ErrorCode::StructuredWorkerTimeout));
                        worker.stop.kill();
                    }
                }
                forced = true;
                deadline = now + cleanup;
                continue;
            }
            // End is sent just before the thread returns. Never block joining a
            // running thread, even in that final publication/return interval.
            if self
                .entries
                .values()
                .any(|w| w.report.is_some() && w.thread.is_some())
            {
                std::thread::yield_now();
            } else {
                let _ = self
                    .wakes
                    .recv_timeout(deadline.saturating_duration_since(now));
            }
        }
    }
    /// Earliest exact S4 observation boundary for current unfinished launches.
    pub fn next_deadline(&self, registry: &Registry) -> Option<Instant> {
        self.entries
            .iter()
            .filter(|(_, w)| !w.finished)
            .filter_map(|(id, w)| {
                registry
                    .get(id)
                    .filter(|(_, _, g)| *g == w.generation)
                    .and_then(|(_, view, _)| view.update.observe_at)
            })
            .min()
    }
}
/// Typed shutdown failures, paired with the affected stable agent identity.
#[derive(Debug, Clone)]
pub struct ShutdownError(pub Vec<(String, ErrorCode)>);
impl std::fmt::Display for ShutdownError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("structured worker shutdown failed")?;
        for (id, code) in &self.0 {
            write!(f, "; {id}: {code}")?;
        }
        Ok(())
    }
}
impl std::error::Error for ShutdownError {}
impl Drop for Workers {
    fn drop(&mut self) {
        if self.shutdown_result.is_none() {
            if let Err(error) = self.shutdown(&mut Registry::default()) {
                crate::logging::structured_worker_shutdown_failed(&error);
            }
        } else {
            for worker in self.entries.values_mut() {
                worker.join_finished();
            }
        }
    }
}
#[cfg(test)]
pub(crate) mod tests;
