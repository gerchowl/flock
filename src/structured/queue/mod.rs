//! Worker-owned queue journal (ADR-0036 §3/§4). Never call on the server loop.
//!
//! One non-cloneable writer holds a stable sidecar OS lock across rename and
//! handoff. Close/drop it before handing ownership to another process. Appends
//! and checkpoint publication require &mut self; replay returns an owned cut.
//! A failed write/sync/rename poisons the handle: reopen under the lease to learn
//! which bytes committed. No success or in-memory change precedes durability.
//! Optional configured checkpoint policy is available through open_with_policy.
//! Production worker/event-loop wiring belongs to #1955 under #1803.
#![deny(missing_docs)]
pub(crate) mod admission;
pub mod model;
mod policy;
pub use policy::RewritePolicy;
mod readonly;
use readonly::corrupt;
pub use readonly::{QueueJournalCorrupt, QueueJournalNewer, ReadOnly};
use std::collections::HashSet;
pub(crate) mod scheduler;
use crate::platform::structured as private_fs;
pub use model::{Change, Item, Record};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

/// Journal format understood by this writer. Unstamped journals are version 1.
/// Bump for every new record kind, top-level field or enum variant name.
pub const FORMAT_VERSION: u64 = 2;

const LINE_LIMIT: u64 = 2 * 1024 * 1024;

