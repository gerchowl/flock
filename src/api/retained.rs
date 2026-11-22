//! Operator-only retained-record maintenance (ADR-0036 retained-record lifetime).
use serde::{Deserialize, Serialize};
/// Explicit rollback-protection selection, independent of age.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    /// Invalid and shadowed records only.
    Dead,
    /// Also destroy forward-compatible data, with explicit confirmation.
    All,
}
/// Server-validated purge request; callers cannot bypass confirmation through JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeParams {
    /// Which kind may be destroyed.
    pub selection: Selection,
    /// Mandatory for all-kind destruction.
    #[serde(default)]
    pub yes: bool,
    /// Strict minimum age in milliseconds; absent stamps do not match.
    pub older_than_ms: Option<u64>,
}
