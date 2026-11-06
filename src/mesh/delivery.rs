//! Held-edge delivery carries an immutable envelope and a decreasing TTL budget.
use super::store::Envelope;
use crate::peers::PeerMessageFailure;
use serde::{Deserialize, Serialize};

/// Maximum concurrent custody pushes and records admitted to one retry batch.
pub(crate) const PUSH_CONCURRENCY: usize = 4;

pub(crate) fn push_concurrency() -> usize {
    if cfg!(debug_assertions) {
        if let Some(limit) = std::env::var("FLOCK_TEST_MESH_PUSH_CONCURRENCY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|limit| *limit > 0)
        {
            return limit;
        }
    }
    PUSH_CONCURRENCY
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deliver {
    pub envelope: Envelope,
    pub remaining_ms: i64,
    pub hops_left: u8,
    pub visited: Vec<String>,
}

/// Durable transport acceptance and the recipient's optional committed queue ID.
/// Keeping both in one value prevents intermediate relays from dropping the ID.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Acknowledged {
    /// True for inbox import, false for transferred custody.
    pub delivered: bool,
    /// Present only after the destination queue owner committed admission.
    pub item_id: Option<String>,
}

/// Push one delivery, retaining any signed terminal refusal receipt (#872).
pub(crate) fn send(
    peer: &crate::config::PeerConfig,
    delivery: &Deliver,
) -> (Result<Acknowledged, PeerMessageFailure>, Option<Envelope>) {
    let mut receipt = None;
    let result = push(peer, delivery, &mut receipt);
    (result, receipt)
}

fn push(
    peer: &crate::config::PeerConfig,
    delivery: &Deliver,
    receipt: &mut Option<Envelope>,
) -> Result<Acknowledged, PeerMessageFailure> {
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
        let failure = delivery_failure(error);
        if matches!(failure, PeerMessageFailure::Refused(_)) {
            *receipt = serde_json::from_value(error["data"]["receipt"].clone()).ok();
        }
        return Err(failure);
    }
    acknowledgement(&response["result"], &delivery.envelope.key)
}

fn acknowledgement(
    result: &serde_json::Value,
    key: &super::key::MessageKey,
) -> Result<Acknowledged, PeerMessageFailure> {
    if result["message_key"]
        != serde_json::to_value(key).map_err(|e| PeerMessageFailure::Refused(e.to_string()))?
        || !matches!(
            result["state"].as_str(),
            Some("delivered" | "duplicate" | "custody")
        )
    {
        return Err(PeerMessageFailure::Unreachable(
            "mesh peer did not acknowledge durable inbox import".into(),
        ));
    }
    let id = match result.get("item_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(id)) if *id == key.lifecycle_id() => Some(id.clone()),
        _ => {
            return Err(PeerMessageFailure::Unreachable(
                "invalid queue lifecycle acknowledgement".into(),
            ))
        }
    };
    Ok(Acknowledged {
        delivered: result["state"] != "custody",
        item_id: id,
    })
}

fn delivery_failure(error: &serde_json::Value) -> PeerMessageFailure {
    let reason = error["message"].as_str().unwrap_or("unknown refusal");
    if error["code"] == "reply_unavailable" {
        // The receiver accepted the question but cannot bind its return path.
        return PeerMessageFailure::Refused(format!("reply_unavailable: {reason}"));
    }
    let terminal = error["code"]
        .as_str()
        .and_then(crate::api::error_code::ErrorCode::from_code)
        .filter(|code| {
            permanent_refusal(code.as_str())
                && *code != crate::api::error_code::ErrorCode::MeshDeliveryRefused
                && matches!(
                    code.retry(),
                    crate::cli::exit::Retry::Never
                        | crate::cli::exit::Retry::AfterHuman
                        | crate::cli::exit::Retry::AfterHandoff
                )
        });
    if let Some(code) = terminal {
        return PeerMessageFailure::Refused(format!("{}: {reason}", code.as_str()));
    }
    let reason_code = reason.split(':').next().unwrap_or(reason);
    if matches!(reason_code, "loop_detected" | "hop_budget_exhausted") {
        return PeerMessageFailure::Reroute(reason.into());
    }
    let permanent = permanent_refusal(reason);
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

pub(crate) fn permanent_refusal(reason: &str) -> bool {
    let code = reason.split(':').next().unwrap_or(reason);
    // Legacy lookup misses and route failures keep their existing recovery policy.
    if matches!(
        code,
        "message_not_found" | "msg_target_not_found" | "loop_detected" | "hop_budget_exhausted"
    ) {
        return false;
    }
    matches!(
        code,
        "recipient_gone"
            | "msg_not_allowed"
            | "origin_mismatch"
            | "invalid_signature"
            | "invalid reply binding"
            | "invalid_envelope"
            | "message_key_conflict"
    ) || crate::api::error_code::ErrorCode::from_code(code).is_some_and(|code| {
        matches!(
            code.retry(),
            crate::cli::exit::Retry::Never
                | crate::cli::exit::Retry::AfterHuman
                | crate::cli::exit::Retry::AfterHandoff
        )
    })
}

pub(crate) fn hop_limit() -> u8 {
    if cfg!(debug_assertions) {
        if let Some(limit) = std::env::var("FLOCK_TEST_MESH_HOP_LIMIT")
            .ok()
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|v| *v <= 8)
        {
            return limit;
        }
    }
    8
}

