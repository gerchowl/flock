//! Generation-scoped queue replay/publication (ADR-0036 §4/§5).
//! The existing I/O owner bootstraps snapshots and captures/registers under one
//! lock. Paging reads no files: immutable decoded cuts survive rename and exit.
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

/// One bounded consumer step. Every replay has exactly one captured barrier.
#[derive(Debug)]
pub(crate) enum Page {
    /// Replacement boundary, including empty snapshots and their high-water.
    Gap(crate::structured::queue::Gap),
    /// Original committed queue record.
    Record(serde_json::Value),
    /// Replay head; subsequent records belong to the live continuation.
    Synced(u64),
    /// Reservation/read-only status and operation refusal.
    Status(Option<Pending>, Option<ErrorCode>),
    /// Terminal refusal: no successful snapshot/barrier was served.
    Refused(ErrorCode),
    /// Bounded progress; page again without waiting for another wake.
    Progress,
    /// Await readiness; this never retries a mutation.
    Pending,
    /// Closed overflow; resume from the last applied cursor.
    Lagged(u64),
    /// Generation ended after draining committed publications.
    End,
}
// Cut, status, operation error, replay flag, and requested cursor respectively.
struct Packet(Arc<Cut>, Option<Pending>, Option<ErrorCode>, bool, u64);
#[derive(Default)]
struct Live {
    packets: VecDeque<Packet>,
    bytes: usize,
    lagged: Option<u64>,
    refusal: Option<ErrorCode>,
}
struct Book {
    retained: Arc<Cut>,
    bootstrapped: bool,
    status: Option<Pending>,
    error: Option<ErrorCode>,
    ended: bool,
    refusal: Option<ErrorCode>,
    readers: Vec<Weak<Mutex<Live>>>,
}
impl Book {
    fn register(&mut self, after: Option<u64>, live: Arc<Mutex<Live>>) -> Result<(), ErrorCode> {
        if let Some(code) = self.refusal {
            live.lock().refusal = Some(code);
            return Ok(());
        }
        if after.is_some_and(|n| n > self.retained.through) {
            return Err(ErrorCode::CursorAhead);
        }
        live.lock().packets.push_back(Packet(
            self.retained.clone(),
            self.status.clone(),
            self.error,
            true,
            after.unwrap_or(0),
        ));
        self.readers.retain(|r| r.strong_count() != 0);
        self.readers.push(Arc::downgrade(&live));
        Ok(())
    }
    fn publish(&mut self, mut cut: Cut, status: Option<Pending>, error: Option<ErrorCode>) {
        let replacement = cut.gap.is_some() && cut.gap != self.retained.gap;
        if !replacement {
            cut.gap = None;
            let start = cut.records.partition_point(|v| {
                v["seq"]
                    .as_u64()
                    .is_some_and(|n| n <= self.retained.through)
            });
            cut.records.drain(..start);
        }
        let bytes = cut
            .records
            .iter()
            .map(|v| v.to_string().len())
            .sum::<usize>();
        let cut = Arc::new(cut);
        self.readers.retain(|reader| {
            let Some(reader) = reader.upgrade() else {
                return false;
            };
            let mut reader = reader.lock();
            if reader.lagged.is_some()
                || reader.packets.len() >= LIVE_CUTS
                || bytes > LIVE_BYTES.saturating_sub(reader.bytes)
            {
                reader.lagged = Some(cut.through);
                reader.packets.clear();
                reader.bytes = 0;
            } else {
                reader.bytes += bytes;
                reader
                    .packets
                    .push_back(Packet(cut.clone(), status.clone(), error, false, 0));
            }
            true
        });
        let retained = Arc::make_mut(&mut self.retained);
        if cut.gap.is_some() {
            *retained = cut.as_ref().clone();
        } else {
            retained.records.extend(cut.records.iter().cloned());
            retained.through = cut.through;
        }
        self.status = status;
        self.error = error;
    }
}
struct Capture(Option<u64>, Arc<Mutex<Live>>);
/// Retained generation source. Cold opening belongs exclusively on an I/O worker.
#[derive(Clone)]
pub(crate) struct Source(
    mpsc::SyncSender<Capture>,
    Arc<Mutex<Book>>,
    Arc<Notify>,
    String,
);
impl Source {
    /// Blocking recovery for an I/O worker only, never the server loop. The
    /// exclusive journal lease rejects a concurrent writer. This source is ended
    /// at its recovered cut; install must recheck the registry generation.
    pub fn open_retained(data: &Path, agent: &str) -> Result<Self, Failure> {
        let mut journal = Journal::open(data, agent)?;
        let result = journal.subscription();
        let (publisher, source) = Publisher::new(agent, journal.replay(0)?);
        if let Err(error) = result {
            source.1.lock().error = Some(Failure::from(error).code);
        }
        drop(publisher);
        Ok(source)
    }
    pub(in crate::structured) fn belongs_to(&self, agent: &str) -> bool {
        self.3 == agent
    }
}
impl Provider for Source {
    type Error = ErrorCode;
    type Subscription = Reader;
    fn subscribe(&self, after: Option<u64>) -> Result<Reader, ErrorCode> {
        let live = Arc::new(Mutex::new(Live::default()));
        let mut book = self.1.try_lock().ok_or(ErrorCode::SessionBusy)?;
        if after.is_some_and(|n| n > book.retained.through) {
            return Err(ErrorCode::CursorAhead);
        }
        if book.ended {
            book.register(after, live.clone())?;
        } else {
            self.0
                .try_send(Capture(after, live.clone()))
                .map_err(|_| ErrorCode::SessionBusy)?;
        }
        Ok(Reader {
            source: self.clone(),
            live,
            current: None,
            index: 0,
            stage: 0,
            closed: false,
        })
    }
}
/// One immutable replay cut and bounded live continuation. No paging file reads.
pub(crate) struct Reader {
    source: Source,
    live: Arc<Mutex<Live>>,
    current: Option<Packet>,
    index: usize,
    stage: u8,
    closed: bool,
}
impl Subscription for Reader {
    type Page = Page;
    fn waker(&self) -> Arc<Notify> {
        self.source.2.clone()
    }
    fn page(&mut self) -> Page {
        if self.closed {
            return Page::End;
        }
        let Some(book) = self.source.1.try_lock() else {
            return Page::Pending;
        };
        let mut live = self.live.lock();
        if let Some(code) = live.refusal.or(book.refusal) {
            self.closed = true;
            return Page::Refused(code);
        }
        if let Some(head) = live.lagged {
            self.closed = true;
            return Page::Lagged(head);
        }
        if self.current.is_none() {
            self.current = live.packets.pop_front();
            if let Some(packet) = &self.current {
                if !packet.3 {
                    live.bytes = live
                        .bytes
                        .saturating_sub(packet.0.records.iter().map(|v| v.to_string().len()).sum());
                }
            }
            self.index = self.current.as_ref().map_or(0, |p| {
                p.0.records
                    .partition_point(|v| v["seq"].as_u64().is_some_and(|n| n <= p.4))
            });
            self.stage = 0;
        }
        let Some(packet) = &self.current else {
            return if book.ended {
                self.closed = true;
                Page::End
            } else {
                Page::Pending
            };
        };
        if self.stage == 0 {
            self.stage = 1;
            if let Some(gap) = &packet.0.gap {
                if !packet.3 || packet.4 <= gap.to {
                    let mut gap = gap.clone();
                    if packet.3 {
                        gap.from = packet.4.saturating_add(1);
                    }
                    return Page::Gap(gap);
                }
            }
        }
        if let Some(record) = packet.0.records.get(self.index) {
            self.index += 1;
            return Page::Record(record.clone());
        }
        if self.stage == 1 {
            self.stage = 2;
            return Page::Status(packet.1.clone(), packet.2);
        }
        let synced = packet.3.then_some(packet.0.through);
        self.current = None;
        synced.map_or(Page::Progress, Page::Synced)
    }
}
/// Sole writer's publication half; capture and registration share its book lock.
pub(super) struct Publisher(Source, mpsc::Receiver<Capture>);
impl Publisher {
    pub fn new(agent: &str, cut: Cut) -> (Self, Source) {
        let (tx, rx) = mpsc::sync_channel(1);
        let source = Source(
            tx,
            Arc::new(Mutex::new(Book {
                retained: Arc::new(cut),
                bootstrapped: false,
                status: None,
                error: None,
                ended: false,
                refusal: None,
                readers: Vec::new(),
            })),
            Arc::new(Notify::new()),
            agent.into(),
        );
        (Self(source.clone(), rx), source)
    }
    pub fn capture(
        &mut self,
        snapshot: impl FnOnce() -> Result<Cut, Failure>,
        status: Option<Pending>,
    ) {
        let Ok(Capture(after, live)) = self.1.try_recv() else {
            return;
        };
        capture_locked(&self.0 .1, Capture(after, live), snapshot, status);
        self.0 .2.notify_waiters();
    }
    pub fn finish(
        &mut self,
        snapshot: impl FnOnce() -> Result<Cut, Failure>,
        status: Option<Pending>,
    ) {
        let mut book = self.0 .1.lock();
        match snapshot() {
            Ok(cut) => {
                if cut.through != book.retained.through || cut.gap != book.retained.gap {
                    book.publish(cut, status, None);
                }
            }
            Err(error) => book.refusal = Some(error.code),
        }
        book.ended = true;
        drop(book);
        self.0 .2.notify_waiters();
    }
    pub fn publish(&mut self, update: &Update) {
        if let Some(cut) = &update.cut {
            self.0 .1.lock().publish(
                cut.clone(),
                update.pending.clone(),
                update.offer.as_ref().err().map(|e| e.code),
            );
        } else {
            self.0 .1.lock().refusal = Some(
                update
                    .offer
                    .as_ref()
                    .err()
                    .map_or(ErrorCode::InternalError, |e| e.code),
            );
        }
        self.0 .2.notify_waiters();
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        let mut book = self.0 .1.lock();
        if !book.ended {
            book.refusal = Some(ErrorCode::InternalError);
        }
        book.ended = true;
        while let Ok(Capture(after, live)) = self.1.try_recv() {
            if let Err(code) = book.register(after, live.clone()) {
                live.lock().refusal = Some(code);
            }
        }
        drop(book);
        self.0 .2.notify_waiters();
    }
}

// The lock type is generic so the same critical section can be checked with
// an unlock witness. Production uses parking_lot's native mutex unchanged.
fn capture_locked<R: parking_lot::lock_api::RawMutex>(
    owner: &parking_lot::lock_api::Mutex<R, Book>,
    Capture(after, live): Capture,
    snapshot: impl FnOnce() -> Result<Cut, Failure>,
    status: Option<Pending>,
) {
    let mut book = owner.lock();
    if !book.bootstrapped && book.refusal.is_none() {
        match snapshot() {
            Ok(cut) => {
                if cut.through != book.retained.through || cut.gap != book.retained.gap {
                    book.publish(cut, status, None);
                }
                book.bootstrapped = true;
            }
            Err(error) => book.refusal = Some(error.code),
        }
    }
    if let Err(error) = book.register(after, live.clone()) {
        live.lock().refusal = Some(error);
    }
    drop(book);
}

#[cfg(test)]
mod tests;
