//! Journal ownership follows handoff admission, not process lifetime.
//! J1b2a still dispatches through the in-memory duplicate list: no live command
//! claims or finishes the owned journal until J1b2b.
use super::{journal::Writer, ErrorCode};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::{Mutex, Notify};

#[cfg(test)]
type OpenBarrier = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
struct Inner {
    data: PathBuf,
    wake: Arc<Notify>,
    phase: Arc<AtomicU64>,
    stopped: AtomicBool,
    writer: Mutex<Option<Writer>>,
    #[cfg(test)]
    open_barrier: parking_lot::Mutex<Option<OpenBarrier>>,
}
#[derive(Clone)]
pub(in crate::web::wire) struct Ownership(Arc<Inner>);
impl Ownership {
    pub(in crate::web::wire) fn new(data: PathBuf, wake: Arc<Notify>) -> Self {
        Self(Arc::new(Inner {
            data,
            wake,
            phase: Arc::new(AtomicU64::new(0)),
            stopped: AtomicBool::new(false),
            writer: Mutex::new(None),
            #[cfg(test)]
            open_barrier: parking_lot::Mutex::new(None),
        }))
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
    pub(in crate::web::wire) async fn acquire(&self) -> Result<(), ErrorCode> {
        self.acquire_at(self.close()).await
    }
    async fn acquire_at(&self, phase: u64) -> Result<(), ErrorCode> {
        let mut slot = self.0.writer.lock().await;
        if self.0.stopped.load(Ordering::Acquire) || self.0.phase.load(Ordering::Acquire) != phase {
            return Err(ErrorCode::SessionBusy);
        }
        if slot.is_none() {
            #[cfg(test)]
            {
                let barrier = self.0.open_barrier.lock().take();
                if let Some((entered, release)) = barrier {
                    let _ = entered.send(());
                    tokio::time::timeout(std::time::Duration::from_secs(5), release)
                        .await
                        .map_err(|_| ErrorCode::SessionBusy)?
                        .map_err(|_| ErrorCode::SessionBusy)?;
                }
            }
            *slot = Some(Writer::open(self.0.data.clone(), self.0.wake.clone()).await?);
        }
        if self.0.stopped.load(Ordering::Acquire)
            || self
                .0
                .phase
                .compare_exchange(phase, phase | 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            if let Some(writer) = slot.take() {
                writer.shutdown().await?;
            }
            return Err(ErrorCode::SessionBusy);
        }
        Ok(())
    }
    pub(in crate::web::wire) async fn freeze(&self) -> Result<(), ErrorCode> {
        self.close();
        let mut slot = self.0.writer.lock().await;
        let result = match slot.take() {
            Some(writer) => writer.shutdown().await,
            None => Ok(()),
        };
        self.close();
        result
    }
    /// Rollback uses the existing off-loop recovery pattern; admission stays closed
    /// until replay and lease acquisition succeed. Errors leave it closed and reported.
    pub(in crate::web::wire) fn reopen(&self) -> tokio::task::JoinHandle<Result<(), ErrorCode>> {
        let phase = self.close();
        let owner = self.clone();
        tokio::spawn(async move {
            let result = owner.acquire_at(phase).await;
            if let Err(error) = &result {
                tracing::warn!(
                    error = error.as_str(),
                    "wire command journal reacquire failed"
                );
            }
            owner.0.wake.notify_one();
            result
        })
    }
    pub(in crate::web::wire) async fn shutdown(&self) -> Result<(), ErrorCode> {
        self.stop();
        self.freeze().await
    }
}
#[cfg(test)]
mod tests;
