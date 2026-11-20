//! Frozen VERSION 5 hello implementation, independent of negotiation code.
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Offer {
    pub mesh: u32,
    pub name: String,
    pub node_id: String,
    pub public_key: [u8; 32],
    pub nonce: [u8; 32],
}

pub(crate) fn offer(key: &SigningKey, name: &str) -> Offer {
    let public_key = key.verifying_key().to_bytes();
    Offer {
        mesh: 5,
        name: name.into(),
        node_id: Sha256::digest(public_key)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        public_key,
        nonce: [31; 32],
    }
}

pub(crate) fn transcript(dialer: &Offer, acceptor: &Offer, role: &str) -> Vec<u8> {
    serde_json::to_vec(&("flock-mesh-hello-v1", role, dialer, acceptor)).unwrap()
}

pub(crate) fn sign(key: &SigningKey, dialer: &Offer, acceptor: &Offer, role: &str) -> Vec<u8> {
    key.sign(&transcript(dialer, acceptor, role))
        .to_bytes()
        .to_vec()
}

pub(crate) fn validate(offer: &Offer) -> Result<(), String> {
    if offer.mesh != 5 {
        let older = if offer.mesh > 5 {
            "this node"
        } else {
            "peer.test"
        };
        return Err(format!(
            "mesh version mismatch: local 5, remote {}; upgrade flk on {older}",
            offer.mesh
        ));
    }
    Ok(())
}

pub(crate) fn verify(dialer: &Offer, acceptor: &Offer, role: &str, signature: &[u8]) {
    // Pre-negotiation peers validate VERSION before verifying the signature.
    validate(dialer).unwrap();
    validate(acceptor).unwrap();
    let signer = if role == "dialer" { dialer } else { acceptor };
    VerifyingKey::from_bytes(&signer.public_key)
        .unwrap()
        .verify_strict(
            &transcript(dialer, acceptor, role),
            &Signature::from_slice(signature).unwrap(),
        )
        .unwrap();
}
