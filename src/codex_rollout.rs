//! Pure framing and parent-turn reduction for ADR-0028 §1a, Codex 0.160.1.
//!
//! Input must belong to a validated binding and current history segment.
//! Binding, reader continuity/resynchronization and authority are #1662–#1664.
//! Construct a fresh reducer only after independently verifying that segment.
use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Parent-turn state from verified complete lifecycle records, never screen state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Turn {
    Unknown,
    Working,
    Idle,
}

/// Diagnostic degradation of this segment. Failures remain sticky until verified reconstruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    PartialRecord,
    MalformedRecord,
    ConflictingTurn,
    UnattributedAbort,
    OrdinalDiscontinuity,
    SourceDiscontinuity,
    CapacityExceeded,
}

impl Reason {
    /// Lifecycle degradation labels, distinct from socket/CLI error codes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PartialRecord => "codex_partial_record",
            Self::MalformedRecord => "codex_malformed_record",
            Self::ConflictingTurn => "codex_conflicting_turn_id",
            Self::UnattributedAbort => "codex_unattributed_abort",
            Self::OrdinalDiscontinuity => "codex_ordinal_discontinuity",
            Self::SourceDiscontinuity => "codex_source_discontinuity",
            Self::CapacityExceeded => "codex_reducer_capacity_exceeded",
        }
    }
}

/// Attributed parent-turn boundary, independent of tool and child activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Start,
    Complete,
    Abort,
}

/// Last accepted matching lifecycle boundary in this verified segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Boundary {
    /// Opaque owner turn ID, compared for equality rather than ordering.
    pub turn_id: String,
    /// The matching lifecycle transition that established this evidence.
    pub kind: Kind,
    /// Byte offset after this record, relative to this verified segment.
    pub complete_offset: u64,
    /// Supplied monotonic observation time, unchanged by duplicates or silence.
    pub at: Instant,
}

/// Current state and evidence diagnostics. Age is informational, never a lease.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Observation<'a> {
    pub turn: Turn,
    pub reason: Option<Reason>,
    pub boundary: Option<&'a Boundary>,
    pub last_evidence_age: Option<Duration>,
}

/// Capacity is supplied by the reader (#1663), never an implicit eviction of
/// old turn IDs: eviction could make a late completion appear unrecognized.
/// Overflow invalidates this reducer; only verified resynchronization may
/// replace it. `next_ordinal` is None for legacy, or the verified paginated
/// segment's first ordinal (including any history_base offset).
#[cfg_attr(test, derive(Clone, Debug, PartialEq, Eq))]
pub(crate) struct Reducer {
    partial: Vec<u8>,
    complete_offset: u64,
    boundary: Option<Boundary>,
    retired: HashSet<String>,
    failure: Option<Reason>,
    max_record_bytes: usize,
    max_turn_ids: usize,
    next_ordinal: Option<u64>,
}

impl Reducer {
    /// Start only at an independently verified segment boundary. The turn-ID
    /// budget includes both remembered retired IDs and the current active turn.
    /// `None` selects legacy records, `Some` requires the exact first ordinal.
    pub fn new(max_record_bytes: usize, max_turn_ids: usize, next_ordinal: Option<u64>) -> Self {
        Self {
            partial: Vec::new(),
            complete_offset: 0,
            boundary: None,
            retired: HashSet::new(),
            failure: None,
            max_record_bytes,
            max_turn_ids,
            next_ordinal,
        }
    }

    /// New bytes only, in verified order. Empty reads do not renew a lease or
    /// clear a partial record. Supplied time timestamps actual boundaries only.
    pub fn feed(&mut self, bytes: &[u8], now: Instant) {
        if self.failure.is_some() {
            return;
        }
        for fragment in bytes.split_inclusive(|byte| *byte == b'\n') {
            let complete = fragment.last() == Some(&b'\n');
            let data = if complete {
                &fragment[..fragment.len() - 1]
            } else {
                fragment
            };
            if data.len() > self.max_record_bytes.saturating_sub(self.partial.len()) {
                self.invalidate(Reason::CapacityExceeded);
                return;
            }
            self.partial.extend_from_slice(data);
            if !complete {
                continue;
            }
            let Some(offset) = self
                .complete_offset
                .checked_add(self.partial.len() as u64)
                .and_then(|offset| offset.checked_add(1))
            else {
                self.invalidate(Reason::SourceDiscontinuity);
                return;
            };
            self.complete_offset = offset;
            let record = serde_json::from_slice::<Value>(&self.partial);
            self.partial.clear();
            let result = record
                .map_err(|_| Reason::MalformedRecord)
                .and_then(|record| self.reduce(&record, now));
            if let Err(reason) = result {
                self.invalidate(reason);
                return;
            }
        }
    }

