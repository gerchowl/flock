//! Held-edge delivery carries an immutable envelope and a decreasing TTL budget.
use super::store::Envelope;
use crate::peers::PeerMessageFailure;
use serde::{Deserialize, Serialize};

/// Maximum concurrent custody pushes and records admitted to one retry batch.
pub(crate) const PUSH_CONCURRENCY: usize = 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deliver {
    pub envelope: Envelope,
    pub remaining_ms: i64,
}

pub(crate) fn send(
    peer: &crate::config::PeerConfig,
    delivery: &Deliver,
) -> Result<bool, PeerMessageFailure> {
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
    })
    .map_err(PeerMessageFailure::Unreachable)?;
    let params =
        serde_json::to_value(&delivery).map_err(|e| PeerMessageFailure::Refused(e.to_string()))?;
    let raw = crate::peer_stream::request(peer, "mesh.deliver", params)
        .map_err(PeerMessageFailure::Unreachable)?;
    let response: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| PeerMessageFailure::Unreachable(e.to_string()))?;
    if let Some(error) = response.get("error") {
        return Err(delivery_failure(error));
    }
    let result = &response["result"];
    if result["message_key"]
        != serde_json::to_value(&delivery.envelope.key)
            .map_err(|e| PeerMessageFailure::Refused(e.to_string()))?
        || !matches!(
            result["state"].as_str(),
            Some("delivered" | "duplicate" | "custody")
        )
    {
        return Err(PeerMessageFailure::Unreachable(
            "mesh peer did not acknowledge durable inbox import".into(),
        ));
    }
    Ok(result["state"] != "custody")
}

fn delivery_failure(error: &serde_json::Value) -> PeerMessageFailure {
    let reason = error["message"].as_str().unwrap_or("unknown refusal");
    if error["code"] == "reply_unavailable" {
        // The receiver accepted the question but cannot bind its return path.
        return PeerMessageFailure::Refused(format!("reply_unavailable: {reason}"));
    }
    let permanent = matches!(
        reason.split(':').next().unwrap_or(reason),
        "forward_limit"
            | "msg_not_allowed"
            | "origin_mismatch"
            | "invalid_envelope"
            | "message_not_found"
            | "msg_target_not_found"
    );
    if permanent
        && matches!(
            error["code"].as_str(),
            Some("mesh_delivery_refused" | "origin_mismatch")
        )
    {
        PeerMessageFailure::Refused(reason.into())
    } else {
        PeerMessageFailure::Unreachable(reason.into())
    }
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

#[cfg(test)]
mod tests {
    #[test]
    fn delivery_classifies_permanent_refusals_and_transient_backpressure() {
        let accepted_without_reply = super::delivery_failure(&serde_json::json!({
            "code":"reply_unavailable", "message":"message has no valid mesh return binding"
        }));
        assert!(accepted_without_reply
            .detail()
            .starts_with("reply_unavailable:"));
        for reason in [
            "forward_limit",
            "msg_not_allowed",
            "origin_mismatch",
            "message_not_found",
            "msg_target_not_found: unknown agent",
            "invalid_envelope",
        ] {
            let failure = super::delivery_failure(
                &serde_json::json!({"code":"mesh_delivery_refused","message":reason}),
            );
            assert!(!failure.retryable(), "{reason}");
            assert_eq!(failure.detail(), reason);
        }
        for reason in [
            "mailbox_full",
            "mail_store_full: quota",
            "fleet_paused",
            "mail_store_unavailable: disk",
            "mesh store suspended for handoff",
        ] {
            assert!(
                super::delivery_failure(
                    &serde_json::json!({"code":"mesh_delivery_refused","message":reason})
                )
                .retryable(),
                "{reason}"
            );
        }
    }

    #[test]
    fn unknown_delivery_refusal_remains_retryable() {
        let reason = "mesh delivery requires an authenticated held edge";
        let failure = super::delivery_failure(&serde_json::json!({
            "code":"mesh_delivery_refused", "message":reason
        }));
        assert!(failure.retryable());
        assert_eq!(failure.detail(), reason);
    }
}
