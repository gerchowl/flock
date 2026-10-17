//! Verified incremental rollout bytes for ADR-0028 §1a (#1855).
//!
//! This is a file/cursor foundation, not runtime lifecycle authority. Read on a
//! supervised worker (#1857); prepare resume/fork segments before trust admission
//! (#1856). A successful empty EOF never changes a turn or renews a lease.
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;
use std::{fs::File, path::Path};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::codex_binding::Validated;
use crate::codex_rollout::{verify_record, Reason, Reducer};
use crate::platform::structured::{rollout_identity, FileIdentity};

/// Per-step I/O and per-record/turn memory budgets, never a total-file cap.
/// A finite history can span arbitrarily many steps. Record/turn overflow still
/// degrades explicitly; history and remembered IDs are never silently evicted.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub work_bytes: usize,
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
/// Only newline-terminated records are validated. Partial final bytes remain
/// unvalidated and must stay buffered until their terminating newline arrives.
pub(crate) struct Update {
    pub bytes: Vec<u8>,
    pub from: u64,
    pub checkpoint: Checkpoint,
    /// Successful EOF observation, even when no bytes were appended. None means
    /// bounded work is still in progress and must never be reported as EOF.
    pub eof_at: Option<Instant>,
    /// Bytes scanned this step, including prefix verification, not model activity.
    pub scanned: usize,
}

/// Owns the same descriptor returned by binding validation. Never searches by
/// filename or reopens a replacement as a new source. Failures are sticky;
/// reconstruction needs a new independently verified binding/segment.
pub(crate) struct Reader {
    validated: Validated,
    scan: Scan,
    failure: Option<Reason>,
}
impl Reader {
    /// Accept the original opened candidate, before lifecycle trust admission.
    pub fn new(validated: Validated, limits: Limits) -> Result<Self, Reason> {
        let scan = Scan::new(&validated.file, &validated.path, limits)?;
        Ok(Self {
            validated,
            scan,
            failure: None,
        })
    }
    /// One bounded work unit. Empty bytes without EOF mean verification progress,
    /// never idle. The next call continues the same hash pass, not a prefix reread.
    pub fn poll(&mut self, now: Instant) -> Result<Update, Reason> {
        self.records(now, |_, _, _| {})
    }
    fn records(
        &mut self,
        now: Instant,
        mut emit: impl FnMut(&Value, &[u8], u64),
    ) -> Result<Update, Reason> {
        if let Some(reason) = self.failure {
            return Err(reason);
        }
        let thread = self.validated.key().thread.clone();
        let mut metadata_matches = true;
        let result = self
            .scan
            .step(
                &mut self.validated.file,
                &self.validated.path,
                now,
                |record, bytes, begin| {
                    if begin == 0 {
                        metadata_matches = record["payload"]["id"].as_str() == Some(&thread)
                            && record["payload"]["cli_version"].as_str()
                                == Some(&self.validated.metadata_version);
                    }
                    emit(record, bytes, begin);
                },
            )
            .and_then(|update| {
                if metadata_matches {
                    Ok(update)
                } else {
                    Err(Reason::SourceDiscontinuity)
                }
            });
        if let Err(reason) = result {
            self.failure = Some(reason);
        }
        result
    }
}

/// One hash pass continues across work units: compare the old prefix digest at
/// its exact offset, then extend that same hasher through new bytes up to EOF.
/// No result may establish authority while that prefix comparison is pending.
struct Pass {
    hash: Sha256,
    offset: u64,
    verified: bool,
}
struct Scan {
    cursor: Checkpoint,
    limits: Limits,
    pass: Option<Pass>,
}
impl Scan {
    fn new(file: &File, path: &Path, limits: Limits) -> Result<Self, Reason> {
        if limits.work_bytes == 0 {
            return Err(Reason::CapacityExceeded);
        }
        Ok(Self {
            cursor: Checkpoint {
                identity: rollout_identity(file, path).map_err(|_| Reason::SourceDiscontinuity)?,
                read_offset: 0,
                complete_offset: 0,
                partial: Vec::new(),
                digest: Sha256::digest([]).into(),
                next_ordinal: None,
            },
            limits,
            pass: None,
        })
    }
    fn step(
        &mut self,
        file: &mut File,
        path: &Path,
        now: Instant,
        mut emit: impl FnMut(&Value, &[u8], u64),
    ) -> Result<Update, Reason> {
        self.check(file, path)?;
        let mut pass = self.pass.take().unwrap_or(Pass {
            hash: Sha256::new(),
            offset: 0,
            verified: false,
        });
        file.seek(SeekFrom::Start(pass.offset))
            .map_err(|_| Reason::SourceDiscontinuity)?;
        let from = self.cursor.read_offset;
        let mut bytes = Vec::new();
        let mut scanned = 0;
        let mut eof = false;
        let mut buffer = [0; 8192];
        while scanned < self.limits.work_bytes {
            if !pass.verified && pass.offset == self.cursor.read_offset {
                if pass.hash.clone().finalize()[..] != self.cursor.digest {
                    return Err(Reason::SourceDiscontinuity);
                }
                pass.verified = true;
            }
            let mut size = buffer.len().min(self.limits.work_bytes - scanned);
            if !pass.verified {
                size = size.min((self.cursor.read_offset - pass.offset).min(size as u64) as usize);
            }
            let n = file
                .read(&mut buffer[..size])
                .map_err(|_| Reason::SourceDiscontinuity)?;
            if n == 0 {
                if !pass.verified {
                    return Err(Reason::SourceDiscontinuity);
                }
                eof = true;
                break;
            }
            pass.hash.update(&buffer[..n]);
            pass.offset += n as u64;
            scanned += n;
            if pass.verified {
                bytes.extend_from_slice(&buffer[..n]);
            }
        }
        self.check(file, path)?;
        if file
            .metadata()
            .map_err(|_| Reason::SourceDiscontinuity)?
            .len()
            < pass.offset
        {
            return Err(Reason::SourceDiscontinuity);
        }
        for fragment in bytes.split_inclusive(|b| *b == b'\n') {
            let complete = fragment.ends_with(b"\n");
            let data = &fragment[..fragment.len() - usize::from(complete)];
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
            if self.cursor.complete_offset == 0 {
                if record["type"] != "session_meta"
                    || record["payload"]["id"].as_str().is_none_or(str::is_empty)
                {
                    return Err(Reason::SourceDiscontinuity);
                }
                self.cursor.next_ordinal = first_ordinal(&record)?;
            }
            verify_record(&record, &mut self.cursor.next_ordinal)?;
            emit(&record, &self.cursor.partial, self.cursor.complete_offset);
            self.cursor.complete_offset += self.cursor.partial.len() as u64 + 1;
            self.cursor.partial.clear();
        }
        if eof && self.cursor.complete_offset == 0 {
            return Err(Reason::SourceDiscontinuity);
        }
        if pass.verified {
            self.cursor.read_offset = pass.offset;
            self.cursor.digest = pass.hash.clone().finalize().into();
        }
        if !eof {
            self.pass = Some(pass);
        }
        Ok(Update {
            bytes,
            from,
            checkpoint: self.cursor.clone(),
            eof_at: eof.then_some(now),
            scanned,
        })
    }
    fn check(&self, file: &File, path: &Path) -> Result<(), Reason> {
        if rollout_identity(file, path).map_err(|_| Reason::SourceDiscontinuity)?
            != self.cursor.identity
        {
            return Err(Reason::SourceDiscontinuity);
        }
        Ok(())
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

pub(crate) mod segment;

#[cfg(test)]
mod tests;
