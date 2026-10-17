//! Pane-free records and session-state ownership (ADR-0036 §1/S3a-3/S4).
//!
//! This registry is pure data: no process, timer, lease authority or EventHub.
//! #1842 owns workers/deadlines, #1843 owns public routing and the
//! StructuredAgentInfo projection. Public start remains unreachable until then
//! and #1734's caller-identity fix. Pane-free EventHub events belong to #1804.
//! Durable records contain identity and launch metadata only. Cold restore
//! starts offline with unknown observations and no lease, never an auto-resume.
#![deny(missing_docs)]

use super::state::{Input, SessionState, StateSink, Update};
use crate::api::agent_state::{AgentState, Lifecycle};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Durable structured identity. The runtime tag cannot describe a PTY record.
/// No credentials, process identifiers, attention, ages or lease proofs persist.
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
    /// Launch working directory, never a fabricated pane location.
    pub cwd: PathBuf,
    /// Requested model, absent when the provider selects its default.
    pub model: Option<String>,
}

/// The only runtime represented by a pane-free registry record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredRuntime {
    /// A process whose authoritative input is a session stream.
    Structured,
}

/// Admission/update failures leave every existing entry untouched.
#[derive(Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// Identity, session, harness or supplied name is invalid.
    InvalidRecord,
    /// Identity, name or native session already belongs to another entry.
    Conflict,
    /// No such identity or the worker belongs to an older launch.
    StaleGeneration,
    /// The generation counter cannot allocate another launch safely.
    GenerationExhausted,
}

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
    record: Record,
    generation: u64,
    reducer: SessionState,
    projection: Projection,
}

impl Entry {
    fn cold(record: Record, generation: u64) -> Self {
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
                stalled_since_ms: None,
                observe_at: None,
            },
        };
        Self {
            record,
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
}
impl Registry {
    /// Whether there are any records, including offline restored agents.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Admit a record before a worker exists. It starts offline, without authority.
    /// A worker must explicitly supply Live after acquiring its actual lease.
    /// Duplicate identity/name/session and unsupported harnesses are refused.
    pub fn register(&mut self, record: Record) -> Result<u64, RegistryError> {
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
        self.entries
            .insert(record.agent_id.clone(), Entry::cold(record, generation));
        self.generation = generation;
        Ok(generation)
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
    pub fn apply(
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
        entry
            .reducer
            .publish(input, now, unix_ms, stall_after, &mut entry.projection);
        Ok(&entry.projection.update)
    }

    /// Remove only the matching generation. Runtime cleanup must precede this.
    pub fn remove(&mut self, agent_id: &str, generation: u64) -> Result<Record, RegistryError> {
        if self
            .get(agent_id)
            .is_none_or(|(_, _, current)| current != generation)
        {
            return Err(RegistryError::StaleGeneration);
        }
        self.entries
            .remove(agent_id)
            .map(|entry| entry.record)
            .ok_or(RegistryError::StaleGeneration)
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
                        Entry::cold(entry.record.clone(), entry.generation),
                    )
                })
                .collect(),
            generation: self.generation,
        }
    }

    /// Restore metadata only. Reject conflicts atomically, never adopt a process.
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
        self.records().serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Registry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let records = Vec::<Record>::deserialize(deserializer)?;
        Self::restore(records).map_err(|error| {
            serde::de::Error::custom(format!("invalid structured registry: {error:?}"))
        })
    }
}

#[cfg(test)]
mod tests;
