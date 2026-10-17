//! Durable admission and pure batch formation (ADR-0036 §3/§4, #1878).
//!
//! One blocking worker owns this value. Admission persists the authenticated
//! producer and gate before acknowledging; formation has no runtime side effects.
//! Runtime reservation/lifecycle belongs to the next S5b slice, server wiring to #1803.
use super::model::{Delivery, Kind, Priority, Producer, State};
use super::{Change, Item, Journal};
use crate::api::error_code::ErrorCode;
use serde::Serialize;
use serde_json::{json, Value};
use std::io;

/// One persisted admission decision; no automatic re-gating exists.
#[derive(Debug, Serialize)]
#[serde(tag = "decision", content = "stash", rename_all = "snake_case")]
pub(crate) enum Gate {
    /// Deliver at the configured idle point.
    Send,
    /// Retain until explicit human release.
    Hold,
    /// Keep in a named stash without automatic delivery.
    Stash(String),
    /// Keep a dropped audit item without delivery.
    Drop,
}
/// Trusted dispatcher context, never decoded from request JSON.
pub(crate) enum Principal {
    /// Authenticated operator or operator-token caller.
    Operator,
    /// Authenticated non-operator origin supplied by the dispatcher.
    Origin(Producer),
}
/// Registered refusal or uncertain storage error; callers must inspect before retry.
#[derive(Debug)]
pub(crate) struct Failure {
    /// Registered refusal code, including retry classification.
    pub code: ErrorCode,
    /// Storage/serialization cause when available.
    pub source: Option<io::Error>,
}
impl From<io::Error> for Failure {
    fn from(source: io::Error) -> Self {
        Self {
            code: ErrorCode::InputWriteUnknown,
            source: Some(source),
        }
    }
}
fn refuse(code: ErrorCode) -> Failure {
    Failure { code, source: None }
}
/// Default upper bound on consecutive human inputs in one turn.
pub(crate) const MAX_BATCH: usize = 8;
/// Owned candidate; forming it does not reserve or deliver anything.
#[derive(Debug)]
pub(crate) struct Batch {
    /// Single-item UUID or fresh multi-item batch UUID.
    pub id: String,
    /// Bounded stream-json user envelope; command exception already applied.
    pub envelope: Value,
    /// Owned members in FIFO order; no reservation has happened yet.
    pub items: Vec<Item>,
}
/// Worker-owned admission and FIFO formation over one durable journal.
pub(crate) struct Admission {
    journal: Journal,
    max_batch: usize,
}
impl Admission {
    /// Refuse future data whose scheduling meaning cannot be proved.
    pub fn new(journal: Journal, max_batch: usize) -> Result<Self, Failure> {
        journal.healthy()?;
        if journal.future || max_batch == 0 {
            return Err(refuse(ErrorCode::InvalidRequest));
        }
        Ok(Self { journal, max_batch })
    }
    /// Immutable committed state/replay for the worker's serialized publisher.
    pub fn journal(&self) -> &Journal {
        &self.journal
    }
    /// Append one new item and its gate before acknowledging acceptance. Operator
    /// authority creates human input; other origins cannot impersonate humans or
    /// create command items, even when their body starts with a slash.
    pub fn enqueue(
        &mut self,
        mut item: Item,
        principal: Principal,
        gate: Gate,
    ) -> Result<(), Failure> {
        if item.delivery != Delivery::AtIdle || item.priority != Priority::Normal {
            return Err(refuse(ErrorCode::QueueModeUnsupported));
        }
        item.producer = match principal {
            Principal::Operator => Producer::Human,
            Principal::Origin(origin)
                if origin != Producer::Human && item.kind != Kind::Command =>
            {
                origin
            }
            _ => return Err(refuse(ErrorCode::OperatorOnly)),
        };
        if !valid_id(&item.item_id)
            || self
                .journal
                .items()
                .iter()
                .any(|i| i.item_id == item.item_id)
            || [
                "gate",
                "batch_id",
                "awaiting_result",
                "refusal",
                "stash",
                "item_id",
                "producer",
                "kind",
                "state",
                "delivery",
                "priority",
                "body",
                "attachments",
            ]
            .iter()
            .any(|k| item.metadata.contains_key(*k))
            || (item.kind == Kind::Command
                && (!item.body.starts_with('/') || !item.attachments.is_empty()))
            || (item.kind == Kind::Reply
                && metadata(&item, "anchor")["question_id"]
                    .as_str()
                    .is_none_or(str::is_empty))
            || (item.kind == Kind::Continuation
                && metadata(&item, "anchor")["item_id"]
                    .as_str()
                    .is_none_or(|id| !valid_id(id)))
        {
            return Err(refuse(ErrorCode::InvalidRequest));
        }
        item.state = match &gate {
            Gate::Send => State::Queued,
            Gate::Hold => State::Held,
            Gate::Stash(name) if !name.is_empty() => State::Stashed,
            Gate::Drop => State::Dropped,
            _ => return Err(refuse(ErrorCode::InvalidRequest)),
        };
        if item.has_unknown() {
            return Err(refuse(ErrorCode::QueueModeUnsupported));
        }
        if let Gate::Stash(name) = &gate {
            item.metadata.insert("stash".into(), json!(name));
        }
        item.metadata.insert(
            "gate".into(),
            serde_json::to_value(gate).map_err(io::Error::from)?,
        );
        let probe = envelope(std::slice::from_ref(&item), &item.item_id)?;
        if !bounded(std::slice::from_ref(&item), &probe) {
            return Err(refuse(ErrorCode::InvalidRequest));
        }
        self.journal.append(Change::Upsert {
            item: Box::new(item),
            position: self.journal.items().len(),
        })?;
        Ok(())
    }
    fn eligible(&self, item: &Item, now_ms: u64) -> bool {
        item.state == State::Queued
            && item.delivery == Delivery::AtIdle
            && item.priority == Priority::Normal
            && metadata(item, "gate")["decision"] == "send"
            && item.metadata.get("edit_lease").is_none_or(|lease| {
                lease["until_ms"]
                    .as_u64()
                    .is_some_and(|until| until <= now_ms)
            })
            && (item.kind != Kind::Continuation
                || self.journal.items().iter().any(|anchor| {
                    Some(anchor.item_id.as_str()) == metadata(item, "anchor")["item_id"].as_str()
                        && anchor.state == State::Done
                        && anchor
                            .metadata
                            .get("awaiting_result")
                            .is_none_or(|v| v == &Value::Bool(false))
                }))
    }
    /// Form one bounded FIFO candidate. Repeated calls have no storage effects.
    /// The runtime owner must reserve atomically and enforce S4/S6 before writing.
    pub fn batch(&self, now_ms: u64) -> Result<Option<Batch>, Failure> {
        self.journal.healthy()?;
        let mut eligible = self
            .journal
            .items()
            .iter()
            .filter(|i| self.eligible(i, now_ms));
        let Some(head) = eligible.next() else {
            return Ok(None);
        };
        let mut items = vec![head.clone()];
        if head.producer == Producer::Human && head.kind != Kind::Command {
            items.extend(
                eligible
                    .take_while(|i| i.producer == Producer::Human && i.kind != Kind::Command)
                    .take(self.max_batch - 1)
                    .cloned(),
            );
        }
        let (id, user) = loop {
            let id = if items.len() == 1 {
                items[0].item_id.clone()
            } else {
                batch_uuid()?
            };
            let user = envelope(&items, &id)?;
            if bounded(&items, &user) {
                break (id, user);
            }
            if items.len() == 1 {
                return Err(refuse(ErrorCode::InvalidRequest));
            }
            items.pop();
        };
        Ok(Some(Batch {
            id,
            envelope: user,
            items,
        }))
    }
    /// Replace existing items together, retaining order; no success precedes sync.
    pub fn replace(&mut self, items: Vec<Item>) -> Result<(), Failure> {
        if !items.is_empty() {
            self.journal.append(Change::Batch { items })?;
        }
        Ok(())
    }
}
fn render(item: &Item) -> Result<String, Failure> {
    if item.kind == Kind::Command {
        return Ok(item.body.clone());
    }
    let producer = json!(item.producer);
    let reply = if item.kind == Kind::Reply {
        let question = metadata(item, "anchor")["question_id"]
            .as_str()
            .ok_or_else(|| refuse(ErrorCode::InvalidRequest))?;
        format!("Answer to {question}:\n")
    } else {
        String::new()
    };
    Ok(format!(
        "From {producer}:\n{reply}{}\nAttachments (blob refs): {}",
        item.body,
        json!(item.attachments)
    ))
}
fn batch_uuid() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

fn metadata<'a>(item: &'a Item, key: &str) -> &'a Value {
    item.metadata.get(key).unwrap_or(&Value::Null)
}
fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
fn envelope(items: &[Item], id: &str) -> Result<Value, Failure> {
    let content = items
        .iter()
        .map(|i| Ok(json!({"type":"text", "text":render(i)?})))
        .collect::<Result<Vec<_>, Failure>>()?;
    Ok(json!({"type":"user", "uuid":id, "message":{"role":"user", "content":content}}))
}
// Reserve space for every later lifecycle and exit transition before acceptance.
// Prefix batching also respects both the journal record and stdin frame ceilings.
fn bounded(items: &[Item], user: &Value) -> bool {
    let mut worst = items.to_vec();
    for item in &mut worst {
        item.state = State::Cancelled;
        item.metadata
            .insert("batch_id".into(), user["uuid"].clone());
        item.metadata.insert("awaiting_result".into(), json!(true));
        item.metadata.insert(
            "refusal".into(),
            json!(ErrorCode::SessionUnconfirmed.as_str()),
        );
    }
    super::encode(&super::Record {
        seq: u64::MAX,
        change: Change::Batch { items: worst },
    })
    .is_ok()
        && super::encode(user).is_ok()
}
