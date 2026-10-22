//! Session-local permission decisions (ADR-0036 §10b/§11, #1429).
//! Caller identity is trusted dispatcher context, never command JSON. Only an
//! authenticated operator token may decide; socket UID and agent ancestry alone
//! grant nothing. A new transport owns a new book, including on resume.
//! There is no lifetime request limit (ADR-0036 S6). Keep at most
//! [`MAX_OPEN_PERMISSION_REQUESTS`] live/in-flight requests in full. Excess
//! requests receive an audited `permission_request_limit` deny, not a session stop.
//! Capacity refusals write a closed audit without a matching open record.
//! Retain the last [`CLOSED_PERMISSION_REQUEST_WINDOW`] durably closed handles in
//! FIFO order. Every decision must match the flk-issued `(request_id, request_seq)`
//! pair. Retained closed pairs return `permission_request_closed`; evicted or
//! never-seen pairs return `permission_request_unknown` (both retry never).
//! Invalid IDs, or duplicates still live or retained, stop the transport rather
//! than risk miscorrelation. Reusing an evicted harness ID is safe because its
//! new sequence cannot match an old decision. Sequence exhaustion fails closed.
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
/// Number of closed handles retained in closure order, independently of live slots
/// and session grants. Beyond this FIFO window, late decisions are unknown.
pub const CLOSED_PERMISSION_REQUEST_WINDOW: usize = 1024;
pub(super) const MAX_GRANTS: usize = 16;

use super::claude::ToolRequest;
use crate::api::error_code::ErrorCode;
use crate::structured_wire::{
    IgnoredPermissionReason, IgnoredPermissionSuggestions, PermissionAnswer, PermissionClose,
    PermissionRecord, PermissionState,
};
pub use crate::structured_wire::{
    PermissionCloseReason as CloseReason, PermissionDecision as Decision,
};
use serde::Deserialize;
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

/// Server arguments for agent.decide; identity is never an argument.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDecide {
    /// Agent whose live runtime receives this decision.
    pub agent: String,
    /// Exact harness request ID, matched together with `request_seq`. Retained
    /// closed pairs return `permission_request_closed`; evicted or never-seen pairs return
    /// `permission_request_unknown`, as bounded by [`CLOSED_PERMISSION_REQUEST_WINDOW`].
    pub request_id: String,
    /// Required flk-issued sequence from the open record, never a harness field.
    /// Strictly increasing within this session, including across runtime resumes.
    pub request_seq: u64,
    /// One of the three permission decisions.
    pub decision: Decision,
    /// Optional reason, carried verbatim in a deny response and audit.
    #[serde(default)]
    pub message: Option<String>,
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
    request_seq: u64,
    subject: String,
    request_id: String,
}
pub(super) struct Open {
    request_seq: u64,
    request: ToolRequest,
    stage: Stage,
    automatic: bool,
    grant: Option<String>,
}
pub(super) struct Answer {
    pub id: String,
    pub request_seq: u64,
    pub response: Value,
    pub audit: Box<PermissionRecord>,
    grant: Option<(String, Grant)>,
}
pub(super) struct Closing {
    pub id: String,
    pub request_seq: u64,
    pub audit: PermissionRecord,
    prior: Stage,
}
pub(super) struct Book {
    agent: String,
    active: bool,
    open: BTreeMap<String, Open>,
    closed_ids: BTreeMap<u64, String>,
    closed_fifo: VecDeque<u64>,
    last_request_seq: u64,
    grants: BTreeMap<String, Grant>,
}
pub(super) fn open_record(id: &str, request_seq: u64, request: &ToolRequest) -> PermissionRecord {
    PermissionRecord::new(
        id,
        request_seq,
        PermissionState::Open {
            request: request.clone(),
        },
    )
}

