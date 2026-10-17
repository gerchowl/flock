//! Client bearer verification and revocation-serialized admission (ADR-0036 §5a).
use super::VerifiedToken;
use crate::app::LocalOperator;
use crate::platform::client_tokens::Namespace;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{File, TryLockError};
use std::io::{self, Read, Write};
use std::path::Path;
use zeroize::Zeroize;

/// Explicit token scopes; operate does not silently grant the other scopes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Read authorized streams.
    Read,
    /// Queue authorized user turns.
    Send,
    /// Answer scoped questions without acquiring operator authority.
    Answer,
    /// Act as an operator for explicitly authorized verbs.
    Operate,
}
/// A wire bearer whose diagnostic form is always redacted.
#[derive(Clone, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Bearer(String);
impl std::fmt::Debug for Bearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bearer(<redacted>)")
    }
}
impl Drop for Bearer {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
/// Public token metadata, never containing a bearer or digest.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TokenInfo {
    /// Lowercase operator-assigned client name.
    pub name: String,
    /// Opaque identity, fresh on every issuance.
    pub principal: String,
    /// Exact grants; no implicit scope hierarchy.
    pub scopes: BTreeSet<Scope>,
}
/// The only explicit secret response, for the authenticated issuing operator.
#[derive(Debug, Serialize)]
pub struct Issued {
    /// Public identity and scope metadata.
    pub info: TokenInfo,
    /// Secret returned only by explicit operator issuance.
    pub token: Bearer,
}
/// Admission failures contain no bearer or file contents.
#[derive(Debug)]
pub enum TokenError {
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
pub struct Session {
    /// Public identity and scope metadata.
    pub info: TokenInfo,
    digest: [u8; 32],
}
/// Store handles share a stable filesystem admission lock, including across processes.
pub struct TokenStore(Namespace);
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn random<const N: usize>() -> Result<String, TokenError> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let value = hex(&bytes);
    bytes.zeroize();
    Ok(value)
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
    pub fn open(data: &Path) -> Result<Self, TokenError> {
        Ok(Self(Namespace::open(data)?))
    }
    fn lock(&self) -> Result<File, TokenError> {
        let file = match self.0.file(".lock", true) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => self.0.file(".lock", false)?,
            Err(e) => return Err(e.into()),
        };
        file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => TokenError::Busy,
            TryLockError::Error(e) => e.into(),
        })?;
        Ok(file)
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
        let temp = format!(".tmp-{}", random::<16>()?);
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
    pub fn issue(
        &self,
        _: &LocalOperator<'_>,
        name: &str,
        scopes: BTreeSet<Scope>,
    ) -> Result<Issued, TokenError> {
        let _guard = self.lock()?;
        let record = Record {
            token: Bearer(random::<32>()?),
            principal: random::<16>()?,
            scopes,
        };
        self.save(name, &record, false)?;
        Ok(Issued {
            info: Self::info(name, &record),
            token: record.token.clone(),
        })
    }
    /// List metadata only, never secrets, after the local-operator boundary.
    pub fn list(&self, _: &LocalOperator<'_>) -> Result<Vec<TokenInfo>, TokenError> {
        let _guard = self.lock()?;
        self.names()?
            .iter()
            .map(|name| Ok(Self::info(name, &self.load(name)?)))
            .collect()
    }
    fn names(&self) -> Result<Vec<String>, TokenError> {
        let mut names: Vec<_> = self
            .0
            .names()?
            .into_iter()
            .filter_map(|n| n.strip_suffix(".token").map(str::to_owned))
            .collect();
        names.sort();
        Ok(names)
    }
    /// Revoke under the same lock as admission. Existing admitted work is not cancelled.
    pub fn revoke(&self, _: &LocalOperator<'_>, name: &str) -> Result<(), TokenError> {
        let _guard = self.lock()?;
        self.load(name)?;
        self.0.remove(&filename(name)?)?;
        Ok(())
    }
    /// Authenticate hello against current token files; address grants no identity.
    pub fn authenticate(&self, presented: &Bearer) -> Result<Session, TokenError> {
        if !valid_hex(&presented.0, 64) {
            return Err(TokenError::Unauthorized);
        }
        let _guard = self.lock()?;
        let digest: [u8; 32] = Sha256::digest(presented.0.as_bytes()).into();
        for name in self.names()? {
            let r = self.load(&name)?;
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
    pub fn admit<T>(
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
