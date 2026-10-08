//! Per-user node keys. A short file lock serializes bootstrap, not server
//! lifetimes: named sessions and live-handoff processes share one identity.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub(crate) struct NodeIdentity {
    key: SigningKey,
    pub(crate) clone_detection_warning: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredIdentity {
    version: u32,
    machine_binding: Option<String>,
    secret_key: Zeroizing<[u8; 32]>,
    public_key: [u8; 32],
}

impl NodeIdentity {
    pub(crate) fn load() -> io::Result<Self> {
        // Bind to the OS user too, without persisting its raw machine id.
        // SAFETY: geteuid has no arguments or memory preconditions.
        let uid = unsafe { libc::geteuid() };
        let binding = crate::platform::machine_identity()
            .map(|machine| format!("flock-node-machine-v1:{uid}:{machine}"));
        Self::load_at(&crate::config::state_dir(), binding)
    }

    pub(crate) fn node_id(&self) -> String {
        digest_hex(self.key.verifying_key().as_bytes())
    }

    fn load_at(state_dir: &Path, machine: io::Result<String>) -> io::Result<Self> {
        let dir = state_dir.join("mesh");
        create_private_dir(&dir)?;
        let lock = private_file(&dir.join("identity.lock"), true)?;
        lock.lock()?;
        let path = dir.join("identity.json");
        let (binding, unavailable_reason) = match machine {
            Ok(machine) => (Some(digest_hex(machine.as_bytes())), None),
            Err(err) => (None, Some(err.to_string())),
        };
        let stored = match private_file(&path, false) {
            Ok(file) => {
                let mut bytes = Zeroizing::new(Vec::new());
                file.take(4097).read_to_end(&mut bytes)?;
                if bytes.len() > 4096 {
                    return Err(invalid(&format!(
                        "node identity file at {} exceeds 4096 bytes",
                        path.display()
                    )));
                }
                serde_json::from_slice::<StoredIdentity>(&bytes)
                    .map_err(|_| invalid(&format!("invalid node identity file at {}; restore the original identity or create a new identity and re-enroll this node", path.display())))?
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut secret_key = Zeroizing::new([0u8; 32]);
                getrandom::fill(&mut *secret_key).map_err(io::Error::other)?;
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
                let mut file = create_private_file(&temporary)?;
                let bytes = Zeroizing::new(serde_json::to_vec(&stored).map_err(io::Error::other)?);
                file.write_all(&bytes)?;
                file.sync_all()?;
                fs::rename(&temporary, &path)?;
                stored
            }
            Err(err) => return Err(err),
        };
        if stored.version != 1 {
            return Err(invalid(&format!(
                "unsupported node identity file version at {}",
                path.display()
            )));
        }
        if matches!((&stored.machine_binding, &binding), (Some(stored), Some(current)) if stored != current)
        {
            return Err(invalid(&format!(
                "cloned node identity refused at {}: identity belongs to another machine or user; move the copied mesh/identity.json aside to generate a new identity, then re-enroll this node with its peers",
                path.display()
            )));
        }
        let key = SigningKey::from_bytes(&stored.secret_key);
        if key.verifying_key().to_bytes() != stored.public_key {
            return Err(invalid(&format!(
                "node identity public key does not match its private key at {}",
                path.display()
            )));
        }
        // Also covers a prior startup interrupted after rename but before fsync.
        File::open(&dir)?.sync_all()?;
        let clone_detection_warning = unavailable_reason
            .or_else(|| {
                stored
                    .machine_binding
                    .is_none()
                    .then(|| "identity was created without a machine binding".into())
            })
            .map(|reason| format!("clone detection unavailable: {reason}"));
        Ok(Self {
            key,
            clone_detection_warning,
        })
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| invalid("node identity directory has no parent"))?;
    create_private_dir(parent)?;
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => (),
        Err(err) => return Err(err),
    }
    File::open(parent)?.sync_all()
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    // fchmod the newly created descriptor: umask must not remove owner write.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    // SAFETY: geteuid has no arguments or memory preconditions.
    validate_private_file(path, &file.metadata()?, unsafe { libc::geteuid() })?;
    Ok(file)
}

fn private_file(path: &Path, create: bool) -> io::Result<File> {
    if create {
        match create_private_file(path) {
            Ok(file) => return Ok(file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => (),
            Err(err) => return Err(err),
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    // SAFETY: geteuid has no arguments or memory preconditions.
    validate_private_file(path, &file.metadata()?, unsafe { libc::geteuid() })?;
    Ok(file)
}

fn validate_private_file(path: &Path, metadata: &fs::Metadata, uid: u32) -> io::Result<()> {
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.uid() != uid
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            format!("node identity requires a regular file owned by the current user with mode 0600: {}", path.display())));
    }
    Ok(())
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
            NodeIdentity::load_at(&self.0, Ok(machine.into()))
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
        let err = fixture.load("machine-a").err().unwrap();
        assert!(err
            .to_string()
            .contains(&fixture.path().display().to_string()));
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

    #[test]
    fn unavailable_binding_preserves_identity_and_only_real_mismatches_are_refused() {
        let fixture = Fixture::new();
        let id = fixture.load("machine-a").unwrap().node_id();
        let unavailable =
            NodeIdentity::load_at(&fixture.0, Err(io::Error::other("machine-id missing"))).unwrap();
        assert_eq!(unavailable.node_id(), id);
        assert_eq!(
            unavailable.clone_detection_warning.as_deref(),
            Some("clone detection unavailable: machine-id missing")
        );
        assert!(fixture.load("machine-b").is_err());
        assert!(fixture
            .load("machine-a")
            .unwrap()
            .clone_detection_warning
            .is_none());

        let unbound = Fixture::new();
        let first =
            NodeIdentity::load_at(&unbound.0, Err(io::Error::other("uninitialized"))).unwrap();
        assert_eq!(
            unbound.load("machine-a").unwrap().node_id(),
            first.node_id()
        );
        assert_eq!(
            unbound.load("machine-b").unwrap().node_id(),
            first.node_id()
        );
        assert!(unbound
            .load("machine-b")
            .unwrap()
            .clone_detection_warning
            .is_some());
    }

    #[test]
    fn foreign_owner_is_refused_even_with_private_permissions() {
        let fixture = Fixture::new();
        fixture.load("machine-a").unwrap();
        let metadata = fs::metadata(fixture.path()).unwrap();
        let err = validate_private_file(&fixture.path(), &metadata, metadata.uid().wrapping_add(1))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("owned by the current user"));
    }

    #[test]
    fn missing_uninitialized_and_boot_only_machine_ids_keep_the_node_running() {
        let fixture = Fixture::new();
        let id = fixture.load("machine-a").unwrap().node_id();
        let path = fixture.0.join("machine-id");
        for contents in [
            None,
            Some(""),
            Some("uninitialized"),
            Some("1234567890abcdef1234567890abcdef"),
        ] {
            if let Some(contents) = contents {
                fs::write(&path, contents).unwrap();
            }
            let machine = crate::platform::machine_id::read_machine_id(&path, 0);
            assert!(machine.is_err());
            let identity = NodeIdentity::load_at(&fixture.0, machine).unwrap();
            assert_eq!(identity.node_id(), id);
            assert!(identity
                .clone_detection_warning
                .unwrap()
                .starts_with("clone detection unavailable: "));
        }
    }
}
