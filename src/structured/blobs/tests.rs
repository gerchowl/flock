use super::*;
use crate::structured::launch::SessionConfigDir;
use serde_json::json;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration;

const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
fn setup() -> (SessionConfigDir, BlobStore) {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let store = BlobStore::open(root.path()).unwrap();
    (root, store)
}
fn names(root: &SessionConfigDir) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(root.path().join("blobs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}
#[test]
fn hashes_round_trip_and_stat_without_interpreting_content() {
    let (root, store) = setup();
    let reference = store.put(&b"abc"[..], 3).unwrap();
    assert_eq!(
        reference,
        BlobRef {
            blob: ABC.into(),
            size: 3,
            preview: "abc".into()
        }
    );
    assert_eq!(store.get(ABC).unwrap(), b"abc");
    assert_eq!(store.stat(ABC).unwrap(), 3);
    assert_eq!(names(&root), [ABC]);
    assert_eq!(
        fs::metadata(root.path().join("blobs")).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(root.path().join("blobs").join(ABC))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
}
#[test]
fn repeated_and_concurrent_puts_deduplicate_without_temporary_files() {
    let (root, store) = setup();
    let store = Arc::new(store);
    let barrier = Arc::new(Barrier::new(5));
    let (tx, rx) = mpsc::channel();
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (store, barrier, tx) = (store.clone(), barrier.clone(), tx.clone());
            std::thread::spawn(move || {
                barrier.wait();
                tx.send(store.put(&b"abc"[..], 3)).unwrap();
            })
        })
        .collect();
    barrier.wait();
    for _ in 0..4 {
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap()
                .blob,
            ABC
        );
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(store.put(&b"abc"[..], 3).unwrap().blob, ABC);
    assert_eq!(names(&root), [ABC]);
}
#[test]
fn invalid_addresses_missing_corruption_and_symlinks_are_distinct() {
    let (root, store) = setup();
    for hash in [
        "",
        "..",
        "../escape",
        "ABC",
        &"g".repeat(64),
        &"A".repeat(64),
    ] {
        assert!(matches!(store.get(hash), Err(BlobError::InvalidHash)));
        assert!(matches!(store.stat(hash), Err(BlobError::InvalidHash)));
    }
    assert!(matches!(store.get(ABC), Err(BlobError::Missing)));
    assert!(matches!(store.stat(ABC), Err(BlobError::Missing)));
    store.put(&b"abc"[..], 3).unwrap();
    let path = root.path().join("blobs").join(ABC);
    fs::write(&path, b"bad").unwrap();
    assert!(matches!(store.get(ABC), Err(BlobError::Corrupt)));
    assert!(matches!(store.put(&b"abc"[..], 3), Err(BlobError::Corrupt)));
    assert_eq!(
        fs::read(&path).unwrap(),
        b"bad",
        "corruption is never silently repaired"
    );
    assert_eq!(names(&root), [ABC]);
    fs::remove_file(&path).unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, b"abc").unwrap();
    symlink(&outside, &path).unwrap();
    assert!(matches!(store.get(ABC), Err(BlobError::UnsafePath)));
    assert!(matches!(store.stat(ABC), Err(BlobError::UnsafePath)));
    fs::remove_file(path).unwrap();
    fs::remove_dir(root.path().join("blobs")).unwrap();
    symlink(root.path(), root.path().join("blobs")).unwrap();
    assert!(matches!(
        BlobStore::open(root.path()),
        Err(BlobError::UnsafePath)
    ));
}
#[test]
fn per_call_caps_and_reader_failures_clean_up_owned_temporary_files() {
    struct Fails(bool);
    impl Read for Fails {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.0 {
                return Err(io::ErrorKind::NotFound.into());
            }
            self.0 = true;
            buffer[0] = b'a';
            Ok(1)
        }
    }
    let (root, store) = setup();
    assert!(matches!(
        store.put(&b"abc"[..], 2),
        Err(BlobError::TooLarge)
    ));
    assert!(names(&root).is_empty());
    assert!(
        matches!(store.put(Fails(false), 10), Err(BlobError::Io(error)) if error.kind() == io::ErrorKind::NotFound)
    );
    assert!(names(&root).is_empty());
    assert_eq!(store.put(&b"abc"[..], 3).unwrap().blob, ABC);
}
#[test]
fn write_failure_is_io_not_missing_and_preserves_the_data_root() {
    let (root, store) = setup();
    fs::remove_dir(root.path().join("blobs")).unwrap();
    assert!(matches!(store.put(&b"abc"[..], 3), Err(BlobError::Io(_))));
    assert!(root.path().is_dir());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}
