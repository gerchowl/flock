//! Bounded stream-json transport; no process spawning or public raw stdin seam.
//!
//! Workers own nonblocking pipe descriptors. Server-facing methods never wait
//! for pipe, queue or disk capacity. Stop/drop cancels workers without joining;
//! the supervisor must observe both completions before releasing session files.
//! Disk and transport failures are explicit completions, never turn results.
//! Each durable record is buffered before one write_all. Reopen repairs a torn
//! final record without a trailing LF, logging discarded bytes
//! and continuing its sequence; interior corruption remains an explicit error.
//! Envelope origins distinguish transport summaries from stdout lookalikes.
//! Any top-level result ends the input gate; its outcome remains adapter work.
#![deny(missing_docs)]

use super::claude::SessionEvent;
use crate::platform::structured as private_fs;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Accept complete lines up to 2 MiB; larger lines produce explicit summaries.
pub const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
const RECORD_LIMIT: u64 = 16 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(2);

/// Durable stream location. Components are validated before any worker starts.
pub struct StreamLocation {
    /// Existing flock data root (not the harness profile).
    pub data_dir: PathBuf,
    /// Storage threshold captured from `[sessions]` at launch.
    pub storage: super::blobs::StorageConfig,
    /// Agent identifier, containing only ASCII letters, digits, '-' or '_'.
    pub agent: String,
    /// Session identifier with the same restrictions; `queue` is reserved (any case).
    pub session: String,
}

/// Trusted source of the record, assigned by the transport, never by stdout JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordOrigin {
    /// Original Claude stdout, including apparent flk marker names.
    Claude,
    /// A transport-generated oversize or invalid-UTF-8 summary.
    Flk,
}

/// Lossless normal-line envelope; oversize/invalid UTF-8 lines use named summaries.
#[derive(Debug, Serialize, Deserialize)]
pub struct StreamRecord {
    /// Dense per-session cursor, continued when an existing stream is reopened.
    pub seq: u64,
    /// Receive time in milliseconds since the Unix epoch.
    pub received_at_ms: u128,
    /// Trusted origin, distinct from any similarly named field inside `raw`.
    pub origin: RecordOrigin,
    /// Top-level result subtype, retained even when S1 does not model it.
    pub result_subtype: Option<Value>,
    /// Exact line without its LF; synthetic summaries explicitly identify loss.
    pub raw: String,
}

/// A live event paired with its envelope and explicit persistence status.
#[derive(Debug)]
pub struct Received {
    /// False for live-only partial deltas; their seq must not advance a cursor.
    pub persisted: bool,
    /// Durable envelope, or a live-only delta with the preceding durable seq.
    pub record: StreamRecord,
    /// Interpretation using the observed harness version.
    pub event: SessionEvent,
}

/// Worker termination, independent of the supervised process's outcome.
#[derive(Debug)]
pub enum Completion {
    /// Stdout reached EOF, was cancelled, or failed reading/persisting.
    Reader(io::Result<()>),
    /// Stdin closed, was cancelled, or failed writing.
    Writer(io::Result<()>),
}

/// Server handle. Inbound/outbound memory is bounded; there is at most one turn
/// pending. Any parsed top-level result reopens the private idle gate, including
/// unmodelled subtypes. A lossy result header also opens it without claiming turn
/// success. Indeterminate loss surfaces `structured_turn_end_unknown` until a
/// result confirms the boundary. The supervisor owns socket refusal and cleanup.
pub struct Transport {
    input: InputHandle,
    output: mpsc::Receiver<Received>,
    completion: mpsc::Receiver<Completion>,
}

/// Submission authority separated from worker-owned event receivers.
#[derive(Clone)]
pub(super) struct InputHandle {
    input: Option<mpsc::SyncSender<Vec<u8>>>,
    busy: Arc<AtomicBool>,
    uncertain: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    close: Arc<AtomicBool>,
}

