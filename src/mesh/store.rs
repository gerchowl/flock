//! SQLite custody, inbox import, and outcome retention (ADR-0026).
//!
//! The caller authenticates origin and return bindings before admission. This
//! library does no transport, directory lookup, or audit-body publication.
use super::{clock::Clock, key::MessageKey};
mod collection;
mod schema;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fmt, fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const DAY_MS: i64 = 86_400_000;
pub const CUSTODY_TTL_MS: i64 = 7 * DAY_MS;
// Charged at admission, including space for future import/receipt metadata.
const ROW_RESERVE: u64 = 1024;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub logical_bytes: u64,
    pub metadata_reserve: u64,
    pub active_envelopes: u64,
    pub disk_reserve: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            logical_bytes: 256 << 20,
            metadata_reserve: 16 << 20,
            active_envelopes: 10_000,
            disk_reserve: 32 << 20,
        }
    }
}

/// Injectable filesystem observation for deterministic disk-full tests.
pub trait DiskSpace {
    fn available(&self, path: &Path) -> std::io::Result<u64>;
}
pub struct SystemDisk;
impl DiskSpace for SystemDisk {
    fn available(&self, path: &Path) -> std::io::Result<u64> {
        crate::platform::disk_space::available(path)
    }
}

#[derive(Debug)]
pub enum Error {
    Sql(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    MailStoreFull,
    Paused,
    InvalidEnvelope,
    ConflictingKey,
    NotFound,
    InvalidState,
    NewerSchema { found: i64, supported: i64 },
    IdentityPinConflict,
}
impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MailStoreFull => "mail_store_full",
            Self::Paused => "fleet_paused",
            Self::InvalidEnvelope => "invalid_envelope",
            Self::ConflictingKey => "message_key_conflict",
            Self::NotFound => "message_not_found",
            Self::InvalidState => "invalid_custody_state",
            Self::NewerSchema { .. } => "mail_store_schema_too_new",
            Self::IdentityPinConflict => "identity_pin_conflict",
            _ => "mail_store_unavailable",
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self::NewerSchema { found, supported } = self {
            return write!(
                f,
                "{}: database schema version {found} is newer than supported version {supported}",
                self.code()
            );
        }
        write!(f, "{}: {self:?}", self.code())
    }
}
impl std::error::Error for Error {}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnBinding {
    pub request: MessageKey,
    pub recipient_node: String,
    pub collection_token: Vec<u8>,
    pub collection_peers: Vec<String>,
}
impl ReturnBinding {
    pub fn mint(
        request: MessageKey,
        recipient_node: String,
        collection_peers: Vec<String>,
    ) -> std::result::Result<Self, getrandom::Error> {
        let mut collection_token = vec![0; 32];
        getrandom::fill(&mut collection_token)?;
        Ok(Self {
            request,
            recipient_node,
            collection_token,
            collection_peers,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub key: MessageKey,
    pub sender: String,
    pub target_agent: String,
    pub target_session: String,
    pub correlation_id: String,
    pub in_reply_to: Option<String>,
    pub request_key: Option<MessageKey>,
    pub return_binding: ReturnBinding,
    pub intent: String,
    pub body: Vec<u8>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Custody,
    Held,
    Inbox,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accepted {
    New,
    Duplicate,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Transferred,
    Delivered,
    Read,
    Expired,
    InboxExpired,
    RecipientGone,
}
impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Self::Transferred => "transferred",
            Self::Delivered => "delivered",
            Self::Read => "read",
            Self::Expired => "expired",
            Self::InboxExpired => "inbox_expired",
            Self::RecipientGone => "recipient_gone",
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    pub envelope: Envelope,
    pub state: String,
    pub remaining_ms: i64,
    /// The inbox admission budget, independent of the transferred custody budget.
    pub mailbox_ttl_ms: i64,
    pub delivered: bool,
    pub retry_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinSource {
    #[default]
    Configured,
    Inbound,
}

impl PinSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Inbound => "inbound",
        }
    }
}

/// How trust was first established, independent of the pin's direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinOrigin {
    #[default]
    Unknown,
    Dialed,
    InboundFirstContact,
}

