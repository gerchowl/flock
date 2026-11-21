//! Journal ownership follows handoff admission, not process lifetime.
use super::{
    journal::{ReplayProgress, Writer},
    ErrorCode,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{oneshot, watch, Mutex, Notify};
/// Lease acquisition and drain callers wait at most one second. Replay is bounded
/// by journal work and a separate stall window. Expiry never releases a live lease.
const JOURNAL_WAIT: Duration = Duration::from_secs(1);
/// No total replay deadline: each consumed chunk or applied record resets this
/// window. Ten seconds gives one storage operation room without waiting forever.
const REPLAY_STALL: Duration = Duration::from_secs(10);
/// Failed acquisitions retry off-loop, without reopening command admission.
const ACQUIRE_RETRY: Duration = Duration::from_millis(250);
#[cfg(test)]
type OpenBarrier = (oneshot::Sender<()>, oneshot::Receiver<()>);
type OpenReply = (
    oneshot::Receiver<()>,
    oneshot::Receiver<Result<(), ErrorCode>>,
);
#[derive(Default)]
struct Slot {
    phase: u64,
    writer: Option<Writer>,
}
struct Inner {
    data: PathBuf,
    wake: Arc<Notify>,
    phase: Arc<AtomicU64>,
    stopped: AtomicBool,
    writer: Mutex<Slot>,
    pending: parking_lot::Mutex<Arc<AtomicUsize>>,
    #[cfg(test)]
    open_barrier: parking_lot::Mutex<Option<OpenBarrier>>,
}
#[derive(Clone)]
pub(in crate::web::wire) struct Ownership(Arc<Inner>);
impl Ownership {
    pub(in crate::web::wire) fn data(&self) -> &std::path::Path {
        &self.0.data
    }

    pub(in crate::web::wire) fn new(data: PathBuf, wake: Arc<Notify>) -> Self {
        #[cfg(test)]
        let open_barrier = tests::take_open_barrier(&data);
        Self(Arc::new(Inner {
            data,
            wake,
            phase: Arc::new(AtomicU64::new(0)),
            stopped: AtomicBool::new(false),
            writer: Mutex::new(Slot::default()),
            pending: parking_lot::Mutex::new(Arc::new(AtomicUsize::new(0))),
            #[cfg(test)]
            open_barrier: parking_lot::Mutex::new(open_barrier),
        }))
    }
    #[cfg(test)]
    pub(in crate::web::wire) fn observe_writes_test(
        &self,
    ) -> super::journal::filesystem::Observation {
        super::journal::filesystem::Observation::start(self.0.data.join("wire"))
    }
    #[cfg(test)]
    pub(in crate::web::wire) async fn seed_capacity_test(&self) {
        self.submit(
            self.0.phase.load(Ordering::Acquire),
            super::journal::Operation::SeedCapacity,
        )
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    }
    #[cfg(test)]
    pub(in crate::web::wire) async fn configure_test(
        &self,
        limits: Option<(usize, usize)>,
        before_sync: Option<Box<dyn FnMut() + Send>>,
    ) {
        let phase = self.0.phase.load(Ordering::Acquire);
        self.submit(
            phase,
            super::journal::Operation::Configure {
                limits,
                before_sync,
            },
        )
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    }
    #[cfg(test)]
    pub(in crate::web::wire) fn claim_test(
        &self,
        command: super::Command,
    ) -> impl std::future::Future<Output = ()> {
        let phase = self.0.phase.load(Ordering::Acquire);
        let reply = self
            .submit(
                phase,
                super::journal::Operation::Claim {
                    namespace: "local".into(),
                    principal: "seed".into(),
                    command,
                    now: crate::app::notifications::now_ms(),
                },
            )
            .unwrap();
        async move {
            reply.await.unwrap().unwrap();
        }
    }
    pub(super) fn submit(
        &self,
        phase: u64,
        operation: super::journal::Operation,
    ) -> Result<super::journal::Response, ErrorCode> {
        let slot = self
            .0
            .writer
            .try_lock()
            .map_err(|_| ErrorCode::SessionBusy)?;
        if !self.current(phase) || phase & 1 == 0 || slot.phase | 1 != phase {
            return Err(ErrorCode::SessionBusy);
        }
        slot.writer
            .as_ref()
            .ok_or(ErrorCode::SessionBusy)?
            .submit(operation)
    }
    pub(in crate::web::wire) fn gate(&self) -> Arc<AtomicU64> {
        self.0.phase.clone()
    }
    // Even phases refuse admission. Each lifecycle intent invalidates older opens.
    fn close(&self) -> u64 {
        let previous = self
            .0
            .phase
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |phase| {
                Some((phase | 1).wrapping_add(1))
            })
            .unwrap_or_else(|phase| phase);
        (previous | 1).wrapping_add(1)
    }
    pub(in crate::web::wire) fn stop(&self) {
        self.0.stopped.store(true, Ordering::Release);
        self.close();
    }
    fn current(&self, phase: u64) -> bool {
        !self.0.stopped.load(Ordering::Acquire) && self.0.phase.load(Ordering::Acquire) == phase
    }
    fn abandoned(&self, stage: &'static str) {
        let pending_writes = self.0.pending.lock().load(Ordering::Acquire);
        tracing::warn!(
            stage,
            pending_writes,
            "command journal caller stopped waiting; work remains fenced"
        );
    }
    /// Startup/commit succeeds only after lease acquisition and replay.
    /// Lease acquisition has a deadline; capped replay has a progress-stall window.
    pub(in crate::web::wire) async fn acquire(&self) -> Result<(), ErrorCode> {
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(ErrorCode::SessionBusy);
        }
        let reply = self.recover(false);
        self.wait_open(reply).await?;
        if self.0.stopped.load(Ordering::Acquire) {
            Err(ErrorCode::SessionBusy)
        } else {
            Ok(())
        }
    }
    fn recover(&self, notify: bool) -> OpenReply {
        let phase = self.close();
        let (acquired, lease) = oneshot::channel();
        let (send, reply) = oneshot::channel();
        let owner = self.clone();
        tokio::spawn(async move {
            let mut send = Some(send);
            let mut acquired = Some(acquired);
            loop {
                let (progress, mut advanced) = watch::channel((0, 0));
                let stalled = Arc::new(AtomicBool::new(false));
                let opening = owner.acquire_at(phase, acquired.take(), progress, stalled.clone());
                tokio::pin!(opening);
                let idle = tokio::time::sleep(REPLAY_STALL);
                tokio::pin!(idle);
                let mut observing = true;
                let result = loop {
                    tokio::select! {
                        biased;
                        result = &mut opening => break result,
                        changed = advanced.changed(), if observing => {
                            observing = changed.is_ok();
                            if observing {
                                idle.as_mut().reset(tokio::time::Instant::now() + REPLAY_STALL);
                                #[cfg(test)]
                                tests::replay_observed(&owner.0.data, *advanced.borrow());
                            }
                        }
                        _ = &mut idle => {
                            stalled.store(true, Ordering::Release);
                            let (bytes, total) = *advanced.borrow();
                            crate::logging::client_journal_replay("stalled", bytes, total);
                            if let Some(send) = send.take() {
                                let _ = send.send(Err(ErrorCode::SessionBusy));
                            }
                            owner.0.wake.notify_one();
                            // Retain the worker and its lease until it stops. A
                            // late completion must drain, never reopen admission.
                            break opening.await;
                        }
                    }
                };
                let delivered = send.take().is_some_and(|send| send.send(result).is_ok());
                // Synchronous startup already observes its reply. Only later
                // recovery or rollback needs to wake the running owner loop.
                if notify || !delivered {
                    owner.0.wake.notify_one();
                }
                if result.is_ok() || !owner.current(phase) {
                    break;
                }
                tokio::time::sleep(ACQUIRE_RETRY).await;
            }
        });
        (lease, reply)
    }
    async fn wait_open(&self, (lease, reply): OpenReply) -> Result<(), ErrorCode> {
        let result = match tokio::time::timeout(JOURNAL_WAIT, lease).await {
            Ok(Ok(())) => reply.await.unwrap_or(Err(ErrorCode::SessionBusy)),
            _ => Err(ErrorCode::SessionBusy),
        };
        if result.is_err() {
            self.abandoned("open");
        }
        result
    }
    async fn acquire_at(
        &self,
        phase: u64,
        acquired: Option<oneshot::Sender<()>>,
        progress: ReplayProgress,
        stalled: Arc<AtomicBool>,
    ) -> Result<(), ErrorCode> {
        let mut slot = self.0.writer.lock().await;
        if !self.current(phase) {
            return Err(ErrorCode::SessionBusy);
        }
        // Recovery after a freeze must finish the old drain before replay, even
        // when its caller already timed out and a later intent reached us first.
        if slot.phase != phase {
            if let Some(writer) = slot.writer.take() {
                writer.shutdown().await?;
            }
        }
        if slot.writer.is_none() {
            #[cfg(test)]
            {
                let barrier = self.0.open_barrier.lock().take();
                if let Some((entered, release)) = barrier {
                    let _ = entered.send(());
                    tokio::time::timeout(Duration::from_secs(5), release)
                        .await
                        .map_err(|_| ErrorCode::SessionBusy)?
                        .map_err(|_| ErrorCode::SessionBusy)?;
                }
            }
            let writer = Writer::open_signalled(
                self.0.data.clone(),
                self.0.wake.clone(),
                acquired,
                Some(progress),
            )
            .await?;
            *self.0.pending.lock() = writer.pending();
            slot.phase = phase;
            slot.writer = Some(writer);
        } else if let Some(acquired) = acquired {
            let _ = acquired.send(());
        }
        if stalled.load(Ordering::Acquire)
            || self.0.stopped.load(Ordering::Acquire)
            || self
                .0
                .phase
                .compare_exchange(phase, phase | 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            if let Some(writer) = slot.writer.take() {
                writer.shutdown().await?;
            }
            return Err(ErrorCode::SessionBusy);
        }
        Ok(())
    }
    pub(in crate::web::wire) async fn freeze(&self) -> Result<(), ErrorCode> {
        let phase = self.close();
        let owner = self.clone();
        // Dropping this JoinHandle on timeout detaches, never cancels, the drain.
        let drain = tokio::spawn(async move {
            let mut slot = owner.0.writer.lock().await;
            // An abandoned old drain must not close a newer recovered owner.
            if slot.phase <= phase {
                if let Some(writer) = slot.writer.take() {
                    writer.shutdown().await?;
                }
            }
            Ok::<_, ErrorCode>(())
        });
        if !matches!(
            tokio::time::timeout(JOURNAL_WAIT, drain).await,
            Ok(Ok(Ok(())))
        ) {
            self.abandoned("drain");
        }
        Ok(())
    }
    /// The first lease wait is bounded; replay and retries retain admission fencing.
    pub(in crate::web::wire) fn reopen(&self) -> tokio::task::JoinHandle<Result<(), ErrorCode>> {
        let reply = self.recover(true);
        let owner = self.clone();
        tokio::spawn(async move { owner.wait_open(reply).await })
    }
    pub(in crate::web::wire) async fn shutdown(&self) -> Result<(), ErrorCode> {
        self.stop();
        self.freeze().await
    }
}
#[cfg(test)]
mod tests;
