//! Bounded provisional fold preferences across finalization and replay gaps.
use super::{BlockKey, Message};
use crate::structured::history::LIVE_RECORDS;
use std::collections::VecDeque;

/// Preferences for the most recently retired live blocks; no content or authority.
#[derive(Default)]
pub(super) struct Folds(VecDeque<(BlockKey, bool)>);
impl Folds {
    pub fn remember(&mut self, key: &BlockKey, folded: bool) {
        self.0.retain(|(old, _)| old != key);
        self.0.push_back((key.clone(), folded));
        if self.0.len() > LIVE_RECORDS {
            self.0.pop_front();
        }
    }
    pub fn get(&self, key: &BlockKey) -> Option<bool> {
        self.0
            .iter()
            .find(|(old, _)| old == key)
            .map(|(_, fold)| *fold)
    }
    pub fn take(&mut self, message: &Message, index: u64, seq: u64) -> Option<bool> {
        let exact = self.0.iter().rposition(|((m, i), _)| {
            matches!(m, Message::Named(_)) && m == message && *i == Some(index)
        });
        let position = exact.or_else(|| {
            self.0.iter().rposition(|((m, i), _)| {
                matches!(m, Message::Cursor(cursor) if *cursor < seq) && *i == Some(index)
            })
        })?;
        self.0.remove(position).map(|(_, folded)| folded)
    }
    pub fn finish(&mut self, message: &Message, seq: u64) {
        self.0.retain(|((m, _), _)| !m.finalized_by(message, seq));
    }
    pub fn skip(&mut self, through: u64) {
        self.0
            .retain(|((m, _), _)| !matches!(m, Message::Cursor(cursor) if *cursor < through));
    }
}
