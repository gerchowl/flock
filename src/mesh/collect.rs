//! Direct-edge collection runs off the app loop. Import precedes acknowledgement.
use super::{delivery::Deliver, key::MessageKey};
use serde::{Deserialize, Serialize};

pub const BATCH_CAP: usize = 16;
pub(crate) const POLL_CONCURRENCY: usize = 4;

#[derive(Debug, Default)]
pub(crate) struct Batch {
    pub receipt: Option<String>,
    pub deliveries: Vec<Deliver>,
    pub quarantined: Vec<OutboundAck>,
}

/// Idle and failed edges use the same bounded cadence as answer collection.
#[derive(Debug, Default)]
pub(crate) struct Poll {
    pub pending: bool,
    pub pinned: bool,
    deadline: Option<std::time::Instant>,
    attempts: usize,
    generation: u64,
    failed: bool,
}
impl Poll {
    pub fn note_receipts(&mut self, pending: bool) {
        self.pending |= pending;
    }

    pub fn ready(&mut self, now: std::time::Instant, pinned: bool, generation: u64) -> bool {
        if pinned && (!self.pinned || self.generation != generation) {
            self.deadline = None;
            self.attempts = 0;
            self.failed = false;
        }
        self.pinned = pinned;
        self.generation = generation;
        pinned && (self.deadline.is_none_or(|at| at <= now) || (self.pending && !self.failed))
    }
    pub fn finished(&mut self, now: std::time::Instant, failed: bool) {
        self.pending = false;
        self.failed = failed;
        let seconds = [5, 60, 299][self.attempts.min(2)];
        self.attempts = self.attempts.saturating_add(1);
        let mut random = [0; 2];
        let _ = getrandom::fill(&mut random);
        let jitter = u64::from(u16::from_le_bytes(random) % 1001);
        self.deadline = Some(now + std::time::Duration::from_millis(seconds * 1000 + jitter));
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Collect {
    Answers(AnswerCollect),
    Outbound { outbound: OutboundCollect },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboundCollect {
    #[serde(default)]
    pub receipts: Vec<Receipt>,
    #[serde(default)]
    pub ack: Vec<OutboundAck>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub key: MessageKey,
    pub token: Vec<u8>,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboundAck {
    pub key: MessageKey,
    pub token: Vec<u8>,
    pub refusal: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerCollect {
    pub request: MessageKey,
    pub token: Vec<u8>,
    #[serde(default)]
    pub ack: Vec<MessageKey>,
}

#[derive(Debug)]
pub(crate) struct Completion {
    pub peer: crate::config::PeerConfig,
    pub query: Collect,
    pub result: Result<Batch, String>,
}

pub(crate) fn work(
    peer: crate::config::PeerConfig,
    query: Collect,
) -> crate::app::message_relay::RelayWork {
    use crate::events::AppEvent;
    let failure = AppEvent::MeshCollected(Box::new(Completion {
        peer: peer.clone(),
        query: query.clone(),
        result: Err("collection worker panicked".into()),
    }));
    crate::app::message_relay::RelayWork {
        failure,
        run: Box::new(move || {
            let result = fetch(&peer, &query);
            AppEvent::MeshCollected(Box::new(Completion {
                peer,
                query,
                result,
            }))
        }),
    }
}

fn fetch(peer: &crate::config::PeerConfig, query: &Collect) -> Result<Batch, String> {
    super::hello::with_store(|store| {
        if store.clock().map_err(|e| e.to_string())?.paused {
            return Err("fleet_paused".into());
        }
        Ok(())
    })?;
    let raw = crate::peer_stream::request(
        peer,
        "mesh.collect",
        serde_json::to_value(query).map_err(|e| e.to_string())?,
    )?;
    decode(&raw)
}

fn decode(raw: &str) -> Result<Batch, String> {
    let response: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if let Some(error) = response.get("error") {
        let reason = error["message"].as_str().unwrap_or("unknown refusal");
        if matches!(reason, "fleet_paused" | "mailbox_full" | "mail_store_full") {
            return Err(reason.into());
        }
        return Err(format!("mesh collection refused: {reason}"));
    }
    let answers = response["result"]["answers"]
        .as_array()
        .ok_or("invalid collection batch")?;
    if answers.len() > BATCH_CAP {
        return Err("mesh collection batch too large".into());
    }
    let mut batch = Batch {
        receipt: response["result"]["receipt"].as_str().map(str::to_owned),
        ..Batch::default()
    };
    for value in answers {
        match serde_json::from_value::<Deliver>(value.clone()) {
            Ok(delivery) => batch.deliveries.push(delivery),
            Err(_) => {
                // Keep the authenticated identity/token even when the payload is undecodable.
                // Unidentifiable garbage is isolated here and never discards other records.
                if let (Ok(key), Ok(token)) = (
                    serde_json::from_value(value["envelope"]["key"].clone()),
                    serde_json::from_value(
                        value["envelope"]["return_binding"]["collection_token"].clone(),
                    ),
                ) {
                    batch.quarantined.push(OutboundAck {
                        key,
                        token,
                        refusal: Some("invalid_envelope".into()),
                    });
                }
            }
        }
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    #[test]
    fn receipts_wake_idle_poll_but_failed_receipt_commit_keeps_backoff() {
        let now = std::time::Instant::now();
        let mut poll = super::Poll::default();
        assert!(poll.ready(now, true, 1));
        poll.finished(now, false);
        assert!(!poll.ready(now, true, 1));
        poll.note_receipts(true);
        assert!(poll.ready(now, true, 1));
        // Transport succeeded but persisting receipts_sent failed.
        poll.finished(now, true);
        for second in 0..60 {
            poll.note_receipts(true);
            assert!(!poll.ready(now + std::time::Duration::from_secs(second), true, 1));
        }
        assert!(poll.ready(now + std::time::Duration::from_secs(61), true, 1));
        poll.finished(now + std::time::Duration::from_secs(61), false);
        assert!(!poll.ready(now + std::time::Duration::from_secs(62), true, 1));
    }

    #[test]
    fn spoke_custody_idle_and_failed_polls_back_off_without_busy_waiting() {
        let now = std::time::Instant::now();
        let mut poll = super::Poll::default();
        assert!(poll.ready(now, true, 1));
        poll.finished(now, false);
        for seconds in 0..5 {
            assert!(!poll.ready(now + std::time::Duration::from_secs(seconds), true, 1));
        }
        assert!(poll.ready(now + std::time::Duration::from_secs(6), true, 1));
        poll.finished(now, false);
        assert!(!poll.ready(now + std::time::Duration::from_secs(59), true, 1));
        poll.pending = true;
        assert!(poll.ready(now, true, 1));
        poll.finished(now, true);
        poll.pending = true;
        assert!(!poll.ready(now + std::time::Duration::from_secs(298), true, 1));
        assert!(poll.ready(now + std::time::Duration::from_secs(300), true, 1));
        assert!(poll.ready(now, true, 2));
        assert!(!poll.ready(now, false, 2));
        assert!(poll.ready(now, true, 1));
    }

    #[test]
    fn spoke_custody_undecodable_records_are_quarantined_individually() {
        let raw = serde_json::json!({"result":{"answers":[
            {"envelope":{"key":{"origin_node":"spoke.example","message_id":"bad"},
                "return_binding":{"collection_token":[1,2,3]}}, "remaining_ms":"broken"},
            null
        ]}});
        let batch = super::decode(&raw.to_string()).unwrap();
        assert!(batch.deliveries.is_empty());
        assert_eq!(batch.quarantined.len(), 1);
        assert_eq!(
            batch.quarantined[0].refusal.as_deref(),
            Some("invalid_envelope")
        );
    }

    #[test]
    fn remote_pause_and_backpressure_do_not_spend_the_import_failure_budget() {
        for reason in ["fleet_paused", "mailbox_full", "mail_store_full"] {
            let raw = serde_json::json!({"error": {
                "code": "mesh_collection_refused", "message": reason
            }});
            assert_eq!(super::decode(&raw.to_string()).unwrap_err(), reason);
        }
        assert!(super::decode(r#"{"error":{"message":"invalid envelope"}}"#)
            .unwrap_err()
            .starts_with("mesh collection refused:"));
    }
}
