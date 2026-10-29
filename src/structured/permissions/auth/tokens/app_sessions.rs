//! Private ephemeral app-session credentials (ADR-0038 §6, #2442 PR1a).
//! The server owner shares this source with socket minting and transport admission.
use super::*;
use crate::platform::process_exit::ExitWatch;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::watch;

const CAPACITY: usize = 128;
/// Fixed lifetime; renewal issues a different credential for the same operator identity.
pub(crate) const TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Fallible clock wait, owned by the caller's async reactor rather than a blocking worker.
#[cfg(any(feature = "web", test))]
pub(crate) type Wait<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<()>> + Send + 'a>>;
/// Supplied non-settable elapsed clock, including suspend, in one deadline domain.
pub(crate) trait Clock: Send + Sync {
    /// Read elapsed time; unavailable clock evidence withholds authorization.
    fn now(&self) -> io::Result<Duration>;
    /// Await the same clock's absolute deadline without polling.
    #[cfg(any(feature = "web", test))]
    fn until(&self, deadline: Duration) -> Wait<'_>;
}
/// Production suspend-inclusive clock: CLOCK_BOOTTIME or mach_continuous_time.
#[derive(Default)]
pub(crate) struct SystemClock<R = crate::platform::suspend_clock::Kernel>(pub(crate) R);
impl SystemClock {
    /// Use the production kernel reader.
    pub(crate) fn new() -> Self {
        Self(crate::platform::suspend_clock::Kernel)
    }
}
impl<R: crate::platform::suspend_clock::Read> Clock for SystemClock<R> {
    fn now(&self) -> io::Result<Duration> {
        crate::platform::suspend_clock::now(&self.0)
    }
    #[cfg(any(feature = "web", test))]
    fn until(&self, deadline: Duration) -> Wait<'_> {
        Box::pin(crate::platform::suspend_clock::until(deadline))
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
    pub(crate) expires: Duration,
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
    #[cfg(all(test, feature = "web"))]
    waiting: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}
