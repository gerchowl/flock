//! Worker-owned runtime scheduling and lifecycle (ADR-0036 §3/§4, #1879).
//!
//! One fresh runtime generation owns this scheduler and all stdin submissions.
//! Feed persisted events in order, publish cuts only after successful operations,
//! and drain final events before exited(). No disk work runs on the server loop.
use super::admission::{Admission, Failure, Gate, Principal};
use super::model::State;
use super::{Item, Journal};
use crate::api::agent_state::{AgentState, Attention, Lifecycle, Turn};
use crate::api::error_code::ErrorCode;
use crate::structured::{claude::SessionEvent, runtime::Runtime};
use serde_json::{json, Value};
use std::io;

/// Own one reservation until its top-level result is consumed.
pub(crate) struct Scheduler {
    admission: Admission,
    reserved: Option<(String, Vec<String>)>,
    virgin: bool,
}
impl Scheduler {
    /// Start a fresh runtime generation; unresolved old offers become held.
    /// Unknown result-flag encodings are unresolved, never treated as idle.
    pub fn new(admission: Admission) -> Result<Self, Failure> {
        let pending = admission
            .journal()
            .items()
            .iter()
            .filter(|i| {
                i.metadata
                    .get("awaiting_result")
                    .is_some_and(|v| v != &Value::Bool(false))
            })
            .cloned()
            .collect();
        let mut this = Self {
            admission,
            reserved: None,
            virgin: true,
        };
        this.hold(pending, ErrorCode::SessionUnconfirmed)?;
        Ok(this)
    }
    /// Durable producer admission through the same worker and gate.
    pub fn enqueue(&mut self, item: Item, principal: Principal, gate: Gate) -> Result<(), Failure> {
        self.admission.enqueue(item, principal, gate)
    }
    /// Committed state and owned replay cuts for serialized publication.
    pub fn journal(&self) -> &Journal {
        self.admission.journal()
    }
    /// Offer once after S4/S6 permit. Runtime admission is the final idle guard.
    /// A fresh generation may bootstrap Unknown exactly once; later offers need
    /// S4 Idle. A refused offer consumes no input. Success is not delivery.
    pub fn deliver(
        &mut self,
        runtime: &Runtime,
        state: &AgentState,
        now_ms: u64,
    ) -> Result<Option<String>, Failure> {
        self.journal().healthy()?;
        let open = runtime
            .waiting_on_human()
            .map_err(|code| Failure { code, source: None })?;
        if self.reserved.is_some()
            || !open.is_empty()
            || state.lifecycle != Lifecycle::Live
            || state.attention != Attention::None
            || !(state.turn == Turn::Idle || (self.virgin && state.turn == Turn::Unknown))
        {
            return Ok(None);
        }
        let Some(batch) = self.admission.batch(now_ms)? else {
            return Ok(None);
        };
        let originals = batch.items.clone();
        let mut members = batch.items;
        for item in &mut members {
            item.metadata.insert("batch_id".into(), json!(batch.id));
            item.metadata.insert("awaiting_result".into(), json!(true));
        }
        self.admission.replace(members)?;
        self.reserved = Some((
            batch.id.clone(),
            originals.iter().map(|i| i.item_id.clone()).collect(),
        ));
        if let Err(error) = runtime.try_submit(&batch.envelope) {
            let code = if error.kind() == io::ErrorKind::WouldBlock {
                ErrorCode::SessionBusy
            } else {
                ErrorCode::InputWriteFailed
            };
            if code == ErrorCode::SessionBusy {
                self.admission.replace(originals)?;
            } else {
                self.hold(originals, code)?;
            }
            self.reserved = None;
            return Err(Failure {
                code,
                source: Some(error),
            });
        }
        self.virgin = false;
        Ok(Some(batch.id))
    }
    fn members(&self) -> Vec<Item> {
        self.admission
            .journal()
            .items()
            .iter()
            .filter(|i| {
                self.reserved
                    .as_ref()
                    .is_some_and(|(_, ids)| ids.contains(&i.item_id))
            })
            .cloned()
            .collect()
    }
    fn hold(&mut self, mut items: Vec<Item>, reason: ErrorCode) -> Result<(), Failure> {
        for item in &mut items {
            if item.state != State::Cancelled {
                item.state = State::Held;
            }
            item.metadata.remove("awaiting_result");
            item.metadata
                .insert("refusal".into(), json!(reason.as_str()));
        }
        self.admission.replace(items)
    }
    /// Persist all matching lifecycle members together. Unknown UUIDs/states and
    /// nested results cannot release the reservation. completed/cancelled still
    /// wait for the top-level result before any subsequent offer.
    pub fn observe(&mut self, event: &SessionEvent) -> Result<(), Failure> {
        let target = match event {
            SessionEvent::CommandLifecycle(e)
                if self
                    .reserved
                    .as_ref()
                    .is_some_and(|(id, _)| id == &e.command_uuid) =>
            {
                match e.state.as_str() {
                    "started" => Some(State::Delivered),
                    "completed" => Some(State::Done),
                    "cancelled" => Some(State::Cancelled),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(target) = target {
            let mut items = self.members();
            for item in &mut items {
                if matches!(item.state, State::Queued | State::Delivered) {
                    item.state = target.clone();
                }
            }
            self.admission.replace(items)?;
        }
        let result = match event {
            SessionEvent::Success(e) => e
                .parent_tool_use_id
                .as_ref()
                .and_then(Option::as_ref)
                .is_none(),
            SessionEvent::ExecutionError(e) => e
                .parent_tool_use_id
                .as_ref()
                .and_then(Option::as_ref)
                .is_none(),
            _ => false,
        };
        if result {
            let mut items = self.members();
            for item in &mut items {
                if item.state != State::Cancelled {
                    item.state = State::Done;
                }
                item.metadata.remove("awaiting_result");
            }
            self.admission.replace(items)?;
            self.reserved = None;
            self.virgin = false;
        }
        Ok(())
    }
    /// No result means held, even if the stdin writer completed or a lifecycle
    /// completed arrived. Call after draining durable events on any process exit.
    pub fn exited(&mut self) -> Result<(), Failure> {
        self.hold(self.members(), ErrorCode::NoResult)?;
        self.reserved = None;
        self.virgin = false;
        Ok(())
    }
}
