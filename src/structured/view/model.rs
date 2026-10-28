//! Pure native projection (ADR-0036 §8/§11). No subscriptions, I/O or authority.
use super::{contracts::*, frame::Frame, Target};
use crate::api::structured_agent::StructuredAgentInfo;
use crate::structured::{
    history,
    transport::{RecordOrigin, StreamRecord},
    workers::live,
};
use serde_json::Value;
use std::{collections::BTreeMap, io};
// Durable cards use positional ordinals; provisional live cards use a disjoint
// range until the matching durable message replaces them.
const LIVE_ORDINAL: u32 = 1 << 31;
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Message {
    Named(String),
    Cursor(u64),
}
impl Message {
    fn finalized_by(&self, message: &Self, seq: u64) -> bool {
        match self {
            Self::Named(_) => self == message,
            Self::Cursor(cursor) => *cursor < seq,
        }
    }
}
type BlockKey = (Message, Option<u64>);
mod folds;
/// Projection plus ephemeral block indexes. Owners supply canonical S4 and queue facts.
pub(crate) struct Model {
    /// Frozen renderer input; mutation belongs to the controller, never render.
    pub snapshot: Frame,
    floor: u64,
    partials: BTreeMap<BlockKey, CardId>,
    folds: folds::Folds,
    blocked_live: Option<u64>,
}
impl Model {
    /// Assemble a cold projection; requested policy is supplied, never inferred from output.
    pub fn new(
        target: Target,
        agent: StructuredAgentInfo,
        requested_permissions: crate::structured::launch::PermissionMode,
        queue: Vec<crate::structured::queue::model::Item>,
    ) -> Self {
        Self {
            snapshot: Snapshot {
                target,
                agent,
                requested_permissions,
                queue,
                reported_model: None,
                reported_permissions: None,
                transcript: vec![],
                earlier_history: None,
                applied_cursor: 0,
                replay: Replay::Replaying { through: 0 },
                usage: None,
                permissions: vec![],
                admission: Admission::Ready,
                composer: Composer {
                    text: String::new(),
                    cursor: 0,
                    text_revision: 0,
                },
                admission_receipt: None,
                scroll_anchor: None,
                new_output: false,
                collapsed: Default::default(),
                selected_permission: None,
            }
            .into(),
            floor: 0,
            partials: BTreeMap::new(),
            folds: Default::default(),
            blocked_live: None,
        }
    }
    /// Apply original history order exactly once; reject missing unannounced ranges.
    pub fn history(&mut self, item: history::Item) -> io::Result<()> {
        match item {
            history::Item::Record(r) if r.seq > self.snapshot.applied_cursor => {
                self.contiguous(r.seq)?;
                self.record(&r, false);
                self.prune();
                self.snapshot.applied_cursor = r.seq;
            }
            history::Item::Gap { from, to, reason } if to > self.snapshot.applied_cursor => {
                self.contiguous(from)?;
                if to < from {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                self.clear_partials();
                self.folds.skip(to);
                self.push(
                    CardId {
                        seq: to,
                        ordinal: 0,
                    },
                    Content::Gap { from, to, reason },
                    false,
                    false,
                );
                self.prune();
                self.snapshot.applied_cursor = to;
            }
            history::Item::Synced(through) => self.snapshot.replay = Replay::Synced { through },
            history::Item::Lagged(through) => self.gap(through),
            history::Item::End => self.snapshot.replay = Replay::Ended,
            _ => {}
        }
        Ok(())
    }
    fn contiguous(&self, next: u64) -> io::Result<()> {
        if self.snapshot.applied_cursor.checked_add(1) == Some(next) {
            Ok(())
        } else {
            Err(io::ErrorKind::InvalidData.into())
        }
    }
    /// Gap.through is sampled at receive time: queued deltas may predate it.
    /// Discard seq < through, clear partial blocks, and resync history from the
    /// applied cursor, never from this diagnostic floor. No live item advances it.
    pub fn live(&mut self, item: live::Item) {
        match item {
            live::Item::Delta { record, .. }
                if record.seq >= self.floor
                    && record.seq >= self.snapshot.applied_cursor
                    && self.blocked_live.is_none_or(|through| record.seq > through) =>
            {
                self.record(&record, true)
            }
            live::Item::Gap { through } => self.gap(through),
            live::Item::End(_) => self.snapshot.replay = Replay::Ended,
            _ => {}
        }
    }
    fn gap(&mut self, through: u64) {
        self.floor = self.floor.max(through);
        self.clear_partials();
        self.snapshot.replay = Replay::Lagged { through };
    }
    fn clear_partials(&mut self) {
        for (key, id) in &self.partials {
            self.folds
                .remember(key, self.snapshot.collapsed.contains(id));
        }
        self.snapshot.transcript.retain(|card| !card.streaming);
        self.partials.clear();
        self.prune();
    }
    fn prune(&mut self) {
        let cursors: std::collections::BTreeSet<_> =
            self.snapshot.transcript.iter().map(|c| c.seq).collect();
        if let Some(&first) = cursors.iter().rev().nth(history::LIVE_RECORDS - 1) {
            let before = self.snapshot.transcript.len();
            self.snapshot.transcript.retain(|c| c.seq >= first);
            if self.snapshot.transcript.len() != before {
                self.snapshot.earlier_history = Some(first);
            }
        }
        let retained: std::collections::BTreeSet<_> =
            self.snapshot.transcript.iter().map(|c| c.id).collect();
        self.snapshot.collapsed.retain(|id| retained.contains(id));
        self.partials.retain(|_, id| retained.contains(id));
    }
    fn push(&mut self, id: CardId, content: Content, streaming: bool, fold: bool) {
        if fold {
            self.snapshot.collapsed.insert(id);
        }
        self.snapshot.transcript.push(Card {
            id,
            seq: id.seq,
            content,
            streaming,
        });
        self.snapshot.new_output |= self.snapshot.scroll_anchor.is_some();
    }
    fn raw(r: &StreamRecord) -> Content {
        Content::RawJson {
            known: r.known,
            shape: r.shape.clone(),
            raw: r.raw.clone(),
        }
    }
    fn message(v: &Value, seq: u64) -> Message {
        match v
            .get("api_message_id")
            .and_then(Value::as_str)
            .or_else(|| v.pointer("/message/id").and_then(Value::as_str))
        {
            Some(id) => Message::Named(id.to_owned()),
            None => Message::Cursor(seq),
        }
    }
    fn partial(&mut self, r: &StreamRecord, v: &Value) {
        if matches!(
            v["event"]["type"].as_str(),
            Some("content_block_start" | "content_block_stop")
        ) {
            return;
        }
        let delta = v["event"]
            .get("delta")
            .unwrap_or(&v["event"]["content_block"]);
        let (thinking, text) = match delta["type"].as_str() {
            Some("text_delta" | "text") => (false, delta["text"].as_str()),
            Some("thinking_delta" | "thinking") => (true, delta["thinking"].as_str()),
            _ => (false, None),
        };
        if text.is_some_and(|s| s.len() > super::super::transport::MAX_LINE_BYTES) {
            self.overflow(r.seq);
            return;
        }
        self.snapshot.new_output |= self.snapshot.scroll_anchor.is_some();
        let key = (Self::message(v, r.seq), v["event"]["index"].as_u64());
        if let Some(id) = self.partials.get(&key) {
            if let Some(card) = self.snapshot.transcript.iter_mut().find(|c| c.id == *id) {
                if let (Some(text), Content::Markdown(s) | Content::Thinking(s)) =
                    (text, &mut card.content)
                {
                    if s.len().saturating_add(text.len()) > super::super::transport::MAX_LINE_BYTES
                    {
                        self.overflow(r.seq);
                    } else {
                        s.push_str(text);
                    }
                } else {
                    card.content = Self::raw(r);
                    self.snapshot.collapsed.insert(*id);
                }
            }
            return;
        }
        if self.partials.len() >= history::LIVE_RECORDS {
            self.overflow(r.seq);
            return;
        }
        // Transient cards occupy a separate ordinal range. Within it the first
        // observation order is stable for the same feed and parser version.
        let Some(ordinal) = self
            .snapshot
            .transcript
            .iter()
            .filter(|c| c.seq == r.seq && c.streaming)
            .map(|c| c.id.ordinal)
            .max()
            .map_or(Some(LIVE_ORDINAL), |n| n.checked_add(1))
        else {
            self.overflow(r.seq);
            return;
        };
        let id = CardId {
            seq: r.seq,
            ordinal,
        };
        let content = match text {
            Some(s) if thinking => Content::Thinking(s.into()),
            Some(s) => Content::Markdown(s.into()),
            None => Self::raw(r),
        };
        let fold = self.folds.get(&key).unwrap_or(matches!(
            content,
            Content::Thinking(_) | Content::RawJson { .. }
        ));
        self.partials.insert(key, id);
        self.push(id, content, true, fold);
        self.prune();
    }
    fn overflow(&mut self, through: u64) {
        self.gap(through);
        self.blocked_live = Some(through);
    }
    fn observe_permission(&mut self, record: crate::structured_wire::PermissionRecord) {
        use crate::structured_wire::PermissionState;
        let records = &mut self.snapshot.permissions;
        records.retain(|old| {
            old.request_id != record.request_id || old.request_seq != record.request_seq
        });
        records.push(record);
        let mut excess = records
            .iter()
            .filter(|r| matches!(r.state, PermissionState::Closed { .. }))
            .count()
            .saturating_sub(history::LIVE_RECORDS);
        records.retain(|r| {
            if excess > 0 && matches!(r.state, PermissionState::Closed { .. }) {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }
    fn record(&mut self, r: &StreamRecord, streaming: bool) {
        let parsed = serde_json::from_str::<Value>(&r.raw);
        if streaming {
            match parsed {
                Ok(v) if r.known == Some(true) => self.partial(r, &v),
                _ => self.partial(r, &Value::Null),
            }
            return;
        }
        let raw_id = CardId {
            seq: r.seq,
            ordinal: 0,
        };
        self.push(
            raw_id,
            Self::raw(r),
            false,
            r.known != Some(true) || parsed.is_err(),
        );
        if r.known != Some(true) {
            return;
        }
        let Ok(v) = parsed else {
            return;
        };
        let message = Self::message(&v, r.seq);
        if v["type"] == "assistant" {
            let removed: std::collections::BTreeSet<_> = self
                .partials
                .iter()
                .filter(|((m, _), _)| m.finalized_by(&message, r.seq))
                .map(|(_, id)| *id)
                .collect();
            for (key, id) in &self.partials {
                if removed.contains(id) {
                    self.folds
                        .remember(key, self.snapshot.collapsed.contains(id));
                }
            }
            self.snapshot
                .transcript
                .retain(|c| !removed.contains(&c.id));
            self.prune();
        }
        if v["type"] == "system" && v["subtype"] == "init" {
            self.snapshot.reported_model = v["model"].as_str().map(str::to_owned);
            self.snapshot.reported_permissions = v["permissionMode"].as_str().map(str::to_owned);
        }
        if let Some(u) = v.get("usage").or_else(|| v.pointer("/message/usage")) {
            self.snapshot.usage = Some(Usage {
                observed_seq: r.seq,
                input_tokens: u["input_tokens"].as_u64(),
                output_tokens: u["output_tokens"].as_u64(),
                cache_read_tokens: u["cache_read_input_tokens"].as_u64(),
                cache_write_tokens: u["cache_creation_input_tokens"].as_u64(),
            });
        }
        if r.origin == RecordOrigin::Flk && v["type"] == "flk_permission_request" {
            match serde_json::from_value(v.clone()) {
                Ok(permission) => {
                    self.observe_permission(permission);
                    self.snapshot.collapsed.insert(raw_id);
                }
                Err(_) => {
                    self.snapshot.collapsed.insert(raw_id);
                }
            }
        }
        if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
            let raw: std::sync::Arc<str> = r.raw.as_str().into();
            for (index, b) in blocks.iter().enumerate() {
                let content = match b["type"].as_str() {
                    Some("text") => b["text"].as_str().map(|s| Content::Markdown(s.into())),
                    Some("thinking") => b["thinking"].as_str().map(|s| Content::Thinking(s.into())),
                    Some("tool_use") => {
                        match serde_json::from_value::<ObservedToolUse>(b.clone()) {
                            Ok(mut tool) => {
                                tool.raw = raw.clone();
                                Some(Content::ToolUse(tool))
                            }
                            Err(_) => None,
                        }
                    }
                    Some("tool_result") => {
                        b["tool_use_id"].as_str().map(|id| Content::ToolResult {
                            tool_use_id: id.into(),
                            text: b["content"]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| b["content"].to_string()),
                            is_error: b["is_error"].as_bool().unwrap_or(false),
                        })
                    }
                    _ => match serde_json::from_value(b.clone()) {
                        Ok(blob) => Some(Content::Attachment(blob)),
                        Err(_) => None,
                    },
                };
                match content {
                    Some(content) => {
                        let default = matches!(content, Content::Thinking(_));
                        let fold = if v["type"] == "assistant" {
                            self.folds
                                .take(&message, index as u64, r.seq)
                                .unwrap_or(default)
                        } else {
                            default
                        };
                        self.snapshot.collapsed.insert(raw_id);
                        self.push(
                            CardId {
                                seq: r.seq,
                                ordinal: index as u32 + 1,
                            },
                            content,
                            false,
                            fold,
                        );
                    }
                    // The exact source event remains inspectable, with its original
                    // classifier provenance, even when a block cannot be projected.
                    None => {
                        self.snapshot.collapsed.insert(raw_id);
                    }
                }
            }
        }
        if v["type"] == "assistant" {
            self.folds.finish(&message, r.seq);
        }
    }
}
#[cfg(test)]
mod tests;
