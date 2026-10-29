//! Typed materialization identity and local-only projection (ADR-0043 N1a).
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Key {
    Agent(Option<String>, String),
    Node(String),
    Project(String, String),
}
/// One persisted row. Explicit tags isolate future entity kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "row", rename_all = "snake_case")]
pub enum Row {
    /// Structured agent identity and state.
    Agent(FleetAgentRow),
    /// Authenticated node status.
    Node(FleetNodeRow),
    /// Node-scoped project.
    Project(FleetProjectRow),
}
pub(super) fn decode(value: Value) -> Result<Row, serde_json::Error> {
    // Legacy bare agents may already carry an opaque kind extension.
    if value.get("kind").is_some() && value.get("id").is_none() {
        serde_json::from_value(value)
    } else {
        serde_json::from_value(value).map(Row::Agent)
    }
}
impl Row {
    pub(super) fn key(&self) -> Key {
        match self {
            Self::Agent(r) => Key::Agent(r.node_id.clone(), r.id.clone()),
            Self::Node(r) => Key::Node(r.node_id.clone()),
            Self::Project(r) => Key::Project(r.node_id.clone(), r.id.clone()),
        }
    }
    pub(super) fn upsert(self) -> FleetDelta {
        match self {
            Self::Agent(row) => FleetDelta::Upsert { row },
            Self::Node(row) => FleetDelta::NodeUpsert { row },
            Self::Project(row) => FleetDelta::ProjectUpsert { row },
        }
    }
    pub(super) fn remove(&self) -> FleetDelta {
        match self {
            Self::Agent(r) => FleetDelta::Remove {
                id: r.id.clone(),
                node_id: r.node_id.clone(),
            },
            Self::Node(r) => FleetDelta::NodeRemove {
                node_id: r.node_id.clone(),
            },
            Self::Project(r) => FleetDelta::ProjectRemove {
                id: r.id.clone(),
                node_id: r.node_id.clone(),
            },
        }
    }
    pub(super) fn valid(&self) -> bool {
        let node = |id: &str| {
            id.len() == 64
                && id
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        };
        let shape = match self {
            Self::Agent(r) => super::valid(r) && r.node_id.as_deref().is_none_or(node),
            Self::Node(r) => node(&r.node_id) && !r.name.is_empty() && r.name.len() <= 80,
            Self::Project(r) => node(&r.node_id) && !r.id.is_empty() && !r.name.is_empty(),
        };
        shape && serde_json::to_value(self).is_ok_and(|v| exact(&v))
    }
}
fn exact(value: &Value) -> bool {
    match value {
        Value::Number(n) => n.as_u64().is_none_or(|n| n <= 9_007_199_254_740_991),
        Value::Array(items) => items.iter().all(exact),
        Value::Object(items) => items.values().all(exact),
        _ => true,
    }
}
/// Project a local coherent registry/S4/S6 cut. Missing home binding emits only
/// legacy-compatible agents, never a synthetic node. A non-UTF-8 cwd leaves the
/// optional project absent with a diagnostic, without excluding the agent or
/// stopping the cut. Remote status is #2750.
pub fn local_rows(agents: Vec<StructuredAgent<'_>>, home: Option<&str>) -> io::Result<Vec<Row>> {
    let mut rows = Vec::new();
    let mut projects = BTreeMap::new();
    for agent in agents {
        let mut row = project(agent);
        if let Some(home) = home {
            row.node_id = Some(home.into());
            let Some(id) = agent.record.cwd.to_str() else {
                // Keep the agent, but never invent a lossy project identity.
                crate::logging::structured_fleet_project_unavailable();
                rows.push(Row::Agent(row));
                continue;
            };
            row.project = Some(id.into());
            projects.insert(
                id.to_owned(),
                FleetProjectRow {
                    id: id.into(),
                    node_id: home.into(),
                    name: agent
                        .record
                        .cwd
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(id)
                        .into(),
                },
            );
        }
        rows.push(Row::Agent(row));
    }
    if let Some(home) = home {
        rows.push(Row::Node(local_node(home)));
    }
    rows.extend(projects.into_values().map(Row::Project));
    Ok(rows)
}

/// Base row for the current local identity; owner observations fill status.
pub(crate) fn local_node(home: &str) -> FleetNodeRow {
    FleetNodeRow {
        node_id: home.into(),
        name: format!("Node {home}"),
        version: Some(crate::build_info::version()),
        health: None,
        latency_ms: None,
        last_ok: None,
        error: None,
        online: true,
        route: None,
        counts: None,
        unbound_configured_peers: None,
    }
}
