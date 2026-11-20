//! Pane-free records and session-state ownership (ADR-0036 §1/S3a-3/S4).
//!
//! This registry is pure data: no process, timer, lease authority or EventHub.
//! #1842 owns workers/deadlines, #1843 owns public routing and the
//! StructuredAgentInfo projection. Public start remains unreachable until then
//! and #1734's caller-identity fix. Pane-free EventHub events belong to #1804.
//! Durable records contain identity and launch metadata only. Cold restore
//! starts offline with unknown observations and no lease, never an auto-resume.
//! Unknown/invalid/duplicate disk entries remain raw, inactive and in order across
//! saves (ADR-0028 §3a). The first supported identity/name/session wins.
//! Explicit removal also drops retained conflicts and compacts storage. Warnings
//! never log raw payloads. Gone processes remain `Lifecycle::Offline`, resumable by explicit
//! launch only. Opaque retained data never supplies process, state or lease
//! authority. Metadata admission cannot replace an unknown collection format.
#![deny(missing_docs)]

mod retention;

use super::state::{Input, SessionState, StateSink, Update};
use crate::api::agent_state::{AgentState, Lifecycle};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// DEAD records older than thirty days since first observation are pruned on save.
pub const RETAINED_DEAD_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// At most 256 DEAD records survive a save, keeping the newest; FORWARD is unbounded.
pub const RETAINED_DEAD_MAX: usize = 256;

/// Retained data never supplies runtime or operator authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetainedKind {
    /// Any unusable object other than an exact duplicate; never auto-prune across versions.
    Forward,
    /// A non-object JSON element or exact canonical duplicate of an earlier record.
    Dead,
}
/// Retained-record metadata, without exposing the preserved payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedInfo {
    /// Position in the serialized structured_agents array.
    pub slot: usize,
    /// Rollback protection category.
    pub kind: RetainedKind,
    /// First save observation, absent until the next save stamps the record.
    pub first_seen_ms: Option<u64>,
}

/// Exact data-only identity of the profile used by a launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileIdentity {
    /// Legacy operator profile directory selector.
    pub profile: String,
    /// Resolved operator binding, absent for legacy single-account launches.
    pub binding: Option<BindingIdentity>,
}

/// Operator binding identity and the policy revision at which it was resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingIdentity {
    /// Stable operator-issued identity.
    pub binding_id: String,
    /// Exact binding revision; a changed revision requires explicit recovery.
    pub revision: u64,
}

/// Durable structured identity. The runtime tag cannot describe a PTY record.
/// This typed record writes no credentials, process ids, attention, ages or lease
/// proofs. Opaque future fields pass through separately without granting authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Required runtime discriminator, never a PTY or an untagged record.
    pub runtime: StructuredRuntime,
    /// Fleet-global identity, shared with the S4 reducer.
    pub agent_id: String,
    /// Optional user-assigned name, unique within the structured registry.
    pub name: Option<String>,
    /// Harness adapter, currently Claude only.
    pub harness: String,
    /// Native conversation identity, exclusively reserved while registered.
    pub session_id: String,
    /// True only after a matching harness `system/init` was observed.
    /// Persisted with the session snapshot. Older records omit this field and
    /// default to false, so their next launch uses fresh-session creation.
    #[serde(default)]
    pub harness_established: bool,
    /// Launch identity observed at establishment; absent older records remain legacy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_identity: Option<ProfileIdentity>,
    /// Launch working directory, never a fabricated pane location.
    pub cwd: PathBuf,
    /// Requested model, absent when the provider selects its default.
    pub model: Option<String>,
}

impl Record {
    /// Refuse changing the identity of an established conversation before launch.
    pub(crate) fn admit_profile(
        &self,
        actual: Option<&ProfileIdentity>,
    ) -> Result<(), crate::api::error_code::ErrorCode> {
        if self.harness_established
            && self
                .profile_identity
                .as_ref()
                .is_some_and(|pin| Some(pin) != actual)
        {
            return Err(crate::api::error_code::ErrorCode::StructuredProfileMismatch);
        }
        Ok(())
    }
}

/// The only runtime represented by a pane-free registry record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum StructuredRuntime {
    /// A process whose authoritative input is a session stream.
    Structured,
}