/// Published revocation intent and owned helper, transferable outside store I/O.
pub(crate) struct PendingMutation {
    files: Arc<TokenStore>,
    pending: PendingLock,
    intent: priority::Intent<Arc<Namespace>>,
}
/// Exclusive ownership retained through the memory mutation and notification.
pub(crate) struct MutationFence {
    files: Arc<TokenStore>,
    _guard: LockFile,
    _intent: priority::Intent<Arc<Namespace>>,
}
impl PendingMutation {
    /// Five-second startup plus two-second contention: seven seconds of IPC wait.
    /// Caller hooks and owned-child cleanup/scheduling add to this bound.
    /// Call only on the separately bounded retirement-wait worker.
    pub(crate) fn wait(self, on_wait: impl FnOnce()) -> Result<MutationFence, TokenError> {
        let guard = self
            .pending
            .finish_notifying(Duration::from_secs(2), on_wait)?;
        Ok(MutationFence {
            files: self.files,
            _guard: guard,
            _intent: self.intent,
        })
    }
}
impl Store {
    /// Bind a fresh memory-only domain to the shared/exclusive file admission lock.
    pub(crate) fn new(files: Arc<TokenStore>, clock: Arc<dyn Clock>) -> Self {
        Self {
            files,
            entries: Mutex::new(HashMap::new()),
            clock,
            #[cfg(all(test, feature = "web"))]
            waiting: Mutex::default(),
        }
    }
    /// Install a fixture gate before the contended IPC read starts.
    #[cfg(all(test, feature = "web"))]
    pub(crate) fn on_waiting(&self, action: impl FnOnce() + Send + 'static) {
        self.waiting.lock().unwrap().push(Box::new(action));
    }
    /// Signal only after the helper has entered the contended acquisition path.
    #[cfg(all(test, feature = "web"))]
    pub(crate) fn notify_waiting(&self) {
        let action = self.waiting.lock().unwrap().pop();
        // Release the hook mutex before a gate pauses this worker.
        if let Some(action) = action {
            action();
        }
    }
    /// Publish intent and start acquisition without waiting on the store worker.
    pub(crate) fn begin_mutation(&self) -> Result<PendingMutation, TokenError> {
        let intent = priority::Intent::begin(self.files.0.clone(), self.files.random::<16>()?)?;
        let pending = PendingLock::begin(self.files.lock_file()?, &self.files.2)?;
        Ok(PendingMutation {
            files: self.files.clone(),
            pending,
            intent,
        })
    }
    fn mutate<T>(
        &self,
        fence: MutationFence,
        change: impl FnOnce(&mut Entries) -> Result<T, TokenError>,
    ) -> Result<T, TokenError> {
        if !Arc::ptr_eq(&self.files, &fence.files) {
            return Err(TokenError::Unsafe);
        }
        let mut entries = self.entries.lock().map_err(|_| TokenError::Unsafe)?;
        // Keep the intent and exclusive fence alive until notification is published.
        let result = change(&mut entries);
        drop(entries);
        drop(fence);
        result
    }
    fn mutation(&self) -> Result<MutationFence, TokenError> {
        self.begin_mutation()?.wait(|| {
            #[cfg(all(test, feature = "web"))]
            self.notify_waiting();
        })
    }
    /// Finish a previously fenced retirement; None retires the entire owner domain.
    pub(crate) fn finish_mutation(
        &self,
        fence: MutationFence,
        session: Option<&Session>,
    ) -> Result<(), TokenError> {
        self.mutate(fence, |entries| match session {
            Some(session) => Self::remove(entries, &session.digest),
            None => {
                for (_, entry) in entries.drain() {
                    entry.revoke.send_replace(true);
                }
                Ok(())
            }
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
        watch: impl Into<Arc<ExitWatch>>,
    ) -> Result<Issued, IssueError> {
        let watch = watch.into();
        if !watch.matches(authority.peer_credentials())
            || watch.exited().map_err(IssueError::PeerProcessChanged)?
        {
            return Err(IssueError::PeerProcessChanged(io::Error::other(
                "peer watch does not match authority",
            )));
        }
        let process = watch;
        let _guard = self.files.lock()?;
        let mut entries = self.entries.lock().map_err(|_| TokenError::Unsafe)?;
        Self::sweep_entries(&mut entries, self.clock.now().map_err(TokenError::from)?)?;
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
        let issued_at = self.clock.now().map_err(TokenError::from)?;
        let expires = issued_at.checked_add(TTL).ok_or(TokenError::Unsafe)?;
        let info = TokenInfo {
            name: format!("app:{name}"),
            principal: format!("app:operator:{uid}"),
            scopes: operator_scopes(),
            expires_in_ms: Some(
                u64::try_from(
                    expires
                        .checked_sub(issued_at)
                        .ok_or(TokenError::Unsafe)?
                        .as_millis(),
                )
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
    /// Await expiry in the exact clock domain used for issuance and admission.
    #[cfg(feature = "web")]
    pub(crate) async fn expired(&self, session: &Session) -> io::Result<()> {
        self.clock.until(session.expires).await
    }
    fn current(&self, digest: &[u8; 32]) -> Result<Session, TokenError> {
        let entries = self.entries.lock().map_err(|_| TokenError::Unsafe)?;
        let entry = entries.get(digest).ok_or(TokenError::Revoked)?;
        if self.clock.now()? >= entry.session.expires || entry.session.process.exited()? {
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
    /// Lookup returns a snapshot; registration and dispatch separately fence its use.
    /// Like file-token lookup, it does not wait for token-management ownership.
    pub(crate) fn authenticate(&self, token: &Bearer) -> Result<Session, TokenError> {
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
    #[cfg(test)]
    pub(crate) fn revoke(&self, session: &Session) -> Result<(), TokenError> {
        self.finish_mutation(self.mutation()?, Some(session))
    }
    /// Quiescence fence for restart/handoff; no session state belongs in their snapshots.
    #[cfg_attr(not(test), allow(dead_code))] // Async transport shutdown uses the split mutation phases.
    pub(crate) fn revoke_all(&self) -> Result<(), TokenError> {
        self.finish_mutation(self.mutation()?, None)
    }
    fn sweep_entries(entries: &mut Entries, now: Duration) -> Result<(), TokenError> {
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
        self.mutate(self.mutation()?, |entries| {
            Self::sweep_entries(entries, self.clock.now()?)
        })
    }
}
#[cfg(test)]
mod tests;
