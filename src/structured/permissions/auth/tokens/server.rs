//! Server-owned union of persistent and ephemeral credentials (ADR-0038 §6).
use super::*;
use std::sync::Arc;
/// Identity sealed by either source; callers cannot substitute a source or digest.
#[derive(Clone)]
pub(crate) struct Session {
    /// Exact authenticated grants.
    pub(crate) info: TokenInfo,
    source: Source,
}
#[derive(Clone)]
enum Source {
    File(super::Session),
    App(app_sessions::Session),
}
impl Session {
    /// Match the authenticated credential, independently of its shared operator identity.
    pub(crate) fn same_credential(&self, other: &Self) -> bool {
        match (&self.source, &other.source) {
            (Source::App(a), Source::App(b)) => a.same_credential(b),
            (Source::File(a), Source::File(b)) => a.digest == b.digest,
            _ => false,
        }
    }
    /// Ephemeral lifecycle evidence, absent for persistent file credentials.
    pub(crate) fn app(&self) -> Option<&app_sessions::Session> {
        match &self.source {
            Source::App(s) => Some(s),
            _ => None,
        }
    }
}
/// Process-instance mismatch stays distinct from token/store failures.
#[derive(Debug)]
pub(crate) enum PeerAdmissionError {
    /// Preserve storage, revocation and scope errors for the transport's existing mapping.
    Token(TokenError),
    /// Registered identity-proof refusal, including app hello without a local peer.
    PeerProcessChanged,
}
impl From<TokenError> for PeerAdmissionError {
    fn from(error: TokenError) -> Self {
        Self::Token(error)
    }
}
impl std::fmt::Display for PeerAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token(error) => std::fmt::Display::fmt(error, f),
            Self::PeerProcessChanged => {
                f.write_str(crate::api::error_code::ErrorCode::PeerProcessChanged.as_str())
            }
        }
    }
}
impl std::error::Error for PeerAdmissionError {}
/// Explicit fixture boundaries around the fenced authentication and insertion.
#[cfg(all(test, feature = "web"))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerStage {
    /// The lookup succeeded, before inspecting the accepted peer readiness.
    Authenticated,
    /// A kernel lock helper is waiting for management or a revocation intent.
    Waiting,
    /// The shared management fence was acquired, before queued store I/O.
    LockAcquired,
    /// Peer checks passed, before invoking the registration callback.
    Checked,
    /// The registration callback returned, before releasing admission ownership.
    Registered,
}
#[cfg(all(test, feature = "web"))]
type PeerHook = (PeerStage, Box<dyn FnOnce() + Send>);
/// Owned hello fence, acquired outside the shared store-I/O pool and held through insertion.
/// Only this store module can construct it; callers cannot substitute an unlocked file.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub(crate) struct PeerFence(Arc<TokenStore>, LockFile);
/// One memory domain shared by socket minting and every transport admission.
pub(crate) struct Store {
    files: Arc<TokenStore>,
    apps: app_sessions::Store,
    #[cfg(all(test, feature = "web"))]
    peer_hooks: Mutex<Vec<PeerHook>>,
}
impl Store {
    /// Bind both sources to the same filesystem admission and revocation fence.
    pub(crate) fn new(files: TokenStore, clock: Arc<dyn app_sessions::Clock>) -> Self {
        #[cfg(test)]
        let files = {
            let mut files = files;
            files.2 = super::tests::helper_binary();
            files
        };
        let files = Arc::new(files);
        Self {
            apps: app_sessions::Store::new(files.clone(), clock),
            files,
            #[cfg(all(test, feature = "web"))]
            peer_hooks: Mutex::default(),
        }
    }
    /// Fixed-scope issuance, requiring the minting socket's LocalOperator proof.
    pub(crate) fn issue(
        &self,
        authority: &LocalOperator<'_>,
        watch: impl Into<Arc<crate::platform::process_exit::ExitWatch>>,
    ) -> Result<Issued, app_sessions::IssueError> {
        self.apps.issue_watched(authority, watch)
    }
    /// Authenticate either source; uncertain store failures never fall back silently.
    pub(crate) fn authenticate(&self, token: &Bearer) -> Result<Session, TokenError> {
        if self.apps.contains(token)? {
            let session = self.apps.authenticate(token)?;
            return Ok(Session {
                info: session.info.clone(),
                source: Source::App(session),
            });
        }
        self.files.authenticate(token).map(|session| Session {
            info: session.info.clone(),
            source: Source::File(session),
        })
    }

