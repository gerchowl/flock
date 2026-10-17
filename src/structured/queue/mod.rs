//! Worker-owned queue journal (ADR-0036 §3/§4). Never call on the server loop.
//!
//! One non-cloneable writer holds a stable sidecar OS lock across rename and
//! handoff. Close/drop it before handing ownership to another process. Appends
//! and checkpoint publication require &mut self; replay returns an owned cut.
//! A failed write/sync/rename poisons the handle: reopen under the lease to learn
//! which bytes committed. No success or in-memory change precedes durability.
//! Threshold configuration and worker/event-loop wiring belong to #1803.
#![deny(missing_docs)]
pub mod model;
use crate::platform::structured as private_fs;
pub use model::{Change, Item, Record};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

const LINE_LIMIT: u64 = 2 * 1024 * 1024;

/// Snapshot gap followed inline by the retained records (ADR-0036 §4).
#[derive(Debug, PartialEq)]
pub struct Gap {
    /// Requested cursor plus one.
    pub from: u64,
    /// Last sequence before replacement.
    pub to: u64,
    /// Last snapshot sequence, including an empty snapshot's high-water.
    pub snapshot_to: u64,
}
/// Immutable replay cut; subsequent mutations cannot interleave its records.
#[derive(Debug)]
pub struct Cut {
    /// Present when the requested history was replaced by a checkpoint.
    pub gap: Option<Gap>,
    /// Snapshot and subsequent records, retaining opaque future kinds.
    pub records: Vec<Value>,
    /// Captured barrier for synced, independent of later appends.
    pub through: u64,
}
/// Blocking storage owned by one dedicated worker, not AppState or render code.
pub struct Journal {
    path: PathBuf,
    agent: String,
    directory: File,
    file: File,
    items: Vec<Item>,
    records: Vec<Value>,
    seq: u64,
    checkpoint: Option<(u64, u64)>,
    future: bool,
    poisoned: bool,
    _lease: File,
}
fn problem(message: &str) -> io::Error {
    io::Error::other(message)
}
fn open_file(path: &Path) -> io::Result<File> {
    match private_fs::create_private_file(path) {
        Ok(file) => file.sync_all()?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    private_fs::open_append_file(path)
}
fn encode(value: &impl serde::Serialize) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > LINE_LIMIT {
        return Err(problem("queue record exceeds 2 MiB"));
    }
    Ok(bytes)
}
impl Journal {
    /// Acquire exclusive queue/<agent> ownership and recover committed state.
    /// Components are ASCII identifiers, never paths. Uppercase letters use an
    /// injective, case-insensitive-safe encoding; the journal verifies the original
    /// id. Root aliases resolve, but
    /// linked/private-namespace violations fail. Malformed complete lines or
    /// non-dense sequences fail; only an unterminated final tail is repaired.
    pub fn open(data: &Path, agent: &str) -> io::Result<Self> {
        if agent.is_empty()
            || !agent
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(problem("invalid queue agent component"));
        }
        let root = data.canonicalize()?;
        let sessions = root.join("sessions");
        private_fs::ensure_private_dir(&sessions)?;
        let component: String = agent
            .chars()
            .map(|c| {
                if c.is_ascii_uppercase() {
                    format!("={}", c.to_ascii_lowercase())
                } else {
                    c.to_string()
                }
            })
            .collect();
        let dir = sessions.join(component);
        private_fs::ensure_private_dir(&dir)?;
        let lease = open_file(&dir.join(".queue.lock"))?;
        lease.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
            fs::TryLockError::Error(error) => error,
        })?;
        let path = dir.join("queue.jsonl");
        let file = open_file(&path)?;
        for path in [&root, &sessions, &dir] {
            File::open(path)?.sync_all()?;
        }
        let mut journal = Self {
            path,
            agent: agent.to_owned(),
            directory: File::open(dir)?,
            file,
            items: Vec::new(),
            records: Vec::new(),
            seq: 0,
            checkpoint: None,
            future: false,
            poisoned: false,
            _lease: lease,
        };
        journal.recover()?;
        for entry in fs::read_dir(
            journal
                .path
                .parent()
                .ok_or_else(|| problem("missing queue directory"))?,
        )? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".queue-") && name.ends_with(".next") {
                fs::remove_file(entry.path())?;
            }
        }
        journal.directory.sync_all()?;
        Ok(journal)
    }
    /// Current materialized state, in queue order; contains no runtime handles.
    pub fn items(&self) -> &[Item] {
        &self.items
    }
    /// Committed encoded size for the worker's configured rewrite threshold.
    pub fn bytes(&self) -> io::Result<u64> {
        self.healthy()?;
        Ok(self.file.metadata()?.len())
    }
    fn healthy(&self) -> io::Result<()> {
        if self.poisoned {
            Err(problem("queue durability uncertain; reopen required"))
        } else {
            Ok(())
        }
    }
    fn durable<T>(&mut self, operation: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        self.healthy()?;
        let result = operation(self);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    /// Append and sync before folding state or publishing the returned record.
    /// Validation failures do not write or spend a sequence; I/O failures poison.
    pub fn append(&mut self, change: Change) -> io::Result<Record> {
        self.healthy()?;
        let mut items = self.items.clone();
        change.apply(&mut items)?;
        let record = Record {
            seq: self
                .seq
                .checked_add(1)
                .ok_or_else(|| problem("queue sequence exhausted"))?,
            change,
        };
        let value = serde_json::to_value(&record)?;
        let bytes = encode(&value)?;
        self.durable(|this| {
            this.file.write_all(&bytes)?;
            this.file.sync_data()
        })?;
        self.items = items;
        self.future |= record.change.has_unknown();
        self.seq = record.seq;
        self.records.push(value);
        Ok(record)
    }
    /// Capture replay under the writer's serialization point. New/empty queues
    /// publish an empty gap; a checkpoint gap always includes snapshot_to.
    pub fn replay(&self, after: u64) -> io::Result<Cut> {
        self.healthy()?;
        if after > self.seq {
            return Err(problem("queue cursor ahead"));
        }
        let from = after
            .checked_add(1)
            .ok_or_else(|| problem("queue sequence exhausted"))?;
        let gap = self
            .checkpoint
            .filter(|(before, _)| after <= *before)
            .or_else(|| (self.seq == 0).then_some((0, 0)))
            .map(|(to, snapshot_to)| Gap {
                from,
                to,
                snapshot_to,
            });
        let records = self
            .records
            .iter()
            .filter(|v| v["seq"].as_u64().is_some_and(|seq| seq > after))
            .cloned()
            .collect();
        Ok(Cut {
            gap,
            records,
            through: self.seq,
        })
    }
    /// Sync a replacement, rename, then sync its directory before publishing.
    /// Header `before` records H as well as high_water=H+N, allowing recovery to
    /// reject a truncated snapshot. Unknown kinds, fields or enum variants block
    /// rewriting rather than discard future state. Failed replacements can leave
    /// unique .next files; open removes these under the exclusive lease.
    pub fn checkpoint(&mut self) -> io::Result<Cut> {
        self.healthy()?;
        if self.future {
            return Err(problem("future queue data prevent checkpoint"));
        }
        let before = self.seq;
        before
            .checked_add(1)
            .ok_or_else(|| problem("queue sequence exhausted"))?;
        let high = before
            .checked_add(self.items.len() as u64)
            .ok_or_else(|| problem("queue sequence exhausted"))?;
        let records: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .map(|(position, item)| {
                serde_json::to_value(Record {
                    seq: before + position as u64 + 1,
                    change: Change::Upsert {
                        item: Box::new(item.clone()),
                        position,
                    },
                })
            })
            .collect::<Result<_, _>>()?;
        let mut bytes = encode(&self.identity())?;
        bytes.extend(encode(
            &json!({"kind":"queue_checkpoint","before":before,"high_water":high}),
        )?);
        for record in &records {
            bytes.extend(encode(record)?);
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let temporary = self.path.with_file_name(format!(".queue-{name}.next"));
        self.durable(|this| {
            let mut file = private_fs::create_private_file(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &this.path)?;
            this.directory.sync_all()?;
            this.file = private_fs::open_append_file(&this.path)?;
            Ok(())
        })?;
        self.seq = high;
        self.records = records;
        self.checkpoint = Some((before, high));
        self.replay(before)
    }
    fn identity(&self) -> Value {
        json!({"kind":"queue_identity", "agent_id":self.agent})
    }
    fn recover(&mut self) -> io::Result<()> {
        // A dead writer may have written complete lines without syncing them.
        // Make those bytes durable before publishing any recovered sequence.
        self.file.sync_all()?;
        if self.file.metadata()?.len() == 0 {
            self.file.write_all(&encode(&self.identity())?)?;
            self.file.sync_all()?;
        }
        self.file.rewind()?;
        let mut reader = BufReader::new(&self.file);
        let mut good = 0;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = Read::by_ref(&mut reader)
                .take(LINE_LIMIT + 1)
                .read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            if n as u64 > LINE_LIMIT {
                return Err(problem("oversize queue record"));
            }
            if !line.ends_with(b"\n") {
                break;
            }
            let value: Value = serde_json::from_slice(&line)?;
            if good == 0 {
                if value["kind"] != "queue_identity" || value["agent_id"] != self.agent {
                    return Err(problem("missing or mismatched queue identity"));
                }
                self.future |= value != self.identity();
            } else if value["kind"] == "queue_identity" {
                return Err(problem("interior queue identity"));
            } else if value["kind"] == "queue_checkpoint" {
                if self.checkpoint.is_some() || !self.records.is_empty() {
                    return Err(problem("interior queue checkpoint"));
                }
                let before = value["before"]
                    .as_u64()
                    .ok_or_else(|| problem("invalid checkpoint before"))?;
                let high = value["high_water"]
                    .as_u64()
                    .filter(|n| *n >= before)
                    .ok_or_else(|| problem("invalid checkpoint high-water"))?;
                self.seq = before;
                self.checkpoint = Some((before, high));
            } else {
                let seq = value["seq"]
                    .as_u64()
                    .ok_or_else(|| problem("missing queue sequence"))?;
                if Some(seq) != self.seq.checked_add(1) {
                    return Err(problem("non-dense queue sequence"));
                }
                if matches!(value["kind"].as_str(), Some("upsert" | "remove")) {
                    let record = serde_json::from_value::<Record>(value.clone())?;
                    self.future |=
                        record.change.has_unknown() || serde_json::to_value(&record)? != value;
                    record.change.apply(&mut self.items)?;
                } else if value["kind"].is_string() {
                    self.future = true;
                } else {
                    return Err(problem("missing queue kind"));
                }
                self.seq = seq;
                self.records.push(value);
            }
            good += n as u64;
        }
        if good == 0 {
            // No complete first line means there can be no subsequent records.
            // The identity is only created in an empty file, so rewrite it safely.
            self.file.set_len(0)?;
            self.file.rewind()?;
            self.file.write_all(&encode(&self.identity())?)?;
            self.file.sync_all()?;
            tracing::warn!("repaired unterminated queue identity");
            return Ok(());
        }
        if self.checkpoint.is_some_and(|(_, high)| self.seq < high) {
            return Err(problem("incomplete queue checkpoint"));
        }
        if good < self.file.metadata()?.len() {
            self.file.set_len(good)?;
            self.file.sync_all()?;
            tracing::warn!("repaired unterminated queue tail");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
