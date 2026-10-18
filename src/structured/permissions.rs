//! Session-local permission decisions (ADR-0036 §10b/§11, #1429).
//! Caller identity is trusted dispatcher context, never command JSON. Only an
//! authenticated operator token may decide; socket UID and agent ancestry alone
//! grant nothing. A new transport owns a new book, including on resume.
//! There is no lifetime request limit (ADR-0036 S6). Keep at most
//! [`MAX_OPEN_PERMISSION_REQUESTS`] live/in-flight requests in full. Excess
//! requests receive an audited `permission_request_limit` deny, not a session stop.
//! Retain the last [`CLOSED_PERMISSION_REQUEST_WINDOW`] durably closed IDs in FIFO
//! order. Late decisions for retained IDs return `permission_request_closed`;
//! evicted or never-seen IDs return `permission_request_unknown` (both retry never).
//! Invalid IDs, or duplicates still live or retained, stop the transport rather
//! than risk miscorrelation. Eviction also ends duplicate-ID detection for that ID.
//! The stream owner audits terminal closes before clearing runtime state.
//! Audits and responses share the transport byte cap; an audit failure cancels
//! delivery. Admission success reserves a decision, not its eventual delivery.
//! Grants live only in this runtime as exact tool/canonical-input matches.
//! Harness suggestions are never interpreted or forwarded. No settings are written.
//! [`MAX_GRANTS`] bounds distinct exact grants, including pending grants.
pub mod auth;

/// Maximum live permission requests, including reserved answers awaiting their
/// durable close and completed stdin write. The harness normally blocks per
/// request; excess requests are denied individually without consuming a slot.
pub const MAX_OPEN_PERMISSION_REQUESTS: usize = 16;
/// Number of closed IDs retained in closure order, independently of live slots
/// and session grants. Beyond this FIFO window, late decisions are unknown.
pub const CLOSED_PERMISSION_REQUEST_WINDOW: usize = 1024;
pub(super) const MAX_GRANTS: usize = 16;

use super::claude::ToolRequest;
use crate::api::error_code::ErrorCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Server-authenticated caller context; deliberately not deserializable.
pub enum Principal<'a> {
    /// Created by token authentication, not by decoding command arguments.
    Token(auth::VerifiedToken<'a>),
    /// An operator UID without a client token does not authorize a decision.
    Operator,
    /// A pane or message attributed to an agent.
    Agent(&'a str),
    /// A structured harness's lease identity.
    Lease(&'a str),
}

impl Principal<'_> {
    pub(super) fn decision_subject(&self) -> Result<&str, ErrorCode> {
        match self {
            Self::Token(token) => token.decision_subject(),
            _ => None,
        }
        .ok_or(ErrorCode::NotAuthorized)
    }
}

/// The operator's decision, independent of a harness-specific response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Approve only this control request.
    AllowOnce,
    /// Approve for this live runtime only (never persistent policy).
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
    /// Exact currently open harness request ID. Retained closed IDs return
    /// `permission_request_closed`; evicted or never-seen IDs return
    /// `permission_request_unknown`, as bounded by [`CLOSED_PERMISSION_REQUEST_WINDOW`].
    pub request_id: String,
    /// One of the three permission decisions.
    pub decision: Decision,
    /// Optional reason, carried verbatim in a deny response and audit.
    #[serde(default)]
    pub message: Option<String>,
}

/// Why a host fact closes a request without an operator decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The live runtime ended, including cancellation and process exit.
    Ended,
    /// The harness cancelled this particular request.
    Cancelled,
    /// A top-level result retired the turn's requests, as in S4.
    Result,
    /// The host interrupted the turn.
    Interrupted,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Open,
    Reserved,
    Auditing,
    Closing,
    Closed,
}
#[derive(Clone)]
struct Grant {
    subject: String,
    request_id: String,
}
pub(super) struct Open {
    request: ToolRequest,
    stage: Stage,
    automatic: bool,
    grant: Option<String>,
}
pub(super) struct Answer {
    pub id: String,
    pub response: Value,
    pub audit: Value,
    grant: Option<(String, Grant)>,
}
pub(super) struct Closing {
    pub id: String,
    pub audit: Value,
    prior: Stage,
}
pub(super) struct Book {
    agent: String,
    active: bool,
    open: BTreeMap<String, Open>,
    closed_ids: BTreeSet<String>,
    closed_fifo: VecDeque<String>,
    grants: BTreeMap<String, Grant>,
}
pub(super) fn open_record(id: &str, request: &ToolRequest) -> Value {
    json!({"type":"flk_permission_request","state":"open","request_id":id,"request":request})
}

