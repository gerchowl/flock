//! Client bearer verification and revocation-serialized admission (ADR-0036 §5a).
use super::VerifiedToken;
use crate::app::LocalOperator;
use crate::platform::client_tokens::{Entry, Namespace};
use crate::platform::{client_token_lock::PendingLock, file_lock::LockFile};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{File, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
mod priority;
use std::time::Duration;
use zeroize::Zeroize;

pub(crate) use crate::api::client_tokens::{Bearer, Issued, Listed, Scope, TokenInfo};
/// Admission failures contain no bearer or file contents.
#[derive(Debug)]
pub(crate) enum TokenError {
    /// The presented bearer does not authenticate.
    Unauthorized,
    /// A previously authenticated credential was removed or replaced.
    Revoked,
    /// A valid credential lacks the current operation scope.
    Scope,
    /// Another admission or management operation holds the lock.
    Busy,
    /// Invalid name or malformed/untrusted storage.
    Unsafe,
    /// An explicit underlying filesystem or entropy failure.
    Io(io::Error),
}
impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "client token: {self:?}")
    }
}
impl std::error::Error for TokenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Io(e) = self {
            Some(e)
        } else {
            None
        }
    }
}
impl From<io::Error> for TokenError {
    fn from(e: io::Error) -> Self {
        if matches!(
            e.kind(),
            io::ErrorKind::PermissionDenied
                | io::ErrorKind::InvalidInput
                | io::ErrorKind::NotADirectory
        ) {
            Self::Unsafe
        } else {
            Self::Io(e)
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    token: Bearer,
    principal: String,
    scopes: BTreeSet<Scope>,
}
/// Connection identity: subsequent admission rechecks current storage and scopes.
#[derive(Clone)]
pub(crate) struct Session {
    /// Public identity and scope metadata.
    pub(crate) info: TokenInfo,
    digest: [u8; 32],
}
/// Store handles share a stable filesystem admission lock, including across processes.
pub(crate) struct TokenStore(Namespace, Mutex<Box<dyn Entropy>>, PathBuf);
/// Injectable randomness; production uses the operating system, fixtures use a sequence.
pub(crate) trait Entropy: Send {
    /// Fill the whole buffer or fail without issuing a credential.
    fn fill(&mut self, bytes: &mut [u8]) -> io::Result<()>;
}
struct SystemEntropy;
impl Entropy for SystemEntropy {
    fn fill(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        getrandom::fill(bytes).map_err(io::Error::other)
    }
}
type LoadedEntry = (Entry, Option<(String, Record)>);

struct PendingRevoke<'a> {
    store: &'a TokenStore,
    entry: String,
    guard: PendingLock,
    _intent: priority::Intent<'a>,
}
impl PendingRevoke<'_> {
    fn finish(self) -> Result<(), TokenError> {
        let _guard = self.guard.finish(Duration::from_secs(2))?;
        self.store.0.remove_entry(&self.entry)?;
        Ok(())
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn valid_hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}
fn filename(name: &str) -> Result<String, TokenError> {
    if name.is_empty()
        || name.starts_with('-')
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
    {
        return Err(TokenError::Unsafe);
    }
    Ok(format!("{name}.token"))
}
impl TokenStore {
    /// Open a private namespace below an existing trusted data directory.
    pub(crate) fn open(data: &Path) -> Result<Self, TokenError> {
        Self::open_with_entropy(data, SystemEntropy)
    }
    /// Open with an explicit entropy provider, including deterministic offline fixtures.
    pub(crate) fn open_with_entropy(
        data: &Path,
        entropy: impl Entropy + 'static,
    ) -> Result<Self, TokenError> {
        let store = Self(
            Namespace::open(data)?,
            Mutex::new(Box::new(entropy)),
            std::env::current_exe()?,
        );
        // The writer lock protects active issue temporaries from the orphan sweep.
        let _guard = store.lock()?;
        for entry in store.0.names()? {
            if entry.id.starts_with("2e746d702d") {
                if let Err(error) = store.0.remove_temporary(&entry.id) {
                    match error.kind() {
                        io::ErrorKind::NotFound => (),
                        io::ErrorKind::IsADirectory => {
                            crate::logging::client_token_entry_unsafe(&entry.id)
                        }
                        _ => return Err(error.into()),
                    }
                }
            }
        }
        store.0.check()?;
        Ok(store)
    }
    pub(crate) fn random<const N: usize>(&self) -> Result<String, TokenError> {
        let mut bytes = [0; N];
        let result = self
            .1
            .lock()
            .map_err(|_| io::Error::other("entropy provider poisoned"))?
            .fill(&mut bytes);
        let value = result.map(|()| hex(&bytes));
        bytes.zeroize();
        Ok(value?)
    }
    fn lock_file(&self) -> Result<File, TokenError> {
        let file = match self.0.file(".lock", true) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => self.0.file(".lock", false)?,
            Err(e) => return Err(e.into()),
        };
        Ok(file)
    }
    fn lock(&self) -> Result<LockFile, TokenError> {
        if priority::pending(&self.0)? {
            return Err(TokenError::Busy);
        }
        let file = LockFile::new(self.lock_file()?);
        file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => TokenError::Busy,
            TryLockError::Error(e) => e.into(),
        })?;
        if priority::pending(&self.0)? {
            return Err(TokenError::Busy);
        }
        Ok(file)
    }
    fn begin_revoke_entry(&self, entry: String) -> Result<PendingRevoke<'_>, TokenError> {
        let intent = priority::Intent::begin(&self.0, self.random::<16>()?)?;
        let guard = PendingLock::begin(self.lock_file()?, &self.2)?;
        Ok(PendingRevoke {
            store: self,
            entry,
            guard,
            _intent: intent,
        })
    }
    fn begin_revoke(&self, name: &str) -> Result<PendingRevoke<'_>, TokenError> {
        self.begin_revoke_entry(hex(filename(name)?.as_bytes()))
    }
    fn load(&self, name: &str) -> Result<Record, TokenError> {
        let mut bytes = Vec::new();
        self.0
            .file(&filename(name)?, false)?
            .take(4097)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            bytes.zeroize();
            return Err(TokenError::Unsafe);
        }
        let parsed = serde_json::from_slice::<Record>(&bytes);
        bytes.zeroize();
        let record = parsed.map_err(|_| TokenError::Unsafe)?;
        if !valid_hex(&record.token.0, 64) || !valid_hex(&record.principal, 32) {
            return Err(TokenError::Unsafe);
        }
        Ok(record)
    }
    fn info(name: &str, r: &Record) -> TokenInfo {
        TokenInfo {
            name: name.into(),
            principal: r.principal.clone(),
            scopes: r.scopes.clone(),
        }
    }
    fn save(&self, name: &str, record: &Record, replace: bool) -> Result<(), TokenError> {
        let destination = filename(name)?;
        let temp = format!(".tmp-{}", self.random::<16>()?);
        let mut file = self.0.file(&temp, true)?;
        let result = (|| {
            serde_json::to_writer(&mut file, record).map_err(io::Error::other)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            self.0.publish(&temp, &destination, replace)?;
            Ok(())
        })();
        match self.0.remove(&temp) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        result
    }
    /// Issue once; never overwrite a token. Only a local-operator proof authorizes this.
    pub(crate) fn issue(
        &self,
        _: &LocalOperator<'_>,
        name: &str,
        scopes: BTreeSet<Scope>,
    ) -> Result<Issued, TokenError> {
        let _guard = self.lock()?;
        let record = Record {
            token: Bearer(self.random::<32>()?),
            principal: self.random::<16>()?,
            scopes,
        };
        self.save(name, &record, false)?;
        Ok(Issued {
            info: Self::info(name, &record),
            token: record.token.clone(),
        })
    }
    /// Reuse only the exact operator credential, or issue it without overwriting any entry.
    pub(crate) fn operator_token(
        &self,
        authority: &LocalOperator<'_>,
    ) -> Result<Bearer, TokenError> {
        let scopes = BTreeSet::from([Scope::Read, Scope::Send, Scope::Answer, Scope::Operate]);
        let guard = self.lock()?;
        match self.load("operator") {
            Ok(record) if record.scopes == scopes => Ok(record.token),
            Ok(_) => Err(TokenError::Unsafe),
            Err(TokenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                drop(guard);
                self.issue(authority, "operator", scopes)
                    .map(|issued| issued.token)
            }
            Err(error) => Err(error),
        }
    }
    /// List trusted metadata and explicit unsafe entries, never secrets.
    pub(crate) fn list(&self, _: &LocalOperator<'_>) -> Result<Vec<Listed>, TokenError> {
        self.entries()?
            .into_iter()
            .map(|(entry, loaded)| {
                Ok(match loaded {
                    Some((name, record)) => Listed::Valid {
                        entry: entry.id,
                        info: Self::info(&name, &record),
                    },
                    None => Listed::Unsafe {
                        entry: entry.id,
                        name: entry.name,
                    },
                })
            })
            .collect()
    }
    // ADR-0028 §3a: a bad entry matches nobody, not a denial of every other client.
    fn entries(&self) -> Result<Vec<LoadedEntry>, TokenError> {
        let mut entries = self.0.names()?;
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        let loaded = entries
            .into_iter()
            .filter(|entry| entry.id.ends_with("2e746f6b656e"))
            .map(|entry| self.load_entry(entry))
            .collect();
        self.0.check()?;
        Ok(loaded)
    }
    fn load_entry(&self, entry: Entry) -> LoadedEntry {
        let loaded = entry
            .name
            .as_deref()
            .and_then(|file| file.strip_suffix(".token"))
            .and_then(|name| self.load(name).ok().map(|record| (name.to_owned(), record)));
        if loaded.is_none() {
            crate::logging::client_token_entry_unsafe(&entry.id);
        }
        (entry, loaded)
    }
    /// Revoke even an unsafe record, waiting at most two seconds for admission.
    pub(crate) fn revoke(&self, _: &LocalOperator<'_>, name: &str) -> Result<(), TokenError> {
        self.begin_revoke(name)?.finish()
    }
    /// Revoke an exact listed token entry, including a non-UTF-8 filename.
    pub(crate) fn revoke_entry(
        &self,
        _: &LocalOperator<'_>,
        entry: &str,
    ) -> Result<(), TokenError> {
        self.begin_revoke_entry(entry.into())?.finish()
    }
    /// Authenticate hello against current token files; address grants no identity.
    pub(crate) fn authenticate(&self, presented: &Bearer) -> Result<Session, TokenError> {
        if !valid_hex(&presented.0, 64) {
            return Err(TokenError::Unauthorized);
        }
        let digest: [u8; 32] = Sha256::digest(presented.0.as_bytes()).into();
        for (_, loaded) in self.entries()? {
            let Some((name, r)) = loaded else { continue };
            if equal(&Sha256::digest(r.token.0.as_bytes()).into(), &digest) {
                return Ok(Session {
                    info: Self::info(&name, &r),
                    digest,
                });
            }
        }
        Err(TokenError::Unauthorized)
    }
    /// Recheck before dispatch and hold the revocation lock through bounded admission.
    /// The callback cannot return the borrowed sealed capability. It must not wait
    /// for model execution or call token management recursively; contention is Busy.
    pub(crate) fn admit<T>(
        &self,
        session: &Session,
        required: Scope,
        dispatch: impl for<'a> FnOnce(VerifiedToken<'a>) -> T,
    ) -> Result<T, TokenError> {
        let _guard = self.lock()?;
        let r = self.load(&session.info.name).map_err(|e| match e {
            TokenError::Io(e) if e.kind() == io::ErrorKind::NotFound => TokenError::Revoked,
            e => e,
        })?;
        if r.principal != session.info.principal
            || !equal(
                &Sha256::digest(r.token.0.as_bytes()).into(),
                &session.digest,
            )
        {
            return Err(TokenError::Revoked);
        }
        if !r.scopes.contains(&required) {
            return Err(TokenError::Scope);
        }
        Ok(dispatch(VerifiedToken::new(
            &r.principal,
            r.scopes.contains(&Scope::Operate),
        )))
    }
}

#[cfg(test)]
mod tests;
