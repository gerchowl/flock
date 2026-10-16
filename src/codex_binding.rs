//! Attributed rollout binding and launch-scoped trust (ADR-0028 §1a).
//!
//! Runtime supplies the launch-resolved home, local writer candidates and S1
//! probe result. This foundation never discovers files, enables authority or
//! treats a rebind as a launch retirement. Reader continuity belongs to #1663.
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::{codex_version::Compatibility, platform};

/// Routing identity captured from the local pane, never a display name or alias.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Owner {
    pub node: String,
    pub pane: String,
}

/// A launch-attributed local writer, including PID reuse protection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Writer {
    pub pid: u32,
    pub birth: u64,
}

/// Full callback capability. Equal paths or labels do not grant this capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub owner: Owner,
    pub launch: u64,
    pub binding: u64,
    pub thread: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Route,
    Path,
    Metadata,
    Writer,
    Version,
    DuplicateOwner,
    SourceLost,
    GenerationExhausted,
}

/// The authority slice selects screen evidence only for the two fallback cases.
/// Neither fallback case manufactures idle or waits for another SessionStart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    AwaitingFirstTurn,
    BindingValidationFailed(Failure),
    Unknown,
    Bound,
}

/// The opened file is handed to #1663, avoiding a path reopen after validation.
/// Its cursor follows the complete first metadata line, not a turn boundary.
#[derive(Debug)]
pub(crate) struct Validated {
    key: Key,
    pub file: File,
    pub path: PathBuf,
    pub writer: Writer,
    pub metadata_version: String,
}

/// Validate on a worker, with a caller-supplied metadata byte budget. The writer
/// candidates must come from this local launch's process attribution, never hook
/// JSON or a filename. No environment/default profile lookup occurs here.
/// Version eligibility is supplied by S1, not re-probed for each claim.
pub(crate) fn validate(
    key: &Key,
    home: &Path,
    transcript: Option<&Path>,
    expected_writer: Writer,
    writers: &[Writer],
    compatibility: &Compatibility,
    metadata_limit: u64,
) -> Result<Validated, Failure> {
    if writers != [expected_writer]
        || platform::process_start_time(expected_writer.pid) != Some(expected_writer.birth)
    {
        return Err(Failure::Writer);
    }
    let home = home.canonicalize().map_err(|_| Failure::Path)?;
    let path = transcript
        .filter(|path| path.is_absolute())
        .ok_or(Failure::Path)?
        .canonicalize()
        .map_err(|_| Failure::Path)?;
    if !path.starts_with(&home) {
        return Err(Failure::Path);
    }
    let relative = path.strip_prefix(&home).map_err(|_| Failure::Path)?;
    let file =
        platform::structured::open_regular_beneath(&home, relative).map_err(|_| Failure::Path)?;
    let mut line = Vec::new();
    let mut reader = BufReader::new(file.take(metadata_limit.saturating_add(1)));
    reader
        .read_until(b'\n', &mut line)
        .map_err(|_| Failure::Metadata)?;
    if line.len() as u64 > metadata_limit || line.last() != Some(&b'\n') {
        return Err(Failure::Metadata);
    }
    let meta: serde_json::Value = serde_json::from_slice(&line).map_err(|_| Failure::Metadata)?;
    if meta["type"] != "session_meta"
        || meta["payload"]["id"].as_str() != Some(&key.thread)
        || key.thread.is_empty()
    {
        return Err(Failure::Metadata);
    }
    let version = meta["payload"]["cli_version"]
        .as_str()
        .ok_or(Failure::Version)?;
    if !compatibility.supports_lifecycle()
        || compatibility.metadata_version.as_deref() != Some(version)
    {
        return Err(Failure::Version);
    }
    // BufReader may read ahead. Recover the same handle and seek only past metadata.
    let mut file = reader.into_inner().into_inner();
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(line.len() as u64))
        .map_err(|_| Failure::Metadata)?;
    Ok(Validated {
        key: key.clone(),
        file,
        path,
        writer: expected_writer,
        metadata_version: version.to_owned(),
    })
}

/// One local runtime registry, shared by all pane bindings on this node.
/// An ownership conflict revokes both capabilities, never last-callback-wins.
#[derive(Default)]
pub(crate) struct Ownership(HashMap<(String, String), (Key, bool)>);

