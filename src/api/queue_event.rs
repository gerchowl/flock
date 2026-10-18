//! Global queue lifecycle payload, ADR-0036 §3/§4.
use crate::structured::queue::model::Producer;
use serde::{Deserialize, Serialize};

/// The seven ADR §3 states. Future names never turn into a known transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueState {
    /// Awaiting admission to a turn.
    Queued,
    /// Requires explicit release.
    Held,
    /// Retained in a named stash.
    Stashed,
    /// Harness reported started.
    Delivered,
    /// Turn completed.
    Done,
    /// Harness or operator cancelled.
    Cancelled,
    /// Operator or policy dropped the item.
    Dropped,
    /// A newer state; readers skip the entire fact.
    #[serde(other)]
    Unknown,
}
/// One committed transition. Queue and global sequences are independent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItemState {
    /// Original agent identity.
    pub agent: String,
    /// Stable lifecycle item identity.
    pub item_id: String,
    /// Batch identity retained on delivered members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    /// Authenticated origin, never a producer label parsed from text.
    pub producer: Producer,
    /// Previous known state.
    pub from: QueueState,
    /// New known state.
    pub to: QueueState,
    /// Per-agent queue stream.
    pub stream: String,
    /// Original committed transition line, never a checkpoint snapshot sequence.
    pub stream_seq: u64,
}
impl QueueItemState {
    /// False means this reader cannot interpret the transition.
    pub fn known(&self) -> bool {
        self.from != QueueState::Unknown && self.to != QueueState::Unknown
    }
}