impl Book {
    pub fn new(agent: String) -> Self {
        Self {
            agent,
            active: true,
            open: BTreeMap::new(),
            closed_ids: BTreeSet::new(),
            closed_fifo: VecDeque::new(),
            grants: BTreeMap::new(),
        }
    }
    pub fn blocked(&self) -> bool {
        !self.active || !self.open.is_empty()
    }
    pub fn waiting(&self) -> Vec<String> {
        self.open
            .iter()
            .filter(|(_, r)| r.stage != Stage::Closed && !r.automatic)
            .map(|(id, _)| id.clone())
            .collect()
    }
    // Called only after the stream owner has persisted terminal closes. An I/O
    // failure is reported by that owner; shutdown cannot promise failed fsyncs.
    pub fn end(&mut self) {
        self.active = false;
        for id in self.open.keys().cloned().collect::<Vec<_>>() {
            self.remember_closed(&id);
        }
        self.open.clear();
        self.grants.clear();
    }
    pub fn observe(&mut self, id: &str, request: ToolRequest) -> Result<Value, ErrorCode> {
        if !self.active
            || id.is_empty()
            || id.len() > 256
            || self.open.contains_key(id)
            || self.closed_ids.contains(id)
        {
            return Err(ErrorCode::InvalidRequest);
        }
        if self.open.len() >= MAX_OPEN_PERMISSION_REQUESTS {
            return Err(ErrorCode::PermissionRequestLimit);
        }
        let audit = open_record(id, &request);
        self.open.insert(
            id.into(),
            Open {
                request,
                stage: Stage::Open,
                automatic: false,
                grant: None,
            },
        );
        Ok(audit)
    }
    pub fn automatic(&mut self, id: &str) -> Result<Option<Answer>, ErrorCode> {
        let grant = self
            .open
            .get(id)
            .and_then(|open| self.grants.get(&key(&open.request)))
            .cloned();
        grant
            .map(|grant| {
                let mut answer = self.answer(
                    id,
                    Decision::AllowOnce,
                    None,
                    &grant.subject,
                    "session_grant",
                )?;
                if let Some(open) = self.open.get_mut(id) {
                    open.automatic = true;
                }
                answer.audit["grant_request_id"] = json!(grant.request_id);
                Ok(answer)
            })
            .transpose()
    }