impl Ownership {
    fn slot(key: &Key) -> (String, String) {
        (key.owner.node.clone(), key.thread.clone())
    }
    fn revoke(&mut self, key: &Key) {
        let slot = Self::slot(key);
        if self.0.get(&slot).is_some_and(|(owner, _)| owner == key) {
            self.0.remove(&slot);
        }
    }
    fn owns(&self, key: &Key) -> bool {
        self.0
            .get(&Self::slot(key))
            .is_some_and(|(owner, conflict)| owner == key && !conflict)
    }
}

/// Pure launch trust. Runtime calls `retire` only for a known operator launch or
/// resume, or source retirement in a move. Typed commands and screen evidence
/// cannot call it. There is no in-TUI replacement-confirmation producer (#1479).
pub(crate) struct Binding {
    owner: Owner,
    launch: u64,
    generation: u64,
    trusted: bool,
    key: Option<Key>,
    decision: Decision,
    failure: Option<Failure>,
}

impl Binding {
    pub fn new(owner: Owner) -> Self {
        Self {
            owner,
            launch: 0,
            generation: 0,
            trusted: false,
            key: None,
            failure: None,
            decision: Decision::AwaitingFirstTurn,
        }
    }

    /// Every attributed SessionStart, including compact and repeated IDs, revokes
    /// the previous generation immediately. A claim alone never resets trust.
    pub fn begin(
        &mut self,
        route: &Owner,
        thread: String,
        owners: &mut Ownership,
    ) -> Result<Key, Failure> {
        if route != &self.owner {
            return Err(Failure::Route);
        }
        if let Some(key) = self.key.take() {
            owners.revoke(&key);
        }
        self.decision = Decision::Unknown;
        self.failure = None;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(Failure::GenerationExhausted)?;
        let key = Key {
            owner: self.owner.clone(),
            launch: self.launch,
            binding: self.generation,
            thread,
        };
        self.key = Some(key.clone());
        Ok(key)
    }

    /// Complete exactly the still-pending capability. Rejected and superseded
    /// callbacks cannot install a file, even if all visible labels/paths match.
    pub fn finish(
        &mut self,
        key: &Key,
        result: Result<Validated, Failure>,
        owners: &mut Ownership,
    ) -> Result<Validated, Failure> {
        if self.key.as_ref() != Some(key) || self.decision != Decision::Unknown {
            return Err(Failure::Route);
        }
        let result = result.and_then(|value| {
            if &value.key != key {
                return Err(Failure::Route);
            }
            let slot = Ownership::slot(key);
            if let Some((_, conflict)) = owners.0.get_mut(&slot) {
                *conflict = true;
                return Err(Failure::DuplicateOwner);
            }
            owners.0.insert(slot, (key.clone(), false));
            Ok(value)
        });
        match result {
            Ok(value) => {
                self.trusted = true;
                self.decision = Decision::Bound;
                Ok(value)
            }
            Err(reason) => {
                self.key = None;
                self.failure = Some(reason);
                self.decision = if self.trusted {
                    Decision::Unknown
                } else {
                    Decision::BindingValidationFailed(reason)
                };
                Err(reason)
            }
        }
    }

    /// Only a currently owned validated capability can deliver reader callbacks.
    pub fn accepts(&self, key: &Key, owners: &Ownership) -> bool {
        self.key.as_ref() == Some(key) && self.decision == Decision::Bound && owners.owns(key)
    }

    pub fn decision(&self, owners: &Ownership) -> Decision {
        if self.decision == Decision::Bound
            && !self.key.as_ref().is_some_and(|key| owners.owns(key))
        {
            Decision::Unknown
        } else {
            self.decision
        }
    }

    /// Loss is sticky within this launch's trust scope. It never authorizes fallback.
    pub fn lost(&mut self, key: &Key, owners: &mut Ownership) {
        if self.accepts(key, owners) {
            owners.revoke(key);
            self.key = None;
            self.decision = Decision::Unknown;
            self.failure = Some(Failure::SourceLost);
        }
    }

    pub fn failure(&self, owners: &Ownership) -> Option<Failure> {
        if self.decision == Decision::Bound && self.decision(owners) == Decision::Unknown {
            Some(Failure::DuplicateOwner)
        } else {
            self.failure
        }
    }

    pub fn retire(&mut self, owners: &mut Ownership) -> Result<(), Failure> {
        if let Some(key) = self.key.take() {
            owners.revoke(&key);
        }
        self.decision = Decision::Unknown;
        self.launch = self
            .launch
            .checked_add(1)
            .ok_or(Failure::GenerationExhausted)?;
        self.trusted = false;
        self.failure = None;
        self.decision = Decision::AwaitingFirstTurn;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
