use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::api::schema::{EventEnvelope, EventKind};

/// One durable log line (#175 O1). Newline-delimited JSON so a torn tail
/// from a crash mid-write is detectable: the last line either parses whole
/// or is dropped on load.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedEvent {
    seq: u64,
    ts_ms: u64,
    envelope: EventEnvelope,
}

#[derive(Clone, Default)]
pub struct EventHub {
    reply_waiters: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
    /// The newest sequence pushed, behind its own lock so a waiter can sleep
    /// on it (#438). Kept apart from `inner` because a condvar pairs with ONE
    /// mutex, and `inner` is held across disk writes — a waiter must never
    /// queue behind an fsync just to learn that nothing new arrived.
    arrived: std::sync::Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
    read_max_wait_ms: ReadMaxWait,
    #[cfg(test)]
    cold_reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Runs once between a read's disk pass and its ring pass, so a test
    /// can interleave a push or a rotation there deterministically.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub(crate) disk_read_hook: std::sync::Arc<std::sync::Mutex<Option<Box<dyn FnMut() + Send>>>>,
}

#[derive(Clone)]
struct ReadMaxWait(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Default for ReadMaxWait {
    fn default() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
            crate::api::schema::EVENTS_READ_MAX_WAIT_MS,
        )))
    }
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
    /// The newest sequence of a persisted kind. A restart resumes numbering
    /// from it, so a sequence above it may be handed out again and must
    /// never become a reader's cursor.
    durable_sequence: u64,
    /// The newest durable sequence no longer in the ring. Every durable
    /// event past it is in the ring, so only an older cursor reads disk.
    evicted_durable: u64,
    #[cfg(test)]
    ring_cap: Option<usize>,
    events: Vec<(u64, EventEnvelope)>,
    log: Option<PersistentLog>,
}

struct PersistentLog {
    path: PathBuf,
    file: std::fs::File,
    /// Events written to the active file since open/rotation.
    active_events: u64,
    /// Bytes written to the active file since open/rotation.
    active_bytes: u64,
    /// Rotations since open. A read outside the lock compares it before and
    /// after, because a rotation in between renames the files it is reading.
    rotations: u64,
}

/// Rotation policy (ADR-0005): rotate the active file past either bound,
/// keep at most `MAX_ROTATED` older files (`event-log.1.jsonl` newest →
/// `event-log.4.jsonl` oldest).
const MAX_ACTIVE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ACTIVE_EVENTS: u64 = 100_000;
const MAX_ROTATED: usize = 4;

impl PersistentLog {
    fn open(path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::options()
            .append(true)
            .create(true)
            .open(&path)?;
        let active_bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            path,
            file,
            // Only used to bound the active file; recounting lines on open
            // is not worth it — the byte bound covers a restarted file.
            active_events: 0,
            active_bytes,
            rotations: 0,
        })
    }

    fn rotated_path(&self, index: usize) -> PathBuf {
        rotated_path(&self.path, index)
    }

    fn append(&mut self, event: &PersistedEvent) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(event)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_data()?;
        self.active_bytes += line.len() as u64;
        self.active_events += 1;
        if self.active_bytes >= MAX_ACTIVE_BYTES || self.active_events >= MAX_ACTIVE_EVENTS {
            self.rotate()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        let oldest = self.rotated_path(MAX_ROTATED);
        if oldest.exists() {
            std::fs::remove_file(&oldest)?;
        }
        for index in (1..MAX_ROTATED).rev() {
            let from = self.rotated_path(index);
            if from.exists() {
                std::fs::rename(&from, self.rotated_path(index + 1))?;
            }
        }
        std::fs::rename(&self.path, self.rotated_path(1))?;
        self.file = std::fs::File::options()
            .append(true)
            .create(true)
            .open(&self.path)?;
        self.active_events = 0;
        self.active_bytes = 0;
        self.rotations += 1;
        Ok(())
    }
}

/// Drop a torn (newline-less) tail from the active file before appending:
/// without this, the next append would concatenate onto the partial line and
/// turn a recoverable torn tail into mid-file corruption.
fn truncate_torn_tail(path: &Path) {
    let Ok(content) = std::fs::read(path) else {
        return;
    };
    if content.is_empty() || content.ends_with(b"\n") {
        return;
    }
    let keep = content
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|position| position + 1)
        .unwrap_or(0);
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_len(keep as u64);
    }
}

/// Read one log file tolerating exactly one torn line at the tail (crash
/// mid-write). A parse failure anywhere *before* the tail is a gap — the
/// whole file is rejected so a silent hole never masquerades as history
/// (#175 §8.2 "no gaps").
/// Highest sequence still readable in a file we are about to quarantine.
///
/// Deliberately lenient where [`read_log_file`] is strict: that one refuses a
/// file with any unparseable interior line, because a partial replay is worse
/// than none. Here the file is already being set aside — the only question is
/// which numbers it consumed, and every line that still parses answers it. A
/// file too damaged to yield any seq returns 0 and the caller falls back to
/// the rotated high-water mark.
fn highest_seq_best_effort(path: &Path) -> u64 {
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut highest = 0;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if let Ok(event) = serde_json::from_str::<PersistedEvent>(line.trim_end()) {
            highest = highest.max(event.seq);
        }
    }
    highest
}

