//! Cold queue source assembly on an owned I/O worker (ADR-0036 §4/§5).
//! Installation is a separate registry-owner step, fenced again after I/O.
use super::{publication::Source, Failure, RewritePolicy};
use crate::{
    api::error_code::ErrorCode,
    structured::{registry::Registry, workers::Workers},
};
use std::{
    path::PathBuf,
    sync::{mpsc, Arc},
    thread::JoinHandle,
};
use tokio::sync::Notify;

/// Complete immutable source whose namespace and request epoch remain bound.
pub(crate) struct Recovered {
    id: String,
    generation: u64,
    source: Source,
}
impl Recovered {
    /// No filesystem work. A retired epoch or competing owner refuses install.
    pub fn install(self, workers: &mut Workers, registry: &Registry) -> Result<(), ErrorCode> {
        workers.install_queue_source(registry, &self.id, self.generation, self.source)
    }
}
/// One nonblocking I/O completion step. No mutation is retried by this adapter.
pub(crate) enum Step {
    /// The owned I/O worker is still loading; await its readiness hint.
    Pending,
    /// I/O and the journal lease have completed; revalidate before installation.
    Ready(Recovered),
    /// Opening/worker failure; inspect before explicitly requesting recovery again.
    Refused(Failure),
    /// This completion was already consumed.
    Closed,
}
/// One request, one bounded result slot. Consumers admit recovery requests under
/// their subscription limits (#1830). Retiring an epoch does not interrupt fsync;
/// its completion is rejected at install. No filesystem work runs in poll/drop.
pub(crate) struct Recovery {
    id: String,
    generation: u64,
    receive: mpsc::Receiver<Result<Source, Failure>>,
    notify: Arc<Notify>,
    done: bool,
    _worker: JoinHandle<()>,
}
impl Recovery {
    /// Start only I/O here; success accepts the request, not a completed source.
    pub fn start(
        data: PathBuf,
        id: String,
        generation: u64,
        policy: RewritePolicy,
    ) -> Result<Self, Failure> {
        let agent = id.clone();
        Self::spawn(id, generation, move || {
            Source::open_retained(&data, &agent, policy)
        })
    }
    fn spawn(
        id: String,
        generation: u64,
        load: impl FnOnce() -> Result<Source, Failure> + Send + 'static,
    ) -> Result<Self, Failure> {
        let (send, receive) = mpsc::sync_channel(1);
        let notify = Arc::new(Notify::new());
        let wake = notify.clone();
        let worker = std::thread::Builder::new()
            .name("queue-recovery".into())
            .spawn(move || {
                // The loader owns its private journal and publishes only on return.
                // Unwind drops its lease; no partial source is installed.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(load))
                    .unwrap_or_else(|_| {
                        Err(Failure {
                            code: ErrorCode::StructuredWorkerPanicked,
                            source: None,
                        })
                    });
                let _ = send.send(result);
                wake.notify_waiters();
            })
            .map_err(|source| Failure {
                code: ErrorCode::InternalError,
                source: Some(source),
            })?;
        Ok(Self {
            id,
            generation,
            receive,
            notify,
            done: false,
            _worker: worker,
        })
    }
    /// Enable this future before poll, then recheck after every wake.
    pub fn waker(&self) -> Arc<Notify> {
        self.notify.clone()
    }
    /// Readiness means all I/O and lease ownership ended. The thread may still
    /// be returning from notification; this method never joins it on the server.
    pub fn poll(&mut self) -> Step {
        if self.done {
            return Step::Closed;
        }
        let result = match self.receive.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return Step::Pending,
            Err(mpsc::TryRecvError::Disconnected) => Err(Failure {
                code: ErrorCode::StructuredWorkerPanicked,
                source: None,
            }),
        };
        self.done = true;
        match result {
            Ok(source) => Step::Ready(Recovered {
                id: self.id.clone(),
                generation: self.generation,
                source,
            }),
            Err(error) => Step::Refused(error),
        }
    }
}
