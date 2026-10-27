//! Pure native projection (ADR-0036 §8/§11). No subscriptions, I/O or authority.
use super::{contracts::*, Target};
use crate::api::structured_agent::StructuredAgentInfo;
use crate::structured::{
    history,
    transport::{RecordOrigin, StreamRecord},
    workers::live,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, io};
/// Projection plus ephemeral block indexes. Owners supply canonical S4 and queue facts.
pub(crate) struct Model {
    /// Frozen renderer input; mutation belongs to the controller, never render.
    pub snapshot: Snapshot,
    floor: u64,
    partials: BTreeMap<u64, usize>,
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
                applied_cursor: 0,
                replay: Replay::Replaying { through: 0 },
                usage: None,
                permissions: vec![],
                admission: Admission::Ready,
                composer: Composer {
                    text: String::new(),
                    cursor: 0,
                },
                scroll_anchor: None,
                new_output: false,
                collapsed: Default::default(),
                selected_permission: None,
            },
            floor: 0,
            partials: BTreeMap::new(),
        }
    }
    /// Apply original history order exactly once; reject missing unannounced ranges.
    pub fn history(&mut self, item: history::Item) -> io::Result<()> {
        match item {
            history::Item::Record(r) if r.seq > self.snapshot.applied_cursor => {
                self.contiguous(r.seq)?;
                self.record(&r, false);
                self.snapshot.applied_cursor = r.seq;
            }
            history::Item::Gap { from, to, reason } if to > self.snapshot.applied_cursor => {
                self.contiguous(from)?;
                if to < from {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                self.clear_partials();
                self.push(to, Content::Gap { from, to, reason }, false);
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
                if record.seq >= self.floor && record.seq >= self.snapshot.applied_cursor =>
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
        self.snapshot.transcript.retain(|card| !card.streaming);
        self.partials.clear();
    }
    fn push(&mut self, seq: u64, content: Content, streaming: bool) {
        if matches!(content, Content::Thinking(_) | Content::RawJson { .. }) {
            self.snapshot.collapsed.insert(seq);
        }
        self.snapshot.transcript.push(Card {
            seq,
            content,
            streaming,
        });
        self.snapshot.new_output |= self.snapshot.scroll_anchor.is_some();
    }
    fn record(&mut self, r: &StreamRecord, streaming: bool) {
        // Always retain the exact payload/provenance, including known extras.
        self.push(
            r.seq,
            Content::RawJson {
                known: r.known,
                shape: r.shape.clone(),
                raw: r.raw.clone(),
            },
            streaming,
        );
        if r.known != Some(true) {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&r.raw) else {
            return;
        };
        if streaming {
            let delta = v["event"]
                .get("delta")
                .unwrap_or(&v["event"]["content_block"]);
            let (thinking, text) = match delta["type"].as_str() {
                Some("text_delta" | "text") => (false, delta["text"].as_str()),
                Some("thinking_delta" | "thinking") => (true, delta["thinking"].as_str()),
                _ => (false, None),
            };
            if let Some(text) = text {
                let Some(index) = v["event"]["index"].as_u64() else {
                    return;
                };
                if let Some(&at) = self.partials.get(&index) {
                    match &mut self.snapshot.transcript[at].content {
                        Content::Markdown(s) | Content::Thinking(s) => s.push_str(text),
                        _ => {}
                    }
                } else {
                    self.partials.insert(index, self.snapshot.transcript.len());
                    let content = if thinking {
                        Content::Thinking(text.into())
                    } else {
                        Content::Markdown(text.into())
                    };
                    self.push(r.seq, content, true);
                }
            }
            return;
        }
        if v["type"] == "assistant" {
            self.clear_partials();
        }
        if v["type"] == "system" && v["subtype"] == "init" {
            self.snapshot.reported_model = v["model"].as_str().map(str::to_owned);
            self.snapshot.reported_permissions = v["permissionMode"].as_str().map(str::to_owned);
        }
        let usage = v.get("usage").or_else(|| v.pointer("/message/usage"));
        if let Some(u) = usage {
            self.snapshot.usage = Some(Usage {
                observed_seq: r.seq,
                input_tokens: u["input_tokens"].as_u64(),
                output_tokens: u["output_tokens"].as_u64(),
                cache_read_tokens: u["cache_read_input_tokens"].as_u64(),
                cache_write_tokens: u["cache_creation_input_tokens"].as_u64(),
            });
        }
        if r.origin == RecordOrigin::Flk && v["type"] == "flk_permission_request" {
            if let Ok(permission) = serde_json::from_value(v.clone()) {
                self.snapshot.permissions.push(permission);
            }
        }
        if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
            for b in blocks {
                let content = match b["type"].as_str() {
                    Some("text") => b["text"].as_str().map(|s| Content::Markdown(s.into())),
                    Some("thinking") => b["thinking"].as_str().map(|s| Content::Thinking(s.into())),
                    Some("tool_use") => serde_json::from_value(json!({"subtype":"can_use_tool", "tool_name":b["name"], "tool_use_id":b["id"], "input":b["input"]})).ok().map(Content::ToolRequest),
                    Some("tool_result") => b["tool_use_id"].as_str().map(|id| Content::ToolResult { tool_use_id: id.into(), text: b["content"].as_str().map(str::to_owned).unwrap_or_else(|| b["content"].to_string()), is_error: b["is_error"].as_bool().unwrap_or(false) }),
                    _ => serde_json::from_value(b.clone()).ok().map(Content::Attachment),
                };
                if let Some(content) = content {
                    self.push(r.seq, content, false);
                }
            }
        }
    }
}
#[cfg(test)]
mod tests;
