//! Bounded registry-owner lookup and retained recovery (ADR-0036 §5a).
use super::History;
use crate::structured::{registry::Registry, workers::Workers};
use std::{io, path::Path, sync::Arc};
use tokio::sync::{mpsc, oneshot, Notify, Semaphore};

/// Mailbox capacity and maximum requests serviced per owner turn.
const REQUESTS: usize = 32;
/// Blocking recovery jobs, including work whose async waiter was cancelled.
const RECOVERIES: usize = 4;

/// Internal consumer handle. Lookup never reads Runtime or the state reducer.
#[derive(Clone)]
pub(crate) struct Client {
    tx: mpsc::Sender<Request>,
    wake: Arc<Notify>,
    io: Budget,
}
/// Owner half, serviced against the owner's current registry and worker set.
pub(crate) struct Owner(mpsc::Receiver<Request>);
/// Off-executor work retains its capacity through completion, even on cancellation.
#[derive(Clone)]
struct Budget(Arc<Semaphore>);
impl Budget {
    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<T> {
        let permit = self
            .0
            .clone()
            .try_acquire_owned()
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))?;
        tokio::task::spawn_blocking(move || {
            let result = work();
            drop(permit);
            result
        })
        .await
        .map_err(io::Error::other)
    }
}
enum Request {
    Lookup(String, oneshot::Sender<Option<Located>>),
    Install(String, u64, History, oneshot::Sender<io::Result<History>>),
}
enum Located {
    Ready(History),
    Busy,
    Retained(std::path::PathBuf, u64),
}
/// Construct the private transport/owner bridge with its existing app wake.
pub(crate) fn channel(wake: Arc<Notify>) -> (Client, Owner) {
    let (tx, rx) = mpsc::channel(REQUESTS);
    (
        Client {
            tx,
            wake,
            io: Budget(Arc::new(Semaphore::new(RECOVERIES))),
        },
        Owner(rx),
    )
}
impl Client {
    async fn request<T>(&self, request: Request, response: oneshot::Receiver<T>) -> io::Result<T> {
        self.tx.try_send(request).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => io::ErrorKind::WouldBlock,
            mpsc::error::TrySendError::Closed(_) => io::ErrorKind::BrokenPipe,
        })?;
        self.wake.notify_one();
        response
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    }
    /// None means unknown subject or an unsafe/reserved stored namespace. Neither
    /// is exposed as a readable stream. Overload is WouldBlock, owner loss BrokenPipe,
    /// stale recovery Interrupted; storage failures retain their original I/O kind.
    /// Recovery performs no filesystem work on the application owner or heartbeat executor.
    pub(crate) async fn lookup(&self, id: String) -> io::Result<Option<History>> {
        let (tx, rx) = oneshot::channel();
        match self.request(Request::Lookup(id.clone(), tx), rx).await? {
            None => Ok(None),
            Some(Located::Ready(source)) => Ok(Some(source)),
            Some(Located::Busy) => Err(io::ErrorKind::WouldBlock.into()),
            Some(Located::Retained(path, generation)) => {
                let source = self.io.run(move || History::open_retained(&path)).await??;
                let (tx, rx) = oneshot::channel();
                self.request(Request::Install(id, generation, source, tx), rx)
                    .await?
                    .map(Some)
            }
        }
    }
}
impl Owner {
    /// Service a bounded turn. Generation and namespace checks precede canonical
    /// source selection; a concurrent recovery cannot replace the winner.
    pub(crate) fn service(&mut self, registry: &Registry, workers: &mut Workers, root: &Path) {
        for _ in 0..REQUESTS {
            let Ok(request) = self.0.try_recv() else {
                break;
            };
            match request {
                Request::Lookup(id, reply) => {
                    if reply.is_closed() {
                        continue;
                    }
                    let found = registry
                        .get(&id)
                        .filter(|(record, _, _)| {
                            crate::structured::transport::valid_component(&record.agent_id)
                                && crate::structured::transport::valid_component(&record.session_id)
                                && !record.session_id.eq_ignore_ascii_case("queue")
                        })
                        .map(|(record, _, generation)| {
                            workers.history(registry, &id).cloned().map_or_else(
                                || {
                                    if !workers.history_recovery_allowed(&id) {
                                        return Located::Busy;
                                    }
                                    Located::Retained(
                                        root.join("sessions")
                                            .join(&record.agent_id)
                                            .join(format!("{}.jsonl", record.session_id)),
                                        generation,
                                    )
                                },
                                Located::Ready,
                            )
                        });
                    let _ = reply.send(found);
                }
                Request::Install(id, generation, source, reply) => {
                    if reply.is_closed() {
                        continue;
                    }
                    let result = (|| {
                        let (record, _, current) =
                            registry.get(&id).ok_or(io::ErrorKind::Interrupted)?;
                        if current != generation
                            || !source.belongs_to(&record.agent_id, &record.session_id)
                        {
                            return Err(io::ErrorKind::Interrupted.into());
                        }
                        if let Some(canonical) = workers.history(registry, &id) {
                            return Ok(canonical.clone());
                        }
                        if !workers.history_recovery_allowed(&id) {
                            return Err(io::ErrorKind::WouldBlock.into());
                        }
                        workers
                            .install_retained(registry, &id, generation, source.clone())
                            .map_err(|error| {
                                io::Error::new(
                                    if error == crate::api::error_code::ErrorCode::SessionBusy {
                                        io::ErrorKind::WouldBlock
                                    } else {
                                        io::ErrorKind::Interrupted
                                    },
                                    error.as_str(),
                                )
                            })?;
                        Ok(source)
                    })();
                    let _ = reply.send(result);
                }
            }
        }
    }
}
#[cfg(test)]
mod tests;
