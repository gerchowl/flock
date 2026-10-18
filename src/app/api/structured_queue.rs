//! One nonblocking structured-send admission service (ADR-0036 §3/S5).
//! Composer and authenticated S7-min dispatchers call `App::structured_send`.
//! Socket/CLI dispatch adapts its verified ancestry, never a producer string.
//! A ticket succeeds only after the single journal owner commits admission.
use super::responses::{encode_error, encode_success};
use crate::{
    api::{
        error_code::ErrorCode,
        schema::{AgentSendParams, ResponseResult},
    },
    app::App,
    structured::{
        permissions::Principal,
        queue::{
            admission::{Failure, Gate},
            model::{Delivery, Kind, Priority, Producer, State},
            Item,
        },
        registry::Registry,
        workers::{queue::Request, Workers},
    },
};
use std::{
    collections::HashMap,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
};

type Key = (String, u64, String);
type ResultId = Result<crate::api::schema::AgentSendResult, ErrorCode>;
/// Pending durable admission. Dropping a ticket never cancels a committed item.
/// No delivery deadline or automatic retry exists. Reuse `item_id` after inspecting
/// an uncertain result; identical committed retries are idempotent.
pub(crate) struct Ticket {
    /// Canonical lifecycle identity, supplied on retries or freshly allocated.
    pub item_id: String,
    /// Completion from the journal owner, not bounded-channel acceptance.
    pub committed: Receiver<ResultId>,
}
pub(crate) struct Staged(String, Ticket);
/// Bounded response routing attached to the existing sole completion consumer.
#[derive(Default)]
pub(crate) struct Sends {
    waiting: HashMap<Key, Sender<ResultId>>,
    parked: Vec<(String, Receiver<ResultId>, Sender<String>)>,
    pub staged: Option<Staged>,
}
impl Sends {
    pub(crate) fn complete(
        &mut self,
        agent: String,
        generation: u64,
        id: String,
        result: Result<(), Failure>,
    ) {
        if let Some(waiter) = self.waiting.remove(&(agent, generation, id.clone())) {
            let _ = waiter.send(
                result
                    .map(|()| crate::api::schema::AgentSendResult { item_id: id })
                    .map_err(|error| error.code),
            );
        }
    }
    pub(crate) fn park(&mut self, Staged(id, ticket): Staged, reply: Sender<String>) {
        tracing::debug!(
            item_id = ticket.item_id.as_str(),
            "waiting for durable queue admission"
        );
        self.parked.push((id, ticket.committed, reply));
    }
    /// Fences unresolved acknowledgements when their original worker disappears.
    /// Such an admission may have committed: report unknown, never auto-resend.
    pub(crate) fn poll(&mut self, workers: &Workers, registry: &Registry) {
        self.waiting.retain(|(agent, generation, _), waiter| {
            if registry.get(agent).is_none_or(|(_, _, g)| g != *generation)
                || workers.report(agent, *generation).is_some()
            {
                let _ = waiter.send(Err(ErrorCode::InputWriteUnknown));
                false
            } else {
                true
            }
        });
        self.parked.retain(|(id, receipt, reply)| {
            let result = match receipt.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => Err(ErrorCode::InputWriteUnknown),
            };
            let response = match result {
                Ok(result) => encode_success(
                    id.clone(),
                    ResponseResult::AgentQueued {
                        item_id: result.item_id,
                    },
                ),
                Err(code) => encode_error(
                    id.clone(),
                    code.as_str(),
                    "structured admission refused or uncertain",
                ),
            };
            let _ = reply.send(response);
            false
        });
    }
}
impl App {
    /// Structured identity is independent of terminal placement. Exact ids win;
    /// names shared with a terminal are ambiguous. Terminal-only targets fall back.
    pub(crate) fn structured_send_target(
        &self,
        target: &str,
    ) -> Result<Option<(String, u64)>, ErrorCode> {
        let own;
        let target = if target == "self" {
            own = match self.lease_caller() {
                crate::structured::lease::Caller::Agent { agent, .. } => agent,
                _ => return Ok(None),
            };
            own.as_str()
        } else {
            target
        };
        if let Some((record, _, generation)) = self.state.structured_agents.get(target) {
            return Ok(Some((record.agent_id.clone(), generation)));
        }
        let matches: Vec<_> = self
            .state
            .structured_agents
            .records()
            .into_iter()
            .filter(|record| record.name.as_deref() == Some(target))
            .collect();
        if matches.is_empty() {
            return Ok(None);
        }
        if matches.len() != 1
            || !matches!(
                self.resolve_terminal_target(target),
                Err(crate::app::terminal_targets::TerminalTargetError::NotFound { .. })
            )
        {
            return Err(ErrorCode::AgentTargetAmbiguous);
        }
        Ok(self
            .state
            .structured_agents
            .get(&matches[0].agent_id)
            .map(|(record, _, generation)| (record.agent_id.clone(), generation)))
    }
    /// Shared composer/S7-min/socket queue API. `principal` must come from the
    /// authenticated dispatcher, never from JSON. No terminal or stdin is touched.
    /// `submit:false` is unsupported in the MVP and refused before queue work.
    /// Success means durable admission, not delivery. At most 128 outstanding
    /// tickets and one in-flight request per item are accepted (`session_busy`).
    pub(crate) fn structured_send(
        &mut self,
        params: AgentSendParams,
        principal: &Principal<'_>,
    ) -> Result<Ticket, ErrorCode> {
        if !params.submit {
            return Err(ErrorCode::StructuredSubmitRequired);
        }
        if params.raw || params.write_timeout_ms.is_some() {
            return Err(ErrorCode::InvalidRequest);
        }
        let origin = principal.queue_origin()?;
        let (agent, generation) = self
            .structured_send_target(&params.target)?
            .ok_or(ErrorCode::AgentNotFound)?;
        let item_id = match params.item_id {
            Some(id) => id,
            None => crate::structured::queue::admission::item_uuid().map_err(|error| error.code)?,
        };
        if !crate::structured::queue::admission::canonical_uuid(&item_id) {
            return Err(ErrorCode::InvalidRequest);
        }
        let key = (agent.clone(), generation, item_id.clone());
        if self.structured_sends.waiting.len() + self.structured_sends.parked.len() >= 128
            || self.structured_sends.waiting.contains_key(&key)
        {
            return Err(ErrorCode::SessionBusy);
        }
        let operator = matches!(
            origin,
            crate::structured::queue::admission::Principal::Operator
        );
        let item = Item {
            item_id: item_id.clone(),
            producer: Producer::Human,
            kind: if operator && params.text.starts_with('/') {
                Kind::Command
            } else if operator {
                Kind::Prompt
            } else {
                Kind::Message
            },
            state: State::Queued,
            delivery: Delivery::AtIdle,
            priority: Priority::Normal,
            body: params.text,
            attachments: vec![],
            metadata: Default::default(),
        };
        self.structured_workers.enqueue(
            &self.state.structured_agents,
            &agent,
            generation,
            Request {
                item,
                principal: origin,
                gate: Gate::Send,
            },
        )?;
        let (waiter, committed) = mpsc::channel();
        self.structured_sends.waiting.insert(key, waiter);
        Ok(Ticket { item_id, committed })
    }
    pub(super) fn handle_structured_send(&mut self, id: String, params: AgentSendParams) -> String {
        if !params.submit {
            return encode_error(
                id,
                ErrorCode::StructuredSubmitRequired.as_str(),
                "structured sends require submit:true",
            );
        }
        let caller = self.spawn_caller();
        let principal = match &caller {
            super::SpawnCaller::Operator { .. } => Principal::Operator,
            super::SpawnCaller::Agent { agent_id, .. }
            | super::SpawnCaller::Structured {
                agent_id: Some(agent_id),
                ..
            } => Principal::Agent(agent_id),
            _ => {
                return encode_error(
                    id,
                    ErrorCode::NotAuthorized.as_str(),
                    "caller identity is unconfirmed",
                )
            }
        };
        match self.structured_send(params, &principal) {
            Ok(ticket) => {
                self.structured_sends.staged = Some(Staged(id, ticket));
                String::new()
            }
            Err(code) => encode_error(id, code.as_str(), "structured admission refused"),
        }
    }
}