/// Admission/update failures leave every existing entry untouched.
#[derive(Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// Unconfirmed ownership must be recovered or cleared before another launch.
    RecoveryRequired,
    /// Identity, session, harness or supplied name is invalid.
    InvalidRecord,
    /// Unknown collection layout: back up session.json and follow the session-state repair guide.
    UnsupportedCollectionShape,
    /// Identity, name or native session already belongs to another entry.
    Conflict,
    /// No such identity or the worker belongs to an older launch.
    StaleGeneration,
    /// The generation counter cannot allocate another launch safely.
    GenerationExhausted,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedCollectionShape => f.write_str("structured_agents is not an array; back up session.json and follow the session-state repair guide"),
            _ => write!(f, "{self:?}"),
        }
    }
}
impl std::error::Error for RegistryError {}

/// Latest S4 publication, including in-process needs-human and deadline facts.
/// It intentionally carries no authority to act as the recorded agent.
pub struct Projection {
    /// The last complete S4 update, also consumed by #1746.
    pub update: Update,
}
impl StateSink for Projection {
    fn publish(&mut self, update: Update) {
        self.update = update;
    }
}

struct Entry {
    pending_profile: Option<ProfileIdentity>,
    record: Record,
    slot: usize,
    generation: u64,
    reducer: SessionState,
    projection: Projection,
}

impl Entry {
    fn cold(record: Record, generation: u64, slot: usize) -> Self {
        let mut reducer = SessionState::new(record.agent_id.clone());
        let mut state = AgentState::unknown();
        state.lifecycle = Lifecycle::Offline;
        state.phase = crate::api::agent_state::Phase::Offline;
        // Establish the reducer lifecycle without inventing an observation clock.
        reducer.set_initial_lifecycle(Lifecycle::Offline);
        let projection = Projection {
            update: Update {
                agent_id: record.agent_id.clone(),
                state,
                phase_changed: None,
                needs_human: Vec::new(),
                permission_requests: Vec::new(),
                stalled_since_ms: None,
                observe_at: None,
            },
        };
        Self {
            pending_profile: None,
            record,
            slot,
            generation,
            reducer,
            projection,
        }
    }
}

/// Pure per-agent registry. Generations fence deliveries after removal/relaunch.
/// Worker ownership, live lease reporting and scheduling are added by #1842.
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<String, Entry>,
    generation: u64,
    establishment_dirty: bool,
    preserved: Vec<Option<Value>>,
    unparsed: Option<Value>,
    retained: BTreeMap<usize, retention::Retained>,
    retention_extra: Vec<Value>,
    retention_opaque: Option<Value>,
}
impl Registry {
    /// Consume new establishment evidence so the owner schedules its normal
    /// synced session-snapshot writer. Cold snapshots carry no pending save work.
    pub(crate) fn take_establishment_dirty(&mut self) -> bool {
        std::mem::take(&mut self.establishment_dirty)
    }

    /// Whether there is nothing durable, including inactive retained records.
    /// Unknown-only sessions must survive the application's empty-session cleanup.
    pub fn is_empty(&self) -> bool {
        self.preserved.iter().all(Option::is_none)
            && self.unparsed.is_none()
            && self.retention_extra.is_empty()
            && self.retention_opaque.is_none()
    }

    /// Admit a record before a worker exists. It starts offline, without authority.
    /// A worker must explicitly supply Live after acquiring its actual lease.
    /// Duplicate identity/name/session and unsupported harnesses are refused.
    /// Retained names are reserved too, without interpreting unknown runtime data.
    pub fn register(&mut self, record: Record) -> Result<u64, RegistryError> {
        if self.unparsed.is_some() {
            return Err(RegistryError::UnsupportedCollectionShape);
        }
        if record.name.as_deref().is_some_and(|name| {
            self.preserved
                .iter()
                .flatten()
                .any(|raw| raw.get("name").and_then(Value::as_str) == Some(name))
        }) {
            return Err(RegistryError::Conflict);
        }
        let raw = serde_json::to_value(&record).map_err(|_| RegistryError::InvalidRecord)?;
        self.register_raw(record, raw)
    }