    fn missing(&self, id: &str) -> ErrorCode {
        if self.open.contains_key(id) || self.closed_ids.contains(id) {
            ErrorCode::PermissionRequestClosed
        } else {
            ErrorCode::PermissionRequestUnknown
        }
    }
    pub fn prepare(
        &mut self,
        principal: &Principal<'_>,
        args: &AgentDecide,
    ) -> Result<Answer, ErrorCode> {
        let subject = principal.decision_subject()?;
        if args.agent != self.agent {
            return Err(ErrorCode::AgentNotFound);
        }
        self.answer(
            &args.request_id,
            args.decision,
            args.message.as_deref(),
            subject,
            "token",
        )
    }
    fn answer(
        &mut self,
        id: &str,
        decision: Decision,
        message: Option<&str>,
        subject: &str,
        source: &str,
    ) -> Result<Answer, ErrorCode> {
        let missing = self.missing(id);
        let open = self
            .open
            .get(id)
            .filter(|r| r.stage == Stage::Open && self.active)
            .ok_or(missing)?;
        let mut grant = None;
        let mut ignored_suggestions = None;
        let response = if decision == Decision::Deny {
            json!({"behavior":"deny","message":message.unwrap_or("Denied by the operator")})
        } else {
            let response = json!({"behavior":"allow","updatedInput":open.request.input});
            if decision == Decision::AllowAlways {
                if open
                    .request
                    .permission_suggestions
                    .as_ref()
                    .and_then(Option::as_ref)
                    .is_some_and(|rules| !rules.is_empty())
                {
                    // The durable open record contains the complete bundle.
                    // Never interpret the harness rule grammar or forward rules.
                    ignored_suggestions = Some(json!({"request_id":id,"reason":"not_forwarded"}));
                }
                let k = key(&open.request);
                let reserved: BTreeSet<_> = self
                    .open
                    .values()
                    .filter_map(|r| r.grant.as_ref())
                    .filter(|key| !self.grants.contains_key(*key))
                    .collect();
                if self.grants.len() + reserved.len() >= MAX_GRANTS
                    && !self.grants.contains_key(&k)
                    && !reserved.contains(&k)
                {
                    return Err(ErrorCode::PermissionGrantLimit);
                }
                grant = Some((
                    k,
                    Grant {
                        subject: subject.into(),
                        request_id: id.into(),
                    },
                ));
            }
            response
        };
        let open = self.open.get_mut(id).ok_or(missing)?;
        open.grant = grant.as_ref().map(|(key, _)| key.clone());
        open.stage = Stage::Reserved;
        let mut audit = json!({"type":"flk_permission_request","state":"closed","request_id":id,"decision":decision,"message":message,"principal":subject,"source":source});
        if let Some(ignored) = ignored_suggestions {
            audit["ignored_suggestions"] = ignored;
        }
        Ok(Answer {
            id: id.into(),
            response: json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":response}}),
            audit,
            grant,
        })
    }
    pub fn oversized(
        &mut self,
        id: &str,
        request: ToolRequest,
    ) -> Result<(Value, Answer), ErrorCode> {
        let open = self.observe(id, request)?;
        let code = ErrorCode::PermissionRequestTooLarge.as_str();
        let mut answer = self.answer(id, Decision::Deny, Some(code), "flk", "limit")?;
        answer.audit["reason"] = json!(code);
        answer.audit["error_code"] = json!(code);
        Ok((open, answer))
    }
    pub fn aborted(&mut self, id: &str) {
        if let Some(open) = self.open.get_mut(id).filter(|r| r.stage == Stage::Reserved) {
            open.stage = Stage::Open;
            open.automatic = false;
            open.grant = None;
        }
    }
    // Once claimed by the stream writer, a decision cannot be replaced by a
    // concurrently queued close during its fsync. Before that claim, cancel wins.
    pub fn begin_answer(&mut self, id: &str) -> bool {
        if let Some(open) = self
            .open
            .get_mut(id)
            .filter(|r| r.stage == Stage::Reserved && self.active)
        {
            open.stage = Stage::Auditing;
            true
        } else {
            false
        }
    }
    pub fn audited(&mut self, answer: &Answer) {
        let Some(open) = self
            .open
            .get_mut(&answer.id)
            .filter(|r| r.stage == Stage::Auditing && self.active)
        else {
            return;
        };
        open.stage = Stage::Closed;
        open.grant = None;
        if let Some((key, grant)) = &answer.grant {
            self.grants.insert(key.clone(), grant.clone());
        }
        self.remember_closed(&answer.id);
    }
    pub fn written(&mut self, answer: Answer) {
        self.open.remove(&answer.id);
    }
    pub fn close(
        &mut self,
        id: Option<&str>,
        reason: CloseReason,
    ) -> Result<Vec<Closing>, ErrorCode> {
        if let Some(id) = id {
            let missing = self.missing(id);
            if !self
                .open
                .get(id)
                .is_some_and(|r| matches!(r.stage, Stage::Open | Stage::Reserved))
            {
                return Err(missing);
            }
        }
        Ok(self.closing(id, reason, false))
    }
    pub fn terminal(&mut self) -> Vec<Closing> {
        self.closing(None, CloseReason::Ended, true)
    }
    fn closing(&mut self, id: Option<&str>, reason: CloseReason, terminal: bool) -> Vec<Closing> {
        self.open.iter_mut().filter(|(key,r)| id.is_none_or(|id|id==key.as_str()) && r.stage!=Stage::Closed
            && (terminal || matches!(r.stage,Stage::Open|Stage::Reserved)))
            .map(|(id,r)| {
                let prior=r.stage;r.stage=Stage::Closing;
                Closing {id:id.clone(),prior,audit:json!({"type":"flk_permission_request","state":"closed","request_id":id,"reason":reason})}
            }).collect()
    }
    pub fn close_aborted(&mut self, closings: &[Closing]) {
        for close in closings {
            if let Some(open) = self.open.get_mut(&close.id) {
                open.stage = close.prior;
            }
        }
    }
    pub fn closed(&mut self, id: &str) {
        self.open.remove(id);
        self.remember_closed(id);
    }
    fn remember_closed(&mut self, id: &str) {
        if self.closed_ids.insert(id.into()) {
            self.closed_fifo.push_back(id.into());
            if self.closed_fifo.len() > CLOSED_PERMISSION_REQUEST_WINDOW {
                if let Some(evicted) = self.closed_fifo.pop_front() {
                    self.closed_ids.remove(&evicted);
                }
            }
        }
    }
    // Called only after observe rejected a valid, uncorrelated ID at capacity.
    // No live slot or grant is reserved; the stream owner durably closes the ID.
    pub fn capacity_refusal(&self, id: &str) -> Answer {
        let code = ErrorCode::PermissionRequestLimit.as_str();
        crate::logging::structured_permission_request_refused(&self.agent, id, code);
        Answer {
            id: id.into(),
            response: json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":{"behavior":"deny","message":code}}}),
            audit: json!({"type":"flk_permission_request","state":"closed","request_id":id,"decision":"deny","message":code,"principal":"flk","source":"limit","reason":code,"error_code":code}),
            grant: None,
        }
    }
}

fn key(request: &ToolRequest) -> String {
    let mut input = Value::Object(request.input.clone());
    input.sort_all_objects();
    json!([request.tool_name, input]).to_string()
}

#[cfg(test)]
mod tests;
