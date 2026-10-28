//! Read-only live partial fan-out; durable replay remains in History (ADR-0036 §8).
use crate::structured::{claude::SessionEvent, transport::Received, transport::StreamRecord};
use std::sync::{
    atomic::{AtomicU64, AtomicU8, Ordering},
    Arc,
};
use tokio::sync::broadcast;
/// Maximum retained partials; each payload also has the transport's line bound.
pub const CAPACITY: usize = 16;

const STATE_OPEN: u8 = 0;
const STATE_PRODUCER_ENDED: u8 = 1;
const STATE_RETIRED: u8 = 2;
/// Why this generation's feed terminated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// Producer completed or its owner disappeared.
    ProducerEnded,
    /// Registry generation changed or was removed.
    Retired,
}
/// Cursor-free live observation. Never acknowledge a Delta as durable progress.
#[derive(Debug)]
pub enum Item {
    /// Original classification and receive envelope, without decision authority.
    Delta {
        /// Preceding durable cursor, not a new cursor.
        record: Arc<StreamRecord>,
        /// Classification at receive time.
        event: Arc<SessionEvent>,
    },
    /// Partial blocks were lost: discard them without advancing the durable cursor.
    Gap {
        /// Latest durable sequence observed by this generation's receiver.
        /// Starts at zero until its first persisted record, so partials that
        /// overflow earlier can produce `Gap { through: 0 }`. This is not
        /// durable progress, including when history predates this generation.
        through: u64,
    },
    /// All queued partials were drained; no further data will arrive.
    End(End),
}
/// Single worker-owned publisher, never cloned into consumer state.
pub(super) struct Source {
    tx: Option<broadcast::Sender<(Arc<StreamRecord>, Arc<SessionEvent>)>>,
    through: Arc<AtomicU64>,
    ended: Arc<AtomicU8>,
}
/// A bounded read-only subscription; callers supply their own finite wait deadline.
pub struct Subscription {
    rx: broadcast::Receiver<(Arc<StreamRecord>, Arc<SessionEvent>)>,
    through: Arc<AtomicU64>,
    ended: Arc<AtomicU8>,
}
impl Default for Source {
    fn default() -> Self {
        Self {
            tx: Some(broadcast::channel(CAPACITY).0),
            through: Arc::default(),
            ended: Arc::new(AtomicU8::new(STATE_OPEN)),
        }
    }
}
impl Source {
    pub(super) fn subscribe(&self) -> Subscription {
        let rx = self
            .tx
            .as_ref()
            .map(|tx| tx.subscribe())
            .unwrap_or_else(|| broadcast::channel(CAPACITY).1);
        Subscription {
            rx,
            through: self.through.clone(),
            ended: self.ended.clone(),
        }
    }
    pub(super) fn observe(&self, received: &Received) {
        if received.persisted {
            self.through.store(received.record.seq, Ordering::Release);
        } else if let Some(tx) = &self.tx {
            if tx.receiver_count() > 0 {
                let _ = tx.send((
                    Arc::new(received.record.clone()),
                    Arc::new(received.event.clone()),
                ));
            }
        }
    }
    pub(super) fn finish(&mut self, reason: End) {
        if self.tx.is_some() {
            self.ended.store(
                match reason {
                    End::ProducerEnded => STATE_PRODUCER_ENDED,
                    End::Retired => STATE_RETIRED,
                },
                Ordering::Release,
            );
            self.tx = None;
        }
    }
}
impl Subscription {
    /// Wait for an observation. A closed producer always returns a typed End.
    pub async fn recv(&mut self) -> Item {
        match self.rx.recv().await {
            Ok((record, event)) => Item::Delta { record, event },
            Err(broadcast::error::RecvError::Lagged(_)) => Item::Gap {
                through: self.through.load(Ordering::Acquire),
            },
            Err(broadcast::error::RecvError::Closed) => {
                Item::End(if self.ended.load(Ordering::Acquire) == STATE_RETIRED {
                    End::Retired
                } else {
                    End::ProducerEnded
                })
            }
        }
    }
}
