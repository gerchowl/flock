//! Durable local command claims. The file is authoritative; leases serialize reload/rewrite.
use super::{ack, Command, ErrorCode, ServerFrame};
use crate::platform::{file_lock::LockFile, structured as files};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, Read, Write},
    path::PathBuf,
};

// Sixteen outstanding commands per credential, budgeted once per 30-second ack window.
const PRINCIPAL_LIMIT: usize =
    super::super::CREDENTIAL_CONNECTIONS * super::super::COMMANDS_PER_SOCKET * 2880;
const GLOBAL_LIMIT: usize =
    PRINCIPAL_LIMIT * super::super::CONNECTIONS as usize / super::super::CREDENTIAL_CONNECTIONS;
const ENTRY_BYTES: usize = super::super::MAX_MESSAGE + 2048;
// Raw torn bytes expand at most fourfold when preserved as a JSON byte array.
const LINE_BYTES: usize = ENTRY_BYTES * 4 + 32;
type Key = (String, String, String);
type Snapshot = (BTreeMap<Key, Record>, Vec<Vec<u8>>, bool);
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    namespace: String,
    principal: String,
    cmd_id: String,
    digest: [u8; 32],
    admitted_ms: u64,
    outcome: Option<Value>,
}
impl Record {
    fn key(&self) -> Key {
        (
            self.namespace.clone(),
            self.principal.clone(),
            self.cmd_id.clone(),
        )
    }
}
pub(super) enum Admission {
    Claimed(u64),
    Replayed(ServerFrame),
    Pending(u64),
}
pub(super) struct Journal {
    root: PathBuf,
    principal_limit: usize,
    global_limit: usize,
}
pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u64::MAX as u128) as u64)
}
impl Journal {
    pub(super) fn new(data: PathBuf) -> Self {
        Self {
            root: data.join("wire"),
            principal_limit: PRINCIPAL_LIMIT,
            global_limit: GLOBAL_LIMIT,
        }
    }
    fn lease(&self) -> io::Result<LockFile> {
        files::ensure_private_dir(&self.root)?;
        fs::File::open(self.root.parent().ok_or(io::ErrorKind::InvalidInput)?)?.sync_all()?;
        let path = self.root.join("commands.lock");
        let file = match files::create_private_file(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => files::open_append_file(&path)?,
            Err(e) => return Err(e),
        };
        let lock = LockFile::new(file);
        lock.try_lock().map_err(io::Error::other)?;
        Ok(lock)
    }
    fn load(&self) -> io::Result<Snapshot> {
        let file = match files::open_token(&self.root.join("commands.jsonl")) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
            Err(e) => return Err(e),
        };
        let mut reader = io::BufReader::new(file);
        let (mut records, mut raw, mut opaque) = (BTreeMap::new(), Vec::new(), false);
        loop {
            let mut line = Vec::new();
            if reader
                .by_ref()
                .take(LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)?
                == 0
            {
                break;
            }
            if line.len() > LINE_BYTES || records.len() + raw.len() >= self.global_limit {
                return Err(io::Error::other("command journal exceeds recovery bounds"));
            }
            if line.last() != Some(&b'\n') {
                tracing::warn!("preserving torn command journal tail");
                if line.len() <= ENTRY_BYTES {
                    raw.push(serde_json::to_vec(&serde_json::json!({"torn_tail":line}))?);
                } else {
                    opaque = true;
                    raw.push(line);
                }
                break;
            }
            match serde_json::from_slice::<Record>(&line) {
                Ok(record)
                    if record.version == 1
                        && line.len() <= ENTRY_BYTES
                        && record.outcome.as_ref().is_none_or(|value| {
                            matches!(serde_json::from_value::<ServerFrame>(value.clone()),
                            Ok(ServerFrame::Ack { cmd_id, .. }) if cmd_id == record.cmd_id)
                        }) =>
                {
                    let key = record.key();
                    if let std::collections::btree_map::Entry::Vacant(entry) = records.entry(key) {
                        entry.insert(record);
                    } else {
                        opaque = true;
                        raw.push(line);
                        tracing::warn!("duplicate command journal key; refusing mutations");
                    }
                }
                _ => {
                    let torn = serde_json::from_slice::<Value>(&line)
                        .ok()
                        .is_some_and(|v| v.get("torn_tail").is_some());
                    opaque |= !torn;
                    tracing::warn!("skipping opaque command journal entry; preserving raw bytes");
                    raw.push(line);
                }
            }
        }
        Ok((records, raw, opaque))
    }
    fn replace(&self, records: &BTreeMap<Key, Record>, raw: &[Vec<u8>]) -> io::Result<()> {
        let temp = self.root.join("commands.tmp");
        let mut file = match files::create_private_file(&temp) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => files::open_append_file(&temp)?,
            Err(e) => return Err(e),
        };
        file.set_len(0)?;
        for record in records.values() {
            serde_json::to_writer(&mut file, record)?;
            file.write_all(b"\n")?;
        }
        for line in raw {
            file.write_all(line)?;
            if line.last() != Some(&b'\n') {
                file.write_all(b"\n")?;
            }
        }
        file.sync_all()?;
        fs::rename(temp, self.root.join("commands.jsonl"))?;
        fs::File::open(&self.root)?.sync_all()
    }
    fn failure(error: io::Error) -> ErrorCode {
        crate::logging::client_wire_unavailable("command_journal", Some(error.kind()));
        ErrorCode::SessionBusy
    }
    /// Claim durably or return the persisted outcome without dispatching again.
    pub(super) fn claim(
        &self,
        principal: &str,
        command: &Command,
        now: u64,
    ) -> Result<Admission, ErrorCode> {
        self.claim_io(principal, command, now)
            .map_err(Self::failure)?
    }
    fn claim_io(
        &self,
        principal: &str,
        command: &Command,
        now: u64,
    ) -> io::Result<Result<Admission, ErrorCode>> {
        let _lease = self.lease()?;
        let (mut records, raw, opaque) = self.load()?;
        let key = ("local".to_owned(), principal.to_owned(), command.id.clone());
        let digest: [u8; 32] =
            Sha256::digest(serde_json::to_vec(&(&command.verb, &command.args))?).into();
        records
            .retain(|_, r| now.saturating_sub(r.admitted_ms) < super::super::IDEMPOTENCY_WINDOW_MS);
        if let Some(record) = records.get(&key) {
            return Ok(if record.digest != digest {
                Err(ErrorCode::InvalidParams)
            } else {
                match &record.outcome {
                    None => Ok(Admission::Pending(record.admitted_ms)),
                    Some(value) => serde_json::from_value(value.clone())
                        .map(Admission::Replayed)
                        .map_err(|_| ErrorCode::SessionUnconfirmed),
                }
            });
        }
        if command.resume_only {
            return Ok(Err(ErrorCode::SessionUnconfirmed));
        }
        if opaque
            || records.len() + raw.len() >= self.global_limit
            || records
                .values()
                .filter(|r| r.principal == principal)
                .count()
                >= self.principal_limit
        {
            return Ok(Err(ErrorCode::SessionBusy));
        }
        let record = Record {
            version: 1,
            namespace: key.0.clone(),
            principal: key.1.clone(),
            cmd_id: key.2.clone(),
            digest,
            admitted_ms: now,
            outcome: None,
        };
        if serde_json::to_vec(&record)?.len() + 1 > ENTRY_BYTES {
            return Ok(Err(ErrorCode::InvalidParams));
        }
        records.insert(key, record);
        self.replace(&records, &raw)?;
        Ok(Ok(Admission::Claimed(now)))
    }
    /// Persist before exposing the ack. A failed or oversized result leaves the claim uncertain.
    pub(super) fn finish(
        &self,
        principal: &str,
        id: &str,
        admitted_ms: u64,
        result: Result<Value, ErrorCode>,
    ) -> ServerFrame {
        let frame = ack(id.to_owned(), result);
        match self.finish_io(principal, id, admitted_ms, &frame) {
            Ok(()) => frame,
            Err(error) => {
                Self::failure(error);
                ack(id.to_owned(), Err(ErrorCode::SessionUnconfirmed))
            }
        }
    }
    fn finish_io(
        &self,
        principal: &str,
        id: &str,
        admitted_ms: u64,
        frame: &ServerFrame,
    ) -> io::Result<()> {
        let _lease = self.lease()?;
        let (mut records, raw, opaque) = self.load()?;
        if opaque {
            return Err(io::Error::other("opaque command journal"));
        }
        let key = ("local".to_owned(), principal.to_owned(), id.to_owned());
        let record = records
            .get_mut(&key)
            .ok_or_else(|| io::Error::other("command claim lost"))?;
        if record.admitted_ms != admitted_ms || record.outcome.is_some() {
            return Err(io::Error::other("command already completed"));
        }
        record.outcome = Some(serde_json::to_value(frame)?);
        if serde_json::to_vec(&record)?.len() + 1 > ENTRY_BYTES
            || serde_json::to_vec(frame)?.len() > super::super::MAX_MESSAGE
        {
            return Err(io::Error::other("command result exceeds durable bound"));
        }
        self.replace(&records, &raw)
    }
}
#[cfg(test)]
mod tests;
