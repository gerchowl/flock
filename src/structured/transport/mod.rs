//! Bounded stream-json transport; no process spawning or public raw stdin seam.
//!
//! Workers own nonblocking pipe descriptors. Server-facing methods never wait
//! for pipe, queue or disk capacity. Stop/drop cancels workers without joining;
//! the supervisor must observe both completions before releasing session files.
//! Disk and transport failures are explicit completions, never turn results.
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
    /// Agent identifier, containing only ASCII letters, digits, '-' or '_'.
    pub agent: String,
    /// Session identifier with the same component restrictions.
    pub session: String,
}

/// Lossless normal-line envelope; oversize/invalid UTF-8 lines use named summaries.
#[derive(Debug, Serialize, Deserialize)]
pub struct StreamRecord {
    /// Dense per-session cursor, continued when an existing stream is reopened.
    pub seq: u64,
    /// Receive time in milliseconds since the Unix epoch.
    pub received_at_ms: u128,
    /// Exact line without its LF; synthetic summaries explicitly identify loss.
    pub raw: String,
}

/// A persisted record paired with S1's infallible adapter interpretation.
#[derive(Debug)]
pub struct Received {
    /// Durable envelope; written before this notification is offered.
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
/// pending. Only a known result reopens the private idle gate. Unknown results
/// fail closed. The supervisor owns socket refusal and process cleanup (#1462).
pub struct Transport {
    input: Option<mpsc::SyncSender<Vec<u8>>>,
    output: mpsc::Receiver<Received>,
    completion: mpsc::Receiver<Completion>,
    busy: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    close: Arc<AtomicBool>,
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
        for component in [&location.agent, &location.session] {
            if component.is_empty()
                || !component
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                return Err(io::Error::other("invalid structured stream component"));
            }
        }
        private_fs::set_nonblocking(&stdout)?;
        private_fs::set_nonblocking(&stdin)?;
        let (input, input_rx) = mpsc::sync_channel(1);
        let (output_tx, output) = mpsc::sync_channel(4);
        let (done, completion) = mpsc::sync_channel(2);
        let busy = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let close = Arc::new(AtomicBool::new(false));
        let (b, c, d) = (busy.clone(), cancel.clone(), done.clone());
        thread::Builder::new()
            .name("session-stdout".into())
            .spawn(move || {
                let result = pump(stdout, location, &version, output_tx, &b, &c);
                if result.is_err() {
                    c.store(true, Ordering::Release);
                }
                let _ = d.try_send(Completion::Reader(result));
            })?;
        let (c, closed) = (cancel.clone(), close.clone());
        if let Err(err) = thread::Builder::new()
            .name("session-stdin".into())
            .spawn(move || {
                let result = write_inputs(stdin, input_rx, &c, &closed);
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
            input: Some(input),
            output,
            completion,
            busy,
            cancel,
            close,
        })
    }

    /// Offer one user envelope at idle. WouldBlock means busy; it never queues a
    /// mid-turn steering write. Input is capped before serialization allocation.
    /// Only this method can reach the private stdin sender; no control bypass.
    pub fn try_submit(&self, user: &Value) -> io::Result<()> {
        if self.cancel.load(Ordering::Acquire) || self.close.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "transport closed",
            ));
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
    pub fn close_stdin(&mut self) {
        self.close.store(true, Ordering::Release);
        self.input.take();
    }

    /// Cancel both workers, including a reader blocked on consumer back-pressure.
    /// Cancellation is explicit Interrupted completion, not a silent EOF.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.cancel();
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
) -> io::Result<()> {
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(cancelled());
        }
        if close.load(Ordering::Acquire) {
            return Ok(());
        }
        match rx.recv_timeout(POLL) {
            Ok(bytes) => {
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
    let mut reader = BufReader::new(&mut file);
    let mut seq = 0;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(cancelled());
        }
        let mut line = Vec::new();
        let n = Read::by_ref(&mut reader)
            .take(RECORD_LIMIT)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            return Err(io::Error::other("incomplete session record"));
        }
        let record: StreamRecord = serde_json::from_slice(&line)?;
        if record.seq != seq + 1 {
            return Err(io::Error::other("non-dense session sequence"));
        }
        seq = record.seq;
    }
    Ok((file, seq))
}

fn pump(
    mut pipe: File,
    location: StreamLocation,
    version: &str,
    tx: mpsc::SyncSender<Received>,
    busy: &AtomicBool,
    cancel: &AtomicBool,
) -> io::Result<()> {
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
                    &mut file, &mut seq, &line, count, version, &tx, busy, cancel,
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
                    &mut file, &mut seq, &line, count, version, &tx, busy, cancel,
                )?;
            }
            return Ok(());
        }
    }
}

fn emit(
    file: &mut File,
    seq: &mut u64,
    line: &[u8],
    count: u64,
    version: &str,
    tx: &mpsc::SyncSender<Received>,
    busy: &AtomicBool,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let raw = if count > MAX_LINE_BYTES as u64 {
        serde_json::json!({"type":"flk_oversize", "bytes":count, "limit":MAX_LINE_BYTES, "prefix_base64":base64::engine::general_purpose::STANDARD.encode(&line[..line.len().min(256)])}).to_string()
    } else {
        match std::str::from_utf8(line) {
            Ok(text) => text.to_owned(),
            Err(_) => serde_json::json!({"type":"flk_invalid_utf8", "base64":base64::engine::general_purpose::STANDARD.encode(line)}).to_string(),
        }
    };
    let event = SessionEvent::parse(&raw, version);
    *seq = seq
        .checked_add(1)
        .ok_or_else(|| io::Error::other("session sequence exhausted"))?;
    let record = StreamRecord {
        seq: *seq,
        received_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
        raw,
    };
    serde_json::to_writer(&mut *file, &record)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    if matches!(
        event,
        SessionEvent::Success(_) | SessionEvent::ExecutionError(_)
    ) {
        busy.store(false, Ordering::Release);
    }
    let mut received = Received { record, event };
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
