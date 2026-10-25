//! Fleet-lite payload declarations shared by the provider core and S7 wire contracts.
//! These are the existing ADR-0036 §5a types, without endpoint or subscription code.
use crate::api::agent_state::AgentState;
use serde::{Deserialize, Serialize};

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
