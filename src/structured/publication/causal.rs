//! Producer-owned ACK fence. No event payloads or per-connection buffers live here.
// Wire admission retains the receipt through durable Finish and ACK flush.
// The receipt endpoints also build without the optional web consumer.
#![allow(dead_code)]
use parking_lot::{Condvar, Mutex};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
};

#[derive(Debug)]
struct State {
    waiters: Mutex<usize>,
    wake: Condvar,
    ready: tokio::sync::Notify,
}
/// Shared by the command's producers. The last ACK waiter opens it permanently.
#[derive(Clone, Debug)]
pub(crate) struct Cause(Arc<State>);
/// Linear ACK ownership: move into the socket writer; dropping also handles loss.
#[derive(Debug)]
pub(crate) struct Ack(Arc<Waiter>);
#[derive(Debug)]
struct Waiter(Mutex<Option<Cause>>);
/// Socket-owned cancellation set; weak registrations never retain completed ACKs.
#[derive(Default)]
pub(crate) struct Origin(Vec<Weak<Waiter>>);
impl Origin {
    pub(crate) fn bind(&mut self, ack: Ack) -> io::Result<Ack> {
        self.0
            .retain(|weak| weak.upgrade().is_some_and(|w| w.0.lock().is_some()));
        if self.0.len() == 32 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.0.push(Arc::downgrade(&ack.0));
        Ok(ack)
    }
}
impl Drop for Origin {
    fn drop(&mut self) {
        for waiter in self.0.iter().filter_map(Weak::upgrade) {
            waiter.release();
        }
    }
}
impl Waiter {
    fn release(&self) {
        let Some(cause) = self.0.lock().take() else {
            return;
        };
        let mut count = cause.0.waiters.lock();
        *count -= 1;
        if *count == 0 {
            cause.0.wake.notify_all();
            cause.0.ready.notify_waiters();
        }
    }
}
impl Ack {
    fn new(cause: Cause) -> Self {
        Self(Arc::new(Waiter(Mutex::new(Some(cause)))))
    }
}
impl Cause {
    /// Create before dispatch; retain the ACK through durable outcome and flush.
    pub(crate) fn hold() -> (Self, Ack) {
        let cause = Self(Arc::new(State {
            waiters: Mutex::new(1),
            wake: Condvar::new(),
            ready: tokio::sync::Notify::new(),
        }));
        (cause.clone(), Ack::new(cause))
    }
    /// Join only while held, before handing a duplicate's ACK to its writer.
    /// Bound matches the command lane. A released receipt cannot be closed again.
    pub(crate) fn join(&self) -> io::Result<Ack> {
        let mut count = self.0.waiters.lock();
        if *count == 0 {
            return Err(io::ErrorKind::NotConnected.into());
        }
        if *count == 32 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        *count += 1;
        Ok(Ack::new(self.clone()))
    }
    /// Async producer wait, with no lost wake between checking and parking.
    pub(crate) async fn released(&self) {
        loop {
            let notified = self.0.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if *self.0.waiters.lock() == 0 {
                return;
            }
            notified.await;
        }
    }
    /// Keep the producer/file lease through release, even during cancellation.
    /// Otherwise a replacement producer could replay the private durable tail.
    pub(crate) fn wait(&self, cancel: &AtomicBool) -> io::Result<()> {
        let mut count = self.0.waiters.lock();
        while *count != 0 {
            self.0.wake.wait(&mut count);
        }
        if cancel.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        Ok(())
    }
}
impl Drop for Ack {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn joins_are_bounded_release_is_final_and_ownership_does_not_leak() {
        let (cause, lease) = Cause::hold();
        let mut joined = Vec::new();
        for _ in 0..31 {
            joined.push(cause.join().unwrap());
        }
        assert!(matches!(cause.join(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
        drop(lease);
        drop(joined);
        tokio::time::timeout(std::time::Duration::from_secs(1), cause.released())
            .await
            .unwrap();
        cause.wait(&AtomicBool::new(false)).unwrap();
        assert_eq!(
            cause.wait(&AtomicBool::new(true)).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(matches!(cause.join(), Err(e) if e.kind() == io::ErrorKind::NotConnected));
        assert_eq!(Arc::strong_count(&cause.0), 1);
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[test]
    fn cancellation_retains_the_producer_until_ack_loss_then_releases_without_leak() {
        let (cause, lease) = Cause::hold();
        let producer = cause.clone();
        let (entered, ready) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            entered.send(()).unwrap();
            finished
                .send(producer.wait(&AtomicBool::new(true)))
                .unwrap();
        });
        ready
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            done.recv_timeout(std::time::Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(lease);
        assert_eq!(
            done.recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        task.join().unwrap();
        assert_eq!(Arc::strong_count(&cause.0), 1);
    }
}

#[cfg(test)]
mod origin_tests {
    use super::*;
    #[tokio::test]
    async fn origin_capacity_is_bounded_and_teardown_releases_unsent_tokens_once() {
        let mut origin = Origin::default();
        let mut tokens = Vec::new();
        let mut causes = Vec::new();
        for _ in 0..32 {
            let (cause, token) = Cause::hold();
            tokens.push(origin.bind(token).unwrap());
            causes.push(cause);
        }
        let (overflow, token) = Cause::hold();
        assert!(matches!(origin.bind(token), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
        overflow.released().await;
        drop(origin);
        for cause in &causes {
            cause.released().await;
        }
        drop(tokens);
        for cause in causes {
            assert_eq!(Arc::strong_count(&cause.0), 1);
        }
    }
    #[test]
    fn origin_reclaims_completed_registrations() {
        let mut origin = Origin::default();
        for _ in 0..64 {
            let (_, token) = Cause::hold();
            drop(origin.bind(token).unwrap());
        }
        assert_eq!(origin.0.len(), 1);
    }
}

pub(crate) mod projection;
