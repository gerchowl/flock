//! Minimal GUI transport payloads specified by #1746 and ADR-0036 §2/§5a.

use crate::api::agent_state::Phase;
use serde::{Deserialize, Serialize};

/// Structured-agent row in the minimal fleet snapshot and its upsert deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct FleetAgentRow {
    /// Opaque structured-agent identity.
    pub id: String,
    /// User-visible agent label.
    pub label: String,
    /// S4's derived phase; ADR-0036 §2 retains ADR-0028's phase derivation.
    pub state: Phase,
    /// Current activity text, omitted when no activity is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub activity: Option<String>,
    /// Open permission request identities, available on this agent's stream.
    pub waiting_on_human: Vec<String>,
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

/// Successful agent.decide ack result, echoing the applied answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct AgentDecideResult {
    /// Harness request identity whose answer was applied.
    pub request_id: String,
    /// Applied permission choice.
    pub decision: PermissionDecision,
}

/// Decision-specific refusal vocabulary; S6 registers these before serving them.
/// These failures have retry class never and use the existing ack error envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum AgentDecideErrorCode {
    /// No permission request has this identity.
    PermissionRequestUnknown,
    /// The request was already decided or expired.
    PermissionRequestClosed,
    /// The caller is not a token-authenticated client principal.
    NotAuthorized,
}

/// Arguments for a token-authenticated client's agent.decide command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct AgentDecide {
    /// Agent whose open permission request is being answered.
    pub agent: String,
    /// Harness request identity from the derived open-request record.
    pub request_id: String,
    /// Permission choice admitted by S6-MVP.
    pub decision: PermissionDecision,
    /// Optional explanation supplied with the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub message: Option<String>,
}

/// Closed permission vocabulary shared by the client and S6-MVP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Permit only this request.
    AllowOnce,
    /// Permit this tool beyond the current request.
    AllowAlways,
    /// Refuse the request.
    Deny,
}

#[cfg(test)]
mod tests {
    use crate::structured_wire::{
        AgentDecide, AgentDecideErrorCode, AgentDecideResult, FleetAgentRow, FleetDelta,
        PermissionDecision,
    };
    use serde_json::json;

    #[test]
    fn structured_rows_round_trip_activity_and_waiting_request_ids() {
        for value in [
            json!({"id":"fixture-agent","label":"Example","state":"working","waiting_on_human":[]}),
            json!({"id":"fixture-agent","label":"Example","state":"needs_human","activity":"Reading a file","waiting_on_human":["request-one","request-two"]}),
        ] {
            let row: FleetAgentRow = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(row).unwrap(), value);
        }
    }

    #[test]
    fn permission_requests_round_trip_all_choices_and_optional_message() {
        for (decision, choice) in [
            (PermissionDecision::AllowOnce, "allow_once"),
            (PermissionDecision::AllowAlways, "allow_always"),
            (PermissionDecision::Deny, "deny"),
        ] {
            assert_eq!(serde_json::to_value(decision).unwrap(), choice);
            for message in [None, Some("Operator explanation")] {
                let mut value =
                    json!({"agent":"fixture-agent","request_id":"request-one","decision":choice});
                if let Some(message) = message {
                    value["message"] = json!(message);
                }
                let request: AgentDecide = serde_json::from_value(value.clone()).unwrap();
                assert_eq!(request.decision, decision);
                assert_eq!(serde_json::to_value(request).unwrap(), value);
            }
        }
        assert!(serde_json::from_value::<AgentDecide>(json!({
            "agent":"fixture-agent","request_id":"request-one","decision":"pass"
        }))
        .is_err());
    }

    #[test]
    fn fleet_payloads_are_closed_and_round_trip_full_rows() {
        let row = json!({"id":"fixture-agent","label":"Example","state":"needs_human",
            "activity":"Permission needed","waiting_on_human":["request-one"]});
        for value in [
            json!({"type":"fleet.upsert","row":row}),
            json!({"type":"fleet.remove","id":"fixture-agent"}),
        ] {
            let delta: FleetDelta = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(delta).unwrap(), value);
        }
        for invalid in [
            json!({"type":"system"}),
            json!({"type":"unknown","raw":"x","shape":"x"}),
            json!({"type":"fleet.upsert"}),
            json!({"type":"fleet.remove"}),
        ] {
            assert!(serde_json::from_value::<FleetDelta>(invalid).is_err());
        }
    }

    #[test]
    fn decision_results_and_failure_codes_use_the_existing_ack_envelope() {
        use crate::structured_wire::{ServerFrame, WireError, WireRetry};
        for decision in [
            PermissionDecision::AllowOnce,
            PermissionDecision::AllowAlways,
            PermissionDecision::Deny,
        ] {
            let result = AgentDecideResult {
                request_id: "request-one".into(),
                decision,
            };
            let value = serde_json::to_value(&result).unwrap();
            assert_eq!(
                serde_json::from_value::<AgentDecideResult>(value.clone()).unwrap(),
                result
            );
            let frame = ServerFrame::Ack {
                cmd_id: "c1".into(),
                ok: true,
                result: Some(value),
                error: None,
            };
            let encoded = serde_json::to_value(&frame).unwrap();
            assert!(encoded.get("error").is_none());
            assert_eq!(encoded["cmd_id"], "c1");
            assert_eq!(
                serde_json::from_value::<ServerFrame>(encoded).unwrap(),
                frame
            );
        }
        for (code, spelling) in [
            (
                AgentDecideErrorCode::PermissionRequestUnknown,
                "permission_request_unknown",
            ),
            (
                AgentDecideErrorCode::PermissionRequestClosed,
                "permission_request_closed",
            ),
            (AgentDecideErrorCode::NotAuthorized, "not_authorized"),
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), spelling);
            assert_eq!(
                serde_json::from_value::<AgentDecideErrorCode>(json!(spelling)).unwrap(),
                code
            );
            let frame = ServerFrame::Ack {
                cmd_id: "c1".into(),
                ok: false,
                result: None,
                error: Some(WireError {
                    code: spelling.into(),
                    message: "Refused".into(),
                    exit: 1,
                    retry: WireRetry::Never,
                    data: None,
                }),
            };
            let encoded = serde_json::to_value(&frame).unwrap();
            assert!(encoded.get("result").is_none());
            assert_eq!(encoded["error"]["retry"], "never");
            assert_eq!(
                serde_json::from_value::<ServerFrame>(encoded).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn generated_minimal_roots_have_optional_strings_and_shared_phase() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/src/gen");
        let row = std::fs::read_to_string(root.join("FleetAgentRow.ts")).unwrap();
        assert!(row.contains("state: Phase,"));
        assert!(row.contains("activity?: string,"));
        assert!(row.contains("waiting_on_human: Array<string>,"));
        let request = std::fs::read_to_string(root.join("AgentDecide.ts")).unwrap();
        assert!(request.contains("message?: string,"));
    }
}
