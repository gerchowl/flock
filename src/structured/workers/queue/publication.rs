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
    /// Await readiness; this never retries a mutation.
    Pending,
    /// Closed overflow; resume from the last applied cursor.
    Lagged(u64),
    /// Generation ended after draining committed publications.
    End,
}
struct Packet(Arc<Cut>, Option<Pending>, Option<ErrorCode>, bool);
#[derive(Default)]
struct Live {
    packets: VecDeque<Packet>,
    bytes: usize,
    lagged: Option<u64>,
}
struct Book {
    retained: Arc<Cut>,
    status: Option<Pending>,
    error: Option<ErrorCode>,
    ended: bool,
    readers: Vec<Weak<Mutex<Live>>>,
}
impl Book {
    fn register(&mut self, after: Option<u64>, live: Arc<Mutex<Live>>) -> Result<(), ErrorCode> {
        if after.is_some_and(|n| n > self.retained.through) {
            return Err(ErrorCode::InvalidRequest);
        }
        let cursor = after.unwrap_or(0);
        let start = self
            .retained
            .records
            .partition_point(|v| v["seq"].as_u64().is_some_and(|n| n <= cursor));
        let mut cut = Cut {
            records: self.retained.records[start..].to_vec(),
            through: self.retained.through,
            gap: self.retained.gap.clone(),
        };
        cut.gap = cut
            .gap
            .filter(|g| after.is_none() || cursor <= g.to)
            .map(|mut g| {
                g.from = cursor.saturating_add(1);
                g
            });
        live.lock()
            .packets
            .push_back(Packet(Arc::new(cut), self.status.clone(), self.error, true));
        self.readers.retain(|r| r.strong_count() != 0);
        self.readers.push(Arc::downgrade(&live));
        Ok(())
    }
    fn publish(&mut self, mut cut: Cut, status: Option<Pending>, error: Option<ErrorCode>) {
        if cut.through <= self.retained.through {
            cut.records.clear();
            cut.gap = None;
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
                    .push_back(Packet(cut.clone(), status.clone(), error, false));
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
        let mut book = self.1.lock();
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
        let book = self.source.1.lock();
        let mut live = self.live.lock();
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
            self.index = 0;
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
                return Page::Gap(gap.clone());
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
        synced.map_or(Page::Pending, Page::Synced)
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
                status: None,
                error: None,
                ended: false,
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
        let mut book = self.0 .1.lock();
        match snapshot() {
            Ok(cut) => {
                if cut.through != book.retained.through || cut.gap != book.retained.gap {
                    book.publish(cut, status, None);
                }
            }
            Err(error) => {
                book.error = Some(error.code);
            }
        }
        if let Err(error) = book.register(after, live.clone()) {
            live.lock().packets.push_back(Packet(
                Arc::new(Cut {
                    gap: None,
                    records: vec![],
                    through: book.retained.through,
                }),
                None,
                Some(error),
                true,
            ));
        }
        self.0 .2.notify_waiters();
    }
    pub fn publish(&mut self, update: &Update) {
        if let Some(cut) = &update.cut {
            self.0 .1.lock().publish(
                cut.clone(),
                update.pending.clone(),
                update.offer.as_ref().err().map(|e| e.code),
            );
        }
        self.0 .2.notify_waiters();
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        let mut book = self.0 .1.lock();
        book.ended = true;
        if std::thread::panicking() {
            book.error = Some(ErrorCode::InternalError);
        }
        while let Ok(Capture(after, live)) = self.1.try_recv() {
            let _ = book.register(after, live);
        }
        self.0 .2.notify_waiters();
    }
}
