//! Attributed segments and transactional admission (ADR-0028 §1a, #1856).
//! Call on the supervised worker (#1857). An attributed launch/SessionStart
//! transaction selects the mode; labels or time alone prove no ownership.
use super::*;
use crate::codex_binding::{Binding, Failure, Key, Ownership};
use crate::codex_rollout::Observation;
use std::{fs::File, path::Path};

/// Verified pre-launch bytes, including any incomplete inherited record. This
/// is history evidence, not a transferable writer/authority capability.
pub(crate) struct Snapshot {
    checkpoint: Checkpoint,
    thread: String,
}
impl Snapshot {
    /// Capture after quiescing the prior writer and before launching/resuming.
    /// The controller guarantees this ordering; the snapshot grants no writer identity.
    /// The caller opens the known local path beneath its attributed home using
    /// the binding's platform opener; this never discovers or follows remote paths.
    pub fn capture(file: &mut File, path: &Path, limits: Limits) -> Result<Self, Error> {
        let identity = rollout_identity(file, path).map_err(|_| Error::Attribution)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| Error::Attribution)?;
        let mut bytes = Vec::new();
        (&mut *file)
            .take((limits.file_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Attribution)?;
        if bytes.len() > limits.file_bytes {
            return Err(Error::Record(Reason::CapacityExceeded));
        }
        let mut next = None;
        let mut end = 0;
        let mut thread = None;
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            if line
                .len()
                .saturating_sub(usize::from(line.ends_with(b"\n")))
                > limits.record_bytes
            {
                return Err(Error::Record(Reason::CapacityExceeded));
            }
            if !line.ends_with(b"\n") {
                break;
            }
            let record: Value =
                serde_json::from_slice(line).map_err(|_| Error::Record(Reason::MalformedRecord))?;
            if thread.is_none() {
                if record["type"] != "session_meta" {
                    return Err(Error::Attribution);
                }
                thread = record["payload"]["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);
                if thread.is_none() {
                    return Err(Error::Attribution);
                }
                next = first_ordinal(&record).map_err(Error::Record)?;
            }
            verify_record(&record, &mut next).map_err(Error::Record)?;
            end += line.len();
        }
        if rollout_identity(file, path).map_err(|_| Error::Attribution)? != identity {
            return Err(Error::Attribution);
        }
        Ok(Self {
            thread: thread.ok_or(Error::Attribution)?,
            checkpoint: Checkpoint {
                identity,
                read_offset: bytes.len() as u64,
                complete_offset: end as u64,
                partial: bytes[end..].to_vec(),
                digest: Sha256::digest(&bytes).into(),
                next_ordinal: next,
            },
        })
    }
}

/// Admission failures retain binding's launch-scoped fallback/trust distinction.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Binding(Failure),
    Record(Reason),
    Attribution,
    Compaction,
}
impl Error {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compaction => "codex_compaction_continuity_unproven",
            Self::Attribution => "codex_segment_attribution_failed",
            Self::Binding(_) => "binding_validation_failed",
            Self::Record(reason) => reason.as_str(),
        }
    }
}

/// Only known local launch/resume intent selects a mode. A reported source alone
/// cannot retire a launch. Copies require a fresh destination-local validation.
pub(crate) enum Mode {
    Fresh,
    Resume { snapshot: Snapshot, copied: bool },
    Fork,
    Compact(Handoff),
}
/// Owned state captured while the previous capability was still accepted.
/// Private construction prevents cloning or manufacturing compaction continuity.
pub(crate) struct Handoff(Box<Segment>);

