//! Verified incremental rollout bytes for ADR-0028 §1a (#1855).
//!
//! This is a file/cursor foundation, not runtime lifecycle authority. Read on a
//! supervised worker (#1857); prepare resume/fork segments before trust admission
//! (#1856). A successful empty EOF never changes a turn or renews a lease.
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::codex_binding::Validated;
use crate::codex_rollout::{verify_record, Reason, Reducer};
use crate::platform::structured::{rollout_identity, FileIdentity};

/// Explicit memory/work budgets. Overflow is degradation, never truncated history
/// or forgotten turn IDs. Runtime policy chooses these before opening a reader.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub file_bytes: usize,
    pub record_bytes: usize,
    pub turn_ids: usize,
}
impl Limits {
    /// Construct a bounded reducer only for an independently attributed segment.
    /// The caller must invalidate it on a reader error; raw history is not authority.
    pub fn reducer(self, first_ordinal: Option<u64>) -> Reducer {
        Reducer::new(self.record_bytes, self.turn_ids, first_ordinal)
    }
}

/// Immutable pre-launch continuity evidence. Keep both offsets and the suffix:
/// completing a pre-launch partial record cannot attribute it to a new writer.
/// A copy still needs destination-local verification and segment admission (#1856).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Checkpoint {
    identity: FileIdentity,
    read_offset: u64,
    complete_offset: u64,
    partial: Vec<u8>,
    digest: [u8; 32],
    next_ordinal: Option<u64>,
}
impl Checkpoint {
    /// Complete-record boundary and exact uncompleted bytes at snapshot capture.
    pub fn position(&self) -> (u64, &[u8]) {
        (self.complete_offset, &self.partial)
    }
}

/// New bytes from one verified read, including any partial final record. Deliver
/// exactly once and in order to the selected segment parser, not to a fresh reducer.
pub(crate) struct Update {
    pub bytes: Vec<u8>,
    pub from: u64,
    pub checkpoint: Checkpoint,
    /// Successful EOF observation, even when no bytes were appended.
    pub eof_at: Instant,
}

/// Owns the same descriptor returned by binding validation. Never searches by
/// filename or reopens a replacement as a new source. Failures are sticky;
/// reconstruction needs a new independently verified binding/segment.
pub(crate) struct Reader {
    validated: Validated,
    limits: Limits,
    cursor: Checkpoint,
    initialized: bool,
    failure: Option<Reason>,
}
impl Reader {
    /// Accept an opened binding candidate, before it grants lifecycle authority.
    pub fn new(validated: Validated, limits: Limits) -> Result<Self, Reason> {
        let identity = rollout_identity(&validated.file, &validated.path)
            .map_err(|_| Reason::SourceDiscontinuity)?;
        Ok(Self {
            validated,
            limits,
            cursor: Checkpoint {
                identity,
                read_offset: 0,
                complete_offset: 0,
                partial: Vec::new(),
                digest: Sha256::digest([]).into(),
                next_ordinal: None,
            },
            initialized: false,
            failure: None,
        })
    }

    /// Verify the entire consumed prefix before returning new bytes. I/O and
    /// retained bytes are bounded by Limits; this deliberately rereads the prefix
    /// so equal-length edits cannot pass as append-only activity. No age lease.
    pub fn poll(&mut self, now: Instant) -> Result<Update, Reason> {
        if let Some(reason) = self.failure {
            return Err(reason);
        }
        let result = self.read(now);
        if let Err(reason) = result {
            self.failure = Some(reason);
        }
        result
    }
    fn read(&mut self, now: Instant) -> Result<Update, Reason> {
        let valid_identity = || {
            rollout_identity(&self.validated.file, &self.validated.path)
                .map_err(|_| Reason::SourceDiscontinuity)
        };
        if valid_identity()? != self.cursor.identity {
            return Err(Reason::SourceDiscontinuity);
        }
        let file = &mut self.validated.file;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| Reason::SourceDiscontinuity)?;
        let mut bytes = Vec::new();
        file.take((self.limits.file_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| Reason::SourceDiscontinuity)?;
        if bytes.len() > self.limits.file_bytes {
            return Err(Reason::CapacityExceeded);
        }
        if rollout_identity(&self.validated.file, &self.validated.path)
            .map_err(|_| Reason::SourceDiscontinuity)?
            != self.cursor.identity
        {
            return Err(Reason::SourceDiscontinuity);
        }
        let from = self.cursor.read_offset as usize;
        if bytes.len() < from || Sha256::digest(&bytes[..from])[..] != self.cursor.digest {
            return Err(Reason::SourceDiscontinuity);
        }
        for fragment in bytes[from..].split_inclusive(|byte| *byte == b'\n') {
            let complete = fragment.last() == Some(&b'\n');
            let data = if complete {
                &fragment[..fragment.len() - 1]
            } else {
                fragment
            };
            if data.len()
                > self
                    .limits
                    .record_bytes
                    .saturating_sub(self.cursor.partial.len())
            {
                return Err(Reason::CapacityExceeded);
            }
            self.cursor.partial.extend_from_slice(data);
            if !complete {
                continue;
            }
            let record: Value = serde_json::from_slice(&self.cursor.partial)
                .map_err(|_| Reason::MalformedRecord)?;
            if !self.initialized {
                if record["type"] != "session_meta"
                    || record["payload"]["id"].as_str() != Some(&self.validated.key().thread)
                    || record["payload"]["cli_version"].as_str()
                        != Some(&self.validated.metadata_version)
                {
                    return Err(Reason::SourceDiscontinuity);
                }
                self.cursor.next_ordinal = first_ordinal(&record)?;
                self.initialized = true;
            }
            verify_record(&record, &mut self.cursor.next_ordinal)?;
            self.cursor.complete_offset += self.cursor.partial.len() as u64 + 1;
            self.cursor.partial.clear();
        }
        if !self.initialized {
            return Err(Reason::SourceDiscontinuity);
        }
        self.cursor.read_offset = bytes.len() as u64;
        self.cursor.digest = Sha256::digest(&bytes).into();
        Ok(Update {
            bytes: bytes[from..].to_vec(),
            from: from as u64,
            checkpoint: self.cursor.clone(),
            eof_at: now,
        })
    }
}

fn first_ordinal(record: &Value) -> Result<Option<u64>, Reason> {
    let meta = &record["payload"];
    let mode = match meta.get("history_mode") {
        None => "legacy",
        Some(value) => value.as_str().ok_or(Reason::OrdinalDiscontinuity)?,
    };
    match mode {
        "legacy"
            if record.get("ordinal").is_none()
                && meta.get("history_base").is_none_or(Value::is_null) =>
        {
            Ok(None)
        }
        "paginated" => {
            let base = &meta["history_base"];
            let first = if base.is_null() {
                0
            } else {
                if base["thread_id"].as_str().is_none_or(|id| id.is_empty())
                    || base["end_byte_offset"].as_u64().is_none()
                {
                    return Err(Reason::OrdinalDiscontinuity);
                }
                base["end_ordinal_exclusive"]
                    .as_u64()
                    .ok_or(Reason::OrdinalDiscontinuity)?
            };
            Ok(Some(first))
        }
        _ => Err(Reason::OrdinalDiscontinuity),
    }
}

#[cfg(test)]
mod tests;