/// Cloneable nonblocking shutdown requests for the process supervisor.
#[derive(Clone)]
pub(super) struct StopHandle {
    cancel: Arc<AtomicBool>,
    close: Arc<AtomicBool>,
}

impl StopHandle {
    /// Close stdin; stop intent must already be recorded.
    pub(super) fn close_stdin(&self) {
        self.close.store(true, Ordering::Release);
    }
    /// Cancel I/O, including blocked notification delivery.
    pub(super) fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    /// Whether cancellation was requested, including by a failed worker.
    pub(super) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
}

impl Transport {
    /// Start workers over owned pipe endpoints, without spawning a process.
    /// File setup/recovery happens on the reader worker. A startup error appears
    /// as Reader(Err), so callers must monitor completions as well as events.
    pub fn start(
        stdout: File,
        stdin: File,
        location: StreamLocation,
        version: String,
    ) -> io::Result<Self> {
        Self::start_guarded(stdout, stdin, location, version, || Ok(()))
    }

    /// Run the registry refusal on the writer worker immediately before its
    /// first input bytes. Failure is explicit Writer(Err), with no bytes written.
    pub(super) fn start_guarded(
        stdout: File,
        stdin: File,
        location: StreamLocation,
        version: String,
        before_first_write: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        for component in [&location.agent, &location.session] {
            if component.is_empty()
                || !component
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                return Err(io::Error::other("invalid structured stream component"));
            }
        }
        if ["queue"]
            .iter()
            .any(|name| location.session.eq_ignore_ascii_case(name))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "structured_reserved_session_id",
            ));
        }
        private_fs::set_nonblocking(&stdout)?;
        private_fs::set_nonblocking(&stdin)?;
        let (input, input_rx) = mpsc::sync_channel(1);
        let (output_tx, output) = mpsc::sync_channel(4);
        let (done, completion) = mpsc::sync_channel(2);
        let busy = Arc::new(AtomicBool::new(false));
        let uncertain = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let close = Arc::new(AtomicBool::new(false));
        let (b, u, c, d) = (
            busy.clone(),
            uncertain.clone(),
            cancel.clone(),
            done.clone(),
        );
        thread::Builder::new()
            .name("session-stdout".into())
            .spawn(move || {
                let result = pump(stdout, location, &version, output_tx, &b, &u, &c);
                if result.is_err() {
                    c.store(true, Ordering::Release);
                }
                let _ = d.try_send(Completion::Reader(result));
            })?;
        let (c, closed) = (cancel.clone(), close.clone());
        if let Err(err) = thread::Builder::new()
            .name("session-stdin".into())
            .spawn(move || {
                let result = write_inputs(stdin, input_rx, &c, &closed, before_first_write);
                if result.is_err() {
                    c.store(true, Ordering::Release);
                }
                let _ = done.try_send(Completion::Writer(result));
            })
        {
            cancel.store(true, Ordering::Release);
            return Err(err);
        }
        Ok(Self {
            input: InputHandle {
                input: Some(input),
                busy,
                uncertain,
                cancel,
                close,
            },
            output,
            completion,
        })
    }

    /// Share shutdown flags without sharing receivers or blocking on a mutex.
    pub(super) fn stop_handle(&self) -> StopHandle {
        StopHandle {
            cancel: self.input.cancel.clone(),
            close: self.input.close.clone(),
        }
    }

    /// Clone submission authority without sharing or locking event receivers.
    pub(super) fn input_handle(&self) -> InputHandle {
        self.input.clone()
    }

    /// Whether an accepted input lacks any top-level result, including Unknown.
    pub(super) fn input_inflight(&self) -> bool {
        self.input.input_inflight()
    }

    /// Offer a bounded user envelope at idle; WouldBlock means input is in flight.
    /// Indeterminate loss returns `structured_turn_end_unknown` instead of busy.
    pub fn try_submit(&self, user: &Value) -> io::Result<()> {
        self.input.try_submit(user)
    }

    /// Poll the bounded event queue without waiting. Empty is not EOF.
    pub fn try_receive(&self) -> Result<Received, mpsc::TryRecvError> {
        self.output.try_recv()
    }

    /// Poll worker termination/failure without waiting; observe both workers.
    pub fn try_completion(&self) -> Result<Completion, mpsc::TryRecvError> {
        self.completion.try_recv()
    }

    /// Request stdin closure, discarding any unfinished input. Never waits for a
    /// full pipe; the writer closes its descriptor within one polling interval.
    /// An in-flight write may leave a partial JSON line before EOF. This is
    /// acceptable only at stop: no further turn is delivered, and supervision
    /// records stop intent before closure and never treats truncation as success.
    pub fn close_stdin(&mut self) {
        self.input.close.store(true, Ordering::Release);
        self.input.input.take();
    }

    /// Cancel both workers, including a reader blocked on consumer back-pressure.
    /// Cancellation is explicit Interrupted completion, not a silent EOF.
    /// Like close_stdin, cancellation can leave a partial JSON line on stdin
    /// before EOF. Record stop intent first; truncation is never turn success.
    pub fn cancel(&self) {
        self.input.cancel.store(true, Ordering::Release);
    }
}

