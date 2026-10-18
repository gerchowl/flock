//! Single journal owner on the session event pump (ADR-0036 §3/§4, #1955).
//! Each bounded completion must be consumed. Saturation backpressures admissions
//! and event delivery. Shutdown retains facts; abandoned cuts require replay.
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
pub(crate) mod recovery;
use crate::structured::queue::custody::Unknown;
use std::{
    cell::RefCell,
    io,
    num::NonZeroU64,
    path::Path,
    sync::{mpsc, Arc},
};
/// Fixed notification bound, separate from one peek and the terminal fact tail.
pub(crate) const COMPLETION_CAPACITY: usize = 4;
type CommittedCustody = Arc<parking_lot::Mutex<CustodyIndex>>;
struct CustodyIndex {
    items: Arc<Vec<Item>>,
    pending: std::collections::HashMap<crate::mesh::key::MessageKey, usize>,
    uncertain: Option<Unknown>,
}
impl CustodyIndex {
    fn finish(&mut self, key: crate::mesh::key::MessageKey, unknown: Option<Unknown>) {
        let remove = self.pending.get_mut(&key).is_some_and(|count| {
            *count -= 1;
            *count == 0
        });
        if remove {
            self.pending.remove(&key);
        }
        self.uncertain = self.uncertain.or(unknown);
    }
}

