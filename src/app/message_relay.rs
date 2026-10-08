//! Legacy SSH message delivery runs outside the app loop. Completion returns
//! through the event channel so mailbox evidence and replies stay serialized.

use std::collections::VecDeque;
use std::sync::mpsc::Sender;

use crate::api::schema::MsgIntent;
use crate::events::AppEvent;

pub(crate) type RelayWork = Box<dyn FnOnce() -> AppEvent + Send>;

#[derive(Default)]
pub(crate) struct MessageRelays {
    pub pending: Option<RelaySend>,
    waiting: VecDeque<RelayWork>,
    running: usize,
}

#[derive(Debug)]
pub(crate) struct RelaySend {
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
    pub result: Result<(), crate::peers::PeerMessageFailure>,
}

impl RelaySend {
    pub fn run(self) -> AppEvent {
        let result = crate::peers::send_peer_message(
            &self.peer,
            &self.to_agent,
            &self.from_agent,
            &self.from_host,
            &self.body,
            &self.correlation_id,
            self.in_reply_to.as_deref(),
            self.intent,
        );
        AppEvent::MsgRelayCompleted(Box::new(RelayCompletion { send: self, result }))
    }
}

impl super::App {
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
                let _ = event_tx.blocking_send(work());
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
            app.enqueue_message_relay(Box::new(move || {
                started.send(index).unwrap();
                AppEvent::ClipboardWrite {
                    content: Vec::new(),
                }
            }));
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
}