#[test]
fn publication_is_atomic_for_concurrent_verified_readers() {
    struct Held {
        started: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
        bytes: io::Cursor<Vec<u8>>,
    }
    impl Read for Held {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if let Some(tx) = self.started.take() {
                tx.send(()).map_err(io::Error::other)?;
                self.release
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(io::Error::other)?;
            }
            self.bytes.read(buffer)
        }
    }
    let (_root, store) = setup();
    let store = Arc::new(store);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let writer = store.clone();
    let worker = std::thread::spawn(move || {
        result_tx
            .send(writer.put(
                Held {
                    started: Some(ready_tx),
                    release: release_rx,
                    bytes: io::Cursor::new(b"abc".to_vec()),
                },
                3,
            ))
            .unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(store.get(ABC), Err(BlobError::Missing)));
    release_tx.send(()).unwrap();
    assert_eq!(
        result_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap()
            .blob,
        ABC
    );
    worker.join().unwrap();
    assert_eq!(store.get(ABC).unwrap(), b"abc");
}
#[test]
fn private_creation_restores_owner_access_under_restrictive_umask() {
    struct Mask(libc::mode_t);
    impl Drop for Mask {
        fn drop(&mut self) {
            // SAFETY: nextest isolates this process-local umask.
            unsafe {
                libc::umask(self.0);
            }
        }
    }
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    // SAFETY: nextest isolates this process-local umask.
    let _mask = Mask(unsafe { libc::umask(0o777) });
    let store = BlobStore::open(root.path()).unwrap();
    let reference = store.put(&b"abc"[..], 3).unwrap();
    assert_eq!(store.get(&reference.blob).unwrap(), b"abc");
    assert_eq!(
        fs::metadata(root.path().join("blobs").join(ABC))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
}
fn block(kind: &str, bytes: usize) -> Value {
    let mut value = json!({"type":kind,"content":""});
    let overhead = serde_json::to_vec(&value).unwrap().len();
    value["content"] = Value::String("x".repeat(bytes - overhead));
    assert_eq!(serde_json::to_vec(&value).unwrap().len(), bytes);
    value
}
#[test]
fn threshold_boundary_and_complete_content_references_resolve() {
    let (_root, store) = setup();
    for kind in ["tool_result", "thinking", "image", "document", "attachment"] {
        for bytes in [2047, 2048, 2049] {
            let original = block(kind, bytes);
            let raw =
                json!({"type":"assistant","message":{"role":"assistant","content":[original]}})
                    .to_string();
            let projected = store
                .project(&raw, StorageConfig::default().blob_min_bytes)
                .unwrap()
                .unwrap();
            if bytes <= 2048 {
                assert_eq!(projected, raw);
            } else {
                let line: Value = serde_json::from_str(&projected).unwrap();
                let reference: BlobRef =
                    serde_json::from_value(line["message"]["content"][0].clone()).unwrap();
                assert_eq!(reference.size, bytes as u64);
                assert!(reference.preview.chars().count() <= 80);
                assert_eq!(
                    serde_json::from_slice::<Value>(&store.get(&reference.blob).unwrap()).unwrap(),
                    original
                );
            }
        }
    }
}
#[test]
fn projection_preserves_tool_input_unknown_payloads_and_live_only_deltas() {
    let (_root, store) = setup();
    for raw in [
        json!({"type":"assistant","message":{"content":[{"type":"tool_use","input":block("thinking",2049)}]}}).to_string(),
        json!({"type":"future","payload":block("thinking",2049)}).to_string(),
        "  diagnostic 1e2  ".into(),
    ] { assert_eq!(store.project(&raw, 2048).unwrap(), Some(raw)); }
    assert_eq!(
        store
            .project(
                r#"{"type":"stream_event","event":{"type":"content_block_delta"}}"#,
                2048
            )
            .unwrap(),
        None
    );
}

#[test]
fn unsafe_namespace_and_file_permissions_fail_without_repair() {
    use std::os::unix::fs::PermissionsExt;
    let (root, store) = setup();
    store.put(&b"abc"[..], 3).unwrap();
    let path = root.path().join("blobs").join(ABC);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(store.get(ABC), Err(BlobError::UnsafePath)));
    assert!(matches!(store.stat(ABC), Err(BlobError::UnsafePath)));
    assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o644);
    fs::set_permissions(root.path().join("blobs"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        BlobStore::open(root.path()),
        Err(BlobError::UnsafePath)
    ));
    assert_eq!(
        fs::metadata(root.path().join("blobs")).unwrap().mode() & 0o777,
        0o755
    );
}
