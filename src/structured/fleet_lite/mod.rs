//! S7-min fleet-lite core (ADR-0036 §4/§5a, ADR-0038 §2a, #1832 slice 6a).
//!
//! No transport, subscriptions, EventHub, PTY, peer or node projection exists here.
//! #1832 slice 6b supplies registry/activity/S6 inputs and publishes returned batches
//! under subscription ownership. Reads create durable inline checkpoints: their
//! allocated seqs must also reach existing subscribers before subsequent deltas.
//! The exclusive file lock fences local writers, not the deferred S7 handoff protocol.
//! Unknown/invalid rows are warned, skipped and preserved raw (ADR-0028 §3a).
//! Invalid high-water metadata refuses opening: recovering it as zero loses cursors.
#![deny(missing_docs)]
// #1832 slice 6b adds the product caller after subscription machinery lands.
#![allow(dead_code)]

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

/// Locked durable materialization. Every mutation/checkpoint persists before
/// publication. On any write failure, this instance refuses further writes until
/// reopened: rename may have succeeded despite a failed directory fsync.
/// Missing files start at zero; malformed headers/files refuse loading. Only rows
/// are lenient. Retained duplicates stay inert and are dropped on explicit removal
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
impl Store {
    /// Acquire exclusive ownership before loading. A busy owner refuses immediately.
    /// Future/invalid rows survive subsequent saves; duplicate IDs keep the first
    /// valid row. Unknown top-level fields also round-trip unchanged.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(parent(&path))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))?;
        lock.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => {
                io::Error::new(io::ErrorKind::WouldBlock, "fleet store busy")
            }
            TryLockError::Error(error) => error,
        })?;
        let document: Value = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => json!({"high_water":0,"rows":[]}),
            Err(e) => return Err(e),
        };
        let high_water = document
            .get("high_water")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("invalid fleet high-water"))?;
        let raw = document
            .get("rows")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("invalid fleet rows collection"))?;
        let mut rows = Rows::new();
        let mut encoded = BTreeMap::new();
        let mut retained = Vec::new();
        for value in raw {
            match serde_json::from_value::<FleetAgentRow>(value.clone()) {
                Ok(row) if valid(&row) && !rows.contains_key(&row.id) => {
                    encoded.insert(row.id.clone(), value.clone());
                    rows.insert(row.id.clone(), row);
                }
                _ => {
                    tracing::warn!("skipping and retaining unsupported or duplicate fleet row");
                    retained.push(value.clone());
                }
            }
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
        })
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
            let mut value = self.encoded.get(id).cloned().unwrap_or_else(|| json!({}));
            let object = value
                .as_object_mut()
                .ok_or_else(|| invalid("invalid fleet row"))?;
            object.remove("activity");
            object.extend(
                serde_json::to_value(row)?
                    .as_object()
                    .ok_or_else(|| invalid("invalid fleet projection"))?
                    .clone(),
            );
            encoded.insert(id.clone(), value);
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
        let mut document = self.document.clone();
        document["high_water"] = json!(head);
        document["rows"] = encoded.values().chain(retained).cloned().collect();
        let bytes = serde_json::to_vec(&document)?;
        self.writable = false;
        let temp = self.path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &self.path)?;
        File::open(parent(&self.path))?.sync_all()?;
        self.high_water = head;
        self.document = document;
        self.writable = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