    fn register_raw(&mut self, record: Record, raw: Value) -> Result<u64, RegistryError> {
        if self.unparsed.is_some() {
            return Err(RegistryError::UnsupportedCollectionShape);
        }
        if record
            .name
            .as_deref()
            .is_some_and(|name| crate::terminal::agent_name::validate(name).is_err())
        {
            crate::logging::agent_name_dropped("registry");
            return Err(RegistryError::InvalidRecord);
        }
        if record.agent_id.is_empty()
            || record.session_id.is_empty()
            || record.harness != "claude"
            || record.name.as_ref().is_some_and(|name| name.is_empty())
        {
            return Err(RegistryError::InvalidRecord);
        }
        if self.entries.values().any(|entry| {
            entry.record.agent_id == record.agent_id
                || entry.record.session_id == record.session_id
                || (record.name.is_some() && entry.record.name == record.name)
        }) {
            return Err(RegistryError::Conflict);
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(RegistryError::GenerationExhausted)?;
        self.entries.insert(
            record.agent_id.clone(),
            Entry::cold(record, generation, self.preserved.len()),
        );
        self.preserved.push(Some(raw));
        self.generation = generation;
        Ok(generation)
    }

    /// Start a fresh launch observation epoch while retaining the durable record.
    /// A new generation fences all previous worker deliveries. Live follows readiness.
    pub(crate) fn begin_launch(
        &mut self,
        agent_id: &str,
        current: u64,
    ) -> Result<u64, RegistryError> {
        let entry = self
            .entries
            .get_mut(agent_id)
            .filter(|entry| entry.generation == current)
            .ok_or(RegistryError::StaleGeneration)?;
        if entry.projection.update.state.lifecycle == Lifecycle::Unknown {
            return Err(RegistryError::RecoveryRequired);
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(RegistryError::GenerationExhausted)?;
        *entry = Entry::cold(entry.record.clone(), generation, entry.slot);
        self.generation = generation;
        Ok(generation)
    }

    /// Stage the admitted launch identity; only matching init makes it durable.
    pub(crate) fn stage_profile(
        &mut self,
        agent_id: &str,
        generation: u64,
        identity: Option<ProfileIdentity>,
    ) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get_mut(agent_id)
            .filter(|entry| entry.generation == generation)
            .ok_or(RegistryError::StaleGeneration)?;
        entry.pending_profile = identity;
        Ok(())
    }

    /// Address by stable identity, never by a placement or a caller-supplied pane.
    pub fn get(&self, agent_id: &str) -> Option<(&Record, &Projection, u64)> {
        self.entries
            .get(agent_id)
            .map(|entry| (&entry.record, &entry.projection, entry.generation))
    }

    /// Apply trusted stream/lifecycle facts to exactly one launch through StateSink.
    /// Caller supplies clocks and stall policy. The returned update carries the
    /// next deadline for #1842, which must schedule it rather than poll.
    pub(crate) fn apply(
        &mut self,
        agent_id: &str,
        generation: u64,
        input: Input<'_>,
        now: Instant,
        unix_ms: u64,
        stall_after: Duration,
    ) -> Result<&Update, RegistryError> {
        let entry = self
            .entries
            .get_mut(agent_id)
            .filter(|entry| entry.generation == generation)
            .ok_or(RegistryError::StaleGeneration)?;
        if let Input::Event(super::claude::SessionEvent::Init(init)) = &input {
            if !entry.record.harness_established
                && init.session_id.as_ref().and_then(Option::as_deref)
                    == Some(entry.record.session_id.as_str())
            {
                entry.record.harness_established = true;
                entry.record.profile_identity = entry.pending_profile.take();
                if let Some(Some(raw)) = self.preserved.get_mut(entry.slot) {
                    raw["harness_established"] = Value::Bool(true);
                    if let Some(identity) = &entry.record.profile_identity {
                        raw["profile_identity"] = serde_json::json!(identity);
                    } else if let Some(raw) = raw.as_object_mut() {
                        raw.remove("profile_identity");
                    }
                }
                self.establishment_dirty = true;
            }
        }
        entry
            .reducer
            .publish(input, now, unix_ms, stall_after, &mut entry.projection);
        Ok(&entry.projection.update)
    }

    /// Remove the matching generation and retained entries sharing its id/session.
    /// Known-runtime shadows also match by name; future-runtime names alone do not
    /// prove identity and must survive removal (#1858).
    /// Compact storage so removed slots cannot accumulate. Runtime cleanup must precede this.
    pub fn remove(&mut self, agent_id: &str, generation: u64) -> Result<Record, RegistryError> {
        if self
            .get(agent_id)
            .is_none_or(|(_, _, current)| current != generation)
        {
            return Err(RegistryError::StaleGeneration);
        }
        let entry = self
            .entries
            .remove(agent_id)
            .ok_or(RegistryError::StaleGeneration)?;
        let record = entry.record;
        let mut slots = vec![0; self.preserved.len()];
        let mut compact = Vec::new();
        let mut retained = BTreeMap::new();
        for (slot, raw) in self.preserved.drain(..).enumerate() {
            let Some(raw) = raw else { continue };
            let conflicts =
                raw.get("agent_id").and_then(Value::as_str) == Some(record.agent_id.as_str())
                    || raw.get("session_id").and_then(Value::as_str)
                        == Some(record.session_id.as_str())
                    || (raw.get("runtime").and_then(Value::as_str) == Some("structured")
                        && record.name.as_deref().is_some_and(|name| {
                            raw.get("name").and_then(Value::as_str) == Some(name)
                        }));
            if slot != entry.slot && !conflicts {
                slots[slot] = compact.len();
                if let Some(metadata) = self.retained.remove(&slot) {
                    retained.insert(compact.len(), metadata);
                }
                compact.push(Some(raw));
            }
        }
        for entry in self.entries.values_mut() {
            entry.slot = slots[entry.slot];
        }
        self.preserved = compact;
        self.retained = retained;
        Ok(record)
    }

    /// Borrow stable identity and its current observation together in agent-id order.
    pub fn observations(&self) -> impl Iterator<Item = (&Record, &Projection)> {
        self.entries
            .values()
            .map(|entry| (&entry.record, &entry.projection))
    }

    /// Durable metadata in stable agent-id order, without observations or authority.
    pub fn records(&self) -> Vec<Record> {
        self.entries
            .values()
            .map(|entry| entry.record.clone())
            .collect()
    }

    /// Cold metadata copy for persistence/handoff, never a clone of live state.
    pub fn snapshot(&self) -> Self {
        Self {
            entries: self
                .entries
                .iter()
                .map(|(id, entry)| {
                    (
                        id.clone(),
                        Entry::cold(entry.record.clone(), entry.generation, entry.slot),
                    )
                })
                .collect(),
            generation: self.generation,
            establishment_dirty: false,
            preserved: self.preserved.clone(),
            unparsed: self.unparsed.clone(),
            retained: self.retained.clone(),
            retention_extra: self.retention_extra.clone(),
            retention_opaque: self.retention_opaque.clone(),
        }
    }

    /// Carry uncertainty across further handoffs even when no worker was restored.
    pub(crate) fn handoff_unknown(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.projection.update.state.lifecycle == Lifecycle::Unknown)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Restore only the exporter's uncertainty, without a process, lease or resume.
    pub(crate) fn restore_handoff_unknown(&mut self, ids: &[String]) {
        for id in ids {
            if let Some(entry) = self.entries.get_mut(id) {
                entry.reducer.set_initial_lifecycle(Lifecycle::Unknown);
                entry.projection.update.state = AgentState::unknown();
            }
        }
    }

    /// Inactive retained entries, for diagnostics without exposing raw payloads.
    /// Slice (a) has no structured status/doctor route; #1843 owns public routing.
    pub fn retained_count(&self) -> usize {
        self.preserved.iter().flatten().count() - self.entries.len()
            + usize::from(self.unparsed.is_some())
    }

    /// Construct from known typed metadata. Conflicts reject this admission batch.
    /// Disk deserialization instead retains incompatible entries individually.
    /// A restored agent whose process is gone stays `Lifecycle::Offline` and resumable,
    /// without automatically resurrecting a process or acquiring a lease.
    pub fn restore(records: Vec<Record>) -> Result<Self, RegistryError> {
        let mut registry = Self::default();
        for record in records {
            registry.register(record)?;
        }
        Ok(registry)
    }
}

impl Serialize for Registry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Some(raw) = &self.unparsed {
            return raw.serialize(serializer);
        }
        self.preserved
            .iter()
            .flatten()
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Registry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        let mut registry = Self::default();
        let Value::Array(records) = raw else {
            tracing::warn!(
                "structured registry has an unknown collection shape; retained inactive"
            );
            registry.unparsed = Some(raw);
            return Ok(registry);
        };
        let mut seen = BTreeSet::new();
        for (index, raw) in records.into_iter().enumerate() {
            let duplicate = !seen.insert(retention::digest(&raw));
            let dead = !raw.is_object() || duplicate;
            let reason = if dead {
                "non_object_or_exact_duplicate"
            } else {
                match serde_json::from_value::<Record>(raw.clone()) {
                    Ok(record) => match registry.register_raw(record, raw.clone()) {
                        Ok(_) => continue,
                        Err(RegistryError::Conflict) => "duplicate",
                        Err(_) => "invalid_record",
                    },
                    Err(_) => "unsupported_shape",
                }
            };
            tracing::warn!(
                index,
                reason,
                "structured record skipped; raw data retained inactive"
            );
            registry
                .retained
                .insert(registry.preserved.len(), retention::Retained::new(dead));
            registry.preserved.push(Some(raw));
        }
        Ok(registry)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod retention_tests;
