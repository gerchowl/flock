//! Mesh delivery and reply collection run outside the app loop. Completion returns
//! through the event channel so mailbox evidence and replies stay serialized.

use std::collections::VecDeque;
use std::sync::mpsc::Sender;

use crate::api::schema::MsgIntent;
use crate::events::AppEvent;

pub(crate) struct RelayWork {
    pub run: Box<dyn FnOnce() -> AppEvent + Send>,
    pub failure: AppEvent,
}

/// Own the failure completion before invoking code that can unwind. Both the
/// normal return and a panic release the slot through the app event channel.
struct CompletionGuard {
    event_tx: tokio::sync::mpsc::Sender<AppEvent>,
    completion: Option<AppEvent>,
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            let _ = self.event_tx.blocking_send(completion);
        }
    }
}

#[derive(Default)]
pub(crate) struct MessageRelays {
    pub pending: Option<RelaySend>,
    waiting: VecDeque<RelayWork>,
    running: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct RelaySend {
    pub mesh: Option<crate::mesh::delivery::Deliver>,
    pub id: String,
    pub peer: crate::config::PeerConfig,
    pub to_agent: String,
    pub host: String,
    pub direct: bool,
    pub from_agent: String,
    pub from_host: String,
    pub body: String,
    pub correlation_id: String,
    pub in_reply_to: Option<String>,
    pub intent: MsgIntent,
    pub settle_original: Option<super::uplink::SettleOnDelivery>,
    pub respond_to: Option<Sender<String>>,
}

#[derive(Debug)]
pub(crate) struct RelayCompletion {
    pub send: RelaySend,
    pub result: Result<bool, crate::peers::PeerMessageFailure>,
}

impl MessageRelays {
    pub(crate) fn start_bounded(
        &mut self,
        work: RelayWork,
        event_tx: tokio::sync::mpsc::Sender<AppEvent>,
        cap: usize,
    ) {
        // Callers claim durable work only when a slot is free.
        debug_assert!(self.running < cap);
        self.running += 1;
        std::thread::spawn(move || {
            let mut guard = CompletionGuard {
                event_tx,
                completion: Some(work.failure),
            };
            guard.completion = Some((work.run)());
        });
    }

    pub(crate) fn slots(&self, cap: usize) -> usize {
        cap.saturating_sub(self.running)
    }
    pub(crate) fn complete(&mut self) {
        self.running = self.running.saturating_sub(1);
    }

    pub fn is_idle(&self) -> bool {
        self.running == 0 && self.waiting.is_empty()
    }
}

impl RelaySend {
    pub fn into_work(self) -> RelayWork {
        let failure = AppEvent::MsgRelayCompleted(Box::new(RelayCompletion {
            send: self.clone(),
            result: Err(crate::peers::PeerMessageFailure::Unreachable(
                "message relay worker panicked".into(),
            )),
        }));
        RelayWork {
            run: Box::new(move || self.run()),
            failure,
        }
    }

    pub fn run(self) -> AppEvent {
        let result = if let Some(delivery) = &self.mesh {
            crate::mesh::delivery::send(&self.peer, delivery)
                .map_err(crate::peers::PeerMessageFailure::Unreachable)
        } else {
            Err(crate::peers::PeerMessageFailure::Refused(
                "mesh custody required".into(),
            ))
        };
        AppEvent::MsgRelayCompleted(Box::new(RelayCompletion { send: self, result }))
    }
}

impl super::App {
    /// An in-process caller may have no transport to park. Detach its relay
    /// before another request can attach its unrelated responder.
    pub(crate) fn detach_pending_message_relay(&mut self) {
        if let Some(mut relay) = self.message_relays.pending.take() {
            relay.respond_to = None;
            self.enqueue_message_relay(relay.into_work());
        }
    }

    pub(crate) fn enqueue_message_relay(&mut self, work: RelayWork) {
        self.message_relays.waiting.push_back(work);
        self.pump_message_relays();
    }

