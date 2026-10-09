//! Held-edge delivery carries an immutable envelope and a decreasing TTL budget.
use super::store::Envelope;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deliver {
    pub envelope: Envelope,
    pub remaining_ms: i64,
}

pub(crate) fn send(peer: &crate::config::PeerConfig, delivery: &Deliver) -> Result<bool, String> {
    let mut delivery = delivery.clone();
    super::hello::with_store(|store| {
        if store.clock().map_err(|e| e.to_string())?.paused {
            return Err("fleet_paused".into());
        }
        let record = store
            .get(&delivery.envelope.key)
            .map_err(|e| e.to_string())?
            .ok_or("message_not_found")?;
        if record.remaining_ms == 0 {
            return Err("message_expired".into());
        }
        delivery.remaining_ms = record.remaining_ms;
        Ok(())
    })?;
    let params = serde_json::to_value(&delivery).map_err(|e| e.to_string())?;
    let raw = crate::peer_stream::request(peer, "mesh.deliver", params)?;
    let response: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if let Some(error) = response.get("error") {
        return Err(error.to_string());
    }
    let result = &response["result"];
    if result["message_key"]
        != serde_json::to_value(&delivery.envelope.key).map_err(|e| e.to_string())?
        || !matches!(
            result["state"].as_str(),
            Some("delivered" | "duplicate" | "custody")
        )
    {
        return Err("mesh peer did not acknowledge durable inbox import".into());
    }
    Ok(result["state"] != "custody")
}

/// Hydrate a metadata notification without opening another custody writer.
/// MCP bridges run in a separate process from the server that owns the store.
pub(crate) fn body(key: Option<&super::key::MessageKey>, legacy: &str) -> Option<String> {
    let Some(key) = key else {
        return Some(legacy.into());
    };
    let connection = rusqlite::Connection::open_with_flags(
        crate::config::state_dir().join("mesh-mail.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT body FROM envelopes WHERE origin=?1 AND id=?2",
            rusqlite::params![key.origin_node, key.message_id],
            |row| row.get(0),
        )
        .ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    payload["message"]["body"].as_str().map(str::to_owned)
}
