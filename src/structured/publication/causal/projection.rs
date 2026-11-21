//! One ordering boundary for caused publication and ordinary snapshot capture.
use super::Cause;
use parking_lot::Mutex;
use std::{collections::VecDeque, io, sync::Arc};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
struct Pending<T> {
    caused: VecDeque<(T, Cause, OwnedSemaphorePermit)>,
    ordinary: Option<T>,
    last: Option<T>,
}
/// Shared producer ingress. Ordinary updates coalesce; caused updates never do.
pub(crate) struct Projection<T> {
    book: Mutex<Pending<T>>,
    slots: Arc<Semaphore>,
    ready: Notify,
}
/// Sole drain owner: all snapshot requests use the same capture operation.
pub(crate) struct Reader<T>(Arc<Projection<T>>);
/// Capacity reserved before effects; submission atomically fences later snapshots.
pub(crate) struct Reservation<T> {
    owner: Arc<Projection<T>>,
    cause: Cause,
    permit: OwnedSemaphorePermit,
}
impl<T: Clone + PartialEq> Projection<T> {
    pub(crate) fn channel() -> (Arc<Self>, Reader<T>) {
        let source = Arc::new(Self {
            book: Mutex::new(Pending {
                caused: VecDeque::new(),
                ordinary: None,
                last: None,
            }),
            slots: Arc::new(Semaphore::new(32)),
            ready: Notify::new(),
        });
        (source.clone(), Reader(source))
    }
    pub(crate) fn observe(&self, value: T) {
        let mut book = self.book.lock();
        if book.last.as_ref() == Some(&value) {
            return;
        }
        book.last = Some(value.clone());
        book.ordinary = Some(value);
        drop(book);
        self.ready.notify_one();
    }
    pub(crate) fn reserve(self: &Arc<Self>, cause: Cause) -> io::Result<Reservation<T>> {
        Ok(Reservation {
            owner: self.clone(),
            cause,
            permit: self
                .slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| io::ErrorKind::WouldBlock)?,
        })
    }
}
impl<T: Clone> Reservation<T> {
    pub(crate) fn send(self, value: T) {
        let mut book = self.owner.book.lock();
        book.last = Some(value.clone());
        book.ordinary = None;
        book.caused.push_back((value, self.cause, self.permit));
        drop(book);
        self.owner.ready.notify_one();
    }
}
impl<T> Reader<T> {
    pub(crate) async fn changed(&self) {
        self.0.ready.notified().await;
    }
    /// Queue choice and snapshot capture linearize under this one lock.
    /// The owner must finish publication/release before taking the next item.
    pub(crate) fn take(&mut self) -> Option<(T, Option<Cause>)> {
        let mut book = self.0.book.lock();
        let item = match book.caused.pop_front() {
            Some((value, cause, _permit)) => Some((value, Some(cause))),
            None => book.ordinary.take().map(|value| (value, None)),
        };
        if !book.caused.is_empty() || book.ordinary.is_some() {
            self.0.ready.notify_one();
        }
        item
    }
}

#[cfg(test)]
mod tests;
