//! Fleet-lite payload declarations shared by the provider core and S7 wire contracts.
//! These are the existing ADR-0036 §5a types, without endpoint or subscription code.
use crate::api::agent_state::AgentState;
use serde::{Deserialize, Serialize};

/// First client-wire version that understands node/project fleet payloads.
pub const FLEET_ENTITY_VERSION: u32 = 1;

/// Structured-agent row in the minimal fleet snapshot and its upsert deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct FleetAgentRow {
    /// Opaque structured-agent identity.
    pub id: String,
    /// User-visible agent label.
    pub label: String,
    /// Shared S4 axes and phase, read leniently per ADR-0028 §3a.
    pub state: AgentState,
    /// Current activity text, omitted when no activity is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub activity: Option<String>,
    /// Open permission request identities, available on this agent's stream.
    pub waiting_on_human: Vec<String>,
    /// Optional mesh node identity and actual route (ADR-0038 §9).
    /// Local rows omit this; remote projection and forwarding await #1380.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub route: Option<Box<crate::mesh::routes::Route>>,
    /// Owning node; absent only in legacy or not-yet-bound observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub node_id: Option<String>,
    /// Exact Record.cwd project key; absent in legacy snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub project: Option<String>,
}

/// Node status, keyed only by authenticated mesh identity (ADR-0043).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct FleetNodeRow {
    /// Authenticated stable node identity, never a hostname.
    pub node_id: String,
    /// Read-scoped display alias.
    pub name: String,
    /// Owner-observed machine measurements; populated by N1b (#2750).
    pub health: Option<crate::api::schema::PeerSystemSummary>,
    /// Home-measured summary duration in milliseconds; N1b supplies observations.
    #[cfg_attr(feature = "ts", ts(as = "Option<f64>"))]
    pub latency_ms: Option<u64>,
    /// Last success in home-clock Unix milliseconds; N1b supplies observations.
    #[cfg_attr(feature = "ts", ts(as = "Option<f64>"))]
    pub last_ok: Option<u64>,
    /// Owner-reported software version, retained while offline.
    pub version: Option<String>,
    /// Classified selected-path failure or retained offline cause; null when healthy.
    pub error: Option<FleetNodeError>,
    /// Current authenticated direct-link failures, independent of selected path and online.
    /// Empty if none are available; legacy persisted rows default to empty.
    #[serde(default)]
    pub link_diagnostics: Vec<FleetNodeLinkDiagnostic>,
    /// Server-owned reachability classification, not agent liveness.
    pub online: bool,
    /// Actual chosen mesh route, with policy-projected name; N1b supplies it.
    pub route: Option<Box<crate::mesh::routes::Route>>,
    /// Last complete inventory; N1b supplies status counts.
    pub counts: Option<FleetAgentCounts>,
    /// Home-only count of unbound/unreached configured peers; N1b supplies it.
    #[cfg_attr(feature = "ts", ts(as = "Option<f64>"))]
    pub unbound_configured_peers: Option<u64>,
}
/// A current failed direct link; its destination is the containing node row.
/// Cleared on direct recovery or binding/observation removal. Never includes
/// SSH aliases, destinations or raw stderr, and never overrides a healthy relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct FleetNodeLinkDiagnostic {
    /// Authenticated serving home that observed the direct-link failure.
    pub source_node_id: String,
    /// Bounded, content-free classification of that link's failure.
    pub error: FleetNodeError,
}
/// Safe classified node failure, never raw SSH output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct FleetNodeError {
    /// Stable classification token, including SshFailureReason tokens.
    pub code: String,
    /// Bounded content-free explanation.
    pub message: String,
}
/// Complete owner inventory counts; unknown information is never zero-filled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct FleetAgentCounts {
    /// Unique agents, including hibernated/offline agents.
    #[cfg_attr(feature = "ts", ts(as = "f64"))]
    pub total: u64,
    /// Disjoint phase buckets, including unknown, summing to total.
    #[cfg_attr(feature = "ts", ts(as = "std::collections::BTreeMap<String, f64>"))]
    pub by_phase: std::collections::BTreeMap<String, u64>,
    /// Agents with open non-automatic requests; unknown remotely today.
    #[cfg_attr(feature = "ts", ts(as = "Option<f64>"))]
    pub waiting_on_human: Option<u64>,
}
/// Project on one owning node; its key is the registry's exact cwd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct FleetProjectRow {
    /// Exact Record.cwd as a UTF-8 path; no basename-based identity.
    pub id: String,
    /// Owning authenticated node identity.
    pub node_id: String,
    /// Display basename, not the project key.
    pub name: String,
}

/// Closed payload vocabulary carried inside fleet stream ev frames.
/// Snapshot rows are full upserts between gap and snapshot_to, followed by synced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type")]
pub enum FleetDelta {
    /// Insert or replace the complete structured-agent row.
    #[serde(rename = "fleet.upsert")]
    Upsert {
        /// Full row, including its current open permission requests.
        row: FleetAgentRow,
    },
    /// Remove an agent from the materialized fleet.
    #[serde(rename = "fleet.remove")]
    Remove {
        /// Opaque identity of the removed agent.
        id: String,
        /// Owning node, omitted for legacy unbound agent removals.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        node_id: Option<String>,
    },
    /// Insert or replace a complete node row.
    #[serde(rename = "fleet.node.upsert")]
    NodeUpsert {
        /// Complete status, with explicit nulls for unknown observations.
        row: FleetNodeRow,
    },
    /// Explicitly remove a node from the materialization.
    #[serde(rename = "fleet.node.remove")]
    NodeRemove {
        /// Authenticated node identity.
        node_id: String,
    },
    /// Insert or replace a project row.
    #[serde(rename = "fleet.project.upsert")]
    ProjectUpsert {
        /// Complete project identity and label.
        row: FleetProjectRow,
    },
    /// Remove a project from its owning node.
    #[serde(rename = "fleet.project.remove")]
    ProjectRemove {
        /// Exact project cwd.
        id: String,
        /// Owning node identity.
        node_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mesh_route_round_trips_the_existing_node_identity_and_path() {
        let route = crate::mesh::routes::Route {
            node: crate::mesh::identity::NodeIdentity::fixture([36; 32]).node_id(),
            name: "origin.example".into(),
            next_hop: crate::mesh::identity::NodeIdentity::fixture([37; 32]).node_id(),
            hops: 2,
        };
        let value = json!({
            "id":"fixture-agent", "label":"Example", "state":AgentState::unknown(),
            "waiting_on_human":[], "route":route,
        });
        let row: FleetAgentRow = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(row).unwrap(), value);
    }

    #[test]
    fn local_row_json_is_byte_identical_to_the_legacy_contract() {
        let legacy = r#"{"id":"fixture-agent","label":"Example","state":{"turn":"unknown","background":"unknown","attention":"unknown","waiting":"unknown","lifecycle":"unknown","phase":"unknown","source":{}},"waiting_on_human":[]}"#;
        let row: FleetAgentRow = serde_json::from_str(legacy).unwrap();
        assert_eq!(serde_json::to_string(&row).unwrap(), legacy);
    }
}
