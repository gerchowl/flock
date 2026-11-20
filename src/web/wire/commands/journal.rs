//! Dormant command journal: no live wire command uses this backend yet.
//! Opening and shutdown are asynchronous lease barriers. Compaction copies a fixed
//! prefix off-thread, catches up appends, then publishes a bounded tail atomically.
//! J1b2 supplies live admission, fresh authorization, dispatch and outcome-before-ack.
use super::{ack, Command, ErrorCode, ServerFrame};
use crate::platform::{file_lock::LockFile, structured as files};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
};
use tokio::sync::{oneshot, watch, Notify};
mod compact;
const PRINCIPAL_LIMIT: usize =
    super::super::CREDENTIAL_CONNECTIONS * super::super::COMMANDS_PER_SOCKET * 2880;
const GLOBAL_LIMIT: usize =
    PRINCIPAL_LIMIT * super::super::CONNECTIONS as usize / super::super::CREDENTIAL_CONNECTIONS;
const ENTRY_BYTES: usize = super::super::MAX_MESSAGE + 2048;
const LOG_BYTES: u64 = (GLOBAL_LIMIT * ENTRY_BYTES * 2) as u64;
const QUEUED: usize = 32;
type Key = (String, String, String);
#[derive(Default)]
struct Index {
    records: HashMap<Key, Record>,
    counts: HashMap<(String, String), usize>,
}
#[cfg(not(test))]
type Stream = File;
#[cfg(test)]
use tests::Stream;
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
    fn decode(line: &[u8]) -> Option<Self> {
        serde_json::from_slice::<Self>(line).ok().filter(|r| r.version == 1 && r.outcome.as_ref().is_none_or(|v| {
            matches!(serde_json::from_value::<ServerFrame>(v.clone()), Ok(ServerFrame::Ack {cmd_id, ..}) if cmd_id == r.cmd_id)
        }))
    }
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
pub(super) enum Operation {
    Claim {
        namespace: String,
        principal: String,
        command: Command,
        now: u64,
    },
    Finish {
        key: Key,
        admitted_ms: u64,
        result: Result<Value, ErrorCode>,
    },
}
type Response = oneshot::Receiver<Result<Admission, ErrorCode>>;
type Job = (Operation, oneshot::Sender<Result<Admission, ErrorCode>>);
enum Message {
    Job(Job),
    Compacted(io::Result<compact::Snapshot>),
}
#[derive(Clone)]
pub(super) struct Writer {
    admission: Arc<parking_lot::Mutex<Option<mpsc::SyncSender<Message>>>>,
    closed: watch::Receiver<bool>,
}
impl Writer {
    /// Return only after replay and exclusive lease acquisition succeed off-thread.
    /// A held lease returns typed busy, never a permanently failed usable writer.
    pub(super) async fn open(data: PathBuf, wake: Arc<Notify>) -> Result<Self, ErrorCode> {
        Self::spawn(move || Journal::open(&data), wake).await
    }
    async fn spawn(
        open: impl FnOnce() -> io::Result<Journal> + Send + 'static,
        wake: Arc<Notify>,
    ) -> Result<Self, ErrorCode> {
        let (tx, rx) = mpsc::sync_channel::<Message>(QUEUED);
        let admission = Arc::new(parking_lot::Mutex::new(Some(tx)));
        let weak = Arc::downgrade(&admission);
        let (opened, ready) = oneshot::channel();
        let (closed, completion) = watch::channel(false);
        std::thread::Builder::new()
            .name("wire-journal".into())
            .spawn(move || {
                if let Ok(mut journal) = open().inspect_err(unavailable) {
                    let _ = opened.send(Ok(()));
                    while let Ok(message) = rx.recv() {
                        match message {
                            Message::Job((operation, reply)) => {
                                let now = match &operation {
                                    Operation::Claim { now, .. } => Some(*now),
                                    _ => None,
                                };
                                let _ = reply.send(journal.apply(operation));
                                if let Some(now) = now {
                                    if let Some(admission) = weak.upgrade() {
                                        let tx = admission.lock().as_ref().cloned();
                                        if let Some(tx) = tx {
                                            journal.start_compaction(now, tx);
                                        }
                                    }
                                }
                            }
                            Message::Compacted(result) => journal.compacted(result),
                        }
                        wake.notify_one();
                    }
                    // The lease is released before shutdown completion becomes observable.
                    drop(journal);
                } else {
                    let _ = opened.send(Err(ErrorCode::SessionBusy));
                }
                closed.send_replace(true);
            })
            .map_err(|_| ErrorCode::SessionBusy)?;
        ready.await.map_err(|_| ErrorCode::SessionBusy)??;
        Ok(Self {
            admission,
            closed: completion,
        })
    }
    /// Admission never waits on filesystem work. Cancelling a reply cannot cancel a job.
    pub(super) fn submit(&self, operation: Operation) -> Result<Response, ErrorCode> {
        let (tx, rx) = oneshot::channel();
        self.admission
            .lock()
            .as_ref()
            .ok_or(ErrorCode::SessionBusy)?
            .try_send(Message::Job((operation, tx)))
            .map_err(|_| ErrorCode::SessionBusy)?;
        Ok(rx)
    }
    /// Close all clones' admission, drain accepted jobs and compaction, release the lease.
    /// Dropping this waiter does not cancel shutdown. Dropping all handles also drains.
    pub(super) async fn shutdown(&self) -> Result<(), ErrorCode> {
        self.admission.lock().take();
        let mut closed = self.closed.clone();
        closed
            .wait_for(|done| *done)
            .await
            .map_err(|_| ErrorCode::SessionBusy)?;
        Ok(())
    }
}
fn unavailable(error: &io::Error) {
    crate::logging::client_wire_unavailable("command_journal", Some(error.kind()));
}
fn private(path: &Path) -> io::Result<File> {
    match files::create_private_file(path) {
        Ok(file) => {
            file.sync_all()?;
            File::open(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?.sync_all()?;
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    files::open_append_file(path)
}
struct Journal {
    file: Stream,
    root: PathBuf,
    _lease: LockFile,
    index: Index,
    compaction: Option<mpsc::Sender<compact::Continue>>,
    next_compaction: u64,
    bytes: u64,
    blocked: bool,
    principal_limit: usize,
    global_limit: usize,
    #[cfg(test)]
    fault: Option<Fault>,
    #[cfg(test)]
    before_sync: Option<Box<dyn FnMut() + Send>>,
    #[cfg(test)]
    compaction_hook: Option<fn(&str)>,
}
impl Journal {
    fn open(data: &Path) -> io::Result<Self> {
        let root = data.canonicalize()?.join("wire");
        match files::create_private_dir(&root) {
            Ok(()) => File::open(data)?.sync_all()?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => files::check_private_dir(&root)?,
            Err(e) => return Err(e),
        }
        let lease = LockFile::new(private(&root.join("commands.lock"))?);
        lease.try_lock().map_err(io::Error::other)?;
        let file = private(&root.join("commands.jsonl"))?;
        #[cfg(test)]
        let file = Stream::from(file);
        let bytes = file.metadata()?.len();
        if bytes > LOG_BYTES {
            return Err(io::Error::other("command journal byte bound"));
        }
        let mut journal = Self {
            file,
            root,
            _lease: lease,
            index: Index::default(),
            compaction: None,
            next_compaction: compact::THRESHOLD,
            bytes,
            blocked: false,
            principal_limit: PRINCIPAL_LIMIT,
            global_limit: GLOBAL_LIMIT,
            #[cfg(test)]
            fault: None,
            #[cfg(test)]
            before_sync: None,
            #[cfg(test)]
            compaction_hook: None,
        };
        journal.replay()?;
        Ok(journal)
    }
    fn replay(&mut self) -> io::Result<()> {
        let mut reader = io::BufReader::new(self.file.try_clone()?);
        let mut good = 0;
        loop {
            let mut line = Vec::new();
            if reader
                .by_ref()
                .take(ENTRY_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)?
                == 0
            {
                break;
            }
            if line.len() > ENTRY_BYTES {
                self.blocked = true;
                good += line.len() as u64;
                tracing::warn!("oversized command journal entry preserved");
                while line.last() != Some(&b'\n') {
                    line.clear();
                    if reader
                        .by_ref()
                        .take(ENTRY_BYTES as u64 + 1)
                        .read_until(b'\n', &mut line)?
                        == 0
                    {
                        break;
                    }
                    good += line.len() as u64;
                }
                continue;
            }
            if line.last() != Some(&b'\n') {
                let archive = serde_json::to_vec(&serde_json::json!({"offset":good,"raw":line}))?;
                let mut file = private(&self.root.join("commands.torn.jsonl"))?;
                if file.metadata()?.len() + archive.len() as u64 + 1 > (ENTRY_BYTES * 32) as u64 {
                    return Err(io::Error::other("command tail archive full"));
                }
                file.write_all(&archive)?;
                file.write_all(b"\n")?;
                file.sync_all()?;
                self.file.set_len(good)?;
                self.file.sync_all()?;
                self.bytes = good;
                tracing::warn!("torn command journal tail archived before truncation");
                break;
            }
            good += line.len() as u64;
            let valid =
                Record::decode(&line).is_some_and(|r| self.index.fold(r, self.global_limit));
            if !valid {
                self.blocked = true;
                tracing::warn!("opaque command journal entry skipped and preserved");
            }
        }
        Ok(())
    }
    fn append(&mut self, record: &Record) -> Result<(), ErrorCode> {
        let mut line = serde_json::to_vec(record).map_err(|_| ErrorCode::SerializationError)?;
        line.push(b'\n');
        if line.len() > ENTRY_BYTES {
            return Err(ErrorCode::InvalidParams);
        }
        if self.blocked || self.bytes.saturating_add(line.len() as u64) > LOG_BYTES {
            return Err(ErrorCode::SessionBusy);
        }
        let before = self.bytes;
        let result = (|| -> io::Result<()> {
            #[cfg(test)]
            if matches!(self.fault, Some(Fault::Partial | Fault::Rollback)) {
                self.file.write_all(&line[..line.len() / 2])?;
                return Err(io::Error::other("injected partial append"));
            }
            self.file.write_all(&line)?;
            #[cfg(test)]
            if let Some(hook) = &mut self.before_sync {
                hook();
            }
            #[cfg(test)]
            if self.fault == Some(Fault::Sync) {
                return Err(io::Error::other("injected sync failure"));
            }
            self.file.sync_data()
        })();
        if let Err(error) = result {
            self.blocked = true;
            unavailable(&error);
            let rollback = (|| -> io::Result<()> {
                #[cfg(test)]
                if self.fault == Some(Fault::Rollback) {
                    return Err(io::Error::other("injected rollback failure"));
                }
                self.file.set_len(before)?;
                self.file.sync_all()
            })();
            if let Err(error) = rollback {
                unavailable(&error);
            }
            return Err(ErrorCode::SessionBusy);
        }
        self.bytes += line.len() as u64;
        Ok(())
    }
    fn apply(&mut self, operation: Operation) -> Result<Admission, ErrorCode> {
        if files::file_link_count(&self.file).map_err(|_| ErrorCode::SessionBusy)? == 0 {
            return Err(ErrorCode::SessionUnconfirmed);
        }
        match operation {
            Operation::Claim {
                namespace,
                principal,
                command,
                now,
            } => self.claim(&namespace, &principal, &command, now),
            Operation::Finish {
                key,
                admitted_ms,
                result,
            } => self
                .finish(&key, admitted_ms, result)
                .map(Admission::Replayed),
        }
    }
    fn claim(
        &mut self,
        namespace: &str,
        principal: &str,
        command: &Command,
        now: u64,
    ) -> Result<Admission, ErrorCode> {
        let key = (
            namespace.to_owned(),
            principal.to_owned(),
            command.id.clone(),
        );
        let digest: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&(&command.verb, &command.args))
                .map_err(|_| ErrorCode::SerializationError)?,
        )
        .into();
        if let Some(record) = self.index.records.get(&key) {
            if now.saturating_sub(record.admitted_ms) >= super::super::IDEMPOTENCY_WINDOW_MS {
                return Err(if command.resume_only {
                    ErrorCode::SessionUnconfirmed
                } else {
                    ErrorCode::SessionBusy
                });
            }
            if digest != record.digest {
                return Err(ErrorCode::InvalidParams);
            }
            return match &record.outcome {
                None => Ok(Admission::Pending(record.admitted_ms)),
                Some(value) => serde_json::from_value(value.clone())
                    .map(Admission::Replayed)
                    .map_err(|_| ErrorCode::SessionUnconfirmed),
            };
        }
        if command.resume_only {
            return Err(ErrorCode::SessionUnconfirmed);
        }
        if self.index.records.len() >= self.global_limit
            || self
                .index
                .counts
                .get(&(key.0.clone(), key.1.clone()))
                .copied()
                .unwrap_or_default()
                >= self.principal_limit
        {
            return Err(ErrorCode::SessionBusy);
        }
        let record = Record {
            version: 1,
            namespace: key.0,
            principal: key.1,
            cmd_id: key.2,
            digest,
            admitted_ms: now,
            outcome: None,
        };
        self.append(&record)?;
        self.index.fold(record, self.global_limit);
        Ok(Admission::Claimed(now))
    }
    fn finish(
        &mut self,
        key: &Key,
        admitted_ms: u64,
        result: Result<Value, ErrorCode>,
    ) -> Result<ServerFrame, ErrorCode> {
        let record = self
            .index
            .records
            .get(key)
            .filter(|r| r.admitted_ms == admitted_ms)
            .ok_or(ErrorCode::SessionUnconfirmed)?;
        if let Some(value) = &record.outcome {
            return serde_json::from_value(value.clone())
                .map_err(|_| ErrorCode::SessionUnconfirmed);
        }
        let frame = ack(key.2.clone(), result);
        if serde_json::to_vec(&frame)
            .map_err(|_| ErrorCode::SerializationError)?
            .len()
            > super::super::MAX_MESSAGE
        {
            return Err(ErrorCode::SessionUnconfirmed);
        }
        let mut record = record.clone();
        record.outcome =
            Some(serde_json::to_value(&frame).map_err(|_| ErrorCode::SerializationError)?);
        self.append(&record)
            .map_err(|_| ErrorCode::SessionUnconfirmed)?;
        self.index.fold(record, self.global_limit);
        Ok(frame)
    }
}
impl Index {
    fn fold(&mut self, record: Record, global_limit: usize) -> bool {
        let key = record.key();
        if let Some(first) = self.records.get_mut(&key) {
            if first.digest != record.digest
                || first.admitted_ms != record.admitted_ms
                || first.outcome.is_some()
                || record.outcome.is_none()
            {
                return false;
            }
            first.outcome = record.outcome;
        } else {
            if record.outcome.is_some() || self.records.len() == global_limit {
                return false;
            }
            *self
                .counts
                .entry((key.0.clone(), key.1.clone()))
                .or_default() += 1;
            self.records.insert(key, record);
        }
        true
    }
}
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    Partial,
    Sync,
    Rollback,
}
#[cfg(test)]
mod tests;
