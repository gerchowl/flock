//! Mutual possession proof bound to both endpoints, fresh challenges and direction.
use super::{
    identity::NodeIdentity,
    store::{IdentityPin, PinSource},
};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Version 5 adds the optional `OutboundAck::receipt` and the `refused`
/// receipt state. Version 4 nodes deny unknown ack fields and would reject
/// whole batches, so hello refuses the mismatch instead.
pub const VERSION: u32 = 5;

/// Previous mesh version supported by this build, floored at the rolling base.
pub const MIN_VERSION: u32 = if VERSION > 5 { VERSION - 1 } else { 5 };

pub(crate) fn version() -> u32 {
    if cfg!(debug_assertions) {
        if let Ok(value) = std::env::var("FLOCK_TEST_MESH_VERSION") {
            if let Ok(version) = value.parse() {
                return version;
            }
        }
    }
    VERSION
}

/// Compile-time support window, with debug-only overrides for mixed-build probes.
pub(crate) fn window() -> (u32, u32) {
    let max = version();
    let min = if cfg!(debug_assertions) {
        std::env::var("FLOCK_TEST_MESH_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
    } else {
        None
    };
    let default_min = if max == VERSION {
        MIN_VERSION
    } else {
        max.saturating_sub(1).max(5).min(max)
    };
    (min.unwrap_or(default_min), max)
}

/// Permanent refusal naming both ranges and the endpoint that needs upgrading.
pub(crate) fn version_mismatch(local: (u32, u32), remote: (u32, u32), peer: &str) -> String {
    let older = if local.1 < remote.1 {
        "this node"
    } else {
        peer
    };
    format!(
        "mesh version mismatch: local {}-{}, remote {}-{}; upgrade flk on {older}",
        local.0, local.1, remote.0, remote.1
    )
}

/// Compute the session version solely from the two signed offers.
pub(crate) fn negotiate(local: &Offer, remote: &Offer, peer: &str) -> Result<u32, String> {
    let local_range = local.range()?;
    let remote_range = remote.range()?;
    let version = local_range.1.min(remote_range.1);
    if version < local_range.0.max(remote_range.0) {
        return Err(version_mismatch(local_range, remote_range, peer));
    }
    Ok(version)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub mesh: u32,
    /// Signed minimum supported version. Omitted for a single-version window
    /// to preserve the pre-negotiation transcript byte for byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_min: Option<u32>,
    pub name: String,
    pub node_id: String,
    pub public_key: [u8; 32],
    pub nonce: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum Hello {
    Check,
    Begin { offer: Offer },
    Finish { signature: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenge {
    /// Unsigned, redundant diagnostic. Both sides select from signed offers.
    /// Only a pre-negotiation VERSION 5 offer may omit this report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated: Option<u32>,
    pub offer: Offer,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enrollment {
    /// Version of the current held-edge session, never persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated: Option<u32>,
    pub peer: String,
    #[serde(default)]
    pub source: PinSource,
    #[serde(default)]
    pub pin_origin: super::store::PinOrigin,
    pub node_id: Option<String>,
    pub state: String,
    pub reason: Option<String>,
}

pub(crate) struct Pending {
    pub pid: u32,
    pub process_started: u64,
    pub started: std::time::Instant,
    pub dialer: Offer,
    pub acceptor: Offer,
}

impl Offer {
    pub(crate) fn new(identity: &NodeIdentity, name: String) -> Result<Self, String> {
        let mut nonce = [0; 32];
        getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
        Ok(Self {
            mesh: version(),
            mesh_min: (window().0 != version()).then_some(window().0),
            name,
            node_id: identity.node_id(),
            public_key: identity.public_key(),
            nonce,
        })
    }

    pub(crate) fn validate(&self, peer: &str) -> Result<(), String> {
        let remote = self.range()?;
        let local = window();
        if local.1.min(remote.1) < local.0.max(remote.0) {
            return Err(version_mismatch(local, remote, peer));
        }
        let id: String = Sha256::digest(self.public_key)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if id != self.node_id
            || self.name.is_empty()
            || self.name.len() > 255
            || self.name.chars().any(char::is_control)
        {
            return Err("invalid mesh identity".into());
        }
        Ok(())
    }

    /// Decode and validate a signed offer's closed support range.
    pub(crate) fn range(&self) -> Result<(u32, u32), String> {
        let min = self.mesh_min.unwrap_or(self.mesh);
        if min == 0 || min > self.mesh {
            return Err(format!(
                "mesh version mismatch: invalid range {min}-{}",
                self.mesh
            ));
        }
        Ok((min, self.mesh))
    }

    pub(crate) fn pin(&self) -> IdentityPin {
        IdentityPin {
            node_id: self.node_id.clone(),
            public_key: self.public_key.to_vec(),
        }
    }
}

fn transcript(dialer: &Offer, acceptor: &Offer, role: &str) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&("flock-mesh-hello-v1", role, dialer, acceptor)).map_err(|e| e.to_string())
}

pub(crate) fn sign(
    identity: &NodeIdentity,
    dialer: &Offer,
    acceptor: &Offer,
    role: &str,
) -> Result<Vec<u8>, String> {
    Ok(identity.sign(&transcript(dialer, acceptor, role)?))
}

pub(crate) fn verify(
    dialer: &Offer,
    acceptor: &Offer,
    role: &str,
    signature: &[u8],
) -> Result<(), String> {
    let signer = if role == "dialer" { dialer } else { acceptor };
    let key = VerifyingKey::from_bytes(&signer.public_key).map_err(|e| e.to_string())?;
    let signature = Signature::from_slice(signature).map_err(|_| "invalid mesh signature")?;
    key.verify_strict(&transcript(dialer, acceptor, role)?, &signature)
        .map_err(|_| "invalid mesh signature".into())
}

impl Challenge {
    /// Authenticate the offers, compute locally, then cross-check the unsigned report.
    pub(crate) fn verify(&self, dialer: &Offer, peer: &str) -> Result<u32, String> {
        verify(dialer, &self.offer, "acceptor", &self.signature)?;
        let version = negotiate(dialer, &self.offer, peer)?;
        match self.negotiated {
            Some(reported) if reported == version => Ok(version),
            None if self.offer.mesh == 5 && self.offer.mesh_min.is_none() => Ok(version),
            _ => Err("mesh version mismatch: missing or disagreeing reported version".into()),
        }
    }
}

pub(crate) use super::runtime_store::with_store;

pub(crate) fn check_pin(
    peer: &str,
    offer: &Offer,
    source: PinSource,
    save: bool,
) -> Result<(), String> {
    check_pin_guarded(peer, offer, source, save, false)
}

pub(crate) fn check_inbound_pin(
    offer: &Offer,
    save: bool,
    configured_name: bool,
) -> Result<(), String> {
    check_pin_guarded(
        &offer.name,
        offer,
        PinSource::Inbound,
        save,
        configured_name,
    )
}

fn check_pin_guarded(
    peer: &str,
    offer: &Offer,
    source: PinSource,
    save: bool,
    configured_name: bool,
) -> Result<(), String> {
    with_store(|store| {
        if configured_name {
            match store.get_pin(peer).map_err(|e| e.to_string())? {
                Some(pin) if pin != offer.pin() => return Err(format!("impersonation of configured peer {peer}: presented node {}, configured node {}", offer.node_id, pin.node_id)),
                Some(_) | None => {}
            }
        }
        if let Some(name) = store
            .conflicting_pin_name(source, peer, &offer.pin())
            .map_err(|e| e.to_string())?
        {
            return Err(format!("node {} is enrolled as {name}", offer.node_id));
        }
        let direction = if source == PinSource::Inbound {
            " --direction inbound"
        } else {
            ""
        };
        if store
            .get_pin_from(source, peer)
            .map_err(|e| e.to_string())?
            .is_some_and(|pin| pin != offer.pin())
        {
            return Err(format!("identity changed for {peer}: possible impersonation or re-key; run flk peers enroll --reset {peer}{direction}"));
        }
        if save {
            if configured_name {
                store.put_inbound_configured_pin(peer, &offer.pin())
            } else {
                store.put_pin_from(source, peer, &offer.pin())
            }
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn offer(key: &SigningKey, name: &str, nonce: u8) -> Offer {
        let public_key = key.verifying_key().to_bytes();
        Offer {
            mesh: VERSION,
            mesh_min: None,
            name: name.into(),
            node_id: Sha256::digest(public_key)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            public_key,
            nonce: [nonce; 32],
        }
    }

    /// The exact offer type and transcript construction used before negotiation.
    #[derive(Clone, Serialize, Deserialize)]
    struct OldOffer {
        mesh: u32,
        name: String,
        node_id: String,
        public_key: [u8; 32],
        nonce: [u8; 32],
    }

    fn old(offer: &Offer) -> OldOffer {
        serde_json::from_value(serde_json::to_value(offer).unwrap()).unwrap()
    }

    fn old_transcript(dialer: &OldOffer, acceptor: &OldOffer, role: &str) -> Vec<u8> {
        serde_json::to_vec(&("flock-mesh-hello-v1", role, dialer, acceptor)).unwrap()
    }

    fn ranged(min: u32, max: u32, seed: u8) -> Offer {
        let mut offer = offer(&SigningKey::from_bytes(&[seed; 32]), "peer.test", seed);
        offer.mesh = max;
        offer.mesh_min = (min != max).then_some(min);
        offer
    }

    fn challenge(dialer: &Offer, acceptor: Offer, report: Option<u32>) -> Challenge {
        let key = SigningKey::from_bytes(&[2; 32]);
        let signature = key
            .sign(&transcript(dialer, &acceptor, "acceptor").unwrap())
            .to_bytes()
            .to_vec();
        Challenge {
            offer: acceptor,
            signature,
            negotiated: report,
        }
    }

    #[test]
    fn singleton_offer_and_transcripts_equal_real_old_wire_shape() {
        let dialer = ranged(5, 5, 1);
        let acceptor = ranged(5, 5, 2);
        assert!(serde_json::to_value(&dialer)
            .unwrap()
            .get("mesh_min")
            .is_none());
        assert_eq!(
            serde_json::to_vec(&dialer).unwrap(),
            serde_json::to_vec(&old(&dialer)).unwrap()
        );
        for role in ["dialer", "acceptor"] {
            assert_eq!(
                transcript(&dialer, &acceptor, role).unwrap(),
                old_transcript(&old(&dialer), &old(&acceptor), role)
            );
        }
    }

    #[test]
    fn real_old_offer_proofs_interoperate_in_both_dial_directions() {
        let dialer = ranged(5, 5, 1);
        let acceptor = ranged(5, 5, 2);
        let old_dialer = old(&dialer);
        let old_acceptor = old(&acceptor);
        // Old acceptor signs the old shape and omits the reported value.
        let signature = SigningKey::from_bytes(&[2; 32])
            .sign(&old_transcript(&old_dialer, &old_acceptor, "acceptor"))
            .to_bytes()
            .to_vec();
        let wire = serde_json::json!({"offer":old_acceptor,"signature":signature});
        let reply: Challenge = serde_json::from_value(wire).unwrap();
        assert_eq!(reply.verify(&dialer, "peer.test").unwrap(), 5);
        // Old dialer reserializes the new offer through its old type.
        let new_reply = challenge(&dialer, acceptor.clone(), Some(5));
        let key = VerifyingKey::from_bytes(&old_acceptor.public_key).unwrap();
        key.verify_strict(
            &old_transcript(&old_dialer, &old_acceptor, "acceptor"),
            &Signature::from_slice(&new_reply.signature).unwrap(),
        )
        .unwrap();
        let finish = SigningKey::from_bytes(&[1; 32])
            .sign(&old_transcript(&old_dialer, &old_acceptor, "dialer"))
            .to_bytes();
        verify(&dialer, &acceptor, "dialer", &finish).unwrap();
        assert_eq!(negotiate(&acceptor, &dialer, "peer.test").unwrap(), 5);
    }

    #[test]
    fn overlapping_windows_compute_identically_in_both_directions() {
        for (a, b, expected) in [((5, 5), (5, 6), 5), ((5, 6), (6, 7), 6)] {
            let local = ranged(a.0, a.1, 1);
            let remote = ranged(b.0, b.1, 2);
            assert_eq!(negotiate(&local, &remote, "peer.test").unwrap(), expected);
            assert_eq!(negotiate(&remote, &local, "peer.test").unwrap(), expected);
            assert_eq!(
                challenge(&local, remote, Some(expected))
                    .verify(&local, "peer.test")
                    .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn disjoint_and_invalid_ranges_are_permanent_and_name_the_older_side() {
        for (a, b, older) in [((5, 5), (7, 8), "this node"), ((6, 7), (5, 5), "peer.test")] {
            assert_eq!(
                negotiate(&ranged(a.0, a.1, 1), &ranged(b.0, b.1, 2), "peer.test").unwrap_err(),
                format!(
                    "mesh version mismatch: local {}-{}, remote {}-{}; upgrade flk on {older}",
                    a.0, a.1, b.0, b.1
                )
            );
        }
        for (min, max) in [(0, 5), (7, 6), (0, 0)] {
            let err = negotiate(&ranged(5, 6, 1), &ranged(min, max, 2), "peer.test").unwrap_err();
            assert!(
                err.starts_with("mesh version mismatch: invalid range"),
                "{err}"
            );
        }
        assert_eq!(ranged(6, 6, 2).range().unwrap(), (6, 6));
    }

    #[test]
    fn stripping_or_editing_signed_ranges_fails_possession_proof() {
        let dialer = ranged(5, 6, 1);
        let reply = challenge(&dialer, ranged(5, 6, 2), Some(6));
        for min in [None, Some(4), Some(6)] {
            let mut changed = reply.clone();
            changed.offer.mesh_min = min;
            assert_eq!(
                changed.verify(&dialer, "peer.test").unwrap_err(),
                "invalid mesh signature"
            );
        }
    }

    #[test]
    fn unsigned_report_cannot_select_or_downgrade_a_session() {
        let dialer = ranged(5, 6, 1);
        let reply = challenge(&dialer, ranged(5, 6, 2), Some(6));
        for report in [None, Some(5), Some(7)] {
            let mut changed = reply.clone();
            changed.negotiated = report;
            assert!(changed
                .verify(&dialer, "peer.test")
                .unwrap_err()
                .starts_with("mesh version mismatch:"));
        }
        assert!(challenge(&ranged(6, 7, 1), ranged(6, 7, 2), None)
            .verify(&ranged(6, 7, 1), "peer.test")
            .is_err());
        let old = challenge(&dialer, ranged(5, 5, 2), None);
        assert_eq!(old.verify(&dialer, "peer.test").unwrap(), 5);
    }

    #[test]
    fn mesh_proofs_bind_both_nodes_nonces_names_and_direction() {
        let key = SigningKey::from_bytes(&[1; 32]);
        let other = SigningKey::from_bytes(&[2; 32]);
        let dialer = offer(&key, "dialer.test", 3);
        let acceptor = offer(&other, "acceptor.test", 4);
        let signature = key
            .sign(&transcript(&dialer, &acceptor, "dialer").unwrap())
            .to_bytes();
        assert!(verify(&dialer, &acceptor, "dialer", &signature).is_ok());
        assert!(verify(&dialer, &acceptor, "acceptor", &signature).is_err());
        let mut changed = acceptor.clone();
        changed.nonce[0] ^= 1;
        assert!(verify(&dialer, &changed, "dialer", &signature).is_err());
        changed = acceptor.clone();
        changed.node_id = dialer.node_id.clone();
        assert!(verify(&dialer, &changed, "dialer", &signature).is_err());
        changed = acceptor.clone();
        changed.name = "impostor.test".into();
        assert!(verify(&dialer, &changed, "dialer", &signature).is_err());
        let mut changed = dialer.clone();
        changed.nonce[0] ^= 1;
        assert!(verify(&changed, &acceptor, "dialer", &signature).is_err());
        assert!(verify(&acceptor, &dialer, "dialer", &signature).is_err());
    }

    #[test]
    fn mesh_version_refusal_names_both_versions_and_configured_peer() {
        let mut remote = offer(&SigningKey::from_bytes(&[2; 32]), "remote.test", 4);
        remote.mesh = 99;
        assert_eq!(
            remote.validate("configured.test").unwrap_err(),
            "mesh version mismatch: local 5-5, remote 99-99; upgrade flk on this node"
        );
        remote.mesh = 4;
        assert_eq!(
            remote.validate("configured.test").unwrap_err(),
            "mesh version mismatch: local 5-5, remote 4-4; upgrade flk on configured.test"
        );
        assert_eq!(
            version_mismatch((99, 99), (1, 1), "configured.test"),
            "mesh version mismatch: local 99-99, remote 1-1; upgrade flk on configured.test"
        );
        remote.mesh = VERSION;
        remote.node_id = "forged".into();
        assert!(remote.validate("configured.test").is_err());
    }
}

#[cfg(test)]
pub(crate) mod legacy;
