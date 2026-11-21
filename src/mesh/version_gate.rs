//! One kind-version lookup for held-edge sends and receives.
use serde_json::Value;

/// Single introduction table shared by command construction and wire validation.
pub(crate) fn introduced(kind: &str) -> Option<u32> {
    match kind {
        "message" | "receipt" => Some(5),
        "command" => Some(6),
        _ => None,
    }
}

/// Validate canonical object-form mesh records before any typed decoding.
/// Envelopes and their delivery, outbound-collection and acknowledgement
/// containers cannot use serde's positional struct sequences. Explicit kinds
/// must be strings, while an absent object tag retains the `message` default.
/// Optional null receipts and collection-result string tokens carry no envelope.
pub(crate) fn check(value: &Value, version: u32) -> Result<(), String> {
    if !value.is_object() {
        return Err("<non-object mesh record>".into());
    }
    walk(value, version, "")
}

/// An older next hop is a command refusal, not a receiver protocol violation.
/// This check runs immediately before the transport writes any request bytes.
pub(crate) fn check_send(value: &Value, version: u32, peer: &str) -> Result<(), String> {
    check(value, version).map_err(|kind| {
        if kind == "command" && introduced(&kind).is_some_and(|minimum| version < minimum) {
            "unsupported_for_agent".into()
        } else {
            violation(peer, version, &kind)
        }
    })
}