/// The sequence of a line that is a well-formed log entry this binary cannot
/// type: a kind added by a newer release (#1020, ADR-0028 §3a). Such a line is
/// skipped, never treated as corruption, so a downgrade or the old side of a
/// live handoff keeps the log instead of quarantining it. A KNOWN kind that
/// fails to parse is real corruption and is not skipped.
fn unknown_kind_seq(line: &str) -> Option<u64> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let kind = value.pointer("/envelope/event")?;
    if !kind.is_string() || serde_json::from_value::<EventKind>(kind.clone()).is_ok() {
        return None;
    }
    value.get("seq")?.as_u64()
}

/// Events in one file, plus the highest sequence among skipped unknown-kind
/// lines: those numbers are spent too, and must not be handed out again.
fn read_log_file(path: &Path) -> Result<(Vec<PersistedEvent>, u64), String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    let mut reader = std::io::BufReader::new(file);
    let mut events = Vec::new();
    let mut skipped_seq = 0;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        if read == 0 {
            break;
        }
        let complete = line.ends_with('\n');
        match serde_json::from_str::<PersistedEvent>(line.trim_end()) {
            Ok(event) => events.push(event),
            Err(err) if !complete => {
                // Torn tail from a crash mid-write: drop it, keep the prefix.
                crate::logging::event_log_torn_tail(&path.display().to_string());
                let _ = err;
                break;
            }
            Err(err) => match unknown_kind_seq(line.trim_end()) {
                Some(seq) => skipped_seq = skipped_seq.max(seq),
                None => return Err(format!("{}: corrupt line: {err}", path.display())),
            },
        }
        if !complete {
            break;
        }
    }
    Ok((events, skipped_seq))
}

#[derive(Default)]
pub(crate) struct EventRead {
    pub gap: Option<(u64, u64)>,
    pub events: Vec<(u64, EventEnvelope)>,
    /// The cursor to hand a reader: never past the newest durable event.
    pub next_after: u64,
    /// How far this read looked, for waiting on the hub.
    pub scanned: u64,
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("event-log");
    path.with_file_name(format!("{stem}.{index}.jsonl"))
}

pub(crate) struct ReplyWaitGuard {
    hub: EventHub,
    correlation: String,
}
impl Drop for ReplyWaitGuard {
    fn drop(&mut self) {
        if let Ok(mut waiters) = self.hub.reply_waiters.lock() {
            if let Some(count) = waiters.get_mut(&self.correlation) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    waiters.remove(&self.correlation);
                }
            }
        }
    }
}

impl EventHub {
    pub(crate) fn track_reply_wait(&self, correlation: &str) -> ReplyWaitGuard {
        if let Ok(mut waiters) = self.reply_waiters.lock() {
            *waiters.entry(correlation.into()).or_default() += 1;
        }
        ReplyWaitGuard {
            hub: self.clone(),
            correlation: correlation.into(),
        }
    }

    pub(crate) fn reply_waits(&self) -> Vec<String> {
        self.reply_waiters
            .lock()
            .map(|waiters| waiters.keys().cloned().collect())
            .unwrap_or_default()
    }

    // #175 phase 4: bumped 512 → 4096 so the check-runner's per-tick fan-out
    // (CheckRan / CheckFired / CheckErrored / ChecksHeartbeat) can't starve the
    // pane-lifecycle events that share the in-memory ring. Operator-approved.
    const MAX_EVENTS: usize = 4096;

    /// A hub whose persisted event kinds are mirrored to an append-only
    /// JSONL log at `path`, seeded from any existing log so sequences stay
    /// monotonic across restarts (#175 O1). A corrupt log is left on disk
    /// untouched (P4: fail toward leaking) and the hub starts from the
    /// highest sequence it could read.
    pub fn with_persistence(path: PathBuf) -> Self {
        let hub = Self::default();
        let mut restored: Vec<PersistedEvent> = Vec::new();
        {
            let Ok(mut state) = hub.inner.lock() else {
                return hub;
            };
            let mut highest_skipped_seq = 0;
            for index in (1..=MAX_ROTATED).rev() {
                let mut name = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("event-log")
                    .to_string();
                name.push_str(&format!(".{index}.jsonl"));
                match read_log_file(&path.with_file_name(name)) {
                    Ok((events, skipped)) => {
                        restored.extend(events);
                        highest_skipped_seq = highest_skipped_seq.max(skipped);
                    }
                    Err(err) => crate::logging::event_log_corrupt(&err),
                }
            }
            let mut highest_quarantined_seq = 0;
            match read_log_file(&path) {
                Ok((events, skipped)) => {
                    restored.extend(events);
                    highest_skipped_seq = highest_skipped_seq.max(skipped);
                }
                Err(err) => {
                    // The active file is append-target: a corrupt one would
                    // swallow every future event on the next load. Quarantine
                    // it aside (rename, never delete — P4) and start fresh.
                    crate::logging::event_log_corrupt(&err);
                    // Its sequences are still SPENT. `next_sequence` below is
                    // derived from what parsed, so without this the numbers
                    // that lived in the quarantined file get handed out a
                    // second time — and #175 O1's "monotonic across restarts"
                    // silently stops holding. Nothing depends on it locally
                    // today (restore reads from 0), but any offset-based
                    // consumer would then skip or replay a whole range with no
                    // way to notice. Salvage the high-water mark first.
                    highest_quarantined_seq = highest_seq_best_effort(&path);
                    let mut name = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or("event-log")
                        .to_string();
                    name.push_str(".corrupt.jsonl");
                    let _ = std::fs::rename(&path, path.with_file_name(name));
                }
            }
            truncate_torn_tail(&path);
            restored.sort_by_key(|event| event.seq);
            state.next_sequence = restored
                .last()
                .map(|event| event.seq)
                .unwrap_or(0)
                .max(highest_quarantined_seq)
                .max(highest_skipped_seq);
            state.durable_sequence = state.next_sequence;
            // Everything below the ring is on disk at best, including what a
            // rotation already deleted, so an older cursor must check there.
            let seeded = restored.len().saturating_sub(Self::MAX_EVENTS);
            state.evicted_durable = restored
                .get(seeded)
                .map(|first| first.seq.saturating_sub(1))
                .unwrap_or(state.next_sequence);
            state.events = restored
                .iter()
                .rev()
                .take(Self::MAX_EVENTS)
                .rev()
                .map(|event| (event.seq, event.envelope.clone()))
                .collect();
            match PersistentLog::open(path) {
                Ok(log) => state.log = Some(log),
                Err(err) => crate::logging::event_log_open_failed(&err.to_string()),
            }
        }
        hub
    }

