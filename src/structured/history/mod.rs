//! Owned session replay cuts and bounded publication (ADR-0036 §5/§8).
//! Paging performs bounded filesystem work and belongs on the consumer's I/O
//! worker, never the heartbeat loop. Neither replay nor publication reads Runtime.
#![deny(missing_docs)]
pub(crate) mod owner;

use super::transport::StreamRecord;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    fs::File,
    io,
    sync::{Arc, Weak},
};

/// Maximum encoded record bytes queued per continuation, including envelopes.
pub const LIVE_BYTES: usize = 256 * 1024;
/// Maximum records queued per continuation.
pub const LIVE_RECORDS: usize = 16;
/// Per-page read budget; a long record yields Progress between chunks.
pub const PAGE_BYTES: usize = 32 * 1024;
/// Storage's bounded record limit, including envelope and newline.
const RECORD_BYTES: usize = super::transport::RECORD_LIMIT as usize;

/// Why a range cannot be reconstructed. Neither kind is an entity snapshot.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    /// The retained file begins after this range.
    RetainedPrefix,
    /// Legacy record lacks original classification or sender shape.
    ProvenanceMissing,
}
/// One bounded replay/publication step. A barrier precedes every live record.
#[derive(Debug)]
pub enum Item {
    /// Original durable envelope, including projected blob references/provenance.
    Record(Arc<StreamRecord>),
    /// Explicit missing range; later retained records remain readable.
    Gap {
        /// Inclusive beginning.
        from: u64,
        /// Inclusive end.
        to: u64,
        /// Loss diagnosis.
        reason: GapReason,
    },
    /// Captured head, emitted once unless lag closes replay before its barrier.
    Synced(u64),
    /// Closed continuation; resubscribe from the applied cursor, not this head.
    Lagged(u64),
    /// Replay made bounded progress; yield to the scheduler and page again.
    Progress,
    /// No live publication available; wait for a hint, then recheck.
    Pending,
    /// Producer ended normally after all queued facts.
    End,
}
#[derive(Default)]
struct Live {
    queue: VecDeque<(Arc<StreamRecord>, usize, u64)>,
    bytes: usize,
    lagged: Option<u64>,
}
#[derive(Default)]
struct Book {
    identity: Option<(String, String)>,
    file: Option<File>,
    head: u64,
    end: u64,
    ended: bool,
    failed: bool,
    live: Vec<Weak<Mutex<Live>>>,
}
/// A session's single publication authority, retained by the registry after exit.
#[derive(Clone, Default)]
pub struct History(Arc<Mutex<Book>>, Arc<tokio::sync::Notify>);
/// Ensures producer panic cannot leave a permanently pending continuation.
pub(super) struct Producer(History);
impl Drop for Producer {
    fn drop(&mut self) {
        self.0.finish(std::thread::panicking());
    }
}
impl History {
    /// Coalesced readiness/publication/end hint. Enable the notified future before
    /// checking readiness or paging; always recheck state after a wake.
    pub fn waker(&self) -> Arc<tokio::sync::Notify> {
        self.1.clone()
    }
    /// Scope the producer lifetime independently of notification delivery.
    pub(super) fn producer(&self) -> Producer {
        Producer(self.clone())
    }
    /// Bind the private source to its actual store namespace.
    pub(super) fn bind(&self, agent: &str, session: &str) {
        self.0.lock().identity = Some((agent.into(), session.into()));
    }
    /// Check identity as well as launch epoch before registry-owner installation.
    pub(super) fn belongs_to(&self, agent: &str, session: &str) -> bool {
        self.0
            .lock()
            .identity
            .as_ref()
            .is_some_and(|(a, s)| a == agent && s == session)
    }
    /// Recover a quiescent retained file on an I/O worker, reading at most one
    /// record-limit tail. This source ends at its captured head. Installation in
    /// Workers rechecks the registry epoch/producer so stale recovery cannot win.
    /// Unterminated/corrupt tails fail visibly; only the writer repairs torn tails.
    pub fn open_retained(path: &std::path::Path) -> io::Result<Self> {
        let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        fn text(value: Option<&std::ffi::OsStr>) -> Option<&str> {
            value.and_then(std::ffi::OsStr::to_str)
        }
        let agent = text(parent.file_name()).ok_or(io::ErrorKind::InvalidInput)?;
        let session = text(path.file_stem()).ok_or(io::ErrorKind::InvalidInput)?;
        if ![agent, session]
            .into_iter()
            .all(super::transport::valid_component)
            || session.eq_ignore_ascii_case("queue")
            || !path.ends_with(
                std::path::Path::new("sessions")
                    .join(agent)
                    .join(format!("{session}.jsonl")),
            )
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        crate::platform::structured::check_private_dir(parent)?;
        crate::platform::structured::check_private_dir(
            parent.parent().ok_or(io::ErrorKind::InvalidInput)?,
        )?;
        let file = crate::platform::structured::open_token(path)?;
        let length = file.metadata()?.len();
        let head = tail_head(&file, length, &mut Vec::new())?;
        if head == 0 && length != 0 {
            return Err(io::Error::other(
                "nonempty history requires a positive head",
            ));
        }
        let source = Self::default();
        source.bind(agent, session);
        source.initialize(&file, head, length)?;
        source.finish(false);
        Ok(source)
    }
    /// Install the recovered file/head before the producer accepts new records.
    pub(super) fn initialize(&self, file: &File, head: u64, end: u64) -> io::Result<()> {
        let mut book = self.0.lock();
        book.file = Some(file.try_clone()?);
        book.head = head;
        book.end = end;
        drop(book);
        self.1.notify_waiters();
        Ok(())
    }
    /// Publish only fsynced records, including terminal permission audits.
    /// Any single envelope over LIVE_BYTES lags every continuation, even caught-up ones.
    pub(super) fn publish(&self, record: Arc<StreamRecord>, bytes: usize) -> io::Result<()> {
        let mut book = self.0.lock();
        if book.ended || book.head.checked_add(1) != Some(record.seq) {
            book.failed = true;
            return Err(io::Error::other("history sequence discontinuity"));
        }
        book.head = record.seq;
        book.end = book
            .end
            .checked_add(bytes as u64)
            .ok_or(io::ErrorKind::InvalidData)?;
        let end = book.end;
        book.live.retain(|weak| {
            let Some(live) = weak.upgrade() else {
                return false;
            };
            let mut live = live.lock();
            if live.lagged.is_some()
                || live.queue.len() >= LIVE_RECORDS
                || bytes > LIVE_BYTES.saturating_sub(live.bytes)
            {
                live.lagged = Some(record.seq);
                live.queue.clear();
                live.bytes = 0;
            } else {
                live.bytes += bytes;
                live.queue.push_back((record.clone(), bytes, end));
            }
            true
        });
        drop(book);
        self.1.notify_waiters();
        Ok(())
    }
    /// Latch completion after all terminal audits, including cancelled delivery.
    pub(super) fn finish(&self, failed: bool) {
        let mut book = self.0.lock();
        book.ended = true;
        book.failed |= failed;
        drop(book);
        self.1.notify_waiters();
    }
    /// Atomically capture the replay barrier and register continuation. Ahead
    /// cursors are refused; absence means the beginning of retained history.
    /// File setup is asynchronous: WouldBlock means the producer is not ready.
    pub fn subscribe(&self, after: Option<u64>) -> io::Result<Cut> {
        let registered = self.0.lock().register(after)?;
        Ok(registered.cut(self.0.clone(), None))
    }
    /// Resume with a cut's opaque byte hint only when its cursor was applied.
    /// Hints are session-source fenced and must match `after`; stale/mismatched
    /// hints are refused, never silently interpreted against a different file.
    pub fn resume(&self, after: u64, hint: &Resume) -> io::Result<Cut> {
        if after != hint.seq || !Weak::ptr_eq(&hint.source, &Arc::downgrade(&self.0)) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut book = self.0.lock();
        if hint.offset > book.end {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let registered = book.register(Some(after))?;
        drop(book);
        Ok(registered.cut(self.0.clone(), Some(hint.offset)))
    }
}
/// Original cursor and byte boundary, meaningful only for its pinned source.
#[derive(Clone)]
pub struct Resume {
    seq: u64,
    offset: u64,
    source: Weak<Mutex<Book>>,
}
/// A registration is already live before its owned replay cut is constructed.
struct Registered {
    file: File,
    head: u64,
    end: u64,
    cursor: u64,
    live: Arc<Mutex<Live>>,
}
impl Book {
    /// Linearization point: this mutable book borrow encloses both operations.
    fn register(&mut self, after: Option<u64>) -> io::Result<Registered> {
        if self.file.is_none() && self.ended {
            return Err(io::Error::other("history producer failed before opening"));
        }
        let file = self
            .file
            .as_ref()
            .ok_or(io::ErrorKind::WouldBlock)?
            .try_clone()?;
        let cursor = after.unwrap_or(0);
        if cursor > self.head {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cursor_ahead"));
        }
        let live = Arc::new(Mutex::new(Live::default()));
        self.live.retain(|weak| weak.strong_count() != 0);
        self.live.push(Arc::downgrade(&live));
        Ok(Registered {
            file,
            head: self.head,
            end: self.end,
            cursor,
            live,
        })
    }
}
impl Registered {
    fn cut(self, book: Arc<Mutex<Book>>, offset: Option<u64>) -> Cut {
        let checkpoint = if self.cursor == self.head {
            Some(self.end)
        } else if self.cursor == 0 {
            Some(0)
        } else {
            offset
        };
        Cut {
            file: self.file,
            head: self.head,
            cursor: self.cursor,
            offset: checkpoint.unwrap_or(0),
            checkpoint,
            reads: 0,
            buffer: Vec::new(),
            used: 0,
            partial: Vec::new(),
            held: None,
            synced: false,
            closed: false,
            book,
            live: self.live,
        }
    }
}
/// Grow the cold tail geometrically, bounded by storage's single-record limit.
fn tail_head(file: &File, length: u64, tail: &mut Vec<u8>) -> io::Result<u64> {
    tail.clear();
    if length == 0 {
        return Ok(0);
    }
    let mut size = length.min(PAGE_BYTES as u64) as usize;
    loop {
        let old = tail.len();
        tail.resize(size, 0);
        tail.copy_within(..old, size - old);
        let start = length - size as u64;
        let mut read = 0;
        while read < size - old {
            let n = crate::platform::structured::read_file_at(
                file,
                &mut tail[read..size - old],
                start + read as u64,
            )?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            read += n;
        }
        if tail.last() != Some(&b'\n') {
            return Err(io::Error::other("unterminated history tail"));
        }
        let prefix = &tail[..size - 1];
        if let Some(at) = prefix.iter().rposition(|b| *b == b'\n') {
            return Ok(serde_json::from_slice::<StreamRecord>(&prefix[at + 1..])?.seq);
        }
        if start == 0 || size == RECORD_BYTES {
            return Ok(serde_json::from_slice::<StreamRecord>(prefix)?.seq);
        }
        size = size
            .saturating_mul(2)
            .min(RECORD_BYTES)
            .min(length as usize);
    }
}
/// Owned file/cursor at one captured head. Drop unregisters the weak continuation.
/// Memory holds one encoded and one decoded record, one page and LIVE_BYTES of queued records.
/// Calls return at most one record/gap/barrier and read at most PAGE_BYTES.
pub struct Cut {
    file: File,
    head: u64,
    cursor: u64,
    offset: u64,
    checkpoint: Option<u64>,
    reads: u64,
    buffer: Vec<u8>,
    used: usize,
    partial: Vec<u8>,
    held: Option<(Arc<StreamRecord>, u64)>,
    synced: bool,
    closed: bool,
    book: Arc<Mutex<Book>>,
    live: Arc<Mutex<Live>>,
}
impl Cut {
    /// Poll on an I/O worker. Errors are explicit; corrupt/truncated cuts never
    /// become Current. No parser or current harness version participates.
    pub fn page(&mut self) -> io::Result<Item> {
        if self.closed {
            return Ok(Item::End);
        }
        if let Some(head) = self.live.lock().lagged {
            self.closed = true;
            return Ok(Item::Lagged(head));
        }
        if !self.synced {
            if let Some((record, end)) = self.held.take() {
                return Ok(self.deliver(record, end));
            }
            if self.cursor == self.head {
                self.synced = true;
                return Ok(Item::Synced(self.head));
            }
            let mut read = false;
            loop {
                if self.used == self.buffer.len() {
                    if read {
                        return Ok(Item::Progress);
                    }
                    self.buffer.resize(PAGE_BYTES, 0);
                    let n = crate::platform::structured::read_file_at(
                        &self.file,
                        &mut self.buffer,
                        self.offset,
                    )?;
                    if n == 0 {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    self.reads = self.reads.saturating_add(n as u64);
                    self.offset += n as u64;
                    self.buffer.truncate(n);
                    self.used = 0;
                    read = true;
                }
                let remaining = &self.buffer[self.used..];
                let consumed = remaining
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(remaining.len(), |i| i + 1);
                if consumed > RECORD_BYTES.saturating_sub(self.partial.len()) {
                    return Err(io::Error::other("history record exceeds limit"));
                }
                self.partial.extend_from_slice(&remaining[..consumed]);
                self.used += consumed;
                if self.partial.last() != Some(&b'\n') {
                    continue;
                }
                let record: StreamRecord = serde_json::from_slice(&self.partial)?;
                let end = self.offset - (self.buffer.len() - self.used) as u64;
                let start = end - self.partial.len() as u64;
                self.partial.clear();
                if record.seq <= self.cursor {
                    if record.seq == self.cursor {
                        self.checkpoint = Some(end);
                    }
                    continue;
                }
                if record.seq > self.head {
                    return Err(io::Error::other("history cut missing head"));
                }
                let record = Arc::new(record);
                if self.cursor.checked_add(1) != Some(record.seq) {
                    let item = Item::Gap {
                        from: self.cursor + 1,
                        to: record.seq - 1,
                        reason: GapReason::RetainedPrefix,
                    };
                    self.cursor = record.seq - 1;
                    self.checkpoint = Some(start);
                    self.held = Some((record, end));
                    return Ok(item);
                }
                return Ok(self.deliver(record, end));
            }
        }
        let book = self.book.lock();
        let mut live = self.live.lock();
        if let Some(head) = live.lagged {
            self.closed = true;
            return Ok(Item::Lagged(head));
        }
        if let Some((record, bytes, end)) = live.queue.pop_front() {
            live.bytes -= bytes;
            drop(live);
            drop(book);
            return Ok(self.deliver(record, end));
        }
        if book.failed {
            return Err(io::Error::other("history producer failed"));
        }
        if book.ended {
            self.closed = true;
            return Ok(Item::End);
        }
        Ok(Item::Pending)
    }
    /// Bytes actually returned by filesystem reads for this cut (not logical offset).
    /// Consumers can account for replay I/O without treating hints as progress.
    pub fn read_bytes(&self) -> u64 {
        self.reads
    }
    /// Capture the last emitted record/gap boundary. Retain this only after that
    /// cursor was applied; a barrier or a lag notification never advances it.
    /// None until a cursor-only scan locates that cursor's byte boundary.
    pub fn checkpoint(&self) -> Option<Resume> {
        self.checkpoint.map(|offset| Resume {
            seq: self.cursor,
            offset,
            source: Arc::downgrade(&self.book),
        })
    }
    fn deliver(&mut self, record: Arc<StreamRecord>, end: u64) -> Item {
        self.cursor = record.seq;
        self.checkpoint = Some(end);
        if record.known.is_none() || (record.known == Some(false) && record.shape.is_none()) {
            Item::Gap {
                from: record.seq,
                to: record.seq,
                reason: GapReason::ProvenanceMissing,
            }
        } else {
            Item::Record(record)
        }
    }
}
#[cfg(test)]
pub(crate) mod tests;
