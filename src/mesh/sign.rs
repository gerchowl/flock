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
    fn fixed_dalek_2_envelope_signature_remains_byte_compatible() {
        // Generated with ed25519-dalek 2.2.0 and independently cross-verified
        // with 3.0.0 in both directions using the fixed seed and envelope.
        let public_key = [
            234, 74, 108, 99, 226, 156, 82, 10, 190, 245, 80, 123, 19, 46, 197, 249, 149, 71, 118,
            174, 190, 190, 123, 146, 66, 30, 234, 105, 20, 70, 210, 44,
        ];
        let signature = [
            86, 96, 214, 131, 22, 172, 123, 57, 159, 113, 255, 167, 0, 252, 85, 233, 69, 78, 108,
            193, 85, 173, 13, 246, 44, 147, 83, 3, 10, 26, 178, 97, 20, 26, 166, 232, 108, 136, 29,
            132, 175, 0, 81, 196, 147, 49, 38, 214, 194, 145, 207, 88, 215, 50, 119, 205, 115, 94,
            152, 87, 253, 157, 237, 6,
        ];
        let identity = NodeIdentity::fixture([7; 32]);
        assert_eq!(identity.public_key(), public_key);
        let mut envelope = signed();
        envelope.key.message_id = "00000000000000000000000000".into();
        envelope.return_binding.request = envelope.key.clone();
        envelope.origin_key = public_key.to_vec();
        envelope.signature = signature.to_vec();
        assert_eq!(verify(&envelope), Ok(()));
        seal(&mut envelope, &identity);
        assert_eq!(envelope.signature, signature);
        assert_eq!(verify(&envelope), Ok(()));
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
