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
    /// Stable node identity from the local runtime.
    pub node: String,
    /// Canonical pane/terminal identifier, independent of its label.
    pub pane: String,
}

/// A launch-attributed local writer, including PID reuse protection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Writer {
    /// Local PID attributed to this launch.
    pub pid: u32,
    /// Platform process start identity, protecting against PID reuse.
    pub birth: u64,
}

/// Immutable facts captured for a local writer launch, never copied from hook
/// JSON. Retain the home override and generation chosen before SessionStart.
pub(crate) struct Launch {
    /// Canonical local node/pane owning the writer.
    pub owner: Owner,
    /// `Binding::launch_generation()` captured when this writer launched.
    pub generation: u64,
    /// Absolute launch-resolved CODEX_HOME, with no profile fallback here.
    pub home: PathBuf,
    /// Local writer PID and process start identity.
    pub writer: Writer,
}

/// Full callback capability. Equal paths or labels do not grant this capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    /// Attested node and pane route.
    pub owner: Owner,
    /// Advances only on known launch/resume retirement.
    pub launch: u64,
    /// Advances for every attributed SessionStart claim.
    pub binding: u64,
    /// Opaque thread ID matched against first session metadata.
    pub thread: String,
}

/// Binding degradation facts, distinct from socket/CLI error codes.
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
    /// No claim after initial creation or known launch retirement.
    AwaitingFirstTurn,
    /// Rejected claim before this launch ever trusted a validated binding.
    BindingValidationFailed(Failure),
    /// Validation pending, or current-launch trust lost without retirement.
    Unknown,
    /// Binding attribution only, never lifecycle turn authority by itself.
    Bound,
}

/// The opened file is handed to #1663, avoiding a path reopen after validation.
/// Its cursor is reset for the reader to verify the full prefix and ordinals.
#[derive(Debug)]
pub(crate) struct Validated {
    key: Key,
    /// The same regular file opened during validation, positioned at byte zero.
    pub file: File,
    /// Resolved primary transcript path within the launch home.
    pub path: PathBuf,
    /// Verified local launch writer identity.
    pub writer: Writer,
    /// Version from actual first metadata, not a cached creation guess.
    pub metadata_version: String,
}

/// Validate on a worker, with a caller-supplied metadata byte budget. The writer
/// candidates must come from this local launch's process attribution, never hook
/// JSON or a filename. No environment/default profile lookup occurs here.
/// Version eligibility is supplied by S1, not re-probed for each claim.
pub(crate) fn validate(
    key: &Key,
    launch: &Launch,
    transcript: Option<&Path>,
    writers: &[Writer],
    compatibility: &Compatibility,
    metadata_limit: u64,
) -> Result<Validated, Failure> {
    if key.owner != launch.owner || key.launch != launch.generation {
        return Err(Failure::Route);
    }
    let home = &launch.home;
    let expected_writer = launch.writer;
    if writers != [expected_writer]
        || platform::process_start_time(expected_writer.pid) != Some(expected_writer.birth)
    {
        return Err(Failure::Writer);
    }
    if !home.is_absolute() {
        return Err(Failure::Path);
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
    // The launch probe can precede the deferred hook and have no metadata yet.
    let current = Compatibility {
        metadata_version: Some(version.to_owned()),
        writer_version: compatibility.writer_version.clone(),
        probe_failure: compatibility.probe_failure,
    };
    if !current.supports_lifecycle() {
        return Err(Failure::Version);
    }
    // Recover the same handle for full reader verification, including metadata.
    let mut file = reader.into_inner().into_inner();
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)).map_err(|_| Failure::Metadata)?;
    Ok(Validated {
        key: key.clone(),
        file,
        path,
        writer: expected_writer,
        metadata_version: version.to_owned(),
    })
}

/// Known live thread claims, never display-name lookup. Node identity stays in
/// each capability, but duplicate thread ownership is rejected across nodes too.
/// A conflict poisons all old capabilities until fresh validation resolves it.
#[derive(Default)]
pub(crate) struct Ownership(HashMap<String, Claims>);
struct Claims {
    keys: Vec<Key>,
    poisoned: bool,
}

