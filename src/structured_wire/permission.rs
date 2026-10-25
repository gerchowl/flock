//! Host permission records shared by S6 producers and S7 consumers (ADR-0036 §6/§10b).

use crate::structured::claude::ToolRequest;
use serde::{Deserialize, Serialize};

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

/// Host-generated event discriminator, independent of the harness control protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum PermissionRecordKind {
    /// Permission lifecycle audit on the agent stream.
    #[serde(rename = "flk_permission_request")]
    PermissionRequest,
}

/// Durable permission lifecycle record. A close may arrive without an open.
/// Only open records carry the bounded tool input. An ack is not a third state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PermissionRecord {
    /// Fixed known event kind.
    #[serde(rename = "type")]
    pub kind: PermissionRecordKind,
    /// Harness identity; clients key cards by this value.
    pub request_id: String,
    /// Flk-issued handle echoed unchanged by agent.decide.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub request_seq: u64,
    /// Open or closed payload, discriminated directly on the wire by state.
    #[serde(flatten)]
    pub state: PermissionState,
}

/// Permission state, with no independent decided state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PermissionState {
    /// Waiting for a decision; render summaries from the existing typed request.
    Open {
        /// Bounded harness tool request.
        request: ToolRequest,
    },
    /// Retired by a decision or a host fact, even without a preceding open.
    Closed {
        /// Existing flat close fields, including optional policy refusal metadata.
        #[serde(flatten)]
        outcome: PermissionClose,
    },
}

/// Decision close or non-decision close. Policy denies retain decision and reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(untagged)]
pub enum PermissionClose {
    /// An admitted answer, including automatic and capacity-limit answers.
    Answer(PermissionAnswer),
    /// A host close without an answer.
    Reason {
        /// Closed vocabulary shared with restart recovery.
        reason: PermissionCloseReason,
    },
}

/// Why a request closed; future unknown reasons remain unknown events, not decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum PermissionCloseReason {
    /// The current runtime ended.
    Ended,
    /// The harness cancelled this request.
    Cancelled,
    /// A top-level result retired the turn.
    Result,
    /// The host interrupted the turn.
    Interrupted,
    /// A prior runtime left an unmatched open record (#2236).
    SessionEnded,
    /// The live request window was full; no open was published.
    PermissionRequestLimit,
    /// The request exceeded the transport byte cap.
    PermissionRequestTooLarge,
}

/// Existing decision-close fields, preserving explicit null messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PermissionAnswer {
    /// Applied decision.
    pub decision: PermissionDecision,
    /// Operator explanation, or null when absent.
    pub message: Option<String>,
    /// Authenticated subject, or flk for a host policy deny.
    pub principal: String,
    /// Token, live session grant, or host limit source.
    pub source: String,
    /// Optional additional policy close reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub reason: Option<PermissionCloseReason>,
    /// Existing policy error field, using the same closed vocabulary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub error_code: Option<PermissionCloseReason>,
    /// Originating exact grant's request identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub grant_request_id: Option<String>,
    /// Originating exact grant's flk-issued handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "number"))]
    pub grant_request_seq: Option<u64>,
    /// Harness suggestions were recorded but never forwarded as policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub ignored_suggestions: Option<IgnoredPermissionSuggestions>,
}

/// Audit metadata for ignored harness permission suggestions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct IgnoredPermissionSuggestions {
    /// Request carrying the ignored suggestion bundle.
    pub request_id: String,
    /// Fixed reason; these suggestions never become persistent policy.
    pub reason: IgnoredPermissionReason,
}

/// Why harness suggestions were ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum IgnoredPermissionReason {
    /// Harness rules are not forwarded or interpreted.
    NotForwarded,
}

impl PermissionAnswer {
    /// Construct an answer without optional policy or grant metadata.
    pub fn new(
        decision: PermissionDecision,
        message: Option<String>,
        principal: String,
        source: String,
    ) -> Self {
        Self {
            decision,
            message,
            principal,
            source,
            reason: None,
            error_code: None,
            grant_request_id: None,
            grant_request_seq: None,
            ignored_suggestions: None,
        }
    }
}

impl PermissionRecord {
    /// Construct a record with the fixed host discriminator.
    pub fn new(id: &str, request_seq: u64, state: PermissionState) -> Self {
        Self {
            kind: PermissionRecordKind::PermissionRequest,
            request_id: id.into(),
            request_seq,
            state,
        }
    }
    /// Mutable decision metadata; non-decision closes and opens have none.
    pub fn answer_mut(&mut self) -> Option<&mut PermissionAnswer> {
        match &mut self.state {
            PermissionState::Closed {
                outcome: PermissionClose::Answer(answer),
            } => Some(answer),
            _ => None,
        }
    }
    /// Originating grant, if this is an automatic decision.
    pub fn grant_request_id(&self) -> Option<&str> {
        match &self.state {
            PermissionState::Closed {
                outcome: PermissionClose::Answer(answer),
            } => answer.grant_request_id.as_deref(),
            _ => None,
        }
    }
}