impl Book {
    pub fn new(agent: String) -> Self {
        Self {
            agent,
            active: true,
            open: BTreeMap::new(),
            closed_ids: BTreeMap::new(),
            last_request_seq: 0,
            closed_fifo: VecDeque::new(),
            grants: BTreeMap::new(),
        }
    }
    // The durable stream cursor bounds every issued handle. Seed before stdout
    // admission on resume so an old runtime's decision cannot match a new one.
    pub fn resume_after(&mut self, stream_seq: u64) {
        self.last_request_seq = self.last_request_seq.max(stream_seq);
    }
    fn next_request_seq(&mut self) -> Result<u64, ErrorCode> {
        self.last_request_seq = self
            .last_request_seq
            .checked_add(1)
            .ok_or(ErrorCode::InvalidRequest)?;
        Ok(self.last_request_seq)
    }
    pub fn blocked(&self) -> bool {
        !self.active || !self.open.is_empty()
    }
    pub fn waiting(&self) -> Vec<(String, u64)> {
        self.open
            .iter()
            .filter(|(_, r)| r.stage != Stage::Closed && !r.automatic)
            .map(|(id, r)| (id.clone(), r.request_seq))
            .collect()
    }
    // Called only after the stream owner has persisted terminal closes. An I/O
    // failure is reported by that owner; shutdown cannot promise failed fsyncs.
    pub fn end(&mut self) {
        self.active = false;
        for (id, request_seq) in self
            .open
            .iter()
            .map(|(id, r)| (id.clone(), r.request_seq))
            .collect::<Vec<_>>()
        {
            self.remember_closed(&id, request_seq);
        }
        self.open.clear();
        self.grants.clear();
    }
    pub fn observe(
        &mut self,
        id: &str,
        request: ToolRequest,
    ) -> Result<PermissionRecord, ErrorCode> {
        if !self.active
            || id.is_empty()
            || id.len() > 256
            || self.open.contains_key(id)
            || self.closed_ids.values().any(|closed| closed == id)
        {
            return Err(ErrorCode::InvalidRequest);
        }
        if self.open.len() >= MAX_OPEN_PERMISSION_REQUESTS {
            return Err(ErrorCode::PermissionRequestLimit);
        }
        let request_seq = self.next_request_seq()?;
        let audit = open_record(id, request_seq, &request);
        self.open.insert(
            id.into(),
            Open {
                request_seq,
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
                    self.open
                        .get(id)
                        .ok_or(ErrorCode::PermissionRequestUnknown)?
                        .request_seq,
                    Decision::AllowOnce,
                    None,
                    &grant.subject,
                    "session_grant",
                )?;
                if let Some(open) = self.open.get_mut(id) {
                    open.automatic = true;
                }
                if let Some(audit) = answer.audit.answer_mut() {
                    audit.grant_request_id = Some(grant.request_id);
                    audit.grant_request_seq = Some(grant.request_seq);
                }
                Ok(answer)
            })
            .transpose()
    }

    fn missing(&self, id: &str, request_seq: u64) -> ErrorCode {
        if self
            .open
            .get(id)
            .is_some_and(|r| r.request_seq == request_seq)
            || self
                .closed_ids
                .get(&request_seq)
                .is_some_and(|closed| closed == id)
        {
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
            args.request_seq,
            args.decision,
            args.message.as_deref(),
            subject,
            "token",
        )
    }
    fn answer(
        &mut self,
        id: &str,
        request_seq: u64,
        decision: Decision,
        message: Option<&str>,
        subject: &str,
        source: &str,
    ) -> Result<Answer, ErrorCode> {
        let missing = self.missing(id, request_seq);
        let open = self
            .open
            .get(id)
            .filter(|r| r.request_seq == request_seq && r.stage == Stage::Open && self.active)
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
                    ignored_suggestions = Some(IgnoredPermissionSuggestions {
                        request_id: id.into(),
                        reason: IgnoredPermissionReason::NotForwarded,
                    });
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
                        request_seq,
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
        let mut fields = PermissionAnswer::new(
            decision,
            message.map(str::to_owned),
            subject.into(),
            source.into(),
        );
        fields.ignored_suggestions = ignored_suggestions;
        let audit = PermissionRecord::new(
            id,
            request_seq,
            PermissionState::Closed {
                outcome: PermissionClose::Answer(fields),
            },
        );
        Ok(Answer {
            id: id.into(),
            request_seq,
            response: json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":response}}),
            audit: Box::new(audit),
            grant,
        })
    }
    pub fn oversized(
        &mut self,
        id: &str,
        request: ToolRequest,
    ) -> Result<(PermissionRecord, Answer), ErrorCode> {
        let open = self.observe(id, request)?;
        let code = ErrorCode::PermissionRequestTooLarge.as_str();
        let request_seq = self
            .open
            .get(id)
            .ok_or(ErrorCode::PermissionRequestUnknown)?
            .request_seq;
        let mut answer =
            self.answer(id, request_seq, Decision::Deny, Some(code), "flk", "limit")?;
        if let Some(audit) = answer.audit.answer_mut() {
            audit.reason = Some(CloseReason::PermissionRequestTooLarge);
            audit.error_code = audit.reason;
        }
        Ok((open, answer))
    }
    pub fn aborted(&mut self, id: &str, request_seq: u64) {
        if let Some(open) = self
            .open
            .get_mut(id)
            .filter(|r| r.request_seq == request_seq && r.stage == Stage::Reserved)
        {
            open.stage = Stage::Open;
            open.automatic = false;
            open.grant = None;
        }
    }
    // Once claimed by the stream writer, a decision cannot be replaced by a
    // concurrently queued close during its fsync. Before that claim, cancel wins.
    pub fn begin_answer(&mut self, id: &str, request_seq: u64) -> bool {
        if let Some(open) = self
            .open
            .get_mut(id)
            .filter(|r| r.request_seq == request_seq && r.stage == Stage::Reserved && self.active)
        {
            open.stage = Stage::Auditing;
            true
        } else {
            false
        }
    }
    pub fn audited(&mut self, answer: &Answer) {
        let Some(open) = self.open.get_mut(&answer.id).filter(|r| {
            r.request_seq == answer.request_seq && r.stage == Stage::Auditing && self.active
        }) else {
            return;
        };
        open.stage = Stage::Closed;
        open.grant = None;
        if let Some((key, grant)) = &answer.grant {
            self.grants.insert(key.clone(), grant.clone());
        }
        self.remember_closed(&answer.id, answer.request_seq);
    }
    pub fn written(&mut self, answer: Answer) {
        if self
            .open
            .get(&answer.id)
            .is_some_and(|r| r.request_seq == answer.request_seq)
        {
            self.open.remove(&answer.id);
        }
    }
    pub fn close(
        &mut self,
        id: Option<&str>,
        reason: CloseReason,
    ) -> Result<Vec<Closing>, ErrorCode> {
        if let Some(id) = id {
            let missing = if self.open.contains_key(id)
                || self.closed_ids.values().any(|closed| closed == id)
            {
                ErrorCode::PermissionRequestClosed
            } else {
                ErrorCode::PermissionRequestUnknown
            };
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
        self.open
            .iter_mut()
            .filter(|(key, r)| {
                id.is_none_or(|id| id == key.as_str())
                    && r.stage != Stage::Closed
                    && (terminal || matches!(r.stage, Stage::Open | Stage::Reserved))
            })
            .map(|(id, r)| {
                let prior = r.stage;
                r.stage = Stage::Closing;
                Closing {
                    id: id.clone(),
                    request_seq: r.request_seq,
                    prior,
                    audit: PermissionRecord::new(
                        id,
                        r.request_seq,
                        PermissionState::Closed {
                            outcome: PermissionClose::Reason { reason },
                        },
                    ),
                }
            })
            .collect()
    }
    pub fn close_aborted(&mut self, closings: &[Closing]) {
        for close in closings {
            if let Some(open) = self
                .open
                .get_mut(&close.id)
                .filter(|r| r.request_seq == close.request_seq)
            {
                open.stage = close.prior;
            }
        }
    }
    pub fn closed(&mut self, id: &str, request_seq: u64) {
        if self
            .open
            .get(id)
            .is_some_and(|r| r.request_seq == request_seq)
        {
            self.open.remove(id);
        }
        self.remember_closed(id, request_seq);
    }
    fn remember_closed(&mut self, id: &str, request_seq: u64) {
        if self.closed_ids.insert(request_seq, id.into()).is_none() {
            self.closed_fifo.push_back(request_seq);
            if self.closed_fifo.len() > CLOSED_PERMISSION_REQUEST_WINDOW {
                if let Some(evicted) = self.closed_fifo.pop_front() {
                    self.closed_ids.remove(&evicted);
                }
            }
        }
    }
    // Called only after observe rejected a valid, uncorrelated ID at capacity.
    // No live slot or grant is reserved; the stream owner durably closes the ID.
    pub fn capacity_refusal(&mut self, id: &str) -> Result<Answer, ErrorCode> {
        let request_seq = self.next_request_seq()?;
        let code = ErrorCode::PermissionRequestLimit.as_str();
        crate::logging::structured_permission_request_refused(&self.agent, id, request_seq, code);
        let mut fields = PermissionAnswer::new(
            Decision::Deny,
            Some(code.into()),
            "flk".into(),
            "limit".into(),
        );
        fields.reason = Some(CloseReason::PermissionRequestLimit);
        fields.error_code = fields.reason;
        Ok(Answer {
            id: id.into(),
            request_seq,
            response: json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":{"behavior":"deny","message":code}}}),
            audit: Box::new(PermissionRecord::new(
                id,
                request_seq,
                PermissionState::Closed {
                    outcome: PermissionClose::Answer(fields),
                },
            )),
            grant: None,
        })
    }
}

fn key(request: &ToolRequest) -> String {
    let mut input = Value::Object(request.input.clone());
    input.sort_all_objects();
    json!([request.tool_name, input]).to_string()
}

#[cfg(test)]
mod tests;
