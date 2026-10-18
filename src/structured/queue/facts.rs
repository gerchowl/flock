//! Transition capture after append durability, before any replacement checkpoint.
use super::{
    model::{Producer, State},
    Item,
};
use crate::api::queue_event::{QueueItemState, QueueState};
fn state(value: &State) -> QueueState {
    match value {
        State::Queued => QueueState::Queued,
        State::Held => QueueState::Held,
        State::Stashed => QueueState::Stashed,
        State::Delivered => QueueState::Delivered,
        State::Done => QueueState::Done,
        State::Cancelled => QueueState::Cancelled,
        State::Dropped => QueueState::Dropped,
        State::Failed | State::Unknown(_) => QueueState::Unknown,
    }
}
pub(super) fn transitions(
    agent: &str,
    before: &[Item],
    after: &[Item],
    seq: u64,
) -> Vec<QueueItemState> {
    after
        .iter()
        .filter_map(|item| {
            let old = before.iter().find(|old| old.item_id == item.item_id)?;
            if old.state == item.state || matches!(item.producer, Producer::Unknown(_)) {
                return None;
            }
            let fact = QueueItemState {
                agent: agent.into(),
                item_id: item.item_id.clone(),
                batch_id: item
                    .metadata
                    .get("batch_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                producer: item.producer.clone(),
                from: state(&old.state),
                to: state(&item.state),
                stream: format!("queue/{agent}"),
                stream_seq: seq,
            };
            fact.known().then_some(fact)
        })
        .collect()
}
impl super::Journal {
    /// Transfer committed facts exactly once to this writer's bounded completion.
    pub(crate) fn take_facts(&mut self) -> Vec<QueueItemState> {
        std::mem::take(&mut self.facts)
    }
}
