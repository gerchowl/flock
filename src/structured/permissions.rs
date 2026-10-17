//! Session-local permission decisions (ADR-0036 §10b/§11, #1429).
//! Caller identity is trusted dispatcher context, never command JSON. Only an
//! authenticated operator token may decide; socket UID and agent ancestry alone
//! grant nothing. A new transport owns a new book, including on resume.
//! At most 16 concurrent requests and 1024 lifetime IDs are accepted. Invalid or
//! repeated harness IDs stop the transport rather than risk miscorrelation.
//! Cancellation/EOF discard open requests without inventing operator decisions.
//! Audits and responses share the transport byte cap; an audit failure cancels
//! delivery. Admission success reserves a decision, not its eventual delivery.
//! S6a refuses allow_always with permission_decision_unsupported (not yet
//! supported); session grants are tracked separately in #1809.
use super::claude::ToolRequest;
use crate::api::error_code::ErrorCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Server-authenticated caller context; deliberately not deserializable.
pub enum Principal<'a> {
    /// Created by token authentication, not by decoding command arguments.
    Token { subject: &'a str, operate: bool },
    /// An operator UID without a client token does not authorize a decision.
    Operator,
    /// A pane or message attributed to an agent.
    Agent(&'a str),
    /// A structured harness's lease identity.
    Lease(&'a str),
}

/// The operator's decision, independent of a harness-specific response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Approve only this control request.
    AllowOnce,
    /// Reserved for session grants, currently refused as not yet supported.
    AllowAlways,
    /// Reject through the measured stdio deny carrier (A.12).
    Deny,
}

/// Server arguments for agent.decide; identity is never an argument.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDecide {
    /// Agent whose live runtime receives this decision.
    pub agent: String,
    /// Exact currently open harness request id.
    pub request_id: String,
    /// One of the three permission decisions.
    pub decision: Decision,
    /// Optional reason, carried verbatim in a deny response and audit.
    #[serde(default)]
    pub message: Option<String>,
}

pub(super) struct Open {
    request: ToolRequest,
    pending: bool,
    closed: bool,
}
pub(super) struct Answer {
    pub id: String,
    pub response: Value,
    pub audit: Value,
}
pub(super) struct Book {
    agent: String,
    active: bool,
    open: BTreeMap<String, Open>,
    seen: BTreeSet<String>,
}
impl Book {
    pub fn new(agent: String) -> Self {
        Self {
            agent,
            active: true,
            open: BTreeMap::new(),
            seen: BTreeSet::new(),
        }
    }
    pub fn blocked(&self) -> bool {
        !self.active || !self.open.is_empty()
    }
    pub fn waiting(&self) -> Vec<String> {
        self.open
            .iter()
            .filter(|(_, r)| !r.closed)
            .map(|(id, _)| id.clone())
            .collect()
    }
    pub fn end(&mut self) {
        self.active = false;
        self.open.clear();
    }
    pub fn observe(&mut self, id: &str, request: ToolRequest) -> Result<Value, ErrorCode> {
        if !self.active
            || id.is_empty()
            || id.len() > 256
            || self.seen.contains(id)
            || self.open.len() >= 16
            || self.seen.len() >= 1024
        {
            return Err(ErrorCode::InvalidRequest);
        }
        let audit = json!({"type":"flk_permission_request","state":"open","request_id":id,"request":request});
        self.open.insert(
            id.into(),
            Open {
                request,
                pending: false,
                closed: false,
            },
        );
        self.seen.insert(id.into());
        Ok(audit)
    }
    pub fn prepare(
        &mut self,
        principal: &Principal<'_>,
        args: &AgentDecide,
    ) -> Result<Answer, ErrorCode> {
        let Principal::Token {
            subject,
            operate: true,
        } = principal
        else {
            return Err(ErrorCode::NotAuthorized);
        };
        if subject.is_empty() {
            return Err(ErrorCode::NotAuthorized);
        }
        if args.agent != self.agent {
            return Err(ErrorCode::AgentNotFound);
        }
        let id = args.request_id.as_str();
        let decision = args.decision;
        let message = args.message.as_deref();
        let missing = if self.seen.contains(id) {
            ErrorCode::PermissionRequestClosed
        } else {
            ErrorCode::PermissionRequestUnknown
        };
        let open = self
            .open
            .get_mut(id)
            .filter(|r| !r.pending && !r.closed && self.active)
            .ok_or(missing)?;
        if decision == Decision::AllowAlways {
            return Err(ErrorCode::PermissionDecisionUnsupported);
        }
        let response = if decision == Decision::Deny {
            json!({"behavior":"deny","message":message.unwrap_or("Denied by the operator")})
        } else {
            json!({"behavior":"allow","updatedInput":open.request.input})
        };
        open.pending = true;
        Ok(Answer {
            id: id.into(),
            response: json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":response}}),
            audit: json!({"type":"flk_permission_request","state":"closed","request_id":id,"decision":decision,"message":message,"principal":subject,"source":"token"}),
        })
    }
    pub fn aborted(&mut self, id: &str) {
        if let Some(open) = self.open.get_mut(id) {
            open.pending = false
        }
    }
    pub fn audited(&mut self, answer: &Answer) {
        if let Some(open) = self.open.get_mut(&answer.id) {
            open.closed = true
        }
    }
    pub fn written(&mut self, answer: Answer) {
        self.open.remove(&answer.id);
    }
}

#[cfg(test)]
mod tests;
