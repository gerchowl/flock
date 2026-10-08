//! Per-user node keys. A short file lock serializes bootstrap, not server
//! lifetimes: named sessions and live-handoff processes share one identity.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) struct NodeIdentity {
    key: SigningKey,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredIdentity {
    version: u32,
    machine_binding: String,
    secret_key: [u8; 32],
    public_key: [u8; 32],
}

impl NodeIdentity {
    pub(crate) fn load() -> io::Result<Self> {
        let machine = crate::platform::machine_identity().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot read node identity machine binding: {err}"),
            )
        })?;
        // Bind to the OS user too, without persisting its raw machine id.
        // SAFETY: geteuid has no arguments or memory preconditions.
        let uid = unsafe { libc::geteuid() };
        let binding = format!("flock-node-machine-v1:{uid}:{machine}");
        Self::load_at(&crate::config::state_dir(), &binding)
    }

    pub(crate) fn node_id(&self) -> String {
        digest_hex(self.key.verifying_key().as_bytes())
    }

    fn load_at(state_dir: &Path, machine: &str) -> io::Result<Self> {
        let dir = state_dir.join("mesh");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        // Persist directory creation before publishing an identity within it.
        File::open(state_dir)?.sync_all()?;
        if let Some(parent) = state_dir.parent() {
            File::open(parent)?.sync_all()?;
        }
        let lock = private_file(&dir.join("identity.lock"), true)?;
        lock.lock()?;
        let path = dir.join("identity.json");
        let binding = digest_hex(machine.as_bytes());
        let stored = match private_file(&path, false) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(4097).read_to_end(&mut bytes)?;
                if bytes.len() > 4096 {
                    return Err(invalid("node identity file exceeds 4096 bytes"));
                }
                serde_json::from_slice::<StoredIdentity>(&bytes)
                    .map_err(|_| invalid("invalid node identity file; restore the original identity or create a new identity and re-enroll this node"))?
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut secret_key = [0u8; 32];
                getrandom::fill(&mut secret_key).map_err(io::Error::other)?;
                let key = SigningKey::from_bytes(&secret_key);
                let stored = StoredIdentity {
                    version: 1,
                    machine_binding: binding.clone(),
                    secret_key,
                    public_key: key.verifying_key().to_bytes(),
                };
                // A crash before rename leaves only this unpublished file.
                // The lock makes removing a previous partial write safe.
                let temporary = dir.join("identity.json.tmp");
                match fs::remove_file(&temporary) {
                    Ok(()) => (),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => (),
                    Err(err) => return Err(err),
                }
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temporary)?;
                file.write_all(&serde_json::to_vec(&stored).map_err(io::Error::other)?)?;
                file.sync_all()?;
                fs::rename(&temporary, &path)?;
                stored
            }
            Err(err) => return Err(err),
        };
        if stored.version != 1 {
            return Err(invalid("unsupported node identity file version"));
        }
        if stored.machine_binding != binding {
            return Err(invalid(&format!(
                "cloned node identity refused at {}: identity belongs to another machine or user; move the copied mesh/identity.json aside to generate a new identity, then re-enroll this node with its peers",
                path.display()
            )));
        }
        let key = SigningKey::from_bytes(&stored.secret_key);
        if key.verifying_key().to_bytes() != stored.public_key {
            return Err(invalid(
                "node identity public key does not match its private key",
            ));
        }
        // Also covers a prior startup interrupted after rename but before fsync.
        File::open(&dir)?.sync_all()?;
        Ok(Self { key })
    }
}

fn private_file(path: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "node identity requires a regular file with mode 0600: {}",
                path.display()
            ),
        ));
    }
    Ok(file)
}

fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new() -> Self {
            Self(crate::test_support::unique_temp_path("mesh-identity"))
        }
        fn load(&self, machine: &str) -> io::Result<NodeIdentity> {
            NodeIdentity::load_at(&self.0, machine)
        }
        fn path(&self) -> std::path::PathBuf {
            self.0.join("mesh/identity.json")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn generation_is_idempotent_and_survives_reopen() {
        let fixture = Fixture::new();
        let original = fixture.load("machine-a").unwrap();
        let id = original.node_id();
        let bytes = fs::read(fixture.path()).unwrap();
        let stored: StoredIdentity = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(id, digest_hex(&stored.public_key));
        drop(original);
        assert_eq!(fixture.load("machine-a").unwrap().node_id(), id);
        assert_eq!(fs::read(fixture.path()).unwrap(), bytes);
        assert_eq!(id.len(), 64);
    }

    #[test]
    fn independently_generated_keys_have_distinct_ids() {
        let a = Fixture::new();
        let b = Fixture::new();
        assert_ne!(
            a.load("machine-a").unwrap().node_id(),
            b.load("machine-a").unwrap().node_id()
        );
    }

    #[test]
    fn copied_identity_is_refused_on_another_machine() {
        let original = Fixture::new();
        original.load("machine-a").unwrap();
        let clone = Fixture::new();
        clone.load("machine-b").unwrap();
        fs::copy(original.path(), clone.path()).unwrap();
        let bytes = fs::read(clone.path()).unwrap();
        let err = clone.load("machine-b").err().unwrap();
        assert!(err.to_string().contains("cloned node identity refused"));
        assert!(err.to_string().contains("re-enroll"));
        assert_eq!(fs::read(clone.path()).unwrap(), bytes);
    }

    #[test]
    fn identity_and_lock_are_private_and_loose_permissions_are_refused() {
        let fixture = Fixture::new();
        fixture.load("machine-a").unwrap();
        for path in [fixture.path(), fixture.0.join("mesh/identity.lock")] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            fixture.load("machine-a").err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn concurrent_bootstrap_publishes_one_key() {
        let fixture = Fixture::new();
        let ids = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| fixture.load("machine-a").unwrap().node_id()))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(ids.iter().all(|id| id == &ids[0]));
    }

    #[test]
    fn corrupt_identity_is_never_silently_replaced() {
        let fixture = Fixture::new();
        fixture.load("machine-a").unwrap();
        fs::write(fixture.path(), b"broken").unwrap();
        assert!(fixture.load("machine-a").is_err());
        assert_eq!(fs::read(fixture.path()).unwrap(), b"broken");
    }

    #[test]
    fn mismatched_keypair_is_refused() {
        let fixture = Fixture::new();
        fixture.load("machine-a").unwrap();
        let mut stored: StoredIdentity =
            serde_json::from_slice(&fs::read(fixture.path()).unwrap()).unwrap();
        stored.public_key[0] ^= 1;
        fs::write(fixture.path(), serde_json::to_vec(&stored).unwrap()).unwrap();
        assert!(fixture
            .load("machine-a")
            .err()
            .unwrap()
            .to_string()
            .contains("does not match"));
    }

    #[test]
    fn identity_symlink_is_refused() {
        let fixture = Fixture::new();
        fixture.load("machine-a").unwrap();
        let saved = fixture.0.join("saved.json");
        fs::rename(fixture.path(), &saved).unwrap();
        std::os::unix::fs::symlink(saved, fixture.path()).unwrap();
        assert!(fixture.load("machine-a").is_err());
    }
}
