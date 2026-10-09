//! Origin authentication for immutable envelopes, independent of custody routing.
use super::{identity::NodeIdentity, store::Envelope};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

pub fn canonical(envelope: &Envelope) -> Vec<u8> {
    let binding = &envelope.return_binding;
    // All tuple members have infallible JSON representations (no maps or floats).
    serde_json::to_vec(&(
        "flock-mesh-envelope-v1",
        &envelope.key,
        envelope.kind,
        &envelope.sender,
        &envelope.target_agent,
        &envelope.target_session,
        &envelope.correlation_id,
        &envelope.in_reply_to,
        &envelope.request_key,
        &envelope.intent,
        Sha256::digest(&envelope.body).as_slice(),
        (
            &binding.request,
            &binding.recipient_node,
            &binding.collection_peers,
            Sha256::digest(&binding.collection_token).as_slice(),
        ),
    ))
    .expect("envelope canonical tuple is JSON serializable")
}

pub(crate) fn seal(envelope: &mut Envelope, identity: &NodeIdentity) {
    envelope.origin_key = identity.public_key().to_vec();
    envelope.signature = identity.sign(&canonical(envelope));
}

pub fn verify(envelope: &Envelope) -> Result<(), &'static str> {
    let bytes: &[u8; 32] = envelope
        .origin_key
        .as_slice()
        .try_into()
        .map_err(|_| "invalid_signature")?;
    let origin = digest_hex(bytes);
    if origin != envelope.key.origin_node {
        return Err("origin_mismatch");
    }
    let key = VerifyingKey::from_bytes(bytes).map_err(|_| "invalid_signature")?;
    let signature = Signature::from_slice(&envelope.signature).map_err(|_| "invalid_signature")?;
    key.verify_strict(&canonical(envelope), &signature)
        .map_err(|_| "invalid_signature")
}

fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mesh::{
        key::MessageKey,
        store::{Kind, ReturnBinding},
    };
    use ed25519_dalek::{Signer, SigningKey};

    pub(crate) fn signed() -> Envelope {
        let signing = SigningKey::from_bytes(&[7; 32]);
        let origin_key = signing.verifying_key().to_bytes().to_vec();
        let key = MessageKey::mint(digest_hex(&origin_key), 0).unwrap();
        let mut envelope = Envelope {
            key: key.clone(),
            kind: Kind::Message,
            origin_key,
            signature: Vec::new(),
            sender: "sender".into(),
            target_agent: "recipient".into(),
            target_session: "session".into(),
            correlation_id: "thread".into(),
            in_reply_to: None,
            request_key: None,
            intent: "notice".into(),
            body: b"hello".to_vec(),
            return_binding: ReturnBinding {
                request: key,
                recipient_node: "receiver.example".into(),
                collection_token: vec![42; 32],
                collection_peers: vec!["peer.example".into()],
            },
        };
        envelope.signature = signing.sign(&canonical(&envelope)).to_bytes().to_vec();
        envelope
    }

    #[test]
    fn signature_rejects_tampered_body_target_request_key_binding_and_kind() {
        let original = signed();
        assert_eq!(verify(&original), Ok(()));
        let mutations: Vec<fn(&mut Envelope)> = vec![
            |e| e.body.push(1),
            |e| e.target_agent.push('x'),
            |e| e.target_session.push('x'),
            |e| e.request_key = Some(e.key.clone()),
            |e| e.kind = Kind::Receipt,
            |e| e.sender.push('x'),
            |e| e.correlation_id.push('x'),
            |e| e.in_reply_to = Some("other".into()),
            |e| e.intent.push('x'),
            |e| e.return_binding.request.message_id.push('x'),
            |e| e.return_binding.recipient_node.push('x'),
            |e| {
                e.return_binding
                    .collection_peers
                    .push("other.example".into())
            },
            |e| e.return_binding.collection_token[0] ^= 1,
        ];
        for mutate in mutations {
            let mut envelope = original.clone();
            mutate(&mut envelope);
            assert_eq!(verify(&envelope), Err("invalid_signature"));
        }
    }

    #[test]
    fn signature_rejects_key_not_matching_origin_node() {
        let mut envelope = signed();
        envelope.key.origin_node = "other.example".into();
        assert_eq!(verify(&envelope), Err("origin_mismatch"));
        envelope.origin_key.clear();
        assert_eq!(verify(&envelope), Err("invalid_signature"));
    }
}