    /// Await management and revocation ownership under the remaining hello deadline.
    /// Run on the separately bounded hello-wait worker, never the store-I/O pool.
    // Native registration is compiled but unused without the web transport.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(crate) fn wait_peer(&self, budget: Duration) -> Result<PeerFence, TokenError> {
        let guard = self.files.hello_lock(budget, || {
            #[cfg(all(test, feature = "web"))]
            self.peer_stage(PeerStage::Waiting);
        })?;
        #[cfg(all(test, feature = "web"))]
        self.peer_stage(PeerStage::LockAcquired);
        Ok(PeerFence(self.files.clone(), guard))
    }
    /// Synchronous fixture entry retains the same fence and callback ordering.
    #[cfg(test)]
    pub(crate) fn admit_peer<T>(
        &self,
        token: &Bearer,
        peer: Option<&crate::app::LocalPeer>,
        budget: Duration,
        register: impl FnOnce(Session) -> T,
    ) -> Result<T, PeerAdmissionError> {
        self.register_peer(self.wait_peer(budget)?, token, peer, register)
    }
    /// Recheck identity and register while retaining the previously acquired fence.
    /// The callback must stay bounded and must not acquire an exclusive store lock.
    // Native registration is compiled but unused without the web transport.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    pub(crate) fn register_peer<T>(
        &self,
        fence: PeerFence,
        token: &Bearer,
        peer: Option<&crate::app::LocalPeer>,
        register: impl FnOnce(Session) -> T,
    ) -> Result<T, PeerAdmissionError> {
        if !Arc::ptr_eq(&self.files, &fence.0) {
            return Err(TokenError::Unsafe.into());
        }
        let _guard = fence.1;
        let session = self.authenticate(token)?;
        #[cfg(all(test, feature = "web"))]
        self.peer_stage(PeerStage::Authenticated);
        let peer_exited = match peer {
            Some(peer) => peer.process().exited().map_err(TokenError::from)?,
            None => false,
        };
        if peer_exited {
            return Err(PeerAdmissionError::PeerProcessChanged);
        }
        if session
            .app()
            .is_some_and(|app| peer.is_none_or(|p| !app.process.matches(p.credentials())))
        {
            return Err(PeerAdmissionError::PeerProcessChanged);
        }
        #[cfg(all(test, feature = "web"))]
        self.peer_stage(PeerStage::Checked);
        let result = register(session);
        #[cfg(all(test, feature = "web"))]
        self.peer_stage(PeerStage::Registered);
        Ok(result)
    }

    /// Install a one-shot fixture gate at an explicit admission boundary.
    #[cfg(all(test, feature = "web"))]
    pub(crate) fn on_peer_stage(&self, stage: PeerStage, action: impl FnOnce() + Send + 'static) {
        self.peer_hooks
            .lock()
            .unwrap()
            .push((stage, Box::new(action)));
    }
    #[cfg(all(test, feature = "web"))]
    fn peer_stage(&self, stage: PeerStage) {
        let action = {
            let mut hooks = self.peer_hooks.lock().unwrap();
            hooks
                .iter()
                .position(|(at, _)| *at == stage)
                .map(|index| hooks.remove(index).1)
        };
        if let Some(action) = action {
            action();
        }
    }

    /// Recheck the original source under its shared fence through bounded dispatch.
    pub(crate) fn admit<T>(
        &self,
        session: &Session,
        required: Scope,
        dispatch: impl for<'a> FnOnce(VerifiedToken<'a>) -> T,
    ) -> Result<T, TokenError> {
        match &session.source {
            Source::App(s) => self.apps.admit(s, required, dispatch),
            Source::File(s) => self.files.admit(s, required, dispatch),
        }
    }
    /// Retire the exact ephemeral credential under the existing exclusive fence.
    pub(crate) fn retire(&self, session: &Session) -> Result<(), TokenError> {
        match session.app() {
            Some(s) => self.apps.revoke(s),
            None => Err(TokenError::Unsafe),
        }
    }
    /// Revoke all ephemeral credentials before server shutdown or handoff.
    pub(crate) fn shutdown(&self) -> Result<(), TokenError> {
        self.apps.revoke_all()
    }
    /// Generate transport epoch identities without disclosing credentials.
    pub(crate) fn random<const N: usize>(&self) -> Result<String, TokenError> {
        self.files.random::<N>()
    }
}
/// Lazy, process-local server ownership; never exported into handoff snapshots.
#[derive(Clone, Default)]
pub(crate) struct Owner(Arc<Mutex<Option<Arc<Store>>>>);
impl Owner {
    /// First open installs the domain once; later transports share it, never reopen it.
    /// Call on the existing bounded store worker when running on an async executor.
    pub(crate) fn get(
        &self,
        data: &Path,
        clock: Arc<dyn app_sessions::Clock>,
    ) -> Result<Arc<Store>, TokenError> {
        let mut store = self.0.lock().map_err(|_| TokenError::Unsafe)?;
        if let Some(store) = &*store {
            return Ok(store.clone());
        }
        let opened = Arc::new(Store::new(TokenStore::open(data)?, clock));
        *store = Some(opened.clone());
        Ok(opened)
    }
}

#[cfg(test)]
mod tests;
