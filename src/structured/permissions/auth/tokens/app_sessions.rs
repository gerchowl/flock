//! Private ephemeral app-session credentials (ADR-0038 §6, #2442 PR1a).
//! The server owner shares this source with socket minting and transport admission.
use super::*;
use crate::platform::process_exit::ExitWatch;
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::sync::watch;

const CAPACITY: usize = 128;
/// Fixed lifetime; renewal issues a different credential for the same operator identity.
pub(crate) const TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Monotonic time supplied by the owning server, never serialized or wall-clock based.
pub(crate) trait Clock: Send + Sync {
    /// Read the current monotonic instant without blocking.
    fn now(&self) -> Instant;
}
/// Production monotonic clock; tests supply the same explicit time domain.
pub(crate) struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}
/// Peer evidence failures stay distinct from entropy, storage and admission failures.
#[derive(Debug)]
pub(crate) enum IssueError {
    /// Kernel registration or accepted start identity could not be proved.
    PeerProcessChanged(io::Error),
    /// Existing token store failure; no token has been returned.
    Store(TokenError),
}
impl From<TokenError> for IssueError {
    fn from(e: TokenError) -> Self {
        Self::Store(e)
    }
}
impl IssueError {
    /// The mint adapter uses the registered non-retryable identity refusal.
    pub(crate) fn peer_code(&self) -> Option<crate::api::error_code::ErrorCode> {
        matches!(self, Self::PeerProcessChanged(_))
            .then_some(crate::api::error_code::ErrorCode::PeerProcessChanged)
    }
}
/// Sealed snapshot; the store rechecks it under shared admission ownership.
#[derive(Clone)]
pub(crate) struct Session {
    /// Server-issued identity and fixed operator scopes.
    pub(crate) info: TokenInfo,
    /// Fixed expiry in the supplied clock domain, never extended by lookup.
    pub(crate) expires: Instant,
    /// Retained kernel evidence for readiness-driven owner scheduling.
    pub(crate) process: Arc<ExitWatch>,
    /// Changes after fenced removal; owner drop closes the sender.
    pub(crate) revoked: watch::Receiver<bool>,
    digest: [u8; 32],
}
impl Session {
    /// Compare sealed bearer digests without exposing credentials to resource owners.
    pub(super) fn same_credential(&self, other: &Self) -> bool {
        self.digest == other.digest
    }
}
struct Entry {
    session: Session,
    revoke: watch::Sender<bool>,
}
type Entries = HashMap<[u8; 32], Entry>;
/// One server-owned memory domain. Clone its Arc, never reconstruct it on lookup.
/// Fresh owners (restart/handoff successors) cannot authenticate predecessor tokens.
/// Mutations share the backing file store's exclusive lock and revocation intent.
/// Admissions retain its shared lock through the callback, with no map lock held.
/// At most 128 entries live; transport owners retire them after expiry/exit readiness.
pub(crate) struct Store {
    files: Arc<TokenStore>,
    entries: Mutex<Entries>,
    clock: Arc<dyn Clock>,
}
struct Mutation<'a> {
    store: &'a Store,
    pending: PendingLock,
    _intent: priority::Intent<'a>,
}
impl Mutation<'_> {
    fn finish<T>(
        self,
        change: impl FnOnce(&mut Entries) -> Result<T, TokenError>,
    ) -> Result<T, TokenError> {
        let _guard = self.pending.finish(Duration::from_secs(2))?;
        let mut entries = self.store.entries.lock().map_err(|_| TokenError::Unsafe)?;
        change(&mut entries)
    }
}
impl Store {
    /// Bind a fresh memory-only domain to the shared/exclusive file admission lock.
    pub(crate) fn new(files: Arc<TokenStore>, clock: Arc<dyn Clock>) -> Self {
        Self {
            files,
            entries: Mutex::new(HashMap::new()),
            clock,
        }
    }
    fn mutation(&self) -> Result<Mutation<'_>, TokenError> {
        let intent = priority::Intent::begin(&self.files.0, self.files.random::<16>()?)?;
        let pending = PendingLock::begin(self.files.lock_file()?, &self.files.2)?;
        Ok(Mutation {
            store: self,
            pending,
            _intent: intent,
        })
    }
    /// Arm the attested process before reading its identity or generating a bearer.
    /// The caller must hold LocalOperator authority; this is not an exposed mint API.
    #[cfg(test)]
    pub(crate) fn issue(&self, authority: &LocalOperator<'_>) -> Result<Issued, IssueError> {
        self.issue_watched(
            authority,
            ExitWatch::arm(authority.peer_credentials()).map_err(IssueError::PeerProcessChanged)?,
        )
    }
    /// Consume evidence armed before LocalOperator's ancestry/start-time checks.
    /// A registration for any other connection cannot authorize this issuance.
    pub(crate) fn issue_watched(
        &self,
        authority: &LocalOperator<'_>,
        watch: ExitWatch,
    ) -> Result<Issued, IssueError> {
        if !watch.matches(authority.peer_credentials())
            || watch.exited().map_err(IssueError::PeerProcessChanged)?
        {
            return Err(IssueError::PeerProcessChanged(io::Error::other(
                "peer watch does not match authority",
            )));
        }
        let process = Arc::new(watch);
        let _guard = self.files.lock()?;
        let mut entries = self.entries.lock().map_err(|_| TokenError::Unsafe)?;
        Self::sweep_entries(&mut entries, self.clock.now())?;
        if entries.len() == CAPACITY {
            return Err(TokenError::Busy.into());
        }
        let token = Bearer(self.files.random::<32>()?);
        let digest = Sha256::digest(token.0.as_bytes()).into();
        if entries.contains_key(&digest) {
            return Err(TokenError::Unsafe.into());
        }
        let name = self.files.random::<16>()?;
        let uid = authority.peer_credentials().uid.ok_or_else(|| {
            IssueError::PeerProcessChanged(io::Error::other("operator UID is unavailable"))
        })?;
        let issued_at = self.clock.now();
        let expires = issued_at + TTL;
        let info = TokenInfo {
            name: format!("app:{name}"),
            principal: format!("app:operator:{uid}"),
            scopes: operator_scopes(),
            expires_in_ms: Some(
                u64::try_from(expires.duration_since(issued_at).as_millis())
                    .map_err(|_| TokenError::Unsafe)?,
            ),
        };
        let (revoke, revoked) = watch::channel(false);
        entries.insert(
            digest,
            Entry {
                session: Session {
                    info: info.clone(),
                    expires,
                    process,
                    revoked,
                    digest,
                },
                revoke,
            },
        );
        Ok(Issued { info, token })
    }
    fn current(&self, digest: &[u8; 32]) -> Result<Session, TokenError> {
        let entries = self.entries.lock().map_err(|_| TokenError::Unsafe)?;
        let entry = entries.get(digest).ok_or(TokenError::Revoked)?;
        if self.clock.now() >= entry.session.expires || entry.session.process.exited()? {
            return Err(TokenError::Revoked);
        }
        Ok(entry.session.clone())
    }
    /// Membership selects the source without imposing an app admission lock on file hello.
    pub(super) fn contains(&self, token: &Bearer) -> Result<bool, TokenError> {
        let digest: [u8; 32] = Sha256::digest(token.0.as_bytes()).into();
        Ok(self
            .entries
            .lock()
            .map_err(|_| TokenError::Unsafe)?
            .contains_key(&digest))
    }
    /// Lookup only in this private memory domain; file-token authentication is unchanged.
    pub(crate) fn authenticate(&self, token: &Bearer) -> Result<Session, TokenError> {
        let _guard = self.files.lock_with(LockFile::try_lock_shared)?;
        if !valid_hex(&token.0, 64) {
            return Err(TokenError::Unauthorized);
        }
        self.current(&Sha256::digest(token.0.as_bytes()).into())
    }
    /// The sealed capability cannot escape this bounded callback or recursively manage tokens.
    pub(crate) fn admit<T>(
        &self,
        session: &Session,
        required: Scope,
        dispatch: impl for<'a> FnOnce(VerifiedToken<'a>) -> T,
    ) -> Result<T, TokenError> {
        let _guard = self.files.lock_with(LockFile::try_lock_shared)?;
        let current = self.current(&session.digest)?;
        if !current.info.scopes.contains(&required) {
            return Err(TokenError::Scope);
        }
        Ok(dispatch(VerifiedToken::new(&current.info.principal, true)))
    }
    fn remove(entries: &mut Entries, digest: &[u8; 32]) -> Result<(), TokenError> {
        let entry = entries.remove(digest).ok_or(TokenError::Revoked)?;
        entry.revoke.send_replace(true);
        Ok(())
    }
    /// Publish intent before waiting; after return, admitted callbacks have drained.
    pub(crate) fn revoke(&self, session: &Session) -> Result<(), TokenError> {
        self.mutation()?
            .finish(|entries| Self::remove(entries, &session.digest))
    }
    /// Quiescence fence for restart/handoff; no session state belongs in their snapshots.
    pub(crate) fn revoke_all(&self) -> Result<(), TokenError> {
        self.mutation()?.finish(|entries| {
            for (_, entry) in entries.drain() {
                entry.revoke.send_replace(true);
            }
            Ok(())
        })
    }
    fn sweep_entries(entries: &mut Entries, now: Instant) -> Result<(), TokenError> {
        let mut ended = Vec::new();
        for (digest, entry) in entries.iter() {
            if now >= entry.session.expires || entry.session.process.exited()? {
                ended.push(*digest);
            }
        }
        for digest in ended {
            Self::remove(entries, &digest)?;
        }
        Ok(())
    }
    /// Owners invoke this after kernel/clock readiness, off the app executor.
    /// Watch I/O errors fail closed; no uncertain observation is silently discarded.
    #[cfg_attr(not(test), allow(dead_code))] // Automatic per-session retirement uses revoke; bulk sweep remains tested.
    pub(crate) fn sweep(&self) -> Result<(), TokenError> {
        self.mutation()?
            .finish(|entries| Self::sweep_entries(entries, self.clock.now()))
    }
}
#[cfg(test)]
mod tests;