impl PinOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Dialed => "dialed",
            Self::InboundFirstContact => "inbound_first_contact",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityPin {
    pub node_id: String,
    pub public_key: Vec<u8>,
}

/// `Store` with the default disk provider is `Send` but not `Sync`. Share it
/// between workers through a `Mutex`, never concurrent unsynchronized access.
/// Message order within the same millisecond is arbitrary, with no FIFO promise.
pub struct Store<D = SystemDisk> {
    connection: Connection,
    path: PathBuf,
    limits: Limits,
    disk: D,
}
impl Store<SystemDisk> {
    /// `path` is normally `config::state_dir()/mesh-mail.sqlite`. Its parent
    /// must already exist. Writer handoff is the runtime owner's responsibility.
    pub fn open(path: &Path, wall_ms: i64) -> Result<Self> {
        Self::open_with(path, wall_ms, Limits::default(), SystemDisk)
    }
    /// Validate without opening a writer, migrating, or advancing the clock.
    pub fn check_generation(path: &Path, minimum: u64) -> Result<()> {
        if minimum == 0 {
            return Ok(());
        }
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let found: i64 = connection.query_row(
            "SELECT generation FROM writer_generation WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if found < 0 || (found as u64) < minimum {
            return Err(Error::Io(std::io::Error::other(format!(
                "mesh store generation {found} is older than handoff generation {minimum}"
            ))));
        }
        Ok(())
    }
}
impl<D: DiskSpace> Store<D> {
    pub fn open_with(path: &Path, wall_ms: i64, limits: Limits, disk: D) -> Result<Self> {
        if limits.metadata_reserve > limits.logical_bytes || limits.logical_bytes > i64::MAX as u64
        {
            return Err(Error::InvalidEnvelope);
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // SQLite sidecars inherit this mode. A strict umask must not remove
        // owner write and leave the durable store impossible to reopen.
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(file) => file.set_permissions(fs::Permissions::from_mode(0o600))?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        let mut connection = Connection::open(path)?;
        schema::check_version(&connection)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA auto_vacuum=INCREMENTAL; PRAGMA journal_mode=WAL;
            PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            PRAGMA wal_autocheckpoint=64; PRAGMA journal_size_limit=1048576;",
        )?;
        schema::migrate(&mut connection)?;
        connection.execute("INSERT OR IGNORE INTO clock VALUES(1,?1,0,0)", [wall_ms])?;
        fs::File::open(path)?.sync_all()?;
        fs::File::open(parent)?.sync_all()?;
        let mut store = Self {
            connection,
            path: parent.canonicalize()?,
            limits,
            disk,
        };
        store.advance(wall_ms, None)?;
        Ok(store)
    }

    /// Advance the durable handoff fence before releasing the writer.
    pub fn handoff_generation(&mut self) -> Result<u64> {
        self.connection.execute(
            "UPDATE writer_generation SET generation=generation+1 WHERE singleton=1",
            [],
        )?;
        let generation: i64 = self.connection.query_row(
            "SELECT generation FROM writer_generation WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        u64::try_from(generation).map_err(|_| Error::InvalidState)
    }

    fn guard_disk(&self, extra: u64) -> Result<()> {
        // Allow for both database and WAL pages, including a conservative
        // transaction/page overhead allowance. Never acknowledge after failure.
        if self.disk.available(&self.path)?
            < self
                .limits
                .disk_reserve
                .saturating_add(extra.saturating_mul(2))
                .saturating_add(1 << 20)
        {
            return Err(Error::MailStoreFull);
        }
        Ok(())
    }

    pub fn clock(&self) -> Result<Clock> {
        Ok(self.connection.query_row(
            "SELECT wall,elapsed,paused FROM clock WHERE singleton=1",
            [],
            |r| {
                Ok(Clock {
                    wall_ms: r.get(0)?,
                    elapsed_ms: r.get(1)?,
                    paused: r.get(2)?,
                })
            },
        )?)
    }

    fn advance(&mut self, wall_ms: i64, paused: Option<bool>) -> Result<Clock> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut clock = tx.query_row(
            "SELECT wall,elapsed,paused FROM clock WHERE singleton=1",
            [],
            |r| {
                Ok(Clock {
                    wall_ms: r.get(0)?,
                    elapsed_ms: r.get(1)?,
                    paused: r.get(2)?,
                })
            },
        )?;
        if let Some(paused) = paused {
            clock.set_paused(paused, wall_ms);
        } else {
            clock.advance(wall_ms);
        }
        tx.execute(
            "UPDATE clock SET wall=?1,elapsed=?2,paused=?3 WHERE singleton=1",
            params![clock.wall_ms, clock.elapsed_ms, clock.paused],
        )?;
        tx.commit()?;
        Ok(clock)
    }

