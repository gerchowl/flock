//! A task-scoped deadline source for harnesses that also exercise real I/O.
#![cfg(test)]
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
type Wait = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
tokio::task_local! { static CLOCK: Wait; }

pub(crate) async fn scope<T>(wait: Wait, work: impl Future<Output = T>) -> T {
    CLOCK.scope(wait, work).await
}

pub(crate) async fn timeout<T>(duration: Duration, work: impl Future<Output = T>) -> Result<T, ()> {
    match CLOCK.try_with(Arc::clone) {
        Ok(wait) => tokio::select! {
            biased;
            result = work => Ok(result),
            _ = wait(duration) => Err(()),
        },
        Err(_) => tokio::time::timeout(duration, work).await.map_err(|_| ()),
    }
}
