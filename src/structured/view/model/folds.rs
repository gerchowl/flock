//! Bounded provisional fold preferences across finalization and replay gaps.
use super::{BlockKey, Content, Message};
use crate::structured::history::LIVE_RECORDS;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

/// Preferences for the most recently retired live blocks; no content or authority.
#[derive(Default)]
pub(super) struct Folds(VecDeque<(BlockKey, bool, Option<[u8; 32]>)>);
impl Folds {
    pub fn remember(&mut self, key: &BlockKey, folded: bool, content: &Content) {
        self.0.retain(|(old, _, _)| old != key);
        self.0
            .push_back((key.clone(), folded, fingerprint(content)));
        if self.0.len() > LIVE_RECORDS {
            self.0.pop_front();
        }
    }
    pub fn get(&self, key: &BlockKey) -> Option<bool> {
        self.0
            .iter()
            .find(|(old, _, _)| old == key)
            .map(|(_, fold, _)| *fold)
    }
    pub fn take(
        &mut self,
        message: &Message,
        index: u64,
        seq: u64,
        content: &Content,
        single_block: bool,
    ) -> Option<bool> {
        let digest = fingerprint(content)?;
        // Public assistant envelopes omit the internal API block index. A single
        // block can occur anywhere in the message: only unique content may match.
        let mut candidates = self
            .0
            .iter()
            .enumerate()
            .filter(|(_, ((m, i), _, hash))| {
                matches!(m, Message::Named(_))
                    && m == message
                    && (single_block || *i == Some(index))
                    && *hash == Some(digest)
            })
            .map(|(position, _)| position);
        let named = candidates.next();
        if candidates.next().is_some() {
            return None;
        }
        let position = named.or_else(|| {
            self.0.iter().rposition(|((m, i), _, hash)| {
                matches!(m, Message::Cursor(cursor) if *cursor < seq)
                    && *i == Some(index)
                    && *hash == Some(digest)
            })
        })?;
        self.0.remove(position).map(|(_, folded, _)| folded)
    }
    pub fn finish(&mut self, seq: u64) {
        // Anonymous hints cannot cross a finalized message. Named siblings may
        // have separate assistant records; keep them within the same bounded cache.
        self.0
            .retain(|((m, _), _, _)| !matches!(m, Message::Cursor(cursor) if *cursor < seq));
    }
    pub fn skip(&mut self, through: u64) {
        self.0
            .retain(|((m, _), _, _)| !matches!(m, Message::Cursor(cursor) if *cursor < through));
    }
}

fn fingerprint(content: &Content) -> Option<[u8; 32]> {
    let (kind, text) = match content {
        Content::Markdown(text) => (0u8, text),
        Content::Thinking(text) => (1u8, text),
        _ => return None,
    };
    let mut hash = Sha256::new();
    hash.update([kind]);
    hash.update(text.as_bytes());
    Some(hash.finalize().into())
}
/// Match a complete public block without guessing its absent API index.
pub(super) fn same_content(content: &Content, block: &serde_json::Value) -> bool {
    match (content, block["type"].as_str()) {
        (Content::Markdown(text), Some("text")) => block["text"].as_str() == Some(text),
        (Content::Thinking(text), Some("thinking")) => block["thinking"].as_str() == Some(text),
        _ => false,
    }
}