fn custody_key(item: &Item) -> Option<crate::mesh::key::MessageKey> {
    let key: crate::mesh::key::MessageKey =
        serde_json::from_value(item.metadata.get("message_key")?.clone()).ok()?;
    (item.kind == crate::structured::queue::model::Kind::Message
        && item.item_id == key.lifecycle_id())
    .then_some(key)
}
type FinalFacts = Arc<parking_lot::Mutex<Vec<crate::api::queue_event::QueueItemState>>>;

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
    /// Already committed transitions, including startup re-hold, before compaction.
    pub facts: Vec<crate::api::queue_event::QueueItemState>,
    /// Admission acknowledgement, correlated by lifecycle item identity.
    pub admitted: Option<(String, Result<(), Failure>)>,
    /// Actual delivery or typed refusal, never an unlabelled pending state.
    pub offer: Result<Offer, Failure>,
    /// Reservation/read-only status, including after a failed offer.
    pub pending: Option<Pending>,
    /// Committed cut only. Uncertain durability requires reopening; a terminal
    /// facts-only completion also has no cut and makes no candidate offer.
    pub cut: Option<Cut>,
}
/// Bounded, nonblocking server-side endpoint. It never owns a file or writer.
pub(in crate::structured) struct Client {
    pub source: publication::Source,
    requests: mpsc::SyncSender<Request>,
    states: mpsc::SyncSender<Snapshot>,
    updates: mpsc::Receiver<Update>,
    peeked: RefCell<Option<Box<Update>>>,
    final_facts: FinalFacts,
    committed_custody: CommittedCustody,
}
impl Client {
    pub fn custody_committed(&self, key: &crate::mesh::key::MessageKey) -> Result<bool, Unknown> {
        // Only clone the committed fold under the lock; lookup is linear in
        // retained items, with no file I/O or extra historical-key allocation.
        let (items, uncertain) = {
            let index = self.committed_custody.lock();
            (
                index.items.clone(),
                index.uncertain.or_else(|| {
                    index
                        .pending
                        .contains_key(key)
                        .then_some(Unknown::AdmissionPending)
                }),
            )
        };
        if items
            .iter()
            .any(|item| custody_key(item).as_ref() == Some(key))
        {
            Ok(true)
        } else if let Some(reason) = uncertain {
            Err(reason)
        } else {
            Ok(false)
        }
    }
    /// Full returns session_busy; disconnection returns agent_not_found. Success
    /// accepts only the buffer slot; the completion acknowledges durability.
    pub fn enqueue(&self, request: Request) -> Result<(), ErrorCode> {
        let key = custody_key(&request.item);
        let mut index = self.committed_custody.lock();
        if let Some(key) = &key {
            *index.pending.entry(key.clone()).or_default() += 1;
        }
        let result = self.requests.try_send(request).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => ErrorCode::SessionBusy,
            mpsc::TrySendError::Disconnected(_) => ErrorCode::AgentNotFound,
        });
        if result.is_err() {
            if let Some(key) = key {
                index.finish(key, None);
            }
        }
        result
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
        peeked.is_some() || !self.final_facts.lock().is_empty()
    }
    /// After producer completion, discard obsolete notification cuts while
    /// retaining committed facts in order until the app consumes them.
    pub fn retire_facts(&self) -> bool {
        let mut facts = self
            .peeked
            .borrow_mut()
            .take()
            .map_or_else(Vec::new, |update| update.facts);
        for _ in 0..COMPLETION_CAPACITY {
            let Ok(update) = self.updates.try_recv() else {
                break;
            };
            facts.extend(update.facts);
        }
        let mut pending = self.final_facts.lock();
        if !facts.is_empty() {
            facts.append(&mut pending);
            *pending = facts;
        }
        !pending.is_empty()
    }
    pub fn update(&self) -> Option<Update> {
        self.peeked
            .borrow_mut()
            .take()
            .map(|update| *update)
            .or_else(|| self.updates.try_recv().ok())
            .or_else(|| {
                let facts = std::mem::take(&mut *self.final_facts.lock());
                (!facts.is_empty()).then_some(Update {
                    facts,
                    admitted: None,
                    offer: Ok(Offer::NoEligibleItems),
                    pending: None,
                    cut: None,
                })
            })
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
    final_facts: FinalFacts,
    committed_custody: CommittedCustody,
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
        let (publisher, source) =
            publication::Publisher::new(agent, scheduler.journal().replay(0)?);
        let (tx, requests) = mpsc::sync_channel(1);
        let (state_tx, states) = mpsc::sync_channel(1);
        // Same four-completion bound as the session stream; no unbounded spool.
        let (updates, rx) = mpsc::sync_channel(COMPLETION_CAPACITY);
        let final_facts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let committed_custody = Arc::new(parking_lot::Mutex::new(CustodyIndex {
            items: scheduler.journal().item_snapshot(),
            pending: Default::default(),
            uncertain: scheduler.journal().commitment_unknown(),
        }));
        Ok((
            Self {
                publisher,
                scheduler,
                requests,
                states,
                updates,
                final_facts: final_facts.clone(),
                committed_custody: committed_custody.clone(),
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
                final_facts,
                committed_custody,
                peeked: RefCell::new(None),
            },
        ))
    }
    pub fn flush(&mut self, cancelled: bool, notify: &impl Fn()) -> bool {
        if let Some(mut update) = self.pending.take() {
            // Once fallback begins, newer facts follow its older transitions.
            let mut final_facts = self.final_facts.lock();
            if !final_facts.is_empty() {
                final_facts.append(&mut update.facts);
            }
            drop(final_facts);
            match self.updates.try_send(update) {
                Ok(()) => notify(),
                Err(mpsc::TrySendError::Full(update)) if !cancelled => {
                    self.pending = Some(update);
                    return false;
                }
                Err(
                    mpsc::TrySendError::Full(update) | mpsc::TrySendError::Disconnected(update),
                ) => {
                    // Retain committed facts even when shutdown abandons notification cuts.
                    self.final_facts.lock().extend(update.facts);
                    self.lost = true;
                    notify();
                }
            }
        }
        true
    }
    /// Service replay requests independently of completion backpressure.
    pub fn capture(&mut self, notify: &impl Fn()) {
        let blocked = &mut self.blocked;
        let scheduler = &mut self.scheduler;
        if self.publisher.capture(
            || {
                let result = match *blocked {
                    Some(code) => Err(failure(code)),
                    None => scheduler.subscription(),
                };
                if let Err(error) = &result {
                    *blocked = Some(error.code);
                }
                result
            },
            self.status.clone(),
        ) {
            notify();
        }
    }
    /// Whether this operation failed at the stdin boundary.
    pub fn tick(&mut self, input: &InputHandle, stopped: bool, notify: &impl Fn()) -> bool {
        self.capture(notify);
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
            let key = custody_key(&request.item);
            let result = if stopped {
                Err(failure(ErrorCode::AgentNotFound))
            } else {
                self.policy().and_then(|_| {
                    self.scheduler
                        .enqueue(request.item, request.principal, request.gate)
                })
            };
            if let Some(key) = key {
                let mut index = self.committed_custody.lock();
                index.items = self.scheduler.journal().item_snapshot();
                index.finish(key, self.scheduler.journal().commitment_unknown());
            }
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
        let pending = self.scheduler.status_on(input).or_else(|| match &offer {
            Ok(Offer::Pending(pending)) => Some(pending.clone()),
            _ => None,
        });
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
        {
            let mut index = self.committed_custody.lock();
            index.items = self.scheduler.journal().item_snapshot();
            index.uncertain = index
                .uncertain
                .or(self.scheduler.journal().commitment_unknown());
        }
        let facts = self.scheduler.take_facts();
        let changed = !facts.is_empty()
            || self.initial
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
                facts,
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
        self.flush(true, notify);
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
