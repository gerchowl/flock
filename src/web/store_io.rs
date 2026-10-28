//! Bounded off-executor store work; cancellation does not release a running slot.
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

/// Bounded active or queued blocking jobs, including abandoned async waiters.
pub(crate) struct StoreIo(Arc<Semaphore>);
impl StoreIo {
    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        self.0.available_permits()
    }

    /// Return None on overload or worker failure; no unbounded waiting queue.
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let permit = self
            .0
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                crate::logging::client_wire_unavailable("store_capacity", None);
            })
            .ok()?;
        tokio::task::spawn_blocking(move || {
            let result = operation();
            drop(permit);
            result
        })
        .await
        .map_err(|_| crate::logging::client_wire_unavailable("store_worker", None))
        .ok()
    }
}
/// Share four I/O slots across all token verification routes.
pub(crate) fn shared() -> &'static StoreIo {
    static POOL: OnceLock<StoreIo> = OnceLock::new();
    POOL.get_or_init(|| StoreIo(Arc::new(Semaphore::new(4))))
}

/// At most eight kernel-lock waits, matching both listeners' four pending hellos.
/// Separate from store I/O: waiting reconnects cannot evict live subscriptions.
/// Cancellation retains a slot until its deadline-bounded helper has been reaped.
pub(crate) fn hello_waits() -> &'static StoreIo {
    static POOL: OnceLock<StoreIo> = OnceLock::new();
    POOL.get_or_init(|| StoreIo(Arc::new(Semaphore::new(8))))
}

/// Bound retirement lock waits independently of live subscription I/O.
/// Shared capacity is the socket cap plus one, not a reserved shutdown slot.
/// Abandoned waits retain slots until their helpers are reaped, so replacement
/// sockets can exhaust the pool; shutdown then fails closed without waiting.
pub(crate) fn retirement_waits() -> &'static StoreIo {
    static POOL: OnceLock<StoreIo> = OnceLock::new();
    POOL.get_or_init(|| {
        StoreIo(Arc::new(Semaphore::new(
            super::wire::CONNECTIONS as usize + 1,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn blocked_store_work_cannot_freeze_the_async_executor() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let pool = Arc::new(StoreIo(Arc::new(Semaphore::new(1))));
                let (ready, started) = tokio::sync::oneshot::channel();
                let (release, wait) = std::sync::mpsc::channel();
                let task = tokio::spawn({
                    let pool = pool.clone();
                    async move {
                        pool.run(move || {
                            ready.send(()).unwrap();
                            wait.recv_timeout(Duration::from_secs(5))
                        })
                        .await
                    }
                });
                tokio::time::timeout(Duration::from_secs(5), started)
                    .await
                    .unwrap()
                    .unwrap();
                let tick = tokio::spawn(async {
                    tokio::task::yield_now().await;
                    7
                });
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(5), tick)
                        .await
                        .unwrap()
                        .unwrap(),
                    7
                );
                release.send(()).unwrap();
                assert!(tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .is_ok());
            });
    }
    #[test]
    fn cancellation_keeps_slots_owned_until_the_blocking_producer_returns() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let pool = Arc::new(StoreIo(Arc::new(Semaphore::new(2))));
                let mut releases = Vec::new();
                for _ in 0..2 {
                    let (ready, started) = tokio::sync::oneshot::channel();
                    let (release, wait) = std::sync::mpsc::channel();
                    releases.push(release);
                    let task = tokio::spawn({
                        let pool = pool.clone();
                        async move {
                            pool.run(move || {
                                ready.send(()).unwrap();
                                wait.recv_timeout(Duration::from_secs(5)).unwrap()
                            })
                            .await
                        }
                    });
                    tokio::time::timeout(Duration::from_secs(5), started)
                        .await
                        .unwrap()
                        .unwrap();
                    task.abort();
                }
                assert!(pool.run(|| 99).await.is_none());
                for release in releases {
                    release.send(()).unwrap();
                }
                let permits = tokio::time::timeout(
                    Duration::from_secs(5),
                    pool.0.clone().acquire_many_owned(2),
                )
                .await
                .unwrap()
                .unwrap();
                drop(permits);
                assert_eq!(pool.run(|| 11).await, Some(11));
            });
    }
}
