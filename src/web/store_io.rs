//! Bounded off-executor store work; cancellation does not release a running slot.
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

/// Four active or queued blocking jobs, including abandoned async waiters.
pub(crate) struct StoreIo(Arc<Semaphore>);
impl StoreIo {
    /// Return None on overload or worker failure; no unbounded waiting queue.
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let permit = self.0.clone().try_acquire_owned().ok()?;
        tokio::task::spawn_blocking(move || {
            let result = operation();
            drop(permit);
            result
        })
        .await
        .ok()
    }
}
/// Share the fixed budget across all token verification routes.
pub(crate) fn shared() -> &'static StoreIo {
    static POOL: OnceLock<StoreIo> = OnceLock::new();
    POOL.get_or_init(|| StoreIo(Arc::new(Semaphore::new(4))))
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
