//! Durable recipient projection binding, ADR-0036 §3 inbox amendment.
use super::{admission::Failure, model::Kind, Item};
use crate::{api::error_code::ErrorCode, mesh::key::MessageKey};

fn failure(error: String) -> Failure {
    crate::logging::mesh_custody_failed("native-inbox-project", "mail_store_unavailable");
    Failure {
        code: ErrorCode::MailStoreUnavailable,
        source: Some(std::io::Error::other(error)),
    }
}
fn key(item: &Item) -> Option<MessageKey> {
    let key: MessageKey = serde_json::from_value(item.metadata.get("message_key")?.clone()).ok()?;
    (item.kind == Kind::Message && item.item_id == key.lifecycle_id()).then_some(key)
}
/// Stamp only a real started transition, before its queue append. The logical
/// custody deadline is persisted in the same item; replay never renews it.
pub(super) fn started(item: &mut Item) -> Result<(), Failure> {
    if key(item).is_none() || item.metadata.contains_key("native_inbox_until") {
        return Ok(());
    }
    let Some(retention) = item
        .metadata
        .get("native_inbox_retention_ms")
        .and_then(|v| v.as_u64())
    else {
        return Ok(());
    };
    let until = crate::mesh::hello::with_store(|s| {
        s.native_inbox_deadline(retention, crate::app::notifications::now_ms() as i64)
            .map_err(|e| e.to_string())
    })
    .map_err(failure)?;
    item.metadata
        .insert("native_inbox_until".into(), until.into());
    Ok(())
}
/// Project explicit committed markers only, including before startup re-hold.
/// Failure fences this owner until restart; the queue marker survives repair.
pub(in crate::structured) fn project(agent: &str, items: &[Item]) -> Result<(), Failure> {
    for item in items {
        let Some(until) = item
            .metadata
            .get("native_inbox_until")
            .and_then(|v| v.as_i64())
        else {
            continue;
        };
        let Some(key) = key(item) else { continue };
        crate::mesh::hello::with_store(|s| {
            s.native_inbox_project(
                &key,
                agent,
                until,
                crate::app::notifications::now_ms() as i64,
            )
            .map_err(|e| e.to_string())
        })
        .map_err(failure)?;
    }
    Ok(())
}