    pub fn set_paused(&mut self, paused: bool, wall_ms: i64) -> Result<()> {
        if self.clock()?.paused != paused {
            self.advance(wall_ms, Some(paused))?;
        }
        Ok(())
    }

    fn writable(&mut self, wall_ms: i64) -> Result<i64> {
        let clock = self.advance(wall_ms, None)?;
        if clock.paused {
            return Err(Error::Paused);
        }
        Ok(clock.elapsed_ms)
    }

    /// Atomic custody acceptance and, when requested, inbox/dedupe import.
    /// Remaining TTL is transport state, not part of immutable identity.
    pub fn accept(
        &mut self,
        envelope: &Envelope,
        ttl_ms: i64,
        admission: Admission,
        wall_ms: i64,
    ) -> Result<Accepted> {
        self.accept_inner(envelope, ttl_ms, admission, wall_ms, false)
    }

    pub fn accept_collected(
        &mut self,
        envelope: &Envelope,
        ttl_ms: i64,
        wall_ms: i64,
    ) -> Result<Accepted> {
        self.accept_inner(envelope, ttl_ms, Admission::Inbox, wall_ms, true)
    }

    fn accept_inner(
        &mut self,
        envelope: &Envelope,
        ttl_ms: i64,
        admission: Admission,
        wall_ms: i64,
        collect_ack: bool,
    ) -> Result<Accepted> {
        let now = self.writable(wall_ms)?;
        if !envelope.key.is_valid()
            || !(1..=CUSTODY_TTL_MS).contains(&ttl_ms)
            || !envelope.return_binding.request.is_valid()
            || envelope.return_binding.collection_token.len() < 32
        {
            return Err(Error::InvalidEnvelope);
        }
        let mut metadata = envelope.clone();
        metadata.body.clear();
        let metadata = serde_json::to_string(&metadata)?;
        let mut hash = Sha256::new();
        hash.update(metadata.as_bytes());
        hash.update(&envelope.body);
        let fingerprint = hash.finalize().to_vec();
        let charge = metadata.len() as u64
            + envelope.key.origin_node.len() as u64
            + envelope.key.message_id.len() as u64
            + envelope.correlation_id.len() as u64
            + ROW_RESERVE;
        self.guard_disk(charge.saturating_add(envelope.body.len() as u64))?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<Vec<u8>> = tx
            .query_row(
                "SELECT fingerprint FROM envelopes WHERE origin=?1 AND id=?2",
                params![envelope.key.origin_node, envelope.key.message_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            if prior != fingerprint {
                return Err(Error::ConflictingKey);
            }
            // No TTL or inbox lifetime renewal on retry. Import of existing
            // custody is explicit through import(), not implicit retransmission.
            collection::record_answer(&tx, envelope, admission, collect_ack)?;
            tx.commit()?;
            return Ok(Accepted::Duplicate);
        }
        let (total, bodies, active): (i64, i64, i64) = tx.query_row(
            "SELECT total_bytes,body_bytes,active FROM usage WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let body_size = envelope.body.len() as u64;
        if (total as u64)
            .saturating_add(charge)
            .saturating_add(body_size)
            > self.limits.logical_bytes
            || (bodies as u64).saturating_add(body_size)
                > self.limits.logical_bytes - self.limits.metadata_reserve
            || active as u64 >= self.limits.active_envelopes
        {
            return Err(Error::MailStoreFull);
        }
        let deadline = now.saturating_add(ttl_ms);
        let inbox = admission == Admission::Inbox;
        tx.execute("INSERT INTO envelopes(origin,id,correlation,metadata,fingerprint,body,state,custody_deadline,
            inbox_deadline,dedupe_until,delivered,retry_at,metadata_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![envelope.key.origin_node,envelope.key.message_id,envelope.correlation_id,metadata,fingerprint,envelope.body,
                match admission { Admission::Inbox => "inbox", Admission::Custody => "custody", Admission::Held => "held" },deadline, if inbox {Some(now.saturating_add(DAY_MS))} else {None},
                deadline.saturating_add(DAY_MS),inbox,now,charge as i64])?;
        tx.execute("UPDATE envelopes SET request_origin=?3,request_id=?4,recipient_node=?5,reply_expected=?6,collection_token=?7 WHERE origin=?1 AND id=?2",
            params![envelope.key.origin_node,envelope.key.message_id,
                envelope.request_key.as_ref().map(|k| &k.origin_node),envelope.request_key.as_ref().map(|k| &k.message_id),
                envelope.return_binding.recipient_node,
                matches!(envelope.intent.as_str(), "\"needs_reply\"" | "\"blocking\""),serde_json::to_string(&envelope.return_binding.collection_token)?])?;
        collection::record_answer(&tx, envelope, admission, collect_ack)?;
        if inbox {
            tx.execute(
                "INSERT INTO inbox_imports VALUES(?1,?2)",
                params![envelope.key.origin_node, envelope.key.message_id],
            )?;
        }
        tx.commit()?;
        Ok(Accepted::New)
    }

    /// Final mailbox import is one transaction. A full mailbox should leave
    /// custody untouched and retry later, before calling this method. This only
    /// references the existing body, so it does not require disk-reserve admission.
    pub fn import(&mut self, key: &MessageKey, wall_ms: i64) -> Result<Accepted> {
        let now = self.writable(wall_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, deadline, delivered): (String, i64, bool) = tx
            .query_row(
                "SELECT state,custody_deadline,delivered FROM envelopes WHERE origin=?1 AND id=?2",
                params![key.origin_node, key.message_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(Error::NotFound)?;
        if delivered {
            return Ok(Accepted::Duplicate);
        }
        if state != "custody" || deadline <= now {
            return Err(Error::InvalidState);
        }
        tx.execute(
            "INSERT INTO inbox_imports VALUES(?1,?2)",
            params![key.origin_node, key.message_id],
        )?;
        tx.execute("UPDATE envelopes SET state='inbox',delivered=1,inbox_deadline=?3 WHERE origin=?1 AND id=?2",
            params![key.origin_node,key.message_id,now.saturating_add(DAY_MS)])?;
        tx.commit()?;
        Ok(Accepted::New)
    }

    /// Terminal transitions remove bodies but keep immutable fingerprints,
    /// dedupe, return bindings and receipts. Transferred is for hubs only:
    /// origins retain custody until final delivery or expiry.
    pub fn finish(&mut self, key: &MessageKey, outcome: Outcome, wall_ms: i64) -> Result<()> {
        let now = self.writable(wall_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, custody, inbox): (String,i64,Option<i64>) = tx.query_row(
            "SELECT state,custody_deadline,inbox_deadline FROM envelopes WHERE origin=?1 AND id=?2", params![key.origin_node,key.message_id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.ok_or(Error::NotFound)?;
        if state == outcome.name() {
            return Ok(());
        }
        let valid = match outcome {
            Outcome::Read => state == "inbox" && inbox.is_some_and(|d| d > now),
            Outcome::InboxExpired => state == "inbox" && inbox.is_some_and(|d| d <= now),
            Outcome::Expired => matches!(state.as_str(), "custody" | "held") && custody <= now,
            _ => matches!(state.as_str(), "custody" | "held") && custody > now,
        };
        if !valid {
            return Err(Error::InvalidState);
        }
        tx.execute(
            "UPDATE envelopes SET state=?3,body=CASE WHEN ?3='read' THEN body ELSE X'' END,outcome_until=?4,
            delivered=CASE WHEN ?3='delivered' THEN 1 ELSE delivered END WHERE origin=?1 AND id=?2",
            params![
                key.origin_node,
                key.message_id,
                outcome.name(),
                now.saturating_add(CUSTODY_TTL_MS)
            ],
        )?;
        if outcome == Outcome::Delivered {
            tx.execute(
                "UPDATE envelopes SET collect_at=?3,collect_attempts=0 WHERE origin=?1 AND id=?2",
                params![key.origin_node, key.message_id, now.saturating_add(5_000)],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read a mailbox atomically, expiring overdue rows without rejecting live ones.
    pub fn read_inbox(&mut self, keys: &[MessageKey], wall_ms: i64) -> Result<Vec<MessageKey>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut clock = self.clock()?;
        if clock.paused {
            return Err(Error::Paused);
        }
        let now = clock.advance(wall_ms);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE clock SET wall=?1,elapsed=?2 WHERE singleton=1",
            params![clock.wall_ms, now],
        )?;
        let mut expired = Vec::new();
        for key in keys {
            tx.execute("UPDATE envelopes SET state=CASE WHEN inbox_deadline>?3 THEN 'read' ELSE 'inbox_expired' END,
                body=CASE WHEN inbox_deadline>?3 THEN body ELSE X'' END,outcome_until=?4
                WHERE origin=?1 AND id=?2 AND state='inbox'",
                params![key.origin_node,key.message_id,now,now.saturating_add(CUSTODY_TTL_MS)])?;
            let state: Option<String> = tx
                .query_row(
                    "SELECT state FROM envelopes WHERE origin=?1 AND id=?2",
                    params![key.origin_node, key.message_id],
                    |r| r.get(0),
                )
                .optional()?;
            if state.as_deref() == Some("inbox_expired") {
                expired.push(key.clone());
            }
        }
        tx.commit()?;
        Ok(expired)
    }

    /// Scheduler deadlines use the same unpaused clock as expiry. The caller
    /// supplies its jittered backoff; the durable deadline survives restart.
    pub fn schedule_retry(&mut self, key: &MessageKey, delay_ms: i64, wall_ms: i64) -> Result<()> {
        let now = self.writable(wall_ms)?;
        if !(60_000..=300_000).contains(&delay_ms) {
            return Err(Error::InvalidEnvelope);
        }
        let changed = self.connection.execute("UPDATE envelopes SET retry_at=?3 WHERE origin=?1 AND id=?2 AND state='custody' AND custody_deadline>?4",
            params![key.origin_node,key.message_id,now.saturating_add(delay_ms),now])?;
        if changed == 0 {
            return Err(Error::InvalidState);
        }
        Ok(())
    }

    /// Read the last persisted snapshot. `maintain` advances expiry. Absence
    /// alone proves no delivery outcome: a caller with an expired authenticated
    /// reference must report `outcome_retention_elapsed` after GC.
    pub fn get(&self, key: &MessageKey) -> Result<Option<Record>> {
        let now = self.clock()?.elapsed_ms;
        let row = self
            .connection
            .query_row(
                "SELECT metadata,body,state,custody_deadline,inbox_deadline,delivered,retry_at
            FROM envelopes WHERE origin=?1 AND id=?2",
                params![key.origin_node, key.message_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, bool>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(metadata, body, state, custody, inbox, delivered, retry_at_ms)| {
                let mut envelope: Envelope = serde_json::from_str(&metadata)?;
                envelope.body = body;
                let remaining_ms = if matches!(state.as_str(), "inbox" | "read") {
                    inbox.unwrap_or(custody)
                } else {
                    custody
                }
                .saturating_sub(now)
                .max(0);
                Ok(Record {
                    envelope,
                    state,
                    remaining_ms,
                    mailbox_ttl_ms: DAY_MS,
                    delivered,
                    retry_at_ms,
                })
            },
        )
        .transpose()
    }

    /// Correlation ids are nonunique threading labels scoped to an origin.
    pub fn conversation(&self, origin: &str, correlation: &str) -> Result<Vec<MessageKey>> {
        let mut stmt = self
            .connection
            .prepare("SELECT id FROM envelopes WHERE origin=?1 AND correlation=?2 ORDER BY id")?;
        let keys = stmt
            .query_map(params![origin, correlation], |r| {
                Ok(MessageKey {
                    origin_node: origin.to_owned(),
                    message_id: r.get(0)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(keys)
    }

    /// Replies referencing a full request identity, independently of threading labels.
    pub fn by_request_key(&self, request: &MessageKey) -> Result<Vec<MessageKey>> {
        let mut stmt = self.connection.prepare(
            "SELECT origin,id FROM envelopes WHERE request_origin=?1 AND request_id=?2 ORDER BY origin,id",
        )?;
        let keys = stmt
            .query_map(params![request.origin_node, request.message_id], |r| {
                Ok(MessageKey {
                    origin_node: r.get(0)?,
                    message_id: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(keys)
    }

    /// Token lookup is not authorization. The transport must also authenticate
    /// the caller and verify the stored conversation's origin/recipient binding.
    pub fn by_collection_token(&self, token: &[u8]) -> Result<Vec<MessageKey>> {
        let encoded = serde_json::to_string(token)?;
        let mut stmt = self.connection.prepare(
            "SELECT origin,id FROM envelopes WHERE collection_token=?1 ORDER BY origin,id",
        )?;
        let keys = stmt
            .query_map([encoded], |r| {
                Ok(MessageKey {
                    origin_node: r.get(0)?,
                    message_id: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(keys)
    }

    pub fn get_pin(&self, peer: &str) -> Result<Option<IdentityPin>> {
        self.get_pin_from(PinSource::Configured, peer)
    }

    pub fn get_pin_from(&self, source: PinSource, peer: &str) -> Result<Option<IdentityPin>> {
        Ok(self
            .connection
            .query_row(
                "SELECT node_id,public_key FROM identity_pins WHERE source=?1 AND peer=?2",
                params![source.as_str(), peer],
                |r| {
                    Ok(IdentityPin {
                        node_id: r.get(0)?,
                        public_key: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Reconnects retain the original trust decision stored with the pin.
    pub fn pin_origin(&self, source: PinSource, peer: &str) -> Result<Option<PinOrigin>> {
        self.connection
            .query_row(
                "SELECT origin FROM identity_pins WHERE source=?1 AND peer=?2",
                params![source.as_str(), peer],
                |row| {
                    Ok(match row.get::<_, String>(0)?.as_str() {
                        "dialed" => PinOrigin::Dialed,
                        "inbound_first_contact" => PinOrigin::InboundFirstContact,
                        _ => PinOrigin::Unknown,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Prefer the operator's configured alias over the authenticated inbound name.
    pub fn origin_name(&self, node_id: &str) -> Result<Option<String>> {
        Ok(self.connection.query_row(
            "SELECT peer FROM identity_pins WHERE node_id=?1 ORDER BY CASE source WHEN 'configured' THEN 0 ELSE 1 END,peer LIMIT 1",
            [node_id], |row| row.get(0),
        ).optional()?)
    }

    /// Configured labels are local aliases, not remote identity claims.
    pub fn pin_name_from(&self, source: PinSource, pin: &IdentityPin) -> Result<Option<String>> {
        Ok(self.connection.query_row(
            "SELECT peer FROM identity_pins WHERE source=?1 AND node_id=?2 AND public_key=?3 ORDER BY peer LIMIT 1",
            params![source.as_str(), pin.node_id, pin.public_key], |r| r.get(0),
        ).optional()?)
    }

    /// Only inbound claims bind a key to a remote name.
    pub fn conflicting_pin_name(
        &self,
        source: PinSource,
        peer: &str,
        pin: &IdentityPin,
    ) -> Result<Option<String>> {
        if source == PinSource::Configured {
            return Ok(None);
        }
        Ok(self.connection.query_row(
            "SELECT peer FROM identity_pins WHERE source='inbound' AND peer!=?1 AND (node_id=?2 OR public_key=?3) LIMIT 1",
            params![peer, pin.node_id, pin.public_key], |r| r.get(0),
        ).optional()?)
    }

    pub fn put_pin(&mut self, peer: &str, pin: &IdentityPin) -> Result<()> {
        self.put_pin_from(PinSource::Configured, peer, pin)
    }

    /// Transactionally bind inbound claims and refuse key replacement in
    /// either direction. A configured alias cannot rename an inbound claim.
    pub fn put_pin_from(&mut self, source: PinSource, peer: &str, pin: &IdentityPin) -> Result<()> {
        let origin = match source {
            PinSource::Configured => PinOrigin::Dialed,
            PinSource::Inbound => PinOrigin::InboundFirstContact,
        };
        self.put_pins(&[source], peer, pin, origin)
    }

    /// First authenticated inbound contact to a configured name pins both directions atomically.
    pub fn put_inbound_configured_pin(&mut self, peer: &str, pin: &IdentityPin) -> Result<()> {
        self.put_pins(
            &[PinSource::Configured, PinSource::Inbound],
            peer,
            pin,
            PinOrigin::InboundFirstContact,
        )
    }

    fn put_pins(
        &mut self,
        sources: &[PinSource],
        peer: &str,
        pin: &IdentityPin,
        origin: PinOrigin,
    ) -> Result<()> {
        if peer.is_empty() || pin.node_id.is_empty() || pin.public_key.len() != 32 {
            return Err(Error::InvalidEnvelope);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for source in sources {
            let conflict: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM identity_pins WHERE
                (?1='inbound' AND source='inbound' AND peer!=?2 AND (node_id=?3 OR public_key=?4)) OR
                (source=?1 AND peer=?2 AND (node_id!=?3 OR public_key!=?4)))",
            params![source.as_str(), peer, pin.node_id, pin.public_key],
            |r| r.get(0),
        )?;
            if conflict {
                return Err(Error::IdentityPinConflict);
            }
            tx.execute(
                "INSERT OR IGNORE INTO identity_pins (source,peer,node_id,public_key,origin) VALUES(?1,?2,?3,?4,?5)",
                params![source.as_str(), peer, pin.node_id, pin.public_key, origin.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Only the operator's explicit re-enrollment path should call this.
    pub fn reset_pin(&mut self, peer: &str) -> Result<bool> {
        self.reset_pin_from(PinSource::Configured, peer)
    }

    pub fn reset_pin_from(&mut self, source: PinSource, peer: &str) -> Result<bool> {
        Ok(self.connection.execute(
            "DELETE FROM identity_pins WHERE source=?1 AND peer=?2",
            params![source.as_str(), peer],
        )? != 0)
    }

    /// Rebuild the unread mailbox projection after restart, including while
    /// paused. Reading this list never consumes a message or changes custody.
    pub fn migration_done(&self, name: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM migrations WHERE name=?1)",
            [name],
            |r| r.get(0),
        )?)
    }

    pub fn finish_migration(&mut self, name: &str) -> Result<()> {
        self.connection
            .execute("INSERT OR IGNORE INTO migrations VALUES(?1)", [name])?;
        Ok(())
    }

    /// Retain undecodable bytes for diagnosis, excluding them from delivery and projection.
    pub fn quarantine(&mut self, key: &MessageKey) -> Result<()> {
        self.connection.execute(
            "UPDATE envelopes SET state='quarantined' WHERE origin=?1 AND id=?2",
            params![key.origin_node, key.message_id],
        )?;
        Ok(())
    }

    pub fn mailbox_keys(&self) -> Result<Vec<MessageKey>> {
        self.projection_keys(true)
    }

    pub fn inbox_keys(&self) -> Result<Vec<MessageKey>> {
        self.projection_keys(false)
    }

    fn projection_keys(&self, include_read: bool) -> Result<Vec<MessageKey>> {
        let mut stmt = self
            .connection
            .prepare("SELECT origin,id FROM envelopes WHERE state='inbox' OR (?1 AND state='read') ORDER BY origin,id")?;
        let keys = stmt
            .query_map([include_read], |r| {
                Ok(MessageKey {
                    origin_node: r.get(0)?,
                    message_id: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(keys)
    }

    /// Atomically claim at most 500 ready keys for 60 seconds of unpaused
    /// time. A second worker cannot select them until that lease expires.
    /// Workers must finish their attempt within the lease. Crashed workers'
    /// claims become retryable after expiry, including across restart.
    pub fn retry_ready(&mut self, wall_ms: i64) -> Result<Vec<MessageKey>> {
        self.retry_ready_limit(wall_ms, 500)
    }

    pub fn retry_ready_limit(&mut self, wall_ms: i64, limit: usize) -> Result<Vec<MessageKey>> {
        let mut clock = self.clock()?;
        if clock.paused {
            return Err(Error::Paused);
        }
        let now = clock.advance(wall_ms);
        let ready: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM envelopes WHERE state='custody' AND retry_at<=?1 AND custody_deadline>?1 AND lease_until<=?1)",
            [now], |row| row.get(0),
        )?;
        if !ready || limit == 0 {
            return Ok(Vec::new());
        }
        let now = self.writable(wall_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let keys = {
            let mut stmt = tx.prepare(
                "SELECT origin,id FROM envelopes WHERE state='custody' AND retry_at<=?1
                 AND custody_deadline>?1 AND lease_until<=?1 ORDER BY retry_at,origin,id LIMIT ?2",
            )?;
            let keys = stmt
                .query_map([now, limit.min(500) as i64], |r| {
                    Ok(MessageKey {
                        origin_node: r.get(0)?,
                        message_id: r.get(1)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            keys
        };
        for key in &keys {
            tx.execute(
                "UPDATE envelopes SET lease_until=?3 WHERE origin=?1 AND id=?2",
                params![key.origin_node, key.message_id, now.saturating_add(60_000)],
            )?;
        }
        tx.commit()?;
        Ok(keys)
    }

    /// Idle runtime maintenance is read-only until expiry or deletion is due.
    pub fn maintain_if_due(&mut self, wall_ms: i64) -> Result<usize> {
        let mut clock = self.clock()?;
        if clock.paused {
            return Err(Error::Paused);
        }
        let now = clock.advance(wall_ms);
        let due: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM envelopes WHERE
             (state IN ('custody','held') AND custody_deadline<=?1)
             OR (state='inbox' AND inbox_deadline<=?1)
             OR (outcome_until<=?1 AND dedupe_until<=?1))",
            [now],
            |r| r.get(0),
        )?;
        if !due {
            return Ok(0);
        }
        self.maintain(wall_ms)
    }

    /// Explicit maintenance records expiry before collection of terminal rows.
    /// Keep dedupe through admitted TTL + 24h even after outcome retention ends.
    pub fn maintain(&mut self, wall_ms: i64) -> Result<usize> {
        let now = self.writable(wall_ms)?;
        // Each transaction changes at most 500 rows, releasing the writer lock
        // between batches so other work can make progress.
        loop {
            let tx = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let expired = tx.execute("UPDATE envelopes SET state=CASE WHEN state='inbox' THEN 'inbox_expired' ELSE 'expired' END,
                body=X'',outcome_until=?1 WHERE rowid IN (SELECT rowid FROM envelopes
                WHERE (state IN ('custody','held') AND custody_deadline<=?2) OR (state='inbox' AND inbox_deadline<=?2) LIMIT 500)",
                params![now.saturating_add(CUSTODY_TTL_MS),now])?;
            tx.commit()?;
            if expired < 500 {
                break;
            }
        }
        let mut deleted = 0;
        loop {
            let tx = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let batch = tx.execute("DELETE FROM envelopes WHERE rowid IN
                (SELECT rowid FROM envelopes WHERE outcome_until<=?1 AND dedupe_until<=?1 LIMIT 500)", [now])?;
            tx.commit()?;
            deleted += batch;
            if batch < 500 {
                break;
            }
        }
        self.checkpoint()?;
        Ok(deleted)
    }

    pub fn checkpoint(&self) -> Result<()> {
        if self.clock()?.paused {
            return Err(Error::Paused);
        }
        self.connection.execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum;
            PRAGMA wal_checkpoint(TRUNCATE);",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
