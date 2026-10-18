//! Minimal GUI transport payloads specified by #1746 and ADR-0036 §2/§5a.

use serde::{Deserialize, Serialize};

pub use crate::structured::fleet_lite::wire::{FleetAgentRow, FleetDelta};

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
    /// The message exceeds the byte bound or carries forbidden control bytes.
    InvalidParams,
}

/// Arguments for a token-authenticated client's agent.decide command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct AgentDecide {
    /// Agent whose open permission request is being answered.
    pub agent: String,
    /// Harness request identity from the derived open-request record.
    pub request_id: String,
    /// Required flk-issued handle from the open record, carried unchanged by S7
    /// slice 5. JSON number, exact in JavaScript below 2^53 like stream cursors.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub request_seq: u64,
    /// Permission choice admitted by S6-MVP.
    pub decision: PermissionDecision,
    /// Optional explanation: CRLF is normalized to LF, then trimmed.
    /// At most 2048 UTF-8 bytes, using prompt_control_bytes rules (newlines and
    /// tabs permitted). Invalid input returns invalid_params before reserving
    /// the request. Empty and absent messages are allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub message: Option<String>,
}

pub use super::permission::PermissionDecision;

#[cfg(test)]
mod tests {
    use crate::api::agent_state::AgentState;
    use crate::structured_wire::{
        AgentDecide, AgentDecideErrorCode, AgentDecideResult, FleetAgentRow, FleetDelta,
        PermissionDecision,
    };
    use serde_json::{json, Value};

    fn state(phase: &str) -> Value {
        serde_json::to_value(serde_json::from_value::<AgentState>(json!({"phase":phase})).unwrap())
            .unwrap()
    }

    #[test]
    fn structured_rows_round_trip_activity_and_waiting_request_ids() {
        for value in [
            json!({"id":"fixture-agent","label":"Example","state":state("working"),"waiting_on_human":[]}),
            json!({"id":"fixture-agent","label":"Example","state":state("needs_human"),"activity":"Reading a file","waiting_on_human":["request-one","request-two"]}),
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
                let mut value = json!({"agent":"fixture-agent","request_id":"request-one","request_seq":1,"decision":choice});
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
        let row = json!({"id":"fixture-agent","label":"Example","state":state("needs_human"),
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
            (AgentDecideErrorCode::InvalidParams, "invalid_params"),
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
    fn newer_fleet_phase_decodes_as_unknown_without_losing_other_axes() {
        let delta: FleetDelta = serde_json::from_value(json!({
            "type":"fleet.upsert","row":{
                "id":"fixture-agent","label":"Example","waiting_on_human":[],
                "state":{"phase":"future_phase","turn":"working","lifecycle":"live"}
            }
        }))
        .expect("newer phases must not discard the fleet delta");
        let value = serde_json::to_value(delta).unwrap();
        assert_eq!(value["row"]["state"]["phase"], "unknown");
        assert_eq!(value["row"]["state"]["turn"], "working");
        assert_eq!(value["row"]["state"]["lifecycle"], "live");
    }

    #[test]
    fn generated_decide_message_documents_server_validation() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/src/gen");
        let request = std::fs::read_to_string(root.join("AgentDecide.ts")).unwrap();
        assert!(request.contains("2048 UTF-8 bytes"));
        assert!(request.contains("trimmed"));
        assert!(request.contains("invalid_params"));
        assert!(request.contains("prompt_control_bytes"));
    }

    #[test]
    fn generated_minimal_roots_have_optional_strings_and_shared_agent_state() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/src/gen");
        let row = std::fs::read_to_string(root.join("FleetAgentRow.ts")).unwrap();
        assert!(row.contains("state: AgentState,"));
        assert!(row.contains("activity?: string,"));
        assert!(row.contains("waiting_on_human: Array<string>,"));
        let request = std::fs::read_to_string(root.join("AgentDecide.ts")).unwrap();
        assert!(request.contains("message?: string,"));
    }
}
