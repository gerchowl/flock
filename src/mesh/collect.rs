//! Direct-edge collection runs off the app loop. Import precedes acknowledgement.
use super::{delivery::Deliver, key::MessageKey};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Collect {
    pub request: MessageKey,
    pub token: Vec<u8>,
    #[serde(default)]
    pub ack: Vec<MessageKey>,
}

#[derive(Debug)]
pub(crate) struct Completion {
    pub peer: crate::config::PeerConfig,
    pub query: Collect,
    pub result: Result<Vec<Deliver>, String>,
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

fn fetch(peer: &crate::config::PeerConfig, query: &Collect) -> Result<Vec<Deliver>, String> {
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
    let response: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if response.get("error").is_some() {
        return Err("mesh collection refused".into());
    }
    let answers: Vec<Deliver> =
        serde_json::from_value(response["result"]["answers"].clone()).map_err(|e| e.to_string())?;
    if answers.len() > 16 {
        return Err("mesh collection batch too large".into());
    }
    Ok(answers)
}
