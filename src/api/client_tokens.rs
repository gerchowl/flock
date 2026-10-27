//! Operator client credential DTOs (ADR-0036 §5); diagnostics redact all secrets.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use zeroize::Zeroize;
/// Explicit token scopes; operate does not silently grant the other scopes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TokenInfo {
    /// Operator-assigned file name or server-assigned app-session label.
    pub name: String,
    /// Authentication identity: stable per operator UID for app sessions, fresh for file tokens.
    pub principal: String,
    /// Exact grants; no implicit scope hierarchy.
    pub scopes: BTreeSet<Scope>,
    /// Remaining app-session lifetime at issuance, in milliseconds, absent for file tokens.
    /// Derived from the store's non-settable, suspend-inclusive clock/deadline (#2489).
    /// Later reads do not refresh it; wall-clock corrections cannot extend it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "number"))]
    pub expires_in_ms: Option<u64>,
}
/// The only explicit secret response, for the authenticated issuing operator.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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

/// Server-chosen scopes and process identity: all caller-supplied fields are refused.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MintParams {
    /// Captured only to return typed invalid_params, never used as authority or grants.
    #[serde(flatten)]
    pub(crate) fields: std::collections::BTreeMap<String, serde_json::Value>,
}
