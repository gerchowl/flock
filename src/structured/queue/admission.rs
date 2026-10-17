//! Durable admission and bounded batch formation (ADR-0036 §3/§4, #1878).
//!
//! One blocking worker owns this value. Admission persists the authenticated
//! producer and gate before acknowledging; formation durably fails oversized heads.
//! Runtime reservation/lifecycle belongs to the next S5b slice, server wiring to #1803.
use super::model::{Delivery, Kind, Priority, Producer, State};
use super::{Change, Item, Journal};
use crate::api::error_code::ErrorCode;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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
        self.journal.healthy()?;
        if !valid_id(&item.item_id)
            || [
                "admission_hash",
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
            || (item.producer != Producer::Human && item.metadata.contains_key("edit_lease"))
            || (item.kind == Kind::Command
                && (!item.body.starts_with('/') || !item.attachments.is_empty()))
            || (item.kind == Kind::Reply
                && metadata(&item, "anchor")["question_id"]
                    .as_str()
                    .is_none_or(|id| !canonical_uuid(id)))
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
            serde_json::to_value(gate).map_err(serialization)?,
        );
        // Hash the normalized admitted request, before lifecycle edits. Retries
        // compare durable identity without re-running a gate or spending a cursor.
        let hash: String = Sha256::digest(serde_json::to_vec(&item).map_err(serialization)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if let Some(old) = self
            .journal
            .items()
            .iter()
            .find(|old| old.item_id == item.item_id)
        {
            return if metadata(old, "admission_hash") == &json!(hash) {
                Ok(())
            } else {
                Err(refuse(ErrorCode::QueueItemConflict))
            };
        }
        item.metadata.insert("admission_hash".into(), json!(hash));
        let probe = envelope(std::slice::from_ref(&item), &item.item_id)?;
        if !bounded(std::slice::from_ref(&item), &probe) {
            return Err(refuse(ErrorCode::InvalidRequest));
        }
        self.commit(Change::Upsert {
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
    /// Form one bounded FIFO candidate. An unbounded head is durably failed
    /// before trying the next item; valid candidates do not spend a cursor.
    /// The runtime owner must reserve atomically and enforce S4/S6 before writing.
    pub fn batch(&mut self, now_ms: u64) -> Result<Option<Batch>, Failure> {
        self.journal.healthy()?;
        loop {
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
                    self.commit(Change::Fail {
                        item_id: items[0].item_id.clone(),
                        reason: ErrorCode::QueueItemTooLarge.as_str().into(),
                    })?;
                    break (id, Value::Null);
                }
                items.pop();
            };
            if !user.is_null() {
                return Ok(Some(Batch {
                    id,
                    envelope: user,
                    items,
                }));
            }
        }
    }
    /// Replace existing items together, retaining order; no success precedes sync.
    pub fn replace(&mut self, items: Vec<Item>) -> Result<(), Failure> {
        for item in &items {
            let old = self
                .journal
                .items()
                .iter()
                .find(|old| old.item_id == item.item_id)
                .ok_or_else(|| refuse(ErrorCode::InvalidRequest))?;
            if old.producer != item.producer
                || old.kind != item.kind
                || metadata(old, "admission_hash") != metadata(item, "admission_hash")
                || item.has_unknown()
                || item.delivery != Delivery::AtIdle
                || item.priority != Priority::Normal
                || (item.kind == Kind::Command
                    && (!item.body.starts_with('/') || !item.attachments.is_empty()))
                || (item.kind == Kind::Continuation
                    && metadata(item, "anchor")["item_id"]
                        .as_str()
                        .is_none_or(|id| !valid_id(id)))
                || (item.producer != Producer::Human && item.metadata.contains_key("edit_lease"))
            {
                return Err(refuse(ErrorCode::InvalidRequest));
            }
            let probe = envelope(std::slice::from_ref(item), &item.item_id)?;
            if !bounded(std::slice::from_ref(item), &probe) {
                return Err(refuse(ErrorCode::InvalidRequest));
            }
        }
        if !items.is_empty() {
            self.commit(Change::Batch { items })?;
        }
        Ok(())
    }
    // Classify all deterministic failures before append. Only an actual append
    // I/O failure (or an already poisoned journal) has uncertain durability.
    fn commit(&mut self, change: Change) -> Result<(), Failure> {
        self.journal.healthy()?;
        let mut items = self.journal.items().to_vec();
        change
            .apply(&mut items)
            .map_err(|_| refuse(ErrorCode::InvalidRequest))?;
        let seq = self
            .journal
            .seq
            .checked_add(1)
            .ok_or_else(|| refuse(ErrorCode::InternalError))?;
        super::encode(&super::Record {
            seq,
            change: change.clone(),
        })
        .map_err(|_| refuse(ErrorCode::InvalidRequest))?;
        self.journal.append(change)?;
        Ok(())
    }
}
fn render(item: &Item) -> Result<String, Failure> {
    if item.kind == Kind::Command {
        return Ok(item.body.clone());
    }
    let reply = if item.kind == Kind::Reply {
        let question = metadata(item, "anchor")["question_id"]
            .as_str()
            .filter(|id| canonical_uuid(id))
            .ok_or_else(|| refuse(ErrorCode::InvalidRequest))?;
        format!(" Answer to {question};")
    } else {
        String::new()
    };
    let nonce = nonce(&item.body, getrandom::fill)?;
    Ok(format!(
        "From {} (item {}):{reply} Attachments (blob refs): {}\n<<<flk-item {nonce}\n{}\nflk-item {nonce}>>>",
        json!(item.producer), item.item_id, json!(item.attachments), item.body
    ))
}
// The callback is the entropy source, kept local so collision/failure handling
// can be exercised without changing process-global random state.
fn nonce(
    body: &str,
    mut fill: impl FnMut(&mut [u8]) -> Result<(), getrandom::Error>,
) -> Result<String, Failure> {
    loop {
        let mut bytes = [0u8; 16];
        fill(&mut bytes).map_err(|e| Failure {
            code: ErrorCode::InternalError,
            source: Some(io::Error::other(e)),
        })?;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if !body.contains(&hex) {
            return Ok(hex);
        }
    }
}
fn batch_uuid() -> Result<String, Failure> {
    let mut h = nonce("", getrandom::fill)?.into_bytes();
    h[12] = b'4';
    h[16] = match h[16] {
        b'0'..=b'3' => b'8',
        b'4'..=b'7' => b'9',
        b'8'..=b'b' => b'a',
        _ => b'b',
    };
    let h = String::from_utf8(h).map_err(|_| refuse(ErrorCode::InternalError))?;
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}
fn serialization(error: serde_json::Error) -> Failure {
    Failure {
        code: ErrorCode::SerializationError,
        source: Some(error.into()),
    }
}
fn canonical_uuid(id: &str) -> bool {
    valid_id(id) && !id.bytes().any(|b| b.is_ascii_uppercase())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_regenerates_on_collision_and_entropy_failure_is_precise() {
        let mut calls = 0;
        let body = "00000000000000000000000000000000";
        let chosen = nonce(body, |bytes| {
            bytes.fill(calls);
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(chosen, "01010101010101010101010101010101");
        let error = nonce("", |_| Err(getrandom::Error::UNSUPPORTED)).unwrap_err();
        assert_eq!(error.code, ErrorCode::InternalError);
        assert!(error.source.is_some());
    }
}