/// Prepared lifecycle evidence; server authority integration remains #1664.
pub(crate) struct Segment {
    reader: Reader,
    reducer: Reducer,
}
impl Segment {
    /// Verify attribution before committing binding trust. A failed candidate
    /// rejects the pending claim; stale callbacks cannot reject a newer claim.
    pub fn admit(
        binding: &mut Binding,
        key: &Key,
        validated: Result<Validated, Failure>,
        mode: Mode,
        owners: &mut Ownership,
        limits: Limits,
        now: Instant,
    ) -> Result<Self, Error> {
        if !binding.pending(key) {
            return Err(Error::Binding(Failure::Route));
        }
        let prepared = Self::prepare(key, validated, mode, limits, now);
        match prepared {
            Ok(mut segment) => {
                let file = binding
                    .finish(key, Ok(segment.reader.validated), owners)
                    .map_err(Error::Binding)?;
                segment.reader.validated = file;
                Ok(segment)
            }
            Err(error) => {
                let reason = match error {
                    Error::Binding(reason) => reason,
                    _ => Failure::Metadata,
                };
                let _ = binding.finish(key, Err(reason), owners);
                Err(error)
            }
        }
    }
    fn prepare(
        key: &Key,
        validated: Result<Validated, Failure>,
        mode: Mode,
        limits: Limits,
        now: Instant,
    ) -> Result<Self, Error> {
        let validated = validated.map_err(Error::Binding)?;
        if validated.key() != key {
            return Err(Error::Binding(Failure::Route));
        }
        let compact = matches!(&mode, Mode::Compact(_));
        let read_error = |reason| {
            if compact {
                Error::Compaction
            } else {
                Error::Record(reason)
            }
        };
        let mut reader = Reader::new(validated, limits).map_err(read_error)?;
        let update = reader.poll(now).map_err(read_error)?;
        let bytes = &update.bytes;
        let mut from = 0;
        let inherited = match mode {
            Mode::Compact(Handoff(old)) => {
                let old_key = old.reader.validated.key();
                if old_key.owner != key.owner
                    || old_key.launch != key.launch
                    || old_key.thread != key.thread
                    || old.reader.validated.writer != reader.validated.writer
                    || !matches_prefix(&old.reader.cursor, &reader, bytes, false)
                {
                    return Err(Error::Compaction);
                }
                from = old.reader.cursor.read_offset as usize;
                Some(old.reducer)
            }
            Mode::Resume { snapshot, copied } => {
                if snapshot.thread != key.thread
                    || !matches_prefix(&snapshot.checkpoint, &reader, bytes, copied)
                {
                    return Err(Error::Attribution);
                }
                from = snapshot.checkpoint.read_offset as usize;
                if !snapshot.checkpoint.partial.is_empty() {
                    from += bytes[from..]
                        .iter()
                        .position(|b| *b == b'\n')
                        .ok_or(Error::Attribution)?
                        + 1;
                }
                None
            }
            Mode::Fork => {
                let delimiters = records(bytes)?
                    .into_iter()
                    .filter(|(_, record)| {
                        record["type"] == "event_msg"
                            && record["payload"]["type"] == "thread_settings_applied"
                            && record["payload"]["thread_id"].as_str() == Some(&key.thread)
                    })
                    .map(|(end, _)| end)
                    .collect::<Vec<_>>();
                if delimiters.len() != 1 {
                    return Err(Error::Attribution);
                }
                from = delimiters[0];
                None
            }
            Mode::Fresh => {
                let first = records(bytes)?
                    .into_iter()
                    .next()
                    .ok_or(Error::Attribution)?
                    .1;
                if first["payload"]
                    .get("forked_from_id")
                    .is_some_and(|v| !v.is_null())
                    || first["payload"]
                        .get("history_base")
                        .is_some_and(|v| !v.is_null())
                {
                    return Err(Error::Attribution);
                }
                None
            }
        };
        let mut reducer = if let Some(reducer) = inherited {
            reducer
        } else {
            let mut offset = from;
            let start = records(&bytes[from..])?
                .into_iter()
                .find_map(|(end, record)| {
                    let begin = offset;
                    offset = from + end;
                    (record["type"] == "event_msg"
                        && matches!(
                            record["payload"]["type"].as_str(),
                            Some("task_started" | "turn_started")
                        ))
                    .then_some((begin, record))
                })
                .ok_or(Error::Attribution)?;
            from = start.0;
            limits.reducer(start.1.get("ordinal").and_then(Value::as_u64))
        };
        reducer.feed(&bytes[from..], now);
        if let Some(reason) = reducer
            .observe(now)
            .reason
            .filter(|r| *r != Reason::PartialRecord)
        {
            return Err(Error::Record(reason));
        }
        Ok(Self { reader, reducer })
    }
    /// Deliver only the current reader's verified suffix; source failure cannot
    /// leave a cached idle/working decision usable. Capability checks are #1857.
    pub fn poll(&mut self, now: Instant) -> Result<(), Error> {
        match self.reader.poll(now) {
            Ok(update) => self.reducer.feed(&update.bytes, now),
            Err(reason) => {
                self.reducer.invalidate(reason);
                return Err(Error::Record(reason));
            }
        }
        Ok(())
    }
    pub fn observe(&self, now: Instant) -> Observation<'_> {
        self.reducer.observe(now)
    }
    /// Capture before SessionStart revokes the prior binding generation.
    pub fn handoff(
        mut self,
        binding: &Binding,
        owners: &Ownership,
        now: Instant,
    ) -> Result<Handoff, Error> {
        if !binding.accepts(self.reader.validated.key(), owners) {
            return Err(Error::Compaction);
        }
        self.poll(now).map_err(|_| Error::Compaction)?;
        if self
            .observe(now)
            .reason
            .is_some_and(|r| r != Reason::PartialRecord)
        {
            return Err(Error::Compaction);
        }
        Ok(Handoff(Box::new(self)))
    }
}
fn matches_prefix(checkpoint: &Checkpoint, reader: &Reader, bytes: &[u8], copied: bool) -> bool {
    let end = checkpoint.read_offset as usize;
    (copied || checkpoint.identity == reader.cursor.identity)
        && bytes.len() >= end
        && Sha256::digest(&bytes[..end])[..] == checkpoint.digest
}
// Complete records only: partial EOF is retained by Reader/Reducer, never used
// as a segment delimiter or as evidence of a new writer's start.
fn records(bytes: &[u8]) -> Result<Vec<(usize, Value)>, Error> {
    let mut end = 0;
    bytes
        .split_inclusive(|b| *b == b'\n')
        .filter(|line| line.ends_with(b"\n"))
        .map(|line| {
            end += line.len();
            serde_json::from_slice(line)
                .map(|record| (end, record))
                .map_err(|_| Error::Record(Reason::MalformedRecord))
        })
        .collect()
}

#[cfg(test)]
mod tests;
