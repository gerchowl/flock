//! Mutual possession proof bound to both endpoints, fresh challenges and direction.
use super::{
    identity::NodeIdentity,
    store::{IdentityPin, PinSource, Store},
};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Mutex, OnceLock};

pub const VERSION: u32 = 1;

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

pub(crate) fn version_mismatch(local: u32, remote: u32, peer: &str) -> String {
    let older = if local < remote { "this node" } else { peer };
    format!("mesh version mismatch: local {local}, remote {remote}; upgrade flk on {older}")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub mesh: u32,
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
    pub offer: Offer,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enrollment {
    pub peer: String,
    #[serde(default)]
    pub source: PinSource,
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
            name,
            node_id: identity.node_id(),
            public_key: identity.public_key(),
            nonce,
        })
    }

    pub(crate) fn validate(&self, peer: &str) -> Result<(), String> {
        if self.mesh != version() {
            return Err(version_mismatch(version(), self.mesh, peer));
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

// The store is opened only by real enrollment, never by pure App construction.
// Transactions serialize pin changes across inbound and outbound handshakes.
pub(crate) fn with_store<T>(f: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    static STORE: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
    let mut guard = STORE
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "mesh store poisoned")?;
    if guard.is_none() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis() as i64;
        *guard = Some(
            Store::open(&crate::config::state_dir().join("mesh-mail.sqlite"), now)
                .map_err(|e| e.to_string())?,
        );
    }
    match guard.as_mut() {
        Some(store) => f(store),
        None => Err("mesh store unavailable".into()),
    }
}

pub(crate) fn check_pin(
    peer: &str,
    offer: &Offer,
    source: PinSource,
    save: bool,
) -> Result<(), String> {
    with_store(|store| {
        if let Some(name) = store
            .conflicting_pin_name(peer, &offer.pin())
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
            store
                .put_pin_from(source, peer, &offer.pin())
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
            name: name.into(),
            node_id: Sha256::digest(public_key)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            public_key,
            nonce: [nonce; 32],
        }
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
            "mesh version mismatch: local 1, remote 99; upgrade flk on this node"
        );
        assert_eq!(
            version_mismatch(99, 1, "configured.test"),
            "mesh version mismatch: local 99, remote 1; upgrade flk on configured.test"
        );
        remote.mesh = VERSION;
        remote.node_id = "forged".into();
        assert!(remote.validate("configured.test").is_err());
    }
}