/// Snapshot gap followed inline by the retained records (ADR-0036 §4).
#[derive(Debug, Clone, PartialEq)]
pub struct Gap {
    /// Requested cursor plus one.
    pub from: u64,
    /// Last sequence before replacement.
    pub to: u64,
    /// Last snapshot sequence, including an empty snapshot's high-water.
    pub snapshot_to: u64,
}
/// Immutable replay cut; subsequent mutations cannot interleave its records.
#[derive(Debug, Clone)]
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
    version: u64,
    future: bool,
    corrupt: Option<QueueJournalCorrupt>,
    opaque: HashSet<String>,
    poisoned: bool,
    rewrite: Option<RewritePolicy>,
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
fn track_opaque(change: &Change, unknown: bool, opaque: &mut HashSet<String>) {
    let items = match change {
        Change::Upsert { item, .. } => std::slice::from_ref(item.as_ref()),
        Change::Batch { items } => items.as_slice(),
        Change::Remove { item_id } => {
            opaque.remove(item_id);
            return;
        }
        Change::Fail { .. } => return,
    };
    for item in items {
        if unknown {
            opaque.insert(item.item_id.clone());
        } else {
            opaque.remove(&item.item_id);
        }
    }
}
impl Journal {
    /// Acquire exclusive `queue/<agent>` ownership and recover committed state.
    /// Components are ASCII identifiers, never paths. Uppercase letters use an
    /// injective, case-insensitive-safe encoding; the journal verifies the original
    /// id. Root aliases resolve, but linked/private-namespace violations fail.
    /// Newer versions, corrupt supported records and legacy unknown data that
    /// prevent upgrade open read-only without repair.
    /// Unknown data are logged and preserved raw, never interpreted for delivery.
    /// Only a supported journal's unterminated final tail is repaired.
    pub fn open(data: &Path, agent: &str) -> io::Result<Self> {
        Self::open_inner(data, agent, None, true)
    }
    fn open_inner(
        data: &Path,
        agent: &str,
        rewrite: Option<RewritePolicy>,
        create: bool,
    ) -> io::Result<Self> {
        if agent.is_empty()
            || !agent
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(problem("invalid queue agent component"));
        }
        let root = data.canonicalize()?;
        let sessions = root.join("sessions");
        let directory = if create {
            private_fs::ensure_private_dir
        } else {
            private_fs::check_private_dir
        };
        let open = if create {
            open_file
        } else {
            private_fs::open_append_file
        };
        directory(&sessions)?;
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
        directory(&dir)?;
        let lease = open(&dir.join(".queue.lock"))?;
        lease.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
            fs::TryLockError::Error(error) => error,
        })?;
        let path = dir.join("queue.jsonl");
        let file = open(&path)?;
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
            version: FORMAT_VERSION,
            future: false,
            corrupt: None,
            opaque: HashSet::new(),
            poisoned: false,
            rewrite,
            _lease: lease,
        };
        if let Err(error) = journal.recover() {
            journal.corrupt = error
                .get_ref()
                .and_then(|cause| cause.downcast_ref::<QueueJournalCorrupt>())
                .copied();
            if let Some(reason) = journal.corrupt {
                crate::logging::structured_queue_corrupt(&reason);
            } else {
                return Err(error);
            }
        }
        if journal.read_only().is_some() {
            return Ok(journal);
        }
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
    /// Read-only state is for inspection; opaque items retain positions but are
    /// never eligible for delivery.
    pub fn items(&self) -> &[Item] {
        &self.items
    }
    /// Committed encoded size for the worker's configured rewrite threshold.
    pub fn bytes(&self) -> io::Result<u64> {
        self.healthy()?;
        Ok(self.file.metadata()?.len())
    }
    /// Last valid format version; absence of a stamp means version 1.
    /// If the header is corrupt, inspect [`Self::read_only`] instead.
    pub fn format_version(&self) -> u64 {
        self.version
    }
    /// Typed read-only status. Reads remain available; mutations must not retry.
    pub fn read_only(&self) -> Option<ReadOnly> {
        self.corrupt.map(ReadOnly::Corrupt).or_else(|| {
            (self.version > FORMAT_VERSION).then_some(ReadOnly::Newer(QueueJournalNewer {
                found: self.version,
                supported: FORMAT_VERSION,
            }))
        })
    }
    fn writable(&self) -> io::Result<()> {
        self.healthy()?;
        if let Some(reason) = self.read_only() {
            return Err(reason.into());
        }
        Ok(())
    }
    fn healthy(&self) -> io::Result<()> {
        if self.poisoned {
            Err(problem("queue durability uncertain; reopen required"))
        } else {
            Ok(())
        }
    }
    fn durable<T>(&mut self, operation: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        self.writable()?;
        let result = operation(self);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    /// Append and sync before folding state or publishing the returned record.
    /// Validation failures do not write or spend a sequence; I/O failures poison.
    /// Read-only journals return [`QueueJournalNewer`] or [`QueueJournalCorrupt`]
    /// without writing. Older formats
    /// checkpoint to this writer's version before appending any new-format data.
    /// Publish replay from the previous cursor to include this upgrade's gap and
    /// snapshot, rather than assuming append always advances by exactly one.
    pub fn append(&mut self, change: Change) -> io::Result<Record> {
        self.writable()?;
        let mut items = self.items.clone();
        change.apply(&mut items)?;
        let needs_checkpoint = self.version < FORMAT_VERSION || self.rewrite_required()?;
        let snapshot = if needs_checkpoint {
            self.items.len() as u64
        } else {
            0
        };
        let record = Record {
            seq: self
                .seq
                .checked_add(snapshot)
                .and_then(|seq| seq.checked_add(1))
                .ok_or_else(|| problem("queue sequence exhausted"))?,
            change,
        };
        let value = serde_json::to_value(&record)?;
        let bytes = encode(&value)?;
        if needs_checkpoint {
            self.checkpoint()?;
        }
        self.durable(|this| {
            this.file.write_all(&bytes)?;
            this.file.sync_data()
        })?;
        self.items = items;
        track_opaque(
            &record.change,
            record.change.has_unknown(),
            &mut self.opaque,
        );
        self.future |= record.change.has_unknown();
        self.seq = record.seq;
        self.records.push(value);
        Ok(record)
    }
    /// Persist the first subscription's snapshot before serving it. Read-only
    /// storage returns its typed refusal without claiming a bootstrap succeeded.
    pub(crate) fn subscription(&mut self) -> io::Result<Cut> {
        if self.checkpoint.is_none() {
            self.checkpoint()?;
        }
        self.replay(0)
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
    /// If unknown data prevent a required rewrite, latch a typed read-only
    /// refusal before any more writes (ADR-0036 §4, owner decision #1922).
    pub fn checkpoint(&mut self) -> io::Result<Cut> {
        self.writable()?;
        if let Err(error) = self.checkpointable() {
            if let Some(reason) = self.corrupt {
                crate::logging::structured_queue_corrupt(&reason);
            }
            return Err(error);
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
        let mut bytes = encode(&self.identity(FORMAT_VERSION))?;
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
        self.version = FORMAT_VERSION;
        self.seq = high;
        self.records = records;
        self.checkpoint = Some((before, high));
        self.replay(before)
    }
    fn checkpointable(&mut self) -> io::Result<()> {
        if self.future {
            let reason = QueueJournalCorrupt {
                seq: Some(self.seq),
                offset: self.file.metadata()?.len(),
                reason: "unknown_data_prevents_checkpoint",
            };
            self.corrupt = Some(reason);
            return Err(io::Error::other(reason));
        }
        Ok(())
    }
    fn identity(&self, version: u64) -> Value {
        let mut header = json!({"kind":"queue_identity", "agent_id":self.agent});
        if version > 1 {
            header["format_version"] = json!(version);
        }
        header
    }
    fn recover(&mut self) -> io::Result<()> {
        // A dead writer may have written complete lines without syncing them.
        // Make those bytes durable before publishing any recovered sequence.
        self.file.sync_all()?;
        if self.file.metadata()?.len() == 0 {
            self.file
                .write_all(&encode(&self.identity(self.version))?)?;
            self.file.sync_all()?;
        }
        self.file.rewind()?;
        let mut reader = BufReader::new(&self.file);
        let mut good = 0;
        let mut line = Vec::new();
        loop {
            line.clear();
            let mut n = Read::by_ref(&mut reader)
                .take(LINE_LIMIT + 1)
                .read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            if n as u64 > LINE_LIMIT {
                while !line.ends_with(b"\n") {
                    line.clear();
                    let chunk = Read::by_ref(&mut reader)
                        .take(LINE_LIMIT + 1)
                        .read_until(b'\n', &mut line)?;
                    n += chunk;
                    if chunk == 0 {
                        break;
                    }
                }
                if self.version <= FORMAT_VERSION {
                    return Err(if good == 0 || !line.ends_with(b"\n") {
                        problem("oversize queue record")
                    } else {
                        corrupt(good, None, "oversized_record")
                    });
                }
                crate::logging::structured_queue_record_skipped(None, "oversized_record");
                good += n as u64;
                continue;
            }
            if !line.ends_with(b"\n") {
                break;
            }
            let value: Value = match serde_json::from_slice(&line) {
                Ok(value) => value,
                Err(_error) if good > 0 => {
                    if self.version <= FORMAT_VERSION {
                        return Err(corrupt(good, None, "invalid_json"));
                    }
                    self.future = true;
                    crate::logging::structured_queue_record_skipped(None, "invalid_json");
                    good += n as u64;
                    continue;
                }
                Err(_error) => return Err(corrupt(good, None, "invalid_json")),
            };
            if good == 0 {
                if value["kind"] != "queue_identity" || value["agent_id"] != self.agent {
                    return Err(problem("missing or mismatched queue identity"));
                }
                self.version = match value.get("format_version") {
                    None => 1,
                    Some(version) => version
                        .as_u64()
                        .filter(|v| *v > 0)
                        .ok_or_else(|| corrupt(0, None, "invalid_format_version"))?,
                };
                let mut expected = self.identity(self.version);
                if self.version == 1 && value.get("format_version").is_some() {
                    expected["format_version"] = json!(1);
                }
                self.future |= value != expected;
            } else if self.read_only().is_some() && value["kind"] == "queue_checkpoint" {
                let checkpoint = value["before"]
                    .as_u64()
                    .zip(value["high_water"].as_u64())
                    .filter(|(before, high)| high >= before);
                if let Some((before, high)) =
                    checkpoint.filter(|_| self.checkpoint.is_none() && self.records.is_empty())
                {
                    self.seq = self.seq.max(before);
                    self.checkpoint = Some((before, high));
                } else {
                    crate::logging::structured_queue_record_skipped(None, "unknown_checkpoint");
                }
            } else if value["kind"] == "queue_identity" && self.read_only().is_none() {
                return Err(corrupt(good, None, "interior_identity"));
            } else if value["kind"] == "queue_checkpoint" {
                if self.checkpoint.is_some() || !self.records.is_empty() {
                    return Err(corrupt(good, None, "interior_checkpoint"));
                }
                let before = value["before"]
                    .as_u64()
                    .ok_or_else(|| corrupt(good, None, "invalid_checkpoint_before"))?;
                let high = value["high_water"]
                    .as_u64()
                    .filter(|n| *n >= before)
                    .ok_or_else(|| corrupt(good, None, "invalid_checkpoint_high_water"))?;
                self.seq = before;
                self.checkpoint = Some((before, high));
            } else {
                let Some(seq) = value["seq"].as_u64() else {
                    if self.version <= FORMAT_VERSION {
                        return Err(corrupt(good, None, "missing_sequence"));
                    }
                    self.future = true;
                    crate::logging::structured_queue_record_skipped(None, "missing_sequence");
                    good += n as u64;
                    continue;
                };
                if self.read_only().is_none() && Some(seq) != self.seq.checked_add(1) {
                    return Err(corrupt(good, Some(seq), "non_dense_sequence"));
                }
                let known = matches!(
                    value["kind"].as_str(),
                    Some("upsert" | "remove" | "batch" | "fail")
                );
                let recovered = if known {
                    serde_json::from_value::<Record>(value.clone())
                        .map_err(io::Error::other)
                        .and_then(|record| {
                            let mut items = self.items.clone();
                            record.change.apply(&mut items)?;
                            let unknown = record.change.has_unknown()
                                || serde_json::to_value(&record)? != value;
                            self.future |= unknown;
                            track_opaque(&record.change, unknown, &mut self.opaque);
                            if unknown {
                                crate::logging::structured_queue_record_skipped(
                                    Some(seq),
                                    "unknown_data",
                                );
                            }
                            self.items = items;
                            Ok(())
                        })
                } else {
                    Err(problem("unknown queue kind"))
                };
                if recovered.is_err() {
                    if known && self.version <= FORMAT_VERSION {
                        return Err(corrupt(good, Some(seq), "malformed_known_record"));
                    }
                    if !known && !value["kind"].is_string() && self.version <= FORMAT_VERSION {
                        return Err(corrupt(good, Some(seq), "missing_kind"));
                    }
                    self.future = true;
                    crate::logging::structured_queue_record_skipped(
                        Some(seq),
                        if known {
                            "invalid_change"
                        } else {
                            "unknown_kind"
                        },
                    );
                }
                self.seq = self.seq.max(seq);
                self.records.push(value);
            }
            good += n as u64;
        }
        if good == 0 {
            // No complete first line means there can be no subsequent records.
            // The identity is only created in an empty file, so rewrite it safely.
            self.file.set_len(0)?;
            self.file.rewind()?;
            self.file
                .write_all(&encode(&self.identity(self.version))?)?;
            self.file.sync_all()?;
            tracing::warn!("repaired unterminated queue identity");
            return Ok(());
        }
        if self.read_only().is_some() {
            return Ok(());
        }
        if self.checkpoint.is_some_and(|(_, high)| self.seq < high) {
            return Err(corrupt(good, None, "incomplete_checkpoint"));
        }
        if self.version < FORMAT_VERSION || self.rewrite_required()? {
            self.checkpointable()?;
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
pub(crate) mod tests;