    pub fn push(&self, event: EventEnvelope) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        if event.event.is_persisted() {
            state.durable_sequence = sequence;
            if let Some(log) = state.log.as_mut() {
                let persisted = PersistedEvent {
                    seq: sequence,
                    ts_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
                        .unwrap_or(0),
                    envelope: event.clone(),
                };
                if let Err(err) = log.append(&persisted) {
                    crate::logging::event_log_write_failed(&err.to_string());
                }
            }
        }
        state.events.push((sequence, event));
        #[cfg(test)]
        let cap = state.ring_cap.unwrap_or(Self::MAX_EVENTS);
        #[cfg(not(test))]
        let cap = Self::MAX_EVENTS;
        let overflow = state.events.len().saturating_sub(cap);
        if overflow > 0 {
            let evicted = state
                .events
                .drain(0..overflow)
                .filter(|(_, event)| event.event.is_persisted())
                .map(|(seq, _)| seq)
                .max();
            if let Some(evicted) = evicted {
                state.evicted_durable = evicted;
            }
        }
        drop(state);
        let (latest, condvar) = &*self.arrived;
        if let Ok(mut latest) = latest.lock() {
            *latest = sequence;
            condvar.notify_all();
        }
    }

    /// Block until an event newer than `sequence` has been pushed, or
    /// `timeout` passes. Returns whether one arrived.
    ///
    /// The event-driven half of a stream subscription (#438): a waiter wakes
    /// on the push itself rather than on its next sleep tick, so delivery
    /// latency is the notify, not the tick. The timeout stays so the caller
    /// can still notice a hung-up client or a stopping server.
    pub fn wait_after(&self, sequence: u64, timeout: std::time::Duration) -> bool {
        let (latest, condvar) = &*self.arrived;
        let Ok(guard) = latest.lock() else {
            return false;
        };
        if *guard > sequence {
            return true;
        }
        match condvar.wait_timeout_while(guard, timeout, |latest| *latest <= sequence) {
            Ok((guard, _)) => *guard > sequence,
            Err(_) => false,
        }
    }

    pub fn events_after(&self, sequence: u64) -> Vec<(u64, EventEnvelope)> {
        let Ok(state) = self.inner.lock() else {
            return Vec::new();
        };
        state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect()
    }

    /// The ring's events with `seq > sequence`, and the cursor a subscriber
    /// that fell behind the ring skips to (#1030, ADR-0028 §3). The ring holds
    /// everything newer than its oldest entry, so a cursor below that has
    /// lost events, and the stream says so instead of dropping them silently.
    pub(crate) fn events_since(&self, sequence: u64) -> (Option<u64>, Vec<(u64, EventEnvelope)>) {
        let Ok(state) = self.inner.lock() else {
            return (None, Vec::new());
        };
        let floor = state
            .events
            .first()
            .map_or(state.next_sequence, |(first, _)| first.saturating_sub(1));
        let lagged = (sequence < floor).then_some(floor);
        let events = state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence.max(floor))
            .cloned()
            .collect();
        (lagged, events)
    }

    pub fn current_sequence(&self) -> u64 {
        let Ok(state) = self.inner.lock() else {
            return 0;
        };
        state.next_sequence
    }

    /// `events.read` (ADR-0028 §3): durable events with `seq > after` that
    /// `keep` accepts, at most `limit`, oldest first.
    ///
    /// The ring holds every durable event newer than `evicted_durable`, so
    /// a reader at or past it never touches disk. An older cursor reads the
    /// log files newest first WITHOUT the hub lock, so a slow disk never
    /// stalls a push. A rotation racing that read can only cost the oldest
    /// end, which then surfaces as a gap, never as a silent hole.
    pub(crate) fn read(
        &self,
        after: u64,
        limit: usize,
        keep: impl Fn(&EventEnvelope) -> bool,
    ) -> EventRead {
        let (evicted, path) = {
            let Ok(state) = self.inner.lock() else {
                return EventRead::default();
            };
            let path = state.log.as_ref().map(|log| log.path.clone());
            (state.evicted_durable, path)
        };
        let mut floor = None;
        let mut disk: Vec<(u64, EventEnvelope)> = Vec::new();
        if after < evicted {
            #[cfg(test)]
            self.cold_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut dropped = false;
            for index in 0..=MAX_ROTATED {
                let Some(path) = path.as_ref() else { break };
                let file = match index {
                    0 => path.clone(),
                    index => rotated_path(path, index),
                };
                dropped |= index == MAX_ROTATED && file.exists();
                let Ok((events, _)) = read_log_file(&file) else {
                    dropped = true;
                    break;
                };
                let reaches = events.first().is_some_and(|event| event.seq <= after + 1);
                disk.extend(events.into_iter().map(|event| (event.seq, event.envelope)));
                if reaches {
                    dropped = false;
                    break;
                }
            }
            disk.retain(|(seq, _)| *seq > after);
            disk.sort_by_key(|(seq, _)| *seq);
            disk.dedup_by_key(|(seq, _)| *seq);
            // `evicted` is a durable event that was pushed before the files
            // were read, so finding nothing past the cursor means it is gone.
            floor = match disk.first() {
                None => Some(evicted + 1),
                Some((first, _)) => dropped.then_some(*first),
            };
            #[cfg(test)]
            if let Some(mut hook) = self.disk_read_hook.lock().ok().and_then(|mut h| h.take()) {
                hook();
            }
        }
        let reached = disk
            .last()
            .map(|(seq, _)| *seq)
            .into_iter()
            .chain(floor.map(|floor| floor - 1))
            .fold(after, u64::max);
        let Ok(state) = self.inner.lock() else {
            return EventRead::default();
        };
        // The ring continues the disk read only if it has not evicted a
        // durable event past it meanwhile. If it has, that event is on disk
        // for the next call.
        let ring_joins = state.evicted_durable <= reached;
        let mut scanned = reached;
        if ring_joins {
            // A rotation can delete a file whose events the ring still
            // holds, so the ring can move the retention floor down.
            let ring_floor = state
                .events
                .iter()
                .find(|(seq, event)| *seq > after && event.event.is_persisted())
                .map(|(seq, _)| *seq);
            floor = floor.map(|floor| ring_floor.map_or(floor, |ring| floor.min(ring)));
            disk.extend(state.events.iter().filter(|(seq, _)| *seq > after).cloned());
            disk.sort_by_key(|(seq, _)| *seq);
            disk.dedup_by_key(|(seq, _)| *seq);
            scanned = scanned.max(state.next_sequence);
        }
        let durable = state.durable_sequence;
        drop(state);
        let gap = floor
            .filter(|floor| *floor > after + 1)
            .map(|floor| (after + 1, floor - 1));
        let mut events = Vec::new();
        for (seq, envelope) in disk {
            if !envelope.event.is_persisted() || !keep(&envelope) {
                continue;
            }
            if events.len() == limit {
                scanned = events.last().map(|(seq, _)| *seq).unwrap_or(after);
                break;
            }
            events.push((seq, envelope));
        }
        EventRead {
            gap,
            events,
            next_after: scanned.min(durable),
            scanned,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_ring_cap(&self, cap: usize) {
        if let Ok(mut state) = self.inner.lock() {
            state.ring_cap = Some(cap);
        }
    }

    /// The newest durable sequence: the furthest cursor a reader may hold.
    pub(crate) fn durable_sequence(&self) -> u64 {
        self.inner
            .lock()
            .map(|state| state.durable_sequence)
            .unwrap_or(0)
    }

    /// `[events] read_max_wait_ms`, applied at start and on config reload.
    pub(crate) fn set_read_max_wait_ms(&self, ms: u64) {
        self.read_max_wait_ms
            .0
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn read_max_wait_ms(&self) -> u64 {
        self.read_max_wait_ms
            .0
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn cold_read_count(&self) -> usize {
        self.cold_reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Stream persisted history (rotated files + active) with `seq >
    /// sequence`, oldest first. COLD PATH: re-reads every log file per call
    /// — fine for one-shot queries like lineage, wrong for polling. Reads from disk, not the in-memory ring, so
    /// it sees events from before this process started. Used by the lineage
    /// walker; an unreadable file contributes nothing rather than failing
    /// the whole read.
    pub fn persisted_events_after(&self, sequence: u64) -> Vec<(u64, u64, EventEnvelope)> {
        #[cfg(test)]
        self.cold_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Parse outside `inner`: a cold read covers up to six files, and
        // `push` on the App loop must not queue behind it. Only the file
        // names and the rotation count are taken under the lock. A rotation
        // during the read renames files under it, so the read is retried, and
        // the last attempt holds the lock throughout.
        let rotations = |hub: &Self| {
            hub.inner
                .lock()
                .ok()
                .and_then(|state| state.log.as_ref().map(|log| log.rotations))
        };
        let mut restored: Vec<PersistedEvent> = Vec::new();
        for attempt in 0..3 {
            let Ok(state) = self.inner.lock() else {
                return Vec::new();
            };
            let Some(log) = state.log.as_ref() else {
                return Vec::new();
            };
            let before = log.rotations;
            let mut paths: Vec<PathBuf> = (1..=MAX_ROTATED)
                .rev()
                .map(|index| log.rotated_path(index))
                .collect();
            paths.push(log.path.clone());
            let held = (attempt == 2).then_some(state);
            restored.clear();
            for path in &paths {
                match read_log_file(path) {
                    Ok((events, _)) => restored.extend(events),
                    Err(err) => crate::logging::event_log_corrupt(&err),
                }
            }
            if held.is_some() {
                break;
            }
            if rotations(self) == Some(before) {
                break;
            }
        }
        restored.sort_by_key(|event| event.seq);
        restored.dedup_by_key(|event| event.seq);
        restored
            .into_iter()
            .filter(|event| event.seq > sequence)
            .map(|event| (event.seq, event.ts_ms, event.envelope))
            .collect()
    }
}

#[cfg(test)]
mod goal_storage_bench;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::EventData;
    use std::sync::{Arc, Mutex};

    fn temp_log(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir()
            .join(format!(
                "flock-event-log-{name}-{}-{nanos}",
                std::process::id()
            ))
            .join("event-log.jsonl")
    }

    fn workspace_event(label: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id: label.to_string(),
            },
        }
    }

    fn output_event() -> EventEnvelope {
        EventEnvelope {
            event: EventKind::PaneOutputChanged,
            data: EventData::PaneOutputChanged {
                pane_id: "w1:p1".into(),
                workspace_id: "w1".into(),
                revision: 1,
            },
        }
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    /// #438: a waiter wakes on the push itself, and a wait that finds the
    /// hub already past its cursor returns without sleeping at all.
    #[test]
    fn wait_after_wakes_on_push_and_times_out_without_one() {
        let hub = EventHub::default();
        let cursor = hub.current_sequence();
        assert!(!hub.wait_after(cursor, std::time::Duration::from_millis(10)));

        let pusher = hub.clone();
        let started = std::time::Instant::now();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            pusher.push(workspace_event("w"));
        });
        assert!(hub.wait_after(cursor, std::time::Duration::from_secs(10)));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        handle.join().unwrap();
        assert!(hub.wait_after(cursor, std::time::Duration::ZERO));
    }

    #[test]
    fn persisted_stream_is_prefix_consistent_across_any_restart_point() {
        // #175 §8.2: for any restart point, the persisted stream stays
        // prefix-consistent with strictly increasing sequences and no gaps,
        // and new pushes continue past the previous maximum.
        for restart_after in 0..=12u64 {
            let path = temp_log(&format!("prefix-{restart_after}"));
            let hub = EventHub::with_persistence(path.clone());
            for index in 0..restart_after {
                // Mix in the non-persisted kind: it consumes a sequence but
                // never reaches disk.
                if index % 3 == 2 {
                    hub.push(output_event());
                } else {
                    hub.push(workspace_event(&format!("w{index}")));
                }
            }
            let sequence_before = hub.current_sequence();
            drop(hub);

            let restarted = EventHub::with_persistence(path.clone());
            assert!(
                restarted.current_sequence() <= sequence_before,
                "restart may not invent sequences"
            );
            let persisted = restarted.persisted_events_after(0);
            let mut previous = 0;
            for (seq, _, _) in &persisted {
                assert!(*seq > previous, "sequences strictly increase");
                previous = *seq;
            }
            restarted.push(workspace_event("after-restart"));
            assert!(
                restarted.current_sequence() > previous,
                "new pushes continue past the persisted maximum"
            );
            let last = restarted.persisted_events_after(previous);
            assert_eq!(last.len(), 1, "exactly the new event after the max");
            cleanup(&path);
        }
    }

    /// ADR-0028 §3a: a kind from a newer release is skipped, not read as
    /// corruption, and its sequence stays spent.
    #[test]
    fn an_unknown_kind_is_skipped_without_quarantining_the_log() {
        let path = temp_log("unknown-kind");
        let hub = EventHub::with_persistence(path.clone());
        hub.push(workspace_event("w1"));
        drop(hub);
        {
            use std::io::Write;
            let mut file = std::fs::File::options().append(true).open(&path).unwrap();
            file.write_all(
                b"{\"seq\":2,\"ts_ms\":1,\"envelope\":{\"event\":\"pr.from_the_future\",\"data\":{\"type\":\"pr.from_the_future\"}}}\n",
            )
            .unwrap();
        }
        let restarted = EventHub::with_persistence(path.clone());
        assert!(
            !path.with_file_name("event-log.corrupt.jsonl").exists(),
            "an unknown kind must not quarantine the log"
        );
        assert_eq!(restarted.persisted_events_after(0).len(), 1);
        restarted.push(workspace_event("w2"));
        let seqs: Vec<u64> = restarted
            .persisted_events_after(0)
            .into_iter()
            .map(|(seq, _, _)| seq)
            .collect();
        assert_eq!(seqs, vec![1, 3], "seq 2 stays spent");
        cleanup(&path);
    }

    /// #1027: the derived agent kinds are new to v2.0.0, whose reader skips
    /// a line when `envelope.event` is a string it cannot type and `seq` is a
    /// number (#1058). Every line this build writes for them has that shape,
    /// under a name v2.0.0 does not know, and reads back whole here.
    #[test]
    fn the_derived_agent_kinds_are_lines_an_older_reader_skips() {
        use crate::api::agent_state::{AgentState, Phase};
        use crate::api::schema::{AgentPhaseEvent, EventData};
        let path = temp_log("derived-agent-kinds");
        let hub = EventHub::with_persistence(path.clone());
        let event = AgentPhaseEvent {
            agent_id: "agent_a".into(),
            pane: "w1:p1".into(),
            from: Phase::Working,
            to: Phase::NeedsHuman,
            state: AgentState::unknown(),
            held_ms: 20_000,
        };
        let kinds = [
            (
                EventKind::AgentPhaseChanged,
                EventData::AgentPhaseChanged(event.clone()),
            ),
            (
                EventKind::AgentNeedsHuman,
                EventData::AgentNeedsHuman(event.clone()),
            ),
            (
                EventKind::AgentLimitReached,
                EventData::AgentLimitReached(event.clone()),
            ),
            (EventKind::AgentGone, EventData::AgentGone(event)),
        ];
        for (kind, data) in kinds.clone() {
            hub.push(EventEnvelope { event: kind, data });
        }
        drop(hub);
        let names: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                assert!(value["seq"].is_u64(), "{line}");
                value["envelope"]["event"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            names,
            [
                "agent.phase_changed",
                "agent.needs_human",
                "agent.limit_reached",
                "agent.gone"
            ]
        );
        // Kind names v2.0.0's `EventKind` has with the `agent_` prefix.
        let known_to_v2_0_0 = [
            "agent_forked",
            "agent_delegated",
            "agent_removed",
            "agent_hibernated",
            "agent_resumed_from_hibernation",
            "agent_restart",
        ];
        assert!(names
            .iter()
            .all(|name| !known_to_v2_0_0.contains(&name.as_str())));
        let restarted = EventHub::with_persistence(path.clone());
        let back: Vec<EventEnvelope> = restarted
            .persisted_events_after(0)
            .into_iter()
            .map(|(_, _, envelope)| envelope)
            .collect();
        let written: Vec<EventEnvelope> = kinds
            .into_iter()
            .map(|(event, data)| EventEnvelope { event, data })
            .collect();
        assert_eq!(back, written);
        cleanup(&path);
    }

    /// Skipping is for kinds this binary does not know. A known kind with
    /// malformed data is corruption, and must still be caught.
    #[test]
    fn a_known_kind_with_malformed_data_is_still_corrupt() {
        let path = temp_log("known-kind-malformed");
        let hub = EventHub::with_persistence(path.clone());
        hub.push(workspace_event("w1"));
        drop(hub);
        {
            use std::io::Write;
            let mut file = std::fs::File::options().append(true).open(&path).unwrap();
            file.write_all(
                b"{\"seq\":2,\"ts_ms\":1,\"envelope\":{\"event\":\"workspace_closed\",\"data\":{\"type\":\"workspace_closed\"}}}\n",
            )
            .unwrap();
        }
        let _restarted = EventHub::with_persistence(path.clone());
        assert!(
            path.with_file_name("event-log.corrupt.jsonl").exists(),
            "a known kind that fails to parse must be quarantined, not skipped"
        );
        cleanup(&path);
    }

    #[test]
    fn torn_tail_is_dropped_and_sequencing_continues() {
        // #175 §8.4: kill mid-write ⇒ the torn line vanishes, the prefix
        // survives, and the next push continues the sequence.
        let path = temp_log("torn");
        let hub = EventHub::with_persistence(path.clone());
        hub.push(workspace_event("w1"));
        hub.push(workspace_event("w2"));
        drop(hub);
        {
            use std::io::Write;
            let mut file = std::fs::File::options().append(true).open(&path).unwrap();
            file.write_all(b"{\"seq\":3,\"ts_ms\":1,\"envel").unwrap();
        }
        let truncated_len = std::fs::metadata(&path).unwrap().len();
        let restarted = EventHub::with_persistence(path.clone());
        assert!(
            std::fs::metadata(&path).unwrap().len() < truncated_len,
            "load must physically truncate the torn tail before any append"
        );
        assert_eq!(restarted.current_sequence(), 2, "torn tail dropped");
        assert_eq!(
            restarted.events_after(0).len(),
            2,
            "ring is seeded from disk at boot"
        );
        restarted.push(workspace_event("w3"));
        let persisted = restarted.persisted_events_after(0);
        assert_eq!(
            persisted.iter().map(|(seq, _, _)| *seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        cleanup(&path);
    }

    #[test]
    fn mid_file_corruption_rejects_the_file_loudly_but_leaves_it_on_disk() {
        // #175 §8.4 + P4: an interior corrupt line is a gap — the file is
        // rejected (empty restore) but never deleted or rewritten.
        let path = temp_log("corrupt");
        let hub = EventHub::with_persistence(path.clone());
        hub.push(workspace_event("w1"));
        hub.push(workspace_event("w2"));
        drop(hub);
        let original = std::fs::read_to_string(&path).unwrap();
        let corrupted = original.replacen("\"seq\":1", "\"seq\":garbage", 1);
        std::fs::write(&path, &corrupted).unwrap();

        let restarted = EventHub::with_persistence(path.clone());
        assert_eq!(
            restarted.persisted_events_after(0).len(),
            0,
            "corrupt file contributes nothing"
        );
        let quarantined = path.with_file_name("event-log.corrupt.jsonl");
        assert_eq!(
            std::fs::read_to_string(&quarantined).unwrap(),
            corrupted,
            "corrupt log quarantined with content intact, never deleted"
        );
        // The fresh active file accepts and serves new events.
        restarted.push(workspace_event("after-quarantine"));
        assert_eq!(restarted.persisted_events_after(0).len(), 1);
        cleanup(&path);
    }

    #[test]
    fn quarantining_a_corrupt_active_file_never_re_uses_its_sequences() {
        // #175 O1: sequences are monotonic ACROSS RESTARTS. The quarantine
        // path is the one place that can falsify it — the corrupt file's
        // events never land in `restored`, so a naive high-water mark is
        // computed as if they had never existed and the next push hands out
        // a number that is already spent. Nothing local reads by offset yet,
        // so this regression is silent: it surfaces only once a consumer
        // resumes from a cursor and skips or replays a whole range.
        let path = temp_log("seq-reuse");
        let hub = EventHub::with_persistence(path.clone());
        hub.push(workspace_event("w1"));
        hub.push(workspace_event("w2"));
        hub.push(workspace_event("w3"));
        let spent = hub
            .persisted_events_after(0)
            .last()
            .map(|(seq, _, _)| *seq)
            .expect("three events persisted");
        drop(hub);

        // Corrupt the FIRST line: the later ones stay parseable, so their
        // sequences are recoverable even though the file is rejected whole.
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, original.replacen("\"seq\":1", "\"seq\":garbage", 1)).unwrap();

        let restarted = EventHub::with_persistence(path.clone());
        assert_eq!(
            restarted.persisted_events_after(0).len(),
            0,
            "quarantined file still contributes no events to the restore"
        );
        restarted.push(workspace_event("after-quarantine"));
        let next = restarted
            .persisted_events_after(0)
            .last()
            .map(|(seq, _, _)| *seq)
            .expect("the post-quarantine push persisted");
        assert!(
            next > spent,
            "seq {next} re-uses a number the quarantined file already spent (highest was {spent})"
        );
        cleanup(&path);
    }

    #[test]
    fn a_totally_unreadable_active_file_still_falls_back_to_rotated_history() {
        // Salvage is best-effort by design: when not one line survives there
        // is no high-water mark to recover, and the rotated files are the
        // only remaining floor. Monotonicity must still hold against those.
        let path = temp_log("seq-unsalvageable");
        let hub = EventHub::with_persistence(path.clone());
        for index in 0..3 {
            hub.push(workspace_event(&format!("w{index}")));
        }
        drop(hub);
        std::fs::write(&path, "\0\0\0 not json at all\n").unwrap();

        let restarted = EventHub::with_persistence(path.clone());
        restarted.push(workspace_event("after"));
        let next = restarted
            .persisted_events_after(0)
            .last()
            .map(|(seq, _, _)| *seq)
            .expect("push persisted");
        assert!(next > 0, "sequence still advances from a zero floor");
        cleanup(&path);
    }

    #[test]
    fn non_persisted_kinds_never_reach_disk_but_ride_the_ring() {
        let path = temp_log("filter");
        let hub = EventHub::with_persistence(path.clone());
        hub.push(output_event());
        hub.push(workspace_event("w1"));
        assert_eq!(hub.events_after(0).len(), 2, "ring sees both");
        let persisted = hub.persisted_events_after(0);
        assert_eq!(persisted.len(), 1, "disk sees only the persisted kind");
        assert_eq!(persisted[0].0, 2, "persisted event keeps its ring seq");
        cleanup(&path);
    }

    /// Drain `read` from `after` to the head the way a consumer would:
    /// every seq seen, plus every range a gap reported.
    fn drain(hub: &EventHub, mut after: u64, limit: usize) -> (Vec<u64>, Vec<(u64, u64)>, u64) {
        let (mut seen, mut gaps) = (Vec::new(), Vec::new());
        loop {
            let read = hub.read(after, limit, |_| true);
            gaps.extend(read.gap);
            seen.extend(read.events.iter().map(|(seq, _)| *seq));
            if read.events.is_empty() && read.gap.is_none() {
                return (seen, gaps, after);
            }
            assert!(read.next_after >= after, "a cursor never moves back");
            after = read.next_after;
        }
    }

    /// Arm a one-shot hook that runs inside the next disk read: `durable`
    /// durable pushes, `churn` ephemeral ones, then a rotation if asked.
    fn interleave(
        hub: &EventHub,
        pushed: &Arc<Mutex<Vec<u64>>>,
        durable: u64,
        churn: u64,
        rotate: bool,
    ) {
        let (target, pushed) = (hub.clone(), pushed.clone());
        *hub.disk_read_hook.lock().unwrap() = Some(Box::new(move || {
            for _ in 0..durable {
                target.push(workspace_event("raced"));
                pushed.lock().unwrap().push(target.current_sequence());
            }
            for _ in 0..churn {
                target.push(output_event());
            }
            if rotate {
                let mut state = target.inner.lock().unwrap();
                state.log.as_mut().unwrap().rotate().unwrap();
            }
        }));
    }

    /// Review of #1056: a durable event pushed and evicted from the ring
    /// while a reader is on disk must reach that reader on a later call.
    #[test]
    fn a_durable_event_evicted_during_the_disk_read_is_not_skipped() {
        let path = temp_log("read-race");
        let hub = EventHub::with_persistence(path.clone());
        hub.inner.lock().unwrap().ring_cap = Some(2);
        for index in 0..5 {
            hub.push(workspace_event(&format!("w{index}")));
        }
        let pushed = Arc::new(Mutex::new(Vec::new()));
        interleave(&hub, &pushed, 1, 3, false);
        let (seen, gaps, _) = drain(&hub, 0, 10);
        assert_eq!(seen, [1, 2, 3, 4, 5, 6]);
        assert!(gaps.is_empty(), "{gaps:?}");
        cleanup(&path);
    }

    /// Review of #1056: a reader at the durable head stays off the disk
    /// however much ephemeral output pushes the ring along.
    #[test]
    fn an_idle_reader_at_the_head_never_reads_disk() {
        let path = temp_log("read-idle");
        let hub = EventHub::with_persistence(path.clone());
        hub.inner.lock().unwrap().ring_cap = Some(2);
        hub.push(workspace_event("w"));
        for _ in 0..10 {
            hub.push(output_event());
            let read = hub.read(1, 10, |_| true);
            assert!(read.events.is_empty() && read.gap.is_none());
            assert_eq!(read.next_after, 1);
        }
        assert_eq!(hub.cold_read_count(), 0);
        cleanup(&path);
    }

    /// #1028 / ADR-0028 §3: across random rotations, restarts, ephemeral
    /// kinds, and pushes or rotations landing in the middle of a disk read,
    /// a reader resuming from any cursor sees every durable event or a gap
    /// that names it. Silent loss fails the test.
    #[test]
    fn read_is_at_least_once_or_an_explicit_gap_across_rotations_and_restarts() {
        // Seeds run in parallel: the per-push fsync dominates the runtime.
        std::thread::scope(|scope| {
            for seed in 1..=32u64 {
                scope.spawn(move || {
                    let path = temp_log(&format!("read-{seed}"));
                    let mut rng = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
                    let mut next = |bound: u64| {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        rng % bound
                    };
                    let ring_cap = Some(2 + next(8) as usize);
                    let mut hub = EventHub::with_persistence(path.clone());
                    hub.inner.lock().unwrap().ring_cap = ring_cap;
                    let durable = Arc::new(Mutex::new(Vec::new()));
                    let (mut cursor, mut seen, mut gaps) = (0, Vec::new(), Vec::new());
                    for step in 0..120 {
                        match next(20) {
                            0..=10 => {
                                hub.push(workspace_event(&format!("w{step}")));
                                durable.lock().unwrap().push(hub.current_sequence());
                            }
                            11..=13 => hub.push(output_event()),
                            14..=15 => {
                                let mut state = hub.inner.lock().unwrap();
                                state.log.as_mut().unwrap().rotate().unwrap();
                            }
                            16 => {
                                drop(hub);
                                hub = EventHub::with_persistence(path.clone());
                                hub.inner.lock().unwrap().ring_cap = ring_cap;
                            }
                            raced => {
                                if raced == 19 {
                                    interleave(&hub, &durable, next(3), next(12), next(2) == 0);
                                }
                                let (got, gap, resumed) = drain(&hub, cursor, 1 + next(4) as usize);
                                *hub.disk_read_hook.lock().unwrap() = None;
                                seen.extend(got);
                                gaps.extend(gap);
                                cursor = resumed;
                            }
                        }
                    }
                    let (got, gap, _) = drain(&hub, cursor, 3);
                    seen.extend(got);
                    gaps.extend(gap);
                    let covered = |seq: u64, seen: &[u64], gaps: &[(u64, u64)]| {
                        seen.contains(&seq)
                            || gaps.iter().any(|(from, to)| (*from..=*to).contains(&seq))
                    };
                    let durable = durable.lock().unwrap().clone();
                    for seq in &durable {
                        assert!(
                            covered(*seq, &seen, &gaps),
                            "seed {seed}: {seq} lost silently"
                        );
                    }

                    // Retained is the disk plus the ring: back-to-back rotations can
                    // delete a file whose events the ring still holds.
                    let mut retained: Vec<u64> = hub
                        .persisted_events_after(0)
                        .iter()
                        .map(|(seq, _, _)| *seq)
                        .chain(
                            hub.events_after(0)
                                .iter()
                                .filter(|(_, e)| e.event.is_persisted())
                                .map(|(seq, _)| *seq),
                        )
                        .collect();
                    retained.sort_unstable();
                    retained.dedup();
                    for start in 0..=hub.current_sequence() {
                        let (got, gap, _) = drain(&hub, start, 8 + next(64) as usize);
                        let later: Vec<u64> =
                            durable.iter().copied().filter(|s| *s > start).collect();
                        for seq in &later {
                            assert!(
                                covered(*seq, &got, &gap),
                                "seed {seed} from {start}: {seq} lost"
                            );
                        }
                        if later.iter().any(|seq| !retained.contains(seq)) {
                            assert!(!gap.is_empty(), "seed {seed} from {start}: past retention");
                        }
                        if retained.first().is_none_or(|first| start + 1 >= *first) {
                            assert!(gap.is_empty(), "seed {seed} from {start}: inside retention");
                        }
                        for seq in &retained {
                            assert!(
                                !gap.iter().any(|(from, to)| (*from..=*to).contains(seq)),
                                "seed {seed} from {start}: a gap hides retained {seq}"
                            );
                        }
                    }
                    cleanup(&path);
                });
            }
        });
    }

    /// A restart resumes numbering from the newest durable event, so the
    /// sequence of an ephemeral event after it is handed out again. A cursor
    /// that pointed at it would skip the durable event that reuses it.
    #[test]
    fn read_cursor_never_lands_on_a_sequence_a_restart_reuses() {
        let path = temp_log("read-reuse");
        let hub = EventHub::with_persistence(path.clone());
        hub.push(workspace_event("w1"));
        hub.push(output_event());
        let cursor = hub.read(0, 10, |_| true).next_after;
        assert_eq!(cursor, 1);
        drop(hub);
        let hub = EventHub::with_persistence(path.clone());
        hub.push(workspace_event("w2"));
        let read = hub.read(cursor, 10, |_| true);
        assert_eq!(
            read.events.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            [2]
        );
        cleanup(&path);
    }

    #[test]
    fn rotation_preserves_history_across_files() {
        let path = temp_log("rotate");
        let hub = EventHub::with_persistence(path.clone());
        {
            let mut state = hub.inner.lock().unwrap();
            // Force tiny rotation bounds via the event counter: pretend the
            // active file is one event away from the cap.
            if let Some(log) = state.log.as_mut() {
                log.active_events = MAX_ACTIVE_EVENTS - 2;
            }
        }
        for index in 0..4 {
            hub.push(workspace_event(&format!("w{index}")));
        }
        assert!(
            path.with_file_name("event-log.1.jsonl").exists(),
            "rotation happened"
        );
        let persisted = hub.persisted_events_after(0);
        assert_eq!(
            persisted.iter().map(|(seq, _, _)| *seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4],
            "reads span rotated + active files"
        );
        let restarted = EventHub::with_persistence(path.clone());
        assert_eq!(restarted.current_sequence(), 4);
        cleanup(&path);
    }
}