    pub(crate) fn finish_message_relay(&mut self) {
        self.message_relays.running = self.message_relays.running.saturating_sub(1);
        self.pump_message_relays();
    }

    fn pump_message_relays(&mut self) {
        let cap = self.state.config.msg.deferral_relay_concurrency.max(1);
        while self.message_relays.running < cap {
            let Some(work) = self.message_relays.waiting.pop_front() else {
                break;
            };
            self.message_relays.running += 1;
            let event_tx = self.event_tx.clone();
            std::thread::spawn(move || {
                let mut guard = CompletionGuard {
                    event_tx,
                    completion: Some(work.failure),
                };
                guard.completion = Some((work.run)());
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_workers_wait_for_a_shared_slot_and_start_in_order() {
        let mut config = crate::config::Config::default();
        config.msg.deferral_relay_concurrency = 1;
        let (_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            super::super::App::new(&config, true, None, api_rx, crate::api::EventHub::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        for index in 0..3 {
            let started = started_tx.clone();
            app.enqueue_message_relay(RelayWork {
                run: Box::new(move || {
                    started.send(index).unwrap();
                    AppEvent::ClipboardWrite {
                        content: Vec::new(),
                    }
                }),
                failure: AppEvent::ClipboardWrite {
                    content: Vec::new(),
                },
            });
        }
        for index in 0..3 {
            tokio::time::timeout(std::time::Duration::from_secs(5), app.event_rx.recv())
                .await
                .unwrap()
                .expect("worker completed");
            assert_eq!(started_rx.try_recv().unwrap(), index);
            assert!(
                started_rx.try_recv().is_err(),
                "no second worker before completion is drained"
            );
            assert_eq!(app.message_relays.running, 1);
            app.finish_message_relay();
        }
        assert_eq!(app.message_relays.running, 0);
        assert!(app.message_relays.waiting.is_empty());
    }
    #[tokio::test]
    async fn a_panicking_relay_answers_with_failure_and_frees_its_slot() {
        let mut config = crate::config::Config::default();
        config.msg.deferral_relay_concurrency = 1;
        let (_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            super::super::App::new(&config, true, None, api_rx, crate::api::EventHub::default());
        let mut replies = Vec::new();
        for index in 0..2 {
            let (tx, rx) = std::sync::mpsc::channel();
            replies.push(rx);
            let relay = RelaySend {
                mesh: None,
                id: format!("request-{index}"),
                peer: crate::config::PeerConfig::default(),
                // An invalid agent id is refused before any SSH is started.
                to_agent: "invalid recipient".into(),
                host: "nodeb".into(),
                direct: true,
                from_agent: "agent_nodea_sender".into(),
                from_host: "nodea".into(),
                body: "test".into(),
                correlation_id: format!("question-{index}"),
                in_reply_to: None,
                intent: MsgIntent::NeedsReply,
                settle_original: None,
                respond_to: Some(tx),
            };
            app.mailboxes.start_relaying_question(&relay.correlation_id);
            let mut work = relay.into_work();
            if index == 0 {
                work.run = Box::new(|| panic!("test relay panic"));
            }
            app.enqueue_message_relay(work);
        }
        for (index, reply) in replies.into_iter().enumerate() {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(5), app.event_rx.recv())
                    .await
                    .unwrap()
                    .expect("even a panic must complete");
            app.handle_internal_event(event);
            let response: serde_json::Value =
                serde_json::from_str(&reply.try_recv().unwrap()).unwrap();
            assert_eq!(response["id"], format!("request-{index}"));
            if index == 0 {
                assert_eq!(response["error"]["code"], "peer_unreachable");
                assert_eq!(
                    response["error"]["data"]["detail"],
                    "message relay worker panicked"
                );
            } else {
                assert_eq!(response["error"]["code"], "peer_refused_message");
            }
        }
        assert_eq!(app.message_relays.running, 0);
        assert!(app.message_relays.waiting.is_empty());
    }
}
