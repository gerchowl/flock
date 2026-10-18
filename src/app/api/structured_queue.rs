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

/// Structured turns deliberately exclude terminal-only raw writes and deadlines.
/// Authority is supplied separately by the authenticated dispatcher.
pub(crate) struct SendParams {
    /// Stable structured identity or unambiguous name.
    pub target: String,
    /// Model input, framed according to the authenticated origin.
    pub text: String,
    /// MVP requires an entire user turn.
    pub submit: bool,
    /// Canonical lifecycle UUID for idempotent retries.
    pub item_id: Option<String>,
}
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
    fn full(&self, operator: bool) -> bool {
        self.waiting.len() + self.parked.len() >= if operator { 128 } else { 112 }
    }
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
    /// Structured identity is independent of terminal placement. Exact ids and names
    /// shared with a terminal are ambiguous. Terminal-only targets fall back.
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

        let matches: Vec<_> = self
            .state
            .structured_agents
            .records()
            .into_iter()
            .filter(|record| record.agent_id == target || record.name.as_deref() == Some(target))
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
    /// tickets/replies and one in-flight request per item are accepted (`session_busy`).
    /// Non-operator requests stop at 112, reserving 16 slots for the operator.
    /// Exit before acknowledgement can report `input_write_unknown` after commit:
    /// inspect the journal, then retry the same lifecycle ID, never a fresh ID.
    pub(crate) fn structured_send(
        &mut self,
        params: crate::app::StructuredSendParams,
        principal: &Principal<'_>,
    ) -> Result<Ticket, ErrorCode> {
        if !params.submit {
            return Err(ErrorCode::StructuredSubmitRequired);
        }
        let origin = principal.queue_origin()?;
        let (agent, generation) = self
            .structured_send_target(&params.target)?
            .ok_or(ErrorCode::AgentNotFound)?;
        if matches!(principal, Principal::Agent(id) | Principal::Lease(id) if *id == agent) {
            return Err(ErrorCode::SelfMessageForbidden);
        }
        let operator = matches!(
            origin,
            crate::structured::queue::admission::Principal::Operator
        );
        let item_id = match params.item_id {
            Some(id) => id,
            None => crate::structured::queue::admission::item_uuid().map_err(|error| error.code)?,
        };
        if !crate::structured::queue::admission::canonical_uuid(&item_id) {
            return Err(ErrorCode::InvalidRequest);
        }
        let key = (agent.clone(), generation, item_id.clone());
        if self.structured_sends.full(operator) || self.structured_sends.waiting.contains_key(&key)
        {
            return Err(ErrorCode::SessionBusy);
        }
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
        if params.raw || params.write_timeout_ms.is_some() {
            return encode_error(
                id,
                ErrorCode::InvalidRequest.as_str(),
                "terminal-only options",
            );
        }
        let operator = self.caller_is_local_operator();
        let caller = self.spawn_caller();
        let principal = match &caller {
            super::SpawnCaller::Operator { .. } if operator => Principal::Operator,
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
        match self.structured_send(
            SendParams {
                target: params.target,
                text: params.text,
                submit: params.submit,
                item_id: params.item_id,
            },
            &principal,
        ) {
            Ok(ticket) => {
                self.structured_sends.staged = Some(Staged(id, ticket));
                String::new()
            }
            Err(code) => encode_error(id, code.as_str(), "structured admission refused"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn send_admission_operator_reserve_survives_agent_saturation() {
        let mut sends = Sends::default();
        for n in 0..112 {
            let (tx, _rx) = mpsc::channel();
            sends
                .waiting
                .insert(("recipient".into(), 1, n.to_string()), tx);
        }
        assert!(sends.full(false));
        assert!(!sends.full(true));
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state
            .structured_agents
            .register(crate::structured::registry::Record {
                runtime: crate::structured::registry::StructuredRuntime::Structured,
                agent_id: "recipient".into(),
                name: None,
                harness: "claude".into(),
                session_id: "reserve-fixture".into(),
                cwd: std::env::temp_dir(),
                model: None,
            })
            .unwrap();
        app.structured_sends = sends;
        let request = || crate::app::StructuredSendParams {
            target: "recipient".into(),
            text: "turn".into(),
            submit: true,
            item_id: Some("00000000-0000-4000-8000-000000001299".into()),
        };
        assert!(matches!(
            app.structured_send(request(), &Principal::Agent("sender.example")),
            Err(ErrorCode::SessionBusy)
        ));
        // The reserved operator request passes capacity and reaches worker lookup.
        assert!(matches!(
            app.structured_send(request(), &Principal::Operator),
            Err(ErrorCode::AgentNotFound)
        ));
        let mut sends = app.structured_sends;
        for n in 112..128 {
            let (tx, _rx) = mpsc::channel();
            sends
                .waiting
                .insert(("recipient".into(), 1, n.to_string()), tx);
        }
        assert!(sends.full(false));
        assert!(sends.full(true));
    }
}
