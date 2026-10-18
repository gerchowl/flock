//! Attributed segments and transactional admission (ADR-0028 §1a, #1856).
//! Call on the supervised worker (#1857). An attributed launch/SessionStart
//! transaction selects the mode; labels or time alone prove no ownership.
use super::*;
use crate::codex_binding::{Binding, Failure, Key, Ownership};
use crate::codex_rollout::Observation;
use std::fs::File;

/// Verified pre-launch bytes, including any incomplete inherited record. This
/// is history evidence, not a transferable writer/authority capability.
pub(crate) struct Snapshot {
    checkpoint: Checkpoint,
    thread: String,
}
/// Incremental pre-launch capture. The caller keeps the old writer quiescent
/// throughout capture and starts the next writer only after successful EOF.
/// Cancellation drops this object; no background work or notification wait.
pub(crate) struct Capture {
    file: File,
    path: std::path::PathBuf,
    scan: Scan,
    thread: Option<String>,
    failed: bool,
}
impl Capture {
    /// Open the known local file beneath the launch home with the binding's
    /// platform opener. This operation never discovers or follows remote paths.
    pub fn new(file: File, path: std::path::PathBuf, limits: Limits) -> Result<Self, Error> {
        let scan = Scan::new(&file, &path, limits).map_err(Error::Record)?;
        Ok(Self {
            file,
            path,
            scan,
            thread: None,
            failed: false,
        })
    }
    /// At most one work budget; None is progress, not a completed snapshot.
    /// After an error or completed capture, this object cannot be reused.
    pub fn step(&mut self, now: Instant) -> Result<Option<Snapshot>, Error> {
        if self.failed {
            return Err(Error::Attribution);
        }
        let result = self
            .scan
            .step(&mut self.file, &self.path, now, |record, _, begin| {
                if begin == 0 {
                    self.thread = record["payload"]["id"].as_str().map(str::to_owned);
                }
            });
        self.failed = result.is_err();
        let update = result.map_err(Error::Record)?;
        if update.eof_at.is_none() {
            return Ok(None);
        }
        self.failed = true;
        Ok(Some(Snapshot {
            checkpoint: update.checkpoint,
            thread: self.thread.take().ok_or(Error::Attribution)?,
        }))
    }
}