/// Drop requests cancellation without joining. An in-flight stdin write may be
/// left as a partial JSON line before EOF, just as with close_stdin/cancel.
impl Drop for Transport {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl InputHandle {
    /// Whether an accepted input lacks any top-level result, including Unknown.
    pub(super) fn input_inflight(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// Offer one user envelope at idle. WouldBlock means busy; it never queues a
    /// mid-turn steering write. Input is capped before serialization allocation.
    /// Only this method can reach the private stdin sender; no control bypass.
    /// Loss with no identifiable event type yields `structured_turn_end_unknown`
    /// until a real result confirms the boundary; never retry it as ordinary busy.
    pub(super) fn try_submit(&self, user: &Value) -> io::Result<()> {
        if self.cancel.load(Ordering::Acquire) || self.close.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "transport closed",
            ));
        }
        if self.uncertain.load(Ordering::Acquire) {
            return Err(io::Error::other("structured_turn_end_unknown"));
        }
        if user.get("type").and_then(Value::as_str) != Some("user") {
            return Err(io::Error::other("stdin accepts only user envelopes"));
        }
        let mut bytes = Vec::new();
        serde_json::to_writer(InputBuffer(&mut bytes), user)?;
        bytes.push(b'\n');
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "session is working",
            ));
        }
        if let Err(err) = self
            .input
            .as_ref()
            .ok_or_else(|| io::Error::other("stdin closed"))?
            .try_send(bytes)
        {
            self.busy.store(false, Ordering::Release);
            return Err(io::Error::other(err.to_string()));
        }
        Ok(())
    }
}

struct InputBuffer<'a>(&'a mut Vec<u8>);
impl Write for InputBuffer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_LINE_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdin envelope exceeds limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "transport cancelled")
}