/// How long a hub keeps custody whose only onward route crosses a node the
/// mail already visited, before it reports the mail undeliverable (#928).
/// Long enough for a flapping edge to come back and for withdrawals to
/// settle, far short of the custody deadline the sender would wait out.
pub(crate) fn loop_grace() -> std::time::Duration {
    if cfg!(debug_assertions) {
        if let Some(ms) = std::env::var("FLOCK_TEST_MESH_LOOP_GRACE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            return std::time::Duration::from_millis(ms);
        }
    }
    std::time::Duration::from_secs(30)
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
    fn custody_completion_transport_obeys_typed_queue_refusals() {
        for code in [
            "queue_item_too_large",
            "queue_item_conflict",
            "message_expired",
            "recipient_gone",
        ] {
            let failure = super::delivery_failure(
                &serde_json::json!({"code":code,"message":"message admission refused"}),
            );
            assert!(!failure.retryable(), "{code}");
            assert!(super::permanent_refusal(&format!("{code}: owner refusal")));
            assert!(failure.detail().starts_with(code));
        }
        for code in ["input_write_unknown", "session_busy", "queue_source_cold"] {
            assert!(
                super::delivery_failure(&serde_json::json!({"code":code,"message":"waiting"}))
                    .retryable(),
                "{code}"
            );
        }
    }
    #[test]
    fn custody_completion_peer_lookup_misses_stay_retryable() {
        for code in ["message_not_found", "msg_target_not_found"] {
            let failure = super::delivery_failure(&serde_json::json!({
                "code": code, "message": "recipient lookup is not ready"
            }));
            assert!(failure.retryable(), "{code}: {failure:?}");
        }
    }
    #[test]
    fn queue_lifecycle_acknowledgement_is_bound_to_complete_message_key() {
        let key = crate::mesh::sign::tests::signed().key;
        let mut ack =
            serde_json::json!({"message_key":key,"state":"delivered","item_id":key.lifecycle_id()});
        assert_eq!(
            super::acknowledgement(&ack, &key).unwrap(),
            super::Acknowledged {
                delivered: true,
                item_id: Some(key.lifecycle_id())
            }
        );
        ack["item_id"] = serde_json::json!("00000000-0000-4000-8000-000000000001");
        assert!(super::acknowledgement(&ack, &key).is_err());
        ack["item_id"] = serde_json::Value::Null;
        assert_eq!(
            super::acknowledgement(&ack, &key).unwrap(),
            super::Acknowledged {
                delivered: true,
                item_id: None
            }
        );
    }
    #[test]
    fn origin_budget_honors_debug_test_override() {
        let (_lock, _scrub) = crate::config::test_config_env_guard();
        std::env::set_var("FLOCK_TEST_MESH_HOP_LIMIT", "0");
        assert_eq!(super::hop_limit(), 0);
        std::env::remove_var("FLOCK_TEST_MESH_HOP_LIMIT");
        assert_eq!(super::hop_limit(), 8);
    }

    #[test]
    fn delivery_classifies_permanent_refusals_and_transient_backpressure() {
        let accepted_without_reply = super::delivery_failure(&serde_json::json!({
            "code":"reply_unavailable", "message":"message has no valid mesh return binding"
        }));
        assert!(accepted_without_reply
            .detail()
            .starts_with("reply_unavailable:"));
        for reason in [
            "recipient_gone",
            "msg_not_allowed",
            "origin_mismatch",
            "invalid_signature",
            "invalid reply binding",
            "invalid_envelope",
        ] {
            let failure = super::delivery_failure(
                &serde_json::json!({"code":"mesh_delivery_refused","message":reason}),
            );
            assert!(!failure.retryable(), "{reason}");
            assert_eq!(failure.detail(), reason);
        }
        for reason in ["loop_detected", "hop_budget_exhausted"] {
            let failure = super::delivery_failure(
                &serde_json::json!({"code":"mesh_delivery_refused", "message":reason}),
            );
            assert!(matches!(
                failure,
                crate::peers::PeerMessageFailure::Reroute(_)
            ));
        }
        for reason in [
            "message_not_found",
            "msg_target_not_found: unknown agent",
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
