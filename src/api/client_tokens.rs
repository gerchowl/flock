//! Operator client credential DTOs (ADR-0036 §5); diagnostics redact all secrets.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
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
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Bearer(pub(crate) String);
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TokenInfo {
    /// Lowercase operator-assigned client name.
    pub name: String,
    /// Opaque identity, fresh on every issuance.
    pub principal: String,
    /// Exact grants; no implicit scope hierarchy.
    pub scopes: BTreeSet<Scope>,
}
/// The only explicit secret response, for the authenticated issuing operator.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Issued {
    /// Public identity and scope metadata.
    pub info: TokenInfo,
    /// Secret returned only by explicit operator issuance.
    pub token: Bearer,
}
/// A listed entry is either trusted public metadata or an explicit unsafe entry.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Listed {
    /// A verified record, with its lossless management identifier.
    Valid {
        /// Lossless hexadecimal filename identifier for exact revocation.
        entry: String,
        /// Verified public identity and scopes, never the credential.
        info: TokenInfo,
    },
    /// An untrusted entry that authenticates nobody; the identifier permits revocation.
    Unsafe {
        /// Lossless hexadecimal filename identifier, even for non-UTF-8 entries.
        entry: String,
        /// Untrusted UTF-8 filename including its suffix, when representable.
        name: Option<String>,
    },
}
/// Explicit local-operator issuance; scopes never imply each other.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AddParams {
    /// Lowercase client name.
    pub name: String,
    /// Exact grants.
    pub scopes: BTreeSet<Scope>,
}
/// Revoke a named token or a lossless unsafe-entry identifier, exclusively.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoveParams {
    /// Trusted token name.
    pub name: Option<String>,
    /// Hexadecimal filename identifier reported by list.
    pub entry: Option<String>,
}