fn write_inputs(
    mut pipe: File,
    rx: mpsc::Receiver<Vec<u8>>,
    cancel: &AtomicBool,
    close: &AtomicBool,
    before_first_write: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let mut before_first_write = Some(before_first_write);
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(cancelled());
        }
        if close.load(Ordering::Acquire) {
            return Ok(());
        }
        match rx.recv_timeout(POLL) {
            Ok(bytes) => {
                if let Some(check) = before_first_write.take() {
                    check()?;
                }
                let mut remaining = bytes.as_slice();
                while !remaining.is_empty() {
                    if cancel.load(Ordering::Acquire) {
                        return Err(cancelled());
                    }
                    if close.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    match pipe.write(remaining) {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "stdin write zero",
                            ))
                        }
                        Ok(n) => remaining = &remaining[n..],
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                        Err(e) => return Err(e),
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Open an exclusively owned session stream. Records are serialized in memory
/// and submitted with one write_all; short writes/crashes can still tear a tail.
/// Recovery truncates only a final tail without a trailing LF, syncs the repair,
/// logs discarded bytes and resumes the last good sequence. Malformed records
/// ending in LF are corruption and are refused, as are non-dense sequences.
fn open_stream(location: StreamLocation, cancel: &AtomicBool) -> io::Result<(File, u64)> {
    let dir = location.data_dir.join("sessions");
    private_fs::ensure_private_dir(&dir)?;
    let dir = dir.join(location.agent);
    private_fs::ensure_private_dir(&dir)?;
    let path = dir.join(format!("{}.jsonl", location.session));
    let mut file = match private_fs::create_private_file(&path) {
        Ok(file) => return Ok((file, 0)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => private_fs::open_append_file(&path)?,
        Err(e) => return Err(e),
    };
    let length = file.metadata()?.len();
    let mut good_bytes = 0;
    let mut seq = 0_u64;
    {
        let mut reader = BufReader::new(&mut file);
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(cancelled());
            }
            let mut line = Vec::new();
            let n = Read::by_ref(&mut reader)
                .take(RECORD_LIMIT)
                .read_until(b'\n', &mut line)? as u64;
            if n == 0 {
                break;
            }
            let record = serde_json::from_slice::<StreamRecord>(&line);
            if line.ends_with(b"\n") {
                let record = record.map_err(|_| io::Error::other("corrupt session record"))?;
                if Some(record.seq) != seq.checked_add(1) {
                    return Err(io::Error::other("non-dense session sequence"));
                }
                seq = record.seq;
                good_bytes += n;
            } else {
                // A bounded scan accepts only an unterminated final tail. Any
                // newline makes this a complete corrupt record, never a repair.
                let mut chunk = [0; 8192];
                loop {
                    if cancel.load(Ordering::Acquire) {
                        return Err(cancelled());
                    }
                    let n = reader.read(&mut chunk)?;
                    if n == 0 {
                        break;
                    }
                    if chunk[..n].contains(&b'\n') {
                        return Err(io::Error::other("corrupt session record"));
                    }
                }
                break;
            }
        }
    }
    if good_bytes < length {
        file.set_len(good_bytes)?;
        file.sync_all()?;
        crate::logging::structured_stream_repaired(length - good_bytes);
    }
    Ok((file, seq))
}

fn pump(
    mut pipe: File,
    location: StreamLocation,
    version: &str,
    tx: mpsc::SyncSender<Received>,
    busy: &AtomicBool,
    uncertain: &AtomicBool,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let blobs = super::blobs::BlobStore::open(&location.data_dir).map_err(io::Error::other)?;
    let threshold = location.storage.blob_min_bytes;
    let (mut file, mut seq) = open_stream(location, cancel)?;
    let mut line = Vec::new();
    let mut count = 0_u64;
    let mut chunk = [0; 8192];
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(cancelled());
        }
        let n = match pipe.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(POLL);
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        for &byte in &chunk[..n] {
            if byte == b'\n' {
                emit(
                    &mut file,
                    &mut seq,
                    &line,
                    count,
                    version,
                    &tx,
                    busy,
                    uncertain,
                    cancel,
                    (&blobs, threshold),
                )?;
                line.clear();
                count = 0;
            } else {
                count = count.saturating_add(1);
                if line.len() < MAX_LINE_BYTES {
                    line.push(byte);
                }
            }
        }
        if n == 0 {
            if count > 0 {
                emit(
                    &mut file,
                    &mut seq,
                    &line,
                    count,
                    version,
                    &tx,
                    busy,
                    uncertain,
                    cancel,
                    (&blobs, threshold),
                )?;
            }
            return Ok(());
        }
    }
}