impl Ownership {
    fn revoke(&mut self, key: &Key) {
        if let Some(claims) = self.0.get_mut(&key.thread) {
            claims.keys.retain(|owner| owner != key);
            if claims.keys.is_empty() {
                self.0.remove(&key.thread);
            }
        }
    }
    fn owns(&self, key: &Key) -> bool {
        self.0.get(&key.thread).is_some_and(|claims| {
            !claims.poisoned && claims.keys.len() == 1 && claims.keys.first() == Some(key)
        })
    }
    fn claim(&mut self, key: &Key) -> Result<(), Failure> {
        let claims = self.0.entry(key.thread.clone()).or_insert(Claims {
            keys: Vec::new(),
            poisoned: false,
        });
        claims.poisoned |= !claims.keys.is_empty();
        claims.keys.push(key.clone());
        if claims.poisoned {
            Err(Failure::DuplicateOwner)
        } else {
            Ok(())
        }
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
    pending: bool,
    key: Option<Key>,
    known: Option<(Key, Writer)>,
    decision: Decision,
    failure: Option<Failure>,
}

impl Binding {
    /// Create once per pane runtime, retaining this trust history across claims.
    /// Known launch/resume changes use `retire`, never a replacement object.
    pub fn new(owner: Owner) -> Self {
        Self {
            owner,
            launch: 0,
            generation: 0,
            trusted: false,
            pending: false,
            key: None,
            known: None,
            failure: None,
            decision: Decision::AwaitingFirstTurn,
        }
    }

    /// Capture this scope with the actual launch, not when a later claim arrives.
    pub fn launch_generation(&self) -> u64 {
        self.launch
    }

    /// Every attributed SessionStart, including compact and repeated IDs, revokes
    /// the previous generation immediately. A claim alone never resets trust.
    pub fn begin(&mut self, route: &Owner, launch: u64, thread: String) -> Result<Key, Failure> {
        if route != &self.owner || launch != self.launch {
            return Err(Failure::Route);
        }
        self.key = None;
        self.decision = Decision::Unknown;
        self.failure = None;
        self.pending = false;
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
        self.pending = true;
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
        if self.key.as_ref() != Some(key) || !self.pending {
            return Err(Failure::Route);
        }
        self.pending = false;
        let result = result.and_then(|value| {
            if &value.key != key {
                return Err(Failure::Route);
            }
            self.revoke_known(owners);
            self.known = Some((key.clone(), value.writer));
            owners.claim(key)?;
            Ok(value)
        });
        match result {
            Ok(value) => {
                self.trusted = true;
                self.decision = Decision::Bound;
                Ok(value)
            }
            Err(reason) => {
                self.reject(reason);
                Err(reason)
            }
        }
    }

    /// Only a currently owned validated capability can deliver reader callbacks.
    pub fn accepts(&self, key: &Key, owners: &Ownership) -> bool {
        self.key.as_ref() == Some(key) && self.decision == Decision::Bound && owners.owns(key)
    }

    /// Read trust state, including revocation by another known live claim.
    pub fn decision(&self, owners: &Ownership) -> Decision {
        if self.decision == Decision::Bound
            && !self.key.as_ref().is_some_and(|key| owners.owns(key))
        {
            Decision::Unknown
        } else {
            self.decision
        }
    }

    /// File/reader loss revokes authority but does not prove writer exit. Keep
    /// known ownership until verified exit, validated replacement or retirement.
    pub fn lost(&mut self, key: &Key) {
        if self.key.as_ref() == Some(key) {
            self.reject(Failure::SourceLost);
        }
    }

    /// Accept a verified local exit only for the known PID/start identity. The
    /// reader supplies this fact in #1663, never screen evidence or silence.
    pub fn writer_gone(&mut self, writer: Writer, owners: &mut Ownership) {
        if self
            .known
            .as_ref()
            .is_some_and(|(_, known)| *known == writer)
        {
            self.revoke_known(owners);
            self.reject(Failure::Writer);
        }
    }

    fn revoke_known(&mut self, owners: &mut Ownership) {
        if let Some((key, _)) = self.known.take() {
            owners.revoke(&key);
        }
    }

    fn reject(&mut self, reason: Failure) {
        self.key = None;
        self.pending = false;
        self.failure = Some(reason);
        self.decision = if self.trusted {
            Decision::Unknown
        } else {
            Decision::BindingValidationFailed(reason)
        };
    }

    /// Current diagnostic cause, retaining a duplicate-ownership revocation.
    pub fn failure(&self, owners: &Ownership) -> Option<Failure> {
        if self.decision == Decision::Bound && self.decision(owners) == Decision::Unknown {
            Some(Failure::DuplicateOwner)
        } else {
            self.failure
        }
    }

    /// Revoke before a known operator launch/resume or dropping/moving the pane.
    /// No typed in-TUI command or screen observation authorizes this transition.
    pub fn retire(&mut self, owners: &mut Ownership) -> Result<(), Failure> {
        self.revoke_known(owners);
        self.key = None;
        self.decision = Decision::Unknown;
        self.pending = false;
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
