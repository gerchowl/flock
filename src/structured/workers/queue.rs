//! Single journal owner on the session event pump (ADR-0036 §3/§4, #1955).
//! Each bounded completion must be consumed. Saturation backpressures admissions
//! and event delivery. Shutdown cancellation explicitly requires durable replay.
use crate::api::{agent_state::AgentState, error_code::ErrorCode};
use crate::structured::{
    queue::{
        admission::{Admission, Failure, Gate, Principal, MAX_BATCH},
        scheduler::{Offer, Pending, Scheduler},
        Cut, Item, Journal, RewritePolicy,
    },
    transport::{InputHandle, Received},
};
pub(crate) mod publication;
use std::{cell::RefCell, io, num::NonZeroU64, path::Path, sync::mpsc};

/// S4 projection after reducing every session record through this sequence.
#[derive(Clone, PartialEq, Eq)]
pub(in crate::structured) struct Snapshot {
    /// Authoritative axes for this exact worker generation.
    pub state: AgentState,
    /// Last session record included, even if it did not change the axes.
    pub through: u64,
}
/// Admission fields are supplied by the authenticated dispatcher, never decoded
/// as authority from a producer string. Buffer acceptance is not durable success.
pub(crate) struct Request {
    /// Proposed lifecycle identity and content.
    pub item: Item,
    /// Proven producer authority.
    pub principal: Principal,
    /// One admission policy decision.
    pub gate: Gate,
}
/// One immutable completion/cut, ready for the #1956 publication adapter.
#[derive(Debug)]
pub(crate) struct Update {
    /// Admission acknowledgement, correlated by lifecycle item identity.
    pub admitted: Option<(String, Result<(), Failure>)>,
    /// Actual delivery or typed refusal, never an unlabelled pending state.
    pub offer: Result<Offer, Failure>,
    /// Reservation/read-only status, including after a failed offer.
    pub pending: Option<Pending>,
    /// Committed cut only; uncertain durability instead requires reopening.
    pub cut: Option<Cut>,
}
/// Bounded, nonblocking server-side endpoint. It never owns a file or writer.
pub(in crate::structured) struct Client {
    pub source: publication::Source,
    requests: mpsc::SyncSender<Request>,
    states: mpsc::SyncSender<Snapshot>,
    updates: mpsc::Receiver<Update>,
    peeked: RefCell<Option<Box<Update>>>,
}
impl Client {
    /// Full returns session_busy; disconnection returns agent_not_found. Success
    /// accepts only the buffer slot; the completion acknowledges durability.
    pub fn enqueue(&self, request: Request) -> Result<(), ErrorCode> {
        self.requests.try_send(request).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => ErrorCode::SessionBusy,
            mpsc::TrySendError::Disconnected(_) => ErrorCode::AgentNotFound,
        })
    }
    pub fn state(&self, state: Snapshot) -> bool {
        self.states.try_send(state).is_ok()
    }
    /// Inspect a finished producer without consuming its next completion.
    pub fn has_updates(&self) -> bool {
        let mut peeked = self.peeked.borrow_mut();
        if peeked.is_none() {
            *peeked = self.updates.try_recv().ok().map(Box::new);
        }
        peeked.is_some()
    }
    pub fn update(&self) -> Option<Update> {
        self.peeked
            .borrow_mut()
            .take()
            .map(|update| *update)
            .or_else(|| self.updates.try_recv().ok())
    }
}
/// Constructed on the launch worker, then moved once to the session event pump.
/// Normal completion releases the lease after durable final-event processing.
pub(in crate::structured) struct Service {
    publisher: publication::Publisher,
    scheduler: Scheduler,
    requests: mpsc::Receiver<Request>,
    states: mpsc::Receiver<Snapshot>,
    updates: mpsc::SyncSender<Update>,
    pending: Option<Update>,
    state: AgentState,
    covered: u64,
    required: u64,
    error: Option<(ErrorCode, Option<(io::ErrorKind, String)>)>,
    cursor: u64,
    initial: bool,
    blocked: Option<ErrorCode>,
    status: Option<Pending>,
    lost: bool,
}
impl Service {
    pub fn open(data: &Path, agent: &str, bytes: NonZeroU64) -> Result<(Self, Client), Failure> {
        let journal = Journal::open_with_policy(data, agent, RewritePolicy::new(bytes))?;
        let scheduler = Scheduler::new(Admission::new(journal, MAX_BATCH)?)?;
        let (publisher, source) = publication::Publisher::new(scheduler.journal().replay(0)?);
        let (tx, requests) = mpsc::sync_channel(1);
        let (state_tx, states) = mpsc::sync_channel(1);
        // Same four-completion bound as the session stream; no unbounded spool.
        let (updates, rx) = mpsc::sync_channel(4);
        Ok((
            Self {
                publisher,
                scheduler,
                requests,
                states,
                updates,
                pending: None,
                state: AgentState::unknown(),
                covered: 0,
                required: 0,
                error: None,
                cursor: 0,
                initial: true,
                blocked: None,
                status: None,
                lost: false,
            },
            Client {
                source,
                requests: tx,
                states: state_tx,
                updates: rx,
                peeked: RefCell::new(None),
            },
        ))
    }
    pub fn flush(&mut self, cancelled: bool, notify: &impl Fn()) -> bool {
        if let Some(update) = self.pending.take() {
            match self.updates.try_send(update) {
                Ok(()) => notify(),
                Err(mpsc::TrySendError::Full(update)) if !cancelled => {
                    self.pending = Some(update);
                    return false;
                }
                Err(_) => self.lost = true,
            }
        }
        true
    }
    /// Whether this operation failed at the stdin boundary.
    pub fn tick(&mut self, input: &InputHandle, stopped: bool, notify: &impl Fn()) -> bool {
        let blocked = self.blocked;
        self.publisher.capture(
            || match blocked {
                Some(code) => Err(failure(code)),
                None => self.scheduler.subscription(),
            },
            self.status.clone(),
        );
        let mut changed = self.initial;
        if let Ok(state) = self.states.try_recv() {
            if state.through >= self.required {
                self.covered = state.through;
                self.state = state.state;
            }
            changed = true;
            notify();
        }
        let admitted = self.requests.try_recv().ok().map(|request| {
            changed = true;
            let id = request.item.item_id.clone();
            let result = if stopped {
                Err(failure(ErrorCode::AgentNotFound))
            } else {
                self.policy().and_then(|_| {
                    self.scheduler
                        .enqueue(request.item, request.principal, request.gate)
                })
            };
            (id, result)
        });
        if changed {
            self.publish(input, stopped, admitted, None)
        } else {
            false
        }
    }
    /// Whether this operation failed at the stdin boundary.
    pub fn event(&mut self, input: &InputHandle, received: &Received, stopped: bool) -> bool {
        // The pump sees this result before the registry reduces it. Only a
        // snapshot covering this sequence may re-establish the stdin gate.
        if crate::structured::queue::scheduler::ends_turn(&received.event) {
            self.required = self.required.max(received.record.seq);
            self.state = AgentState::unknown();
        }
        let result = self
            .policy()
            .and_then(|_| self.scheduler.observe(&received.event));
        self.publish(input, stopped, None, result.err())
    }
    fn policy(&mut self) -> Result<(), Failure> {
        if let Some(code) = self.blocked {
            return Err(failure(code));
        }
        let result = self.scheduler.checkpoint_if_required().map(|_| ());
        if let Err(error) = &result {
            self.blocked = Some(error.code);
        }
        result
    }
    fn publish(
        &mut self,
        input: &InputHandle,
        stopped: bool,
        admitted: Option<(String, Result<(), Failure>)>,
        error: Option<Failure>,
    ) -> bool {
        let mut offer = if let Some(error) = error {
            Err(error)
        } else if stopped || self.scheduler.journal().items().is_empty() {
            self.policy().map(|_| Offer::NoEligibleItems)
        } else {
            self.policy().and_then(|_| {
                self.scheduler
                    .deliver_on(input, &self.state, crate::app::notifications::now_ms())
            })
        };
        if let Err(error) = &offer {
            if matches!(
                error.code,
                ErrorCode::QueueJournalCorrupt
                    | ErrorCode::QueueJournalNewer
                    | ErrorCode::InputWriteUnknown
            ) {
                self.blocked = Some(error.code);
            }
        }
        if matches!(offer, Ok(Offer::Submitted { .. })) {
            self.required = self.covered.saturating_add(1);
            self.state = AgentState::unknown();
        }
        let pending = self.scheduler.status_on(input);
        let cut = match self.scheduler.journal().replay(self.cursor) {
            Ok(cut) => Some(cut),
            Err(error) => {
                self.blocked = Some(ErrorCode::InputWriteUnknown);
                offer = Err(error.into());
                None
            }
        };
        let error = offer
            .as_ref()
            .err()
            .map(|e| (e.code, e.source.as_ref().map(|s| (s.kind(), s.to_string()))));
        let changed = self.initial
            || cut.as_ref().is_some_and(|c| c.through != self.cursor)
            || pending != self.status
            || admitted.is_some()
            || error != self.error;
        if changed {
            if let Some(cut) = &cut {
                self.cursor = cut.through;
            }
            self.initial = false;
            self.error = error.clone();
            self.status = pending.clone();
            let update = Update {
                admitted,
                offer,
                pending,
                cut,
            };
            self.publisher.publish(&update);
            self.pending = Some(update);
        }
        error.is_some_and(|e| e.0 == ErrorCode::InputWriteFailed)
    }
    pub fn finish(&mut self, input: &InputHandle, notify: &impl Fn()) -> bool {
        let blocked = self.blocked;
        self.publisher.capture(
            || match blocked {
                Some(code) => Err(failure(code)),
                None => self.scheduler.subscription(),
            },
            self.status.clone(),
        );
        let error = self.policy().and_then(|_| self.scheduler.exited()).err();
        self.publish(input, true, None, error);
        let blocked = self.blocked;
        self.publisher.finish(
            || match blocked {
                Some(code) => Err(failure(code)),
                None => self.scheduler.subscription(),
            },
            self.status.clone(),
        );
        self.flush(true, notify);
        self.lost
    }
}
fn failure(code: ErrorCode) -> Failure {
    Failure { code, source: None }
}
pub(crate) fn launch_error(failure: Failure) -> io::Error {
    failure
        .source
        .unwrap_or_else(|| io::Error::other(failure.code.as_str()))
}
