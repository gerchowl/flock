use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::api::schema::EventEnvelope;

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
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
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
        })
    }

    fn rotated_path(&self, index: usize) -> PathBuf {
        let mut name = self
            .path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("event-log")
            .to_string();
        name.push_str(&format!(".{index}.jsonl"));
        self.path.with_file_name(name)
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
fn read_log_file(path: &Path) -> Result<Vec<PersistedEvent>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    let mut reader = std::io::BufReader::new(file);
    let mut events = Vec::new();
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
            Err(err) => return Err(format!("{}: corrupt line: {err}", path.display())),
        }
        if !complete {
            break;
        }
    }
    Ok(events)
}

impl EventHub {
    const MAX_EVENTS: usize = 512;

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
            for index in (1..=MAX_ROTATED).rev() {
                let mut name = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("event-log")
                    .to_string();
                name.push_str(&format!(".{index}.jsonl"));
                match read_log_file(&path.with_file_name(name)) {
                    Ok(events) => restored.extend(events),
                    Err(err) => crate::logging::event_log_corrupt(&err),
                }
            }
            match read_log_file(&path) {
                Ok(events) => restored.extend(events),
                Err(err) => {
                    // The active file is append-target: a corrupt one would
                    // swallow every future event on the next load. Quarantine
                    // it aside (rename, never delete — P4) and start fresh.
                    crate::logging::event_log_corrupt(&err);
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
            state.next_sequence = restored.last().map(|event| event.seq).unwrap_or(0);
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
        let overflow = state.events.len().saturating_sub(Self::MAX_EVENTS);
        if overflow > 0 {
            state.events.drain(0..overflow);
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

    pub fn current_sequence(&self) -> u64 {
        let Ok(state) = self.inner.lock() else {
            return 0;
        };
        state.next_sequence
    }

    /// Stream persisted history (rotated files + active) with `seq >
    /// sequence`, oldest first. COLD PATH: re-reads every log file per call
    /// — fine for one-shot queries like lineage, wrong for polling. Reads from disk, not the in-memory ring, so
    /// it sees events from before this process started. Used by the lineage
    /// walker; an unreadable file contributes nothing rather than failing
    /// the whole read.
    pub fn persisted_events_after(&self, sequence: u64) -> Vec<(u64, u64, EventEnvelope)> {
        let Ok(state) = self.inner.lock() else {
            return Vec::new();
        };
        let Some(log) = state.log.as_ref() else {
            return Vec::new();
        };
        let mut restored: Vec<PersistedEvent> = Vec::new();
        for index in (1..=MAX_ROTATED).rev() {
            match read_log_file(&log.rotated_path(index)) {
                Ok(events) => restored.extend(events),
                Err(err) => crate::logging::event_log_corrupt(&err),
            }
        }
        match read_log_file(&log.path) {
            Ok(events) => restored.extend(events),
            Err(err) => crate::logging::event_log_corrupt(&err),
        }
        restored.sort_by_key(|event| event.seq);
        restored
            .into_iter()
            .filter(|event| event.seq > sequence)
            .map(|event| (event.seq, event.ts_ms, event.envelope))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventData, EventKind};

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
