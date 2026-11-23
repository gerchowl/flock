//! Destination-local gateway authority; no bearer or reusable credential.
use super::client_tokens::Scope;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Named, explicit maximum authority for principals issued by one enrolled home.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub name: String,
    pub home_node_id: String,
    pub principals: BTreeSet<String>,
    pub verbs: BTreeSet<String>,
    pub scopes: BTreeSet<Scope>,
    /// Union of `agent:<id>`, `workspace:<id>`, and explicit `*`; empty denies all.
    pub targets: BTreeSet<String>,
    /// Absent on pre-Q9 rows; those rows retain their standing A1 semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<Box<Lifecycle>>,
}
/// Invalid/future entries are listed by name with null authority, and preserved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub generation: u64,
    pub entries: BTreeMap<String, Option<Grant>>,
}
/// Exact operator-assigned management name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revoke {
    pub name: String,
}

/// Authentication class, or an operator-assigned role expanded into `principals`.
/// Role members are explicit namespaced principals, never inferred from lineage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Subject {
    User,
    Agent,
    Role { role: Role, kind: SubjectKind },
}
/// Roles narrow explicit verbs/targets; none confers issuance authority.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Operator,
    OwnerOfAgent,
    Delegate,
}
/// Dormant lifecycle authority. Only the sealed local operator may install it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lifecycle {
    pub subject: Subject,
    /// Pinned destination NodeId, not a routing name. `*` targets remain explicit.
    pub destination_node_id: String,
    /// None is standing; one is once; any other value must be positive.
    pub max_uses: Option<u64>,
    /// Requested positive lifetime; the destination stamps both deadlines.
    pub lifetime_ms: Option<u64>,
    /// Assigned by the destination on add, never accepted from the issuer client.
    pub issuance: Option<Issuance>,
}
/// Immutable issuance identity and destination-issued clock evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Issuance {
    pub id: String,
    /// Local operator at the pinned destination, not a bearer identity.
    pub issuer: String,
    pub issued: Option<ClockPair>,
    pub expires_at_ms: Option<u64>,
    pub elapsed_deadline_ms: Option<u64>,
}
/// Paired destination UTC and suspend-inclusive elapsed milliseconds.
/// Domain equality is required; missing recovery evidence refuses timed authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockPair {
    pub domain: String,
    pub wall_ms: u64,
    pub elapsed_ms: u64,
}

/// Authentication class of every explicit member in a role grant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    User,
    Agent,
}
