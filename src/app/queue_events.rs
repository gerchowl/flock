//! Consume the single queue owner, never reconstruct transitions from snapshots.
//! Completions preserve original transition seqs even across a replacement cut.
use crate::{
    api::{
        error_code::ErrorCode,
        schema::{EventData, EventEnvelope, EventKind},
        EventHub,
    },
    structured::{
        publication::{Provider, Subscription},
        queue::scheduler::Pending,
        registry::Registry,
        workers::{
            queue::publication::{Page, Reader},
            Workers,
        },
    },
};
use std::collections::HashMap;

/// In-process status from the same provider used by S7-min consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Observation {
    /// Reservation or read-only refusal, with the last operation error.
    Status(Option<Pending>, Option<ErrorCode>),
    /// Source admission or terminal failure; retry follows the registered code.
    Refused(ErrorCode),
    /// Explicit overflow. Resume from the last applied cursor, not this head.
    Lagged(u64),
    /// Generation drained; any preceding refusal/status remains inspectable.
    End,
}
/// One bounded provider continuation per currently registered agent generation.
#[derive(Default)]
pub(crate) struct QueueEvents {
    readers: HashMap<String, (u64, Reader)>,
    observations: HashMap<String, (u64, Observation)>,
}
impl QueueEvents {
    fn publish(workers: &Workers, hub: &EventHub, terminal: bool) -> bool {
        let mut changed = false;
        for (_, _, update) in workers.queue_completions(terminal) {
            changed = true;
            if let Err(error) = &update.offer {
                tracing::warn!(
                    code = error.code.as_str(),
                    "structured queue operation refused"
                );
            }
            for fact in update.facts {
                hub.push(EventEnvelope {
                    event: EventKind::QueueItemState,
                    data: EventData::QueueItemState(fact),
                });
                changed = true;
            }
        }
        changed
    }
    /// Drain at most four completions and 64 provider pages per agent per wake.
    /// No recovery, retry, extra journal writer or filesystem read is started.
    /// Global appends use the existing EventHub durability/retention policy.
    pub(crate) fn poll(&mut self, workers: &Workers, registry: &Registry, hub: &EventHub) -> bool {
        let before = self.observations.clone();
        let observations = &mut self.observations;
        self.readers.retain(|id, (generation, _)| {
            let current = registry.get(id).is_some_and(|(_, _, g)| g == *generation);
            if !current {
                observations.remove(id);
            }
            current
        });
        self.observations.retain(|id, (generation, _)| {
            registry
                .get(id)
                .is_some_and(|(_, _, current)| current == *generation)
        });
        let changed = Self::publish(workers, hub, false);
        for record in registry.records() {
            let id = &record.agent_id;
            let Some((_, _, generation)) = registry.get(id) else {
                continue;
            };
            if !self.readers.contains_key(id) {
                match workers
                    .queue_source(registry, id, generation)
                    .and_then(|source| source.subscribe(None))
                {
                    Ok(reader) => {
                        self.readers.insert(id.clone(), (generation, reader));
                    }
                    Err(code) => {
                        self.observations
                            .insert(id.clone(), (generation, Observation::Refused(code)));
                        continue;
                    }
                }
            }
            let Some((_, reader)) = self.readers.get_mut(id) else {
                continue;
            };
            for _ in 0..64 {
                let status = match reader.page() {
                    Page::Status(pending, error) => Observation::Status(pending, error),
                    Page::Refused(code) => Observation::Refused(code),
                    Page::Lagged(head) => Observation::Lagged(head),
                    Page::End => {
                        self.observations
                            .entry(id.clone())
                            .or_insert((generation, Observation::End));
                        break;
                    }
                    Page::Pending => break,
                    _ => continue,
                };
                self.observations.insert(id.clone(), (generation, status));
            }
        }
        changed || before != self.observations
    }
}

impl super::App {
    /// Last provider status for the current generation, updated by worker service.
    // S7-min subscription consumers use this status seam (#1830).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn queue_observation(&self, agent: &str) -> Option<&Observation> {
        self.queue_events
            .observations
            .get(agent)
            .filter(|(generation, _)| {
                self.state
                    .structured_agents
                    .get(agent)
                    .is_some_and(|(_, _, current)| current == *generation)
            })
            .map(|(_, state)| state)
    }
}

impl super::App {
    /// Stop sessions before server shutdown or handoff using the existing policy.
    pub(crate) fn shutdown_structured_workers(
        &mut self,
    ) -> Result<(), crate::structured::workers::ShutdownError> {
        let result = self
            .structured_workers
            .shutdown(&mut self.state.structured_agents);
        // Fixed capacity plus one peek and one terminal tail; no producer retry.
        QueueEvents::publish(&self.structured_workers, &self.event_hub, true);
        result
    }
}
