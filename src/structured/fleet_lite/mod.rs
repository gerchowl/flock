//! S7-min fleet-lite core (ADR-0036 §4/§5a, ADR-0038 §2a, #1832 slice 6a).
//!
//! No transport, subscriptions, EventHub, PTY, peer or node projection exists here.
//! #1832 slice 6b supplies registry/activity/S6 inputs and publishes returned batches
//! under subscription ownership. Reads create durable inline checkpoints: their
//! allocated seqs must also reach existing subscribers before subsequent deltas.
//! The exclusive file lock fences local writers, not the deferred S7 handoff protocol.
//! Unknown/invalid rows are warned, skipped and preserved raw (ADR-0028 §3a).
//! A separate durable cursor reserves positions before snapshot replacement (#1976).
//! Lost/corrupt snapshots recover empty at that floor with a typed diagnostic.
//! Missing/corrupt cursor evidence cannot silently authorize a reset to zero.
#![deny(missing_docs)]
// The non-web binary has no stream consumer.
#![cfg_attr(not(feature = "web"), allow(dead_code))]

use super::registry::Record;
use crate::api::agent_state::AgentState;
pub(crate) mod wire;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
pub use wire::{FleetAgentRow, FleetDelta};

type Rows = BTreeMap<String, FleetAgentRow>;

/// Trusted structured-only projection inputs. S6 IDs come from the permission
/// book, not phase inference. The provider must propagate unavailable observations
/// rather than manufacture an empty fleet or empty attention list.
pub struct StructuredAgent<'a> {
    /// Registry identity and optional user label; no PTY record fits this type.
    pub record: &'a Record,
    /// Complete S4 state, including sources and independent axes.
    pub state: &'a AgentState,
    /// Registry's current activity, without transcript-based guesses.
    pub activity: Option<&'a str>,
    /// S6 requests currently awaiting a human decision.
    pub waiting_on_human: &'a [String],
}

/// Provider seam for 6b. Each successful call is one coherent registry/S4/S6 cut.
pub trait Provider {
    /// Read structured agents only; observation failure must leave the store intact.
    fn agents(&self) -> io::Result<Vec<StructuredAgent<'_>>>;
}

/// Project the existing generated wire contract without pane or task fields.
pub fn project(agent: StructuredAgent<'_>) -> FleetAgentRow {
    FleetAgentRow {
        id: agent.record.agent_id.clone(),
        label: agent
            .record
            .name
            .clone()
            .unwrap_or_else(|| agent.record.agent_id.clone()),
        state: agent.state.clone(),
        activity: agent.activity.map(str::to_owned),
        waiting_on_human: agent.waiting_on_human.to_vec(),
    }
}

/// Pure deterministic changes: removals by ID, then changed/new rows by ID.
/// Unchanged rows consume no seq. Input maps must be keyed by their row IDs.
pub fn changes(previous: &Rows, current: &Rows) -> Vec<FleetDelta> {
    previous
        .keys()
        .filter(|id| !current.contains_key(*id))
        .map(|id| FleetDelta::Remove { id: id.clone() })
        .chain(
            current
                .iter()
                .filter(|(id, row)| previous.get(*id) != Some(*row))
                .map(|(_, row)| FleetDelta::Upsert { row: row.clone() }),
        )
        .collect()
}

/// One dense fleet stream position and its generated wire payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    /// Persisted stream-local position, never reset at restart.
    pub seq: u64,
    /// Closed fleet.upsert/fleet.remove vocabulary.
    pub event: FleetDelta,
}

/// Immutable initial/recovery cut for 6b's inline gap/checkpoint/synced delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Head before checkpoint allocation (gap.to).
    pub to: u64,
    /// Completion and synced cursor; equals `to` for an empty snapshot.
    pub snapshot_to: u64,
    /// Complete upserts at to+1 through snapshot_to, in ID order.
    pub rows: Vec<Delta>,
}

/// Recoverable snapshot loss, exposed to 6b's status/operator diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnostic {
    /// The snapshot disappeared; rebuild rows from the provider before serving.
    SnapshotMissing {
        /// Independent durable floor retained through the loss.
        high_water: u64,
    },
    /// JSON, header or row-collection metadata is corrupt; no rows are trusted.
    SnapshotCorrupt {
        /// Independent durable floor retained through the corruption.
        high_water: u64,
    },
}