    /// A verified prefix/file/cursor failure is sticky, including while working.
    /// Further records cannot silently restore authority in the same reducer.
    pub fn invalidate(&mut self, reason: Reason) {
        self.failure.get_or_insert(reason);
        self.partial.clear();
    }

    /// Read current evidence at the supplied clock without advancing state or
    /// expiring quiet working/idle. Unterminated bytes mask idle as unknown.
    pub fn observe(&self, now: Instant) -> Observation<'_> {
        let working = self
            .boundary
            .as_ref()
            .is_some_and(|b| b.kind == Kind::Start);
        let reason = self
            .failure
            .or_else(|| (!self.partial.is_empty() && !working).then_some(Reason::PartialRecord));
        let turn = if reason.is_some() || self.boundary.is_none() {
            Turn::Unknown
        } else if working {
            Turn::Working
        } else {
            Turn::Idle
        };
        Observation {
            turn,
            reason,
            boundary: self.boundary.as_ref(),
            last_evidence_age: self
                .boundary
                .as_ref()
                .map(|b| now.saturating_duration_since(b.at)),
        }
    }

    fn reduce(&mut self, record: &Value, now: Instant) -> Result<(), Reason> {
        let record_type = record
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .ok_or(Reason::MalformedRecord)?;
        match (self.next_ordinal, record.get("ordinal")) {
            (None, None) => (),
            (Some(expected), Some(value)) if value.as_u64() == Some(expected) => {
                self.next_ordinal = Some(
                    expected
                        .checked_add(1)
                        .ok_or(Reason::OrdinalDiscontinuity)?,
                );
            }
            _ => return Err(Reason::OrdinalDiscontinuity),
        }
        if matches!(
            record_type,
            "session_meta" | "response_item" | "compacted" | "turn_context" | "event_msg"
        ) && !record.get("payload").is_some_and(Value::is_object)
        {
            return Err(Reason::MalformedRecord);
        }
        if record_type != "event_msg" {
            return Ok(());
        }
        let payload = record.get("payload").ok_or(Reason::MalformedRecord)?;
        let event = payload
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .ok_or(Reason::MalformedRecord)?;
        let kind = match event {
            "task_started" | "turn_started" => Kind::Start,
            "task_complete" | "turn_complete" => Kind::Complete,
            "turn_aborted" => Kind::Abort,
            _ => return Ok(()),
        };
        let id = payload
            .get("turn_id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or(if kind == Kind::Abort {
                Reason::UnattributedAbort
            } else {
                Reason::MalformedRecord
            })?;
        if self.retired.contains(id) {
            return Ok(());
        }
        if let Some(current) = &self.boundary {
            if current.turn_id == id {
                if kind == Kind::Start || current.kind != Kind::Start {
                    return Ok(());
                }
            } else if kind != Kind::Start || current.kind == Kind::Start {
                return Err(Reason::ConflictingTurn);
            } else {
                // Count the active turn as well as all remembered retired IDs.
                if self.retired.len().saturating_add(1) >= self.max_turn_ids {
                    return Err(Reason::CapacityExceeded);
                }
                self.retired.insert(current.turn_id.clone());
            }
        } else if kind != Kind::Start {
            return Err(Reason::ConflictingTurn);
        } else if self.max_turn_ids == 0 {
            return Err(Reason::CapacityExceeded);
        }
        self.boundary = Some(Boundary {
            turn_id: id.to_owned(),
            kind,
            complete_offset: self.complete_offset,
            at: now,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;
