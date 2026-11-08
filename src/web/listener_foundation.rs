//! Reusable accept recovery policy, without binding or mounting any listener.
use crate::{
    api::client_listener::{Health, Status},
    platform::client_listener::{accept_failure, AcceptFailure},
    web::wire::timing::Clock,
};
use std::{future::Future, io, time::Duration};
/// Resource or unclassified errors pause accepts for 100 ms, preventing a busy loop while descriptors recover.
pub(crate) const ACCEPT_RESOURCE_BACKOFF: Duration = Duration::from_millis(100);
/// Accept the next connection, recovering peer errors and backing off resource failures.
/// Invalid listener descriptors publish failure and return once, leaving the server owner alive.
pub(crate) async fn accept_next<T, A, F>(
    accept: &mut A,
    clock: &dyn Clock,
    health: &Health,
) -> io::Result<T>
where
    A: FnMut() -> F,
    F: Future<Output = io::Result<T>>,
{
    loop {
        match accept().await {
            Ok(connection) => return Ok(connection),
            Err(error) => {
                let class = accept_failure(&error);
                crate::logging::client_accept_failed(&error, class);
                match class {
                    AcceptFailure::Connection => {}
                    AcceptFailure::Resource | AcceptFailure::Unknown => {
                        clock.until(clock.now() + ACCEPT_RESOURCE_BACKOFF).await
                    }
                    AcceptFailure::Fatal => {
                        health.publish(Status::Failed);
                        return Err(error);
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod tests;
