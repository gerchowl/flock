//! Queue provider for the shared stream seam (ADR-0036 §4/§5, #1956/#1829).
//! A subscription belongs to one worker generation. End requires a new source;
//! overflow closes it with lagged, never silently discarding committed changes.
use super::*;
use crate::structured::publication::{Provider, Subscription};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::{Arc, Weak},
};
use tokio::sync::Notify;

const LIVE_CUTS: usize = 4;
const LIVE_BYTES: usize = 256 * 1024;

/// One bounded step; snapshot_to distinguishes queue gaps from history loss.
#[derive(Debug)]
pub(crate) enum Page {
    /// Durable replacement boundary, including zero-line checkpoints.
    Gap(super::super::super::queue::Gap),
    /// Original queue record and independent per-stream sequence.
    Record(serde_json::Value),
    /// Captured replay head, emitted once before live records.
    Synced(u64),
    /// Typed reservation/read-only reason and latest operation refusal.
    Status(Option<Pending>, Option<ErrorCode>),
    /// No data yet; await a hint rather than retrying a mutation.
    Pending,
    /// This subscription closed; recover from the last applied cursor.
    Lagged(u64),
    /// Source generation ended after its remaining buffered publications.
    End,
}
struct Packet {
    cut: Option<Arc<Cut>>,
    pending: Option<Pending>,
    error: Option<ErrorCode>,
    replay: bool,
}
#[derive(Default)]
struct Live {
    first: Option<Packet>,
    queue: VecDeque<(Packet, usize)>,
    bytes: usize,
    lagged: Option<u64>,
    ended: bool,
}
struct Capture {
    after: Option<u64>,
    live: Arc<Mutex<Live>>,
}
/// Generation-scoped provider, obtained through the registry's worker owner.
#[derive(Clone)]
pub(crate) struct Source(mpsc::SyncSender<Capture>, Arc<Notify>);
/// Replay retains one immutable cut; live storage is capped at four cuts/256 KiB.
/// A page emits at most one record, so a large snapshot cannot monopolize output.
pub(crate) struct Reader {
    live: Arc<Mutex<Live>>,
    notify: Arc<Notify>,
    current: Option<Packet>,
    index: usize,
    stage: u8,
    closed: bool,
}
impl Provider for Source {
    type Subscription = Reader;
    fn subscribe(&self, after: Option<u64>) -> Result<Reader, ErrorCode> {
        let live = Arc::new(Mutex::new(Live::default()));
        self.0
            .try_send(Capture {
                after,
                live: live.clone(),
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => ErrorCode::SessionBusy,
                mpsc::TrySendError::Disconnected(_) => ErrorCode::AgentNotFound,
            })?;
        Ok(Reader {
            live,
            notify: self.1.clone(),
            current: None,
            index: 0,
            stage: 0,
            closed: false,
        })
    }
}
impl Subscription for Reader {
    type Page = Page;
    fn waker(&self) -> Arc<Notify> {
        self.notify.clone()
    }
    fn page(&mut self) -> Page {
        if self.closed {
            return Page::End;
        }
        if self.current.is_none() {
            let mut live = self.live.lock();
            self.current = live.first.take().or_else(|| {
                live.queue.pop_front().map(|(p, n)| {
                    live.bytes -= n;
                    p
                })
            });
            if self.current.is_none() {
                if let Some(head) = live.lagged {
                    self.closed = true;
                    return Page::Lagged(head);
                }
                return if live.ended { Page::End } else { Page::Pending };
            }
            self.stage = 0;
            self.index = 0;
        }
        let packet = self
            .current
            .as_ref()
            .expect("current packet installed above");
        if self.stage == 0 {
            self.stage = 1;
            if let Some(gap) = packet.cut.as_ref().and_then(|c| c.gap.as_ref()) {
                return Page::Gap(super::super::super::queue::Gap {
                    from: gap.from,
                    to: gap.to,
                    snapshot_to: gap.snapshot_to,
                });
            }
        }
        if let Some(record) = packet.cut.as_ref().and_then(|c| c.records.get(self.index)) {
            self.index += 1;
            return Page::Record(record.clone());
        }
        if self.stage == 1 {
            self.stage = 2;
            return Page::Status(packet.pending.clone(), packet.error);
        }
        let packet = self
            .current
            .take()
            .expect("packet remains until its barrier");
        if packet.replay {
            if let Some(cut) = packet.cut {
                return Page::Synced(cut.through);
            }
        }
        Page::Pending
    }
}
/// Only the journal owner can capture replay or append live publication packets.
pub(super) struct Publisher {
    captures: mpsc::Receiver<Capture>,
    live: Vec<Weak<Mutex<Live>>>,
    notify: Arc<Notify>,
}
impl Publisher {
    pub fn new() -> (Self, Source) {
        let (tx, captures) = mpsc::sync_channel(1);
        let notify = Arc::new(Notify::new());
        (
            Self {
                captures,
                live: Vec::new(),
                notify: notify.clone(),
            },
            Source(tx, notify),
        )
    }
    pub fn capture(&mut self, scheduler: &mut Scheduler, pending: Option<Pending>) {
        let Ok(request) = self.captures.try_recv() else {
            return;
        };
        let result = scheduler.replay_for_subscription(request.after);
        let (cut, error) = match result {
            Ok(cut) => (Some(Arc::new(cut)), None),
            Err(e) => (None, Some(e.code)),
        };
        request.live.lock().first = Some(Packet {
            cut,
            pending,
            error,
            replay: true,
        });
        self.live.push(Arc::downgrade(&request.live));
        self.notify.notify_waiters();
    }
    pub fn publish(&mut self, update: &Update) {
        let cut = update.cut.as_ref().map(|c| {
            Arc::new(Cut {
                gap: c.gap.as_ref().map(|g| super::super::super::queue::Gap {
                    from: g.from,
                    to: g.to,
                    snapshot_to: g.snapshot_to,
                }),
                records: c.records.clone(),
                through: c.through,
            })
        });
        let bytes = cut
            .as_ref()
            .map_or(0, |c| c.records.iter().map(|v| v.to_string().len()).sum());
        self.live.retain(|weak| {
            let Some(live) = weak.upgrade() else {
                return false;
            };
            let mut live = live.lock();
            if live.lagged.is_some()
                || live.queue.len() >= LIVE_CUTS
                || bytes > LIVE_BYTES.saturating_sub(live.bytes)
            {
                live.lagged = Some(cut.as_ref().map_or(0, |c| c.through));
                live.queue.clear();
                live.bytes = 0;
            } else {
                live.bytes += bytes;
                live.queue.push_back((
                    Packet {
                        cut: cut.clone(),
                        pending: update.pending.clone(),
                        error: update.offer.as_ref().err().map(|e| e.code),
                        replay: false,
                    },
                    bytes,
                ));
            }
            true
        });
        self.notify.notify_waiters();
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        for live in self.live.iter().filter_map(Weak::upgrade) {
            live.lock().ended = true;
        }
        while let Ok(request) = self.captures.try_recv() {
            request.live.lock().ended = true;
        }
        self.notify.notify_waiters();
    }
}
