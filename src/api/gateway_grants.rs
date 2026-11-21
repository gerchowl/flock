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