/// Inspect a bounded JSON object prefix structurally, never matching nested
/// text. The harness's top-level type header precedes its potentially lost body.
/// A deliberate parse abort after that header tolerates a truncated/invalid tail.
fn prefix_type(prefix: &[u8]) -> Option<String> {
    struct Header<'a>(&'a mut Option<String>);
    impl<'de> serde::de::Visitor<'de> for Header<'_> {
        type Value = ();
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("event object prefix")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                if key == "type" {
                    *self.0 = Some(map.next_value()?);
                    return Err(serde::de::Error::custom("event header inspected"));
                }
                map.next_value::<serde::de::IgnoredAny>()?;
            }
            Ok(())
        }
    }
    let mut kind = None;
    let _ = serde::de::Deserializer::deserialize_map(
        &mut serde_json::Deserializer::from_slice(prefix),
        Header(&mut kind),
    );
    kind
}

fn emit(
    file: &mut File,
    seq: &mut u64,
    line: &[u8],
    count: u64,
    version: &str,
    tx: &mpsc::SyncSender<Received>,
    busy: &AtomicBool,
    uncertain: &AtomicBool,
    cancel: &AtomicBool,
    storage: (&super::blobs::BlobStore, usize),
) -> io::Result<()> {
    let origin = if count > MAX_LINE_BYTES as u64 || std::str::from_utf8(line).is_err() {
        RecordOrigin::Flk
    } else {
        RecordOrigin::Claude
    };
    let kind = if origin == RecordOrigin::Flk {
        prefix_type(&line[..line.len().min(256)])
    } else {
        None
    };
    let turn_end = match kind.as_deref() {
        Some("result") => "result",
        Some(_) => "not_result",
        None => "unknown",
    };
    let raw = if count > MAX_LINE_BYTES as u64 {
        serde_json::json!({"type":"flk_oversize", "bytes":count, "limit":MAX_LINE_BYTES, "prefix_base64":base64::engine::general_purpose::STANDARD.encode(&line[..line.len().min(256)]), "turn_end":turn_end}).to_string()
    } else {
        match std::str::from_utf8(line) {
            Ok(text) => text.to_owned(),
            Err(_) => serde_json::json!({"type":"flk_invalid_utf8", "base64":base64::engine::general_purpose::STANDARD.encode(line), "turn_end":turn_end}).to_string(),
        }
    };
    let event = SessionEvent::parse(&raw, version);
    let (ends_turn, result_subtype) = match &event {
        SessionEvent::Success(_) => (true, Some(Value::String("success".into()))),
        SessionEvent::ExecutionError(_) => {
            (true, Some(Value::String("error_during_execution".into())))
        }
        SessionEvent::Unknown { value, .. }
            if value.get("type").and_then(Value::as_str) == Some("result") =>
        {
            (true, value.get("subtype").cloned())
        }
        _ => (false, None),
    };
    let stored_raw = storage
        .0
        .project(&raw, storage.1)
        .map_err(io::Error::other)?;
    let persisted = stored_raw.is_some();
    if persisted {
        *seq = seq
            .checked_add(1)
            .ok_or_else(|| io::Error::other("session sequence exhausted"))?;
    }
    let mut record = StreamRecord {
        seq: *seq,
        received_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
        origin,
        result_subtype,
        raw,
    };
    if let Some(raw) = stored_raw {
        let live_raw = std::mem::replace(&mut record.raw, raw);
        let mut encoded = serde_json::to_vec(&record)?;
        encoded.push(b'\n');
        file.write_all(&encoded)?;
        file.sync_data()?;
        record.raw = live_raw;
    }
    if ends_turn || (origin == RecordOrigin::Flk && turn_end == "result") {
        uncertain.store(false, Ordering::Release);
        busy.store(false, Ordering::Release);
    } else if origin == RecordOrigin::Flk && turn_end == "unknown" && busy.load(Ordering::Acquire) {
        uncertain.store(true, Ordering::Release);
    }
    let mut received = Received {
        persisted,
        record,
        event,
    };
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(cancelled());
        }
        match tx.try_send(received) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Full(value)) => {
                received = value;
                thread::sleep(POLL);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return Err(cancelled()),
        }
    }
}

#[cfg(test)]
mod tests;
