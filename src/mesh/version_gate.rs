//! One kind-version lookup for held-edge sends and receives.
use serde_json::Value;

/// Every currently implemented kind is available throughout the support window.
/// Missing tags retain the historical `message` default.
pub(crate) fn check(value: &Value, version: u32) -> Result<(), String> {
    match value {
        Value::Array(values) => {
            for value in values {
                check(value, version)?;
            }
        }
        Value::Object(fields) => {
            for (name, value) in fields {
                if matches!(name.as_str(), "envelope" | "receipt") && value.is_object() {
                    let kind = value.get("kind").map_or(Some("message"), Value::as_str);
                    let introduced = match kind {
                        Some("message" | "receipt") => Some(5),
                        _ => None,
                    };
                    if introduced.is_none_or(|minimum| version < minimum) {
                        return Err(kind.unwrap_or("<invalid>").chars().take(128).collect());
                    }
                }
                check(value, version)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// A permanent, body-free diagnostic shared by both transport directions.
pub(crate) fn violation(peer: &str, version: u32, kind: &str) -> String {
    crate::logging::mesh_protocol_violation(peer, version, kind);
    format!("mesh_protocol_violation: peer {peer}, session VERSION {version}, kind {kind}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_kind_is_message_and_every_current_kind_flows_at_five() {
        for envelope in [
            json!({}),
            json!({"kind":"message"}),
            json!({"kind":"receipt"}),
        ] {
            assert_eq!(check(&json!({"envelope":envelope}), 5), Ok(()));
        }
    }

    #[test]
    fn unknown_or_too_new_kind_refuses_the_entire_batch_before_decode() {
        for (kind, version) in [("future", 5), ("receipt", 4)] {
            let mut answers = vec![json!({"envelope":{"kind":kind}})];
            if version >= 5 {
                answers.insert(0, json!({"envelope":{}}));
            }
            let batch = json!({"result":{"answers":answers}});
            assert_eq!(check(&batch, version), Err(kind.into()));
            let ack = json!({"outbound":{"ack":[{"receipt":{"kind":kind}}]}});
            assert_eq!(check(&ack, version), Err(kind.into()));
        }
        assert_eq!(
            check(&json!({"envelope":{"kind":null}}), 5),
            Err("<invalid>".into())
        );
    }
}
