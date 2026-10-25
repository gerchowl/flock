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
    /// Ephemeral lifecycle evidence, absent for persistent file credentials.
    pub(crate) fn app(&self) -> Option<&app_sessions::Session> {
        match &self.source {
            Source::App(s) => Some(s),
            _ => None,
        }
    }
}
/// One memory domain shared by socket minting and every transport admission.
pub(crate) struct Store {
    files: Arc<TokenStore>,
    apps: app_sessions::Store,
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
        }
    }
    /// Fixed-scope issuance, requiring the minting socket's LocalOperator proof.
    pub(crate) fn issue(
        &self,
        authority: &LocalOperator<'_>,
        watch: crate::platform::process_exit::ExitWatch,
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
