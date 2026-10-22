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

/// Why a live queue cannot offer another input. None of these causes expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingCause {
    /// An accepted batch still needs its real result; runtime exit/restart also
    /// ends the wait, holding unresolved items instead of resending them.
    AwaitingResult,
    /// Loss obscured the boundary. The transport guard remains closed until a
    /// real result, or runtime exit/restart; no input is written across it.
    TurnEndUnknown,
    /// The harness cancelled the batch but has not reported its real result.
    /// Items stay cancelled; only a real result or runtime exit/restart ends it.
    CancelledAwaitingResult,
    /// S4 does not report live, attention-free idle (or fresh bootstrap).
    StateNotReady,
    /// S6 has an open permission/question request; its closure permits a retry.
    WaitingOnHuman,
}
/// Status of an accepted batch or a candidate refused without any stdin write.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Pending {
    /// Accepted/refused batch UUID, or none when no candidate was formed.
    pub batch_id: Option<String>,
    /// Reservation/first-refusal time, or observation time for an S4/S6 gate.
    pub since_ms: u64,
    /// Explicit blocking cause; age never triggers queue recovery.
    pub cause: PendingCause,
}
/// An offer always names its outcome; pending is never disguised as no work.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", content = "detail", rename_all = "snake_case")]
pub(crate) enum Offer {
    /// Stdin accepted this batch; this does not mean the harness started it.
    Submitted {
        /// Single-item UUID or multi-item batch UUID.
        batch_id: String,
    },
    /// This attempt made no write; inspect the cause and await recovery.
    Pending(Pending),
    /// Admission found no eligible queued input.
    NoEligibleItems,
}

struct Reservation {
    id: String,
    members: Vec<String>,
    since_ms: u64,
    cancelled: bool,
}

/// Own one reservation until its top-level result is consumed.
pub(crate) struct Scheduler {
    admission: Admission,
    reserved: Option<Reservation>,
    refused: Option<Pending>,
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
            refused: None,
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
    /// Current accepted-turn blocker or unconfirmed refused candidate. Status and
    /// deliver report the same cause/time. No status observation writes a record;
    /// S4/S6 gates and no-eligible-input are reported by deliver when unreserved.
    pub fn status(&self, runtime: &Runtime) -> Option<Pending> {
        self.reserved
            .as_ref()
            .map(|reservation| Pending {
                batch_id: Some(reservation.id.clone()),
                since_ms: reservation.since_ms,
                cause: if runtime.turn_end_unknown() {
                    PendingCause::TurnEndUnknown
                } else if reservation.cancelled {
                    PendingCause::CancelledAwaitingResult
                } else {
                    PendingCause::AwaitingResult
                },
            })
            .or_else(|| self.refused.clone())
    }
    /// Offer once after S4/S6 permit. Runtime admission is the final idle guard.
    /// A fresh generation may bootstrap Unknown exactly once; later offers need
    /// S4 Idle. A refused offer consumes no input. Success is not delivery.
    pub fn deliver(
        &mut self,
        runtime: &Runtime,
        state: &AgentState,
        now_ms: u64,
    ) -> Result<Offer, Failure> {
        self.journal().healthy()?;
        if let Some(pending) = self.status(runtime) {
            return Ok(Offer::Pending(pending));
        }
        let open = runtime
            .waiting_on_human()
            .map_err(|code| Failure { code, source: None })?;
        let cause = if !open.is_empty() {
            Some(PendingCause::WaitingOnHuman)
        } else if state.lifecycle != Lifecycle::Live
            || state.attention != Attention::None
            || !(state.turn == Turn::Idle || (self.virgin && state.turn == Turn::Unknown))
        {
            Some(PendingCause::StateNotReady)
        } else {
            None
        };
        if let Some(cause) = cause {
            return Ok(Offer::Pending(Pending {
                batch_id: None,
                since_ms: now_ms,
                cause,
            }));
        }
        let Some(batch) = self.admission.batch(now_ms)? else {
            return Ok(Offer::NoEligibleItems);
        };
        let originals = batch.items.clone();
        let mut members = batch.items;
        for item in &mut members {
            item.metadata.insert("batch_id".into(), json!(batch.id));
            item.metadata.insert("awaiting_result".into(), json!(true));
        }
        self.admission.replace(members)?;
        self.reserved = Some(Reservation {
            id: batch.id.clone(),
            members: originals.iter().map(|i| i.item_id.clone()).collect(),
            since_ms: now_ms,
            cancelled: false,
        });
        if let Err(error) = runtime.try_submit(&batch.envelope) {
            // Refused input never owns result processing, even if undo fails.
            self.reserved = None;
            if error
                .get_ref()
                .is_some_and(|e| e.is::<crate::structured::transport::TurnEndUnknown>())
            {
                let pending = Pending {
                    batch_id: Some(batch.id),
                    since_ms: now_ms,
                    cause: PendingCause::TurnEndUnknown,
                };
                self.refused = Some(pending.clone());
                self.admission.replace(originals)?;
                return Ok(Offer::Pending(pending));
            }
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
            return Err(Failure {
                code,
                source: Some(error),
            });
        }
        self.refused = None;
        self.virgin = false;
        Ok(Offer::Submitted { batch_id: batch.id })
    }
    fn members(&self) -> Vec<Item> {
        self.admission
            .journal()
            .items()
            .iter()
            .filter(|i| {
                self.reserved
                    .as_ref()
                    .is_some_and(|reservation| reservation.members.contains(&i.item_id))
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
                    .is_some_and(|reservation| reservation.id == e.command_uuid) =>
            {
                match e.state.as_str() {
                    "started" => Some(State::Delivered),
                    "completed" => Some(State::Done),
                    // ADR §3 take-back returns to queued only for an operator
                    // take-back, which is not implemented here. Harness cancellation
                    // remains terminal and does not confirm the turn boundary.
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
            // Track the session signal independently of terminal item outcomes.
            if target == State::Cancelled {
                if let Some(reservation) = &mut self.reserved {
                    reservation.cancelled = true;
                }
            }
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
            self.refused = None;
            self.virgin = false;
        }
        Ok(())
    }
    /// No result means held, even if the stdin writer completed or a lifecycle
    /// completed arrived. Call after draining durable events on any process exit.
    pub fn exited(&mut self) -> Result<(), Failure> {
        self.hold(self.members(), ErrorCode::NoResult)?;
        self.reserved = None;
        self.refused = None;
        self.virgin = false;
        Ok(())
    }
}