/// Admission failures retain binding's launch-scoped fallback/trust distinction.
#[derive(Clone, Debug, PartialEq, Eq)]
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
/// A bounded admission transaction. Trust stays pending until verified EOF;
/// dropping it cancels local work and the controller must reject/replace its claim.
pub(crate) struct Admission {
    key: Key,
    reader: Option<Reader>,
    reducer: Option<Reducer>,
    mode: Mode,
    minimum: u64,
    delimiters: usize,
    failure: Option<Error>,
    compact: bool,
}
impl Admission {
    /// Prepare local ownership/continuity constraints; no file-sized I/O here.
    /// The current pending claim is rejected on failure, preserving launch trust.
    pub fn begin(
        binding: &mut Binding,
        key: &Key,
        validated: Result<Validated, Failure>,
        mode: Mode,
        owners: &mut Ownership,
        limits: Limits,
    ) -> Result<Self, Error> {
        if !binding.pending(key) {
            return Err(Error::Binding(Failure::Route));
        }
        let result = Self::prepare(key, validated, mode, limits);
        if let Err(error) = &result {
            let reason = if let Error::Binding(reason) = error {
                *reason
            } else {
                Failure::Metadata
            };
            let _ = binding.finish(key, Err(reason), owners);
        }
        result
    }
    fn prepare(
        key: &Key,
        validated: Result<Validated, Failure>,
        mode: Mode,
        limits: Limits,
    ) -> Result<Self, Error> {
        let validated = validated.map_err(Error::Binding)?;
        if validated.key() != key {
            return Err(Error::Binding(Failure::Route));
        }
        let mut reader = Reader::new(validated, limits).map_err(Error::Record)?;
        let mut reducer = None;
        let mut minimum = 0;
        let compact = matches!(mode, Mode::Compact(_));
        let mode = match mode {
            Mode::Compact(Handoff(old)) => {
                let prior = old.reader.validated.key();
                if prior.owner != key.owner
                    || prior.launch != key.launch
                    || prior.thread != key.thread
                    || old.reader.validated.writer != reader.validated.writer
                {
                    return Err(Error::Compaction);
                }
                seed(&mut reader, old.reader.scan.cursor, false).map_err(|_| Error::Compaction)?;
                reducer = Some(old.reducer);
                Mode::Fresh
            }
            Mode::Resume { snapshot, copied } => {
                if snapshot.thread != key.thread {
                    return Err(Error::Attribution);
                }
                minimum = snapshot.checkpoint.read_offset;
                seed(&mut reader, snapshot.checkpoint, copied)?;
                Mode::Fresh
            }
            other => other,
        };
        Ok(Self {
            key: key.clone(),
            reader: Some(reader),
            reducer,
            mode,
            minimum,
            delimiters: 0,
            failure: None,
            compact,
        })
    }
    /// One bounded scan. Only this still-pending capability can commit trust.
    /// Stale callbacks cannot fail or finish a newer claim. Errors are terminal.
    pub fn step(
        &mut self,
        binding: &mut Binding,
        owners: &mut Ownership,
        now: Instant,
    ) -> Result<Option<Segment>, Error> {
        if !binding.pending(&self.key) {
            return Err(Error::Binding(Failure::Route));
        }
        let result = self.advance(now);
        match result {
            Ok(Some(mut segment)) => {
                segment.reader.validated = binding
                    .finish(&self.key, Ok(segment.reader.validated), owners)
                    .map_err(Error::Binding)?;
                Ok(Some(segment))
            }
            Err(error) => {
                let reason = if let Error::Binding(reason) = error {
                    reason
                } else {
                    Failure::Metadata
                };
                let _ = binding.finish(&self.key, Err(reason), owners);
                Err(error)
            }
            pending => pending,
        }
    }
    fn advance(&mut self, now: Instant) -> Result<Option<Segment>, Error> {
        let reader = self.reader.as_mut().ok_or(Error::Attribution)?;
        let compact = self.compact;
        let limits = reader.scan.limits;
        let update = reader
            .records(now, |record, bytes, begin| {
                if compact || self.failure.is_some() {
                    return;
                }
                if begin == 0
                    && matches!(self.mode, Mode::Fresh)
                    && self.minimum == 0
                    && ["forked_from_id", "history_base"]
                        .iter()
                        .any(|k| record["payload"].get(k).is_some_and(|v| !v.is_null()))
                {
                    self.failure = Some(Error::Attribution);
                }
                if record["type"] == "event_msg"
                    && record["payload"]["type"] == "thread_settings_applied"
                    && record["payload"]["thread_id"].as_str() == Some(&self.key.thread)
                    && matches!(self.mode, Mode::Fork)
                {
                    self.delimiters += 1;
                    if self.delimiters != 1 {
                        self.failure = Some(Error::Attribution);
                    }
                }
                if begin < self.minimum || (matches!(self.mode, Mode::Fork) && self.delimiters != 1)
                {
                    return;
                }
                if self.reducer.is_none()
                    && record["type"] == "event_msg"
                    && matches!(
                        record["payload"]["type"].as_str(),
                        Some("task_started" | "turn_started")
                    )
                {
                    self.reducer =
                        Some(limits.reducer(record.get("ordinal").and_then(Value::as_u64)));
                }
                if let Some(reducer) = &mut self.reducer {
                    reducer.feed(bytes, now);
                    reducer.feed(b"\n", now);
                }
            })
            .map_err(|reason| {
                if compact {
                    Error::Compaction
                } else {
                    Error::Record(reason)
                }
            })?;
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        if compact {
            self.reducer
                .as_mut()
                .ok_or(Error::Compaction)?
                .feed(&update.bytes, now);
        }
        if let Some(reason) = self
            .reducer
            .as_ref()
            .and_then(|r| r.observe(now).reason)
            .filter(|r| *r != Reason::PartialRecord)
        {
            return Err(Error::Record(reason));
        }
        if update.eof_at.is_none() {
            return Ok(None);
        }
        let mut reducer = self.reducer.take().ok_or(Error::Attribution)?;
        if !compact {
            reducer.feed(&update.checkpoint.partial, now);
        }
        if let Some(reason) = reducer
            .observe(now)
            .reason
            .filter(|r| *r != Reason::PartialRecord)
        {
            return Err(Error::Record(reason));
        }
        Ok(Some(Segment {
            reader: self.reader.take().ok_or(Error::Attribution)?,
            reducer,
        }))
    }
}
fn seed(reader: &mut Reader, mut checkpoint: Checkpoint, copied: bool) -> Result<(), Error> {
    if !copied && checkpoint.identity != reader.scan.cursor.identity {
        return Err(Error::Attribution);
    }
    checkpoint.identity = reader.scan.cursor.identity;
    reader.scan.cursor = checkpoint;
    Ok(())
}
impl Segment {
    /// Deliver only the current reader's verified suffix; source failure cannot
    /// leave a cached idle/working decision usable. Capability checks are #1857.
    pub fn poll(&mut self, now: Instant) -> Result<Update, Error> {
        match self.reader.poll(now) {
            Ok(update) => {
                self.reducer.feed(&update.bytes, now);
                Ok(update)
            }
            Err(reason) => {
                self.reducer.invalidate(reason);
                Err(Error::Record(reason))
            }
        }
    }
    pub fn observe(&self, now: Instant) -> Observation<'_> {
        self.reducer.observe(now)
    }
    /// Capture the owned verified cursor before SessionStart revokes its capability.
    /// Admission rechecks its entire prefix and consumes subsequent appended bytes.
    pub fn handoff(
        self,
        binding: &Binding,
        owners: &Ownership,
        now: Instant,
    ) -> Result<Handoff, Error> {
        if !binding.accepts(self.reader.validated.key(), owners) {
            return Err(Error::Compaction);
        }
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
pub(crate) mod worker;

#[cfg(test)]
mod tests;
