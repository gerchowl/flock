//! Frozen summary payload and decoder from the v5 parent, before #2474.
//! Keep this independent of the new optional discovery field.
use super::PeerWorkspaceSummary;
#[derive(Debug, Clone, PartialEq)]
pub struct PeerSummaryPayload {
    pub node_id: Option<String>,
    pub outbound_pending: bool,
    pub host: String,
    pub version: Option<String>,
    pub protocol: Option<u32>,
    /// The peer's self-declared fleet icon name (#164), if any.
    pub icon: Option<String>,
    pub system: Option<crate::api::schema::PeerSystemSummary>,
    pub workspaces: Vec<PeerWorkspaceSummary>,
    /// Round-trip wall time of the summary SSH call (free latency probe).
    pub latency_ms: u64,
    /// Gossip v3 relay: the peer's own polled peers, so the hub can render
    /// two-hop fleet visibility. Empty when the peer is v(N-1) — additive
    /// with a serde default keeps mixed-version fleets safe.
    pub relayed_fleet: Vec<crate::api::schema::RelayedFleetPeer>,
}

fn parse_summary_response(stdout: &str, latency_ms: u64) -> Result<PeerSummaryPayload, String> {
    // Login shells can print banners before the JSON; find the envelope line.
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .ok_or_else(|| "no JSON in summary output".to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|err| format!("summary parse error: {err}"))?;
    if let Some(error) = value.get("error") {
        return Err(format!("peer error: {error}"));
    }
    let result = value
        .get("result")
        .ok_or_else(|| "summary response has no result".to_string())?;
    let host = result
        .get("host")
        .and_then(|host| host.as_str())
        .unwrap_or_default()
        .to_string();
    let version = result
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let protocol = result
        .get("protocol")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u32::try_from(p).ok());
    // #164: the peer's self-declared icon name. Additive/optional — a v(N-1)
    // peer never emits it, parsing as None (no icon).
    let icon = result
        .get("icon")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // #291: the JSON path does not pass through the bincode `From` impl, so
    // the host-declared thermal rank/label is normalized here instead.
    let system = result
        .get("system")
        .filter(|system| !system.is_null())
        .cloned()
        .map(serde_json::from_value::<crate::api::schema::PeerSystemSummary>)
        .transpose()
        .map_err(|err| format!("summary system parse error: {err}"))?
        .map(crate::api::schema::PeerSystemSummary::sanitized);
    let workspaces: Vec<PeerWorkspaceSummary> = result
        .get("workspaces")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| format!("summary workspaces parse error: {err}"))?
        .unwrap_or_default();
    // Gossip v3 (#101): relayed_fleet is additive with a serde default so a
    // v(N-1) peer that never emits the field parses cleanly.
    let mut relayed_fleet: Vec<crate::api::schema::RelayedFleetPeer> = result
        .get("relayed_fleet")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| format!("summary relayed_fleet parse error: {err}"))?
        .unwrap_or_default();
    // #291: a relayed entry is host-authored two hops back — sanitize it on
    // the same boundary rather than trusting the middle hop to have done it.
    for entry in &mut relayed_fleet {
        entry.system = entry
            .system
            .take()
            .map(crate::api::schema::PeerSystemSummary::sanitized);
    }
    Ok(PeerSummaryPayload {
        node_id: result
            .get("node_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        outbound_pending: result
            .get("outbound_pending")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        host,
        version,
        protocol,
        icon,
        system,
        workspaces,
        latency_ms,
        relayed_fleet,
    })
}

#[test]
fn v5_decoder_ignores_structured_agents_from_new_summary() {
    let summary = serde_json::json!({"id":"compat", "result":{
        "type":"peers_summary", "host":"fixture.example", "workspaces":[],
        "structured_agents":[{"agent_id":"native-id", "runtime":"structured", "agent":"claude", "status":"idle"}]
    }});
    let decoded = parse_summary_response(&summary.to_string(), 0).unwrap();
    assert_eq!(decoded.host, "fixture.example");
    assert!(decoded.workspaces.is_empty());
    assert_eq!(crate::mesh::hello::VERSION, 5);
}