/// Enforce the object boundaries of every wire slot that can carry an envelope.
fn walk(value: &Value, version: u32, parent: &str) -> Result<(), String> {
    match value {
        Value::Array(values) => {
            for value in values {
                walk(value, version, parent)?;
            }
        }
        Value::Object(fields) => {
            for (name, value) in fields {
                // Collection results carry string tokens, and optional receipts may be absent.
                let envelope = name == "envelope"
                    || (name == "receipt"
                        && !value.is_null()
                        && !(parent == "result" && value.is_string()));
                if envelope {
                    let fields = value.as_object().ok_or("<non-object envelope>")?;
                    let kind = fields.get("kind").map_or(Some("message"), Value::as_str);
                    let introduced = kind.and_then(introduced);
                    if introduced.is_none_or(|minimum| version < minimum) {
                        return Err(kind.unwrap_or("<invalid>").chars().take(128).collect());
                    }
                }
                // Positional containers must not hide their envelope or receipt fields.
                if (name == "outbound" && !value.is_object())
                    || ((name == "answers" || (name == "ack" && parent == "outbound"))
                        && value
                            .as_array()
                            .is_some_and(|items| items.iter().any(Value::is_array)))
                {
                    return Err("<non-object mesh record>".into());
                }
                walk(value, version, name)?;
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
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_send_gate_refuses_without_writing_to_an_older_next_hop() {
        let frame = json!({"envelope":{"kind":"command"},"elapsed_ms":10});
        let mut writes = Vec::new();
        let send = |version, writes: &mut Vec<Value>| -> Result<(), String> {
            check_send(&frame, version, "peer.example")?;
            writes.push(frame.clone());
            Ok(())
        };
        assert_eq!(send(5, &mut writes), Err("unsupported_for_agent".into()));
        assert!(writes.is_empty());
        assert_eq!(send(6, &mut writes), Ok(()));
        assert_eq!(writes, vec![frame]);
    }

    #[test]
    fn command_requires_six_and_decodes_at_six() {
        let mut envelope = serde_json::to_value(crate::mesh::sign::tests::signed()).unwrap();
        envelope["kind"] = json!("command");
        let frame = json!({"envelope":envelope});
        assert_eq!(check(&frame, 5), Err("command".into()));
        assert_eq!(check(&frame, 6), Ok(()));
        assert!(serde_json::from_value::<crate::mesh::store::Envelope>(envelope).is_ok());
        assert_eq!(crate::mesh::hello::VERSION, 5);
        assert_eq!(crate::mesh::hello::MIN_VERSION, 5);
    }

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

    pub(crate) fn sequence_envelope() -> Value {
        let envelope = serde_json::to_value(crate::mesh::sign::tests::signed()).unwrap();
        let fields = [
            "kind",
            "origin_key",
            "signature",
            "key",
            "sender",
            "target_agent",
            "target_session",
            "correlation_id",
            "in_reply_to",
            "request_key",
            "return_binding",
            "intent",
            "body",
        ];
        Value::Array(
            fields
                .iter()
                .map(|name| {
                    envelope.get(*name).cloned().unwrap_or_else(|| {
                        assert_eq!(*name, "kind");
                        json!("message")
                    })
                })
                .collect(),
        )
    }

    pub(crate) fn noncanonical_payloads() -> Vec<(Value, bool)> {
        let sequence = sequence_envelope();
        let envelope = serde_json::to_value(crate::mesh::sign::tests::signed()).unwrap();
        let mut future_sequence = sequence.clone();
        future_sequence[0] = json!("future");
        let mut payloads = Vec::new();
        for value in [
            future_sequence.clone(),
            sequence,
            json!([]),
            Value::Null,
            json!(true),
            json!(42),
            json!("message"),
        ] {
            payloads.push((
                json!({"envelope":value,"remaining_ms":1000,"hops_left":8,"visited":[]}),
                true,
            ));
        }
        for kind in [
            json!("future"),
            json!({"future":null}),
            json!({"message":null}),
            Value::Null,
            json!(true),
            json!(42),
            json!([]),
            json!(["message"]),
        ] {
            let mut value = envelope.clone();
            value["kind"] = kind;
            payloads.push((
                json!({"envelope":value,"remaining_ms":1000,"hops_left":8,"visited":[]}),
                true,
            ));
        }
        // Every envelope-bearing struct can itself arrive in sequence form.
        for value in [envelope.clone(), future_sequence.clone()] {
            payloads.push((json!([value, 1000, 8, []]), true));
            let ack = json!([envelope["key"], [], null, true, value]);
            payloads.push((json!({"outbound":{"ack":[ack]}}), false));
            let outbound = json!([[], [{"key":envelope["key"],"token":[],"refusal":null,"delivered":true,"receipt":value}]]);
            payloads.push((json!({"outbound":outbound}), false));
        }
        payloads.push((
            json!({"outbound":{"ack":[{"receipt":future_sequence}]}}),
            false,
        ));
        payloads
    }

    #[test]
    fn all_sequence_record_forms_are_refused_before_decode() {
        use crate::mesh::{
            collect::{Collect, OutboundAck, OutboundCollect},
            delivery::Deliver,
            store::{Envelope, Kind},
        };
        let sequence = sequence_envelope();
        let envelope: Envelope = serde_json::from_value(sequence.clone()).unwrap();
        assert_eq!(envelope.kind, Kind::Message);
        let object = serde_json::to_value(&envelope).unwrap();
        let delivery_sequence = json!([object, 1000, 8, []]);
        serde_json::from_value::<Deliver>(delivery_sequence.clone()).unwrap();
        let ack_sequence = json!([object["key"], [], null, true, object]);
        serde_json::from_value::<OutboundAck>(ack_sequence.clone()).unwrap();
        let outbound_sequence = json!([[], [ack_sequence]]);
        serde_json::from_value::<OutboundCollect>(outbound_sequence.clone()).unwrap();
        let collect = json!({"outbound":outbound_sequence});
        serde_json::from_value::<Collect>(collect.clone()).unwrap();
        for value in [json!({"envelope":sequence}), delivery_sequence, collect] {
            assert!(check(&value, 5).is_err(), "{value}");
        }
    }

    #[test]
    fn noncanonical_forms_and_tag_variants_cannot_bypass_the_gate() {
        for (payload, _) in noncanonical_payloads() {
            assert!(check(&payload, 5).is_err(), "{payload}");
        }
        let envelope = serde_json::to_value(crate::mesh::sign::tests::signed()).unwrap();
        assert!(envelope.get("kind").is_none());
        let decoded: crate::mesh::store::Envelope =
            serde_json::from_value(envelope.clone()).unwrap();
        assert_eq!(decoded.kind, crate::mesh::store::Kind::Message);
        assert!(check(&json!({"envelope":envelope}), 5).is_ok());
        serde_json::from_value::<crate::mesh::store::Kind>(json!({"message":null})).unwrap();
        assert!(check(
            &json!({"outbound":{"ack":[{"receipt":null}]},"result":{"receipt":"token"}}),
            5
        )
        .is_ok());
    }
}