/// Locked durable materialization. Every mutation/checkpoint persists before
/// publication. On any write failure, this instance refuses further writes until
/// reopened: rename may have succeeded despite a failed directory fsync.
/// A genuinely new store starts at zero. A separate `fleet.cursor` record survives
/// lost/corrupt `fleet.json`; corrupt cursor evidence refuses loading. Failed
/// writes can reserve unpublished positions; recovery snapshots cover that gap.
/// Changed rows drop unknown top-level fields; unchanged rows keep their raw form.
/// Collection entries are lenient. Retained duplicates stay inert and are dropped on explicit removal
/// of their ID, preventing resurrection on the next load.
pub struct Store {
    path: PathBuf,
    _lock: File,
    document: Value,
    rows: Rows,
    encoded: BTreeMap<String, Value>,
    retained: Vec<Value>,
    high_water: u64,
    writable: bool,
    diagnostics: Vec<Diagnostic>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
fn valid(row: &FleetAgentRow) -> bool {
    !row.id.is_empty()
        && !row.label.is_empty()
        && row.waiting_on_human.iter().all(|id| !id.is_empty())
}
fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(parent(path))?.sync_all()
}
// Both records use the same temp/fsync/rename/fsync-dir discipline, under one lock.
fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace_with_parent_sync(path, bytes, sync_parent)
}
// Argument-only seam for testing directory-sync failures after a real rename.
fn replace_with_parent_sync(
    path: &Path,
    bytes: &[u8],
    sync: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    let temp = PathBuf::from(name);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path)?;
    sync(path)
}
fn cursor(path: &Path) -> io::Result<Option<u64>> {
    match fs::read(path.with_extension("cursor")) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)?
            .get("high_water")
            .and_then(Value::as_u64)
            .map(Some)
            .ok_or_else(|| invalid("invalid fleet cursor; restore a trusted high-water floor")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl Store {
    /// Acquire exclusive ownership before loading. A busy owner refuses immediately.
    /// Future/invalid rows survive subsequent saves; duplicate IDs keep the first
    /// valid row. Unknown document fields also round-trip unchanged. A legacy
    /// snapshot with an unmarked legacy lock seeds the cursor; later cursor loss
    /// refuses loading even if an older snapshot survives. Lost/corrupt snapshots
    /// recover only with a valid cursor. Never delete both recovery records.
    /// Opening preserves damaged snapshot bytes; a later write replaces them.
    /// An empty, unmarked lock without either record opens at zero, including a
    /// legacy snapshot-only lock (#1974). Legacy locks were always unmarked, even
    /// after publishing positions. That layout's only floor was `fleet.json`:
    /// if it is lost, no better floor can be recovered, preserving the old behavior.
    /// Before marking the lock, opening syncs the cursor's parent directory even
    /// when its value already matches: a previous rename may have succeeded but
    /// its directory sync failed.
    /// A directory-sync failure refuses opening and leaves the lock unmarked.
    /// `fleet.lock` carries the initialization marker: deleting it together with
    /// `fleet.cursor` while an older `fleet.json` survives reopens at that older
    /// head. Operators must never delete the lock.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        Self::open_with_parent_sync(path, sync_parent)
    }

    // Test seam shared by cursor replacement and initialization recovery.
    fn open_with_parent_sync(
        path: PathBuf,
        mut sync: impl FnMut(&Path) -> io::Result<()>,
    ) -> io::Result<Self> {
        fs::create_dir_all(parent(&path))?;
        let lock_path = path.with_extension("lock");
        let mut lock = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                OpenOptions::new().read(true).write(true).open(lock_path)?
            }
            Err(e) => return Err(e),
        };
        lock.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => {
                io::Error::new(io::ErrorKind::WouldBlock, "fleet store busy")
            }
            TryLockError::Error(error) => error,
        })?;
        let initialized = lock.metadata()?.len() != 0;
        let reserved = cursor(&path)?;
        if initialized && reserved.is_none() {
            return Err(invalid(
                "fleet cursor missing; restore a trusted high-water floor",
            ));
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let parsed: Option<Value> = bytes.as_ref().and_then(|b| serde_json::from_slice(b).ok());
        let snapshot_head = parsed
            .as_ref()
            .filter(|d| d.get("rows").is_some_and(Value::is_array))
            .and_then(|d| d.get("high_water").and_then(Value::as_u64));
        let high_water = match (reserved, snapshot_head) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) if !initialized && bytes.is_none() => 0,
            _ => {
                return Err(invalid(
                    "fleet cursor unavailable; restore a trusted high-water floor",
                ))
            }
        };
        let mut diagnostics = Vec::new();
        let document = if snapshot_head.is_some() {
            parsed.ok_or_else(|| invalid("invalid fleet snapshot"))?
        } else {
            if reserved.is_some() {
                diagnostics.push(if bytes.is_some() {
                    Diagnostic::SnapshotCorrupt { high_water }
                } else {
                    Diagnostic::SnapshotMissing { high_water }
                });
                let reason = if bytes.is_some() {
                    "snapshot_corrupt"
                } else {
                    "snapshot_missing"
                };
                tracing::warn!(reason, high_water, "fleet rows require provider recovery");
            }
            json!({"high_water":high_water,"rows":[]})
        };
        if reserved != Some(high_water) {
            replace_with_parent_sync(
                &path.with_extension("cursor"),
                &serde_json::to_vec(&json!({"high_water":high_water}))?,
                &mut sync,
            )?;
        } else if !initialized {
            // A previous open may have renamed the cursor without syncing its directory.
            sync(&path.with_extension("cursor"))?;
        }
        if !initialized {
            // Mark the locked inode only after cursor durability, before any reservations.
            // Its marker distinguishes legacy migration from later cursor loss.
            lock.write_all(b"1")?;
            lock.sync_all()?;
        }
        let raw = document
            .get("rows")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("invalid fleet rows collection"))?;
        let mut rows = Rows::new();
        let mut encoded = BTreeMap::new();
        let mut retained = Vec::new();
        for (index, value) in raw.iter().enumerate() {
            let reason = match serde_json::from_value::<FleetAgentRow>(value.clone()) {
                Ok(row) if !valid(&row) => "invalid",
                Ok(row) if rows.contains_key(&row.id) => "duplicate",
                Ok(row) => {
                    encoded.insert(row.id.clone(), value.clone());
                    rows.insert(row.id.clone(), row);
                    continue;
                }
                Err(_) if value.is_object() && value.get("id").is_none() => "unsupported",
                Err(_) => "invalid",
            };
            tracing::warn!(index, reason, "skipping and retaining fleet row");
            retained.push(value.clone());
        }
        Ok(Self {
            path,
            _lock: lock,
            document,
            rows,
            encoded,
            retained,
            high_water,
            writable: true,
            diagnostics,
        })
    }

    /// Durable allocation floor, including reserved checkpoint positions.
    pub fn high_water(&self) -> u64 {
        self.high_water
    }

    /// Recovery facts from opening, retained until this owner is dropped.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Refresh from a coherent provider cut. Duplicate/invalid identities refuse
    /// the entire update; valid observations never silently disappear. No-op cuts
    /// allocate nothing. Returned deltas are already durable and densely numbered.
    pub fn refresh(&mut self, provider: &dyn Provider) -> io::Result<Vec<Delta>> {
        let mut rows = Rows::new();
        for agent in provider.agents()? {
            let row = project(agent);
            if !valid(&row) || rows.insert(row.id.clone(), row).is_some() {
                return Err(invalid("invalid or duplicate fleet projection"));
            }
        }
        let events = changes(&self.rows, &rows);
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let batch = self.number(events)?;
        let changed: BTreeSet<_> = batch
            .iter()
            .map(|delta| match &delta.event {
                FleetDelta::Upsert { row } => row.id.as_str(),
                FleetDelta::Remove { id } => id.as_str(),
            })
            .collect();
        let mut encoded = BTreeMap::new();
        for (id, row) in &rows {
            if !changed.contains(id.as_str()) {
                if let Some(value) = self.encoded.get(id) {
                    encoded.insert(id.clone(), value.clone());
                    continue;
                }
            }
            encoded.insert(id.clone(), serde_json::to_value(row)?);
        }
        let removed: BTreeSet<_> = batch
            .iter()
            .filter_map(|delta| match &delta.event {
                FleetDelta::Remove { id } => Some(id.as_str()),
                FleetDelta::Upsert { .. } => None,
            })
            .collect();
        let retained = self
            .retained
            .iter()
            .filter(|raw| {
                raw.get("id")
                    .and_then(Value::as_str)
                    .is_none_or(|id| !removed.contains(id))
            })
            .cloned()
            .collect::<Vec<_>>();
        self.commit(
            batch.last().map_or(self.high_water, |d| d.seq),
            &encoded,
            &retained,
        )?;
        self.rows = rows;
        self.encoded = encoded;
        self.retained = retained;
        Ok(batch)
    }

    /// Create and persist an initial/recovery checkpoint. The returned owned cut
    /// remains stable through later mutations. Every row consumes a new seq per
    /// ADR-0036 §4; an empty checkpoint retains its nonzero head across restart.
    /// 6b must serialize publication of the whole cut with live deltas.
    pub fn snapshot(&mut self) -> io::Result<Snapshot> {
        let rows = self.number(
            self.rows
                .values()
                .cloned()
                .map(|row| FleetDelta::Upsert { row })
                .collect(),
        )?;
        let snapshot_to = rows.last().map_or(self.high_water, |d| d.seq);
        let cut = Snapshot {
            to: self.high_water,
            snapshot_to,
            rows,
        };
        self.commit(snapshot_to, &self.encoded.clone(), &self.retained.clone())?;
        Ok(cut)
    }

    fn number(&self, events: Vec<FleetDelta>) -> io::Result<Vec<Delta>> {
        let mut seq = self.high_water;
        events
            .into_iter()
            .map(|event| {
                seq = seq
                    .checked_add(1)
                    .ok_or_else(|| invalid("fleet seq exhausted"))?;
                Ok(Delta { seq, event })
            })
            .collect()
    }

    fn commit(
        &mut self,
        head: u64,
        encoded: &BTreeMap<String, Value>,
        retained: &[Value],
    ) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::other(
                "fleet durability uncertain; reopen required",
            ));
        }
        if head < self.high_water {
            return Err(invalid("fleet cursor cannot decrease"));
        }
        let mut document = self.document.clone();
        document["high_water"] = json!(head);
        document["rows"] = encoded.values().chain(retained).cloned().collect();
        let bytes = serde_json::to_vec(&document)?;
        self.writable = false;
        // Reserve first: a failed snapshot write may burn positions, never reuse them.
        replace(
            &self.path.with_extension("cursor"),
            &serde_json::to_vec(&json!({"high_water":head}))?,
        )?;
        replace(&self.path, &bytes)?;
        self.high_water = head;
        self.document = document;
        self.writable = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
