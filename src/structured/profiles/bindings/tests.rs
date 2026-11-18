use super::*;
use std::os::unix::{fs::PermissionsExt, net::UnixListener};

fn binding() -> Value {
    json!({"binding_id":"binding-a","target":{"repo_id":"repo-a","node_ids":["node-a"],"agent_ids":["agent-a"]},
        "account_alias":"fixture-account","credential_ref":"fixture-token-ref","credential_generation":1,
        "bundle_digests":["fixture-digest"],"grants":["fixture-selection"],"revision":7,"revoked":null})
}

fn envelope(entries: Value) -> Value {
    json!({"schema":1,"policy_revision":7,"bindings":entries,"unknown":[]})
}

fn fixture() -> (std::path::PathBuf, std::path::PathBuf) {
    let root = crate::test_support::unique_temp_path("profile-bindings");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("profile-bindings.json");
    (root, path)
}

#[test]
fn round_trip_preserves_switch_revision_and_private_mode() {
    let (root, path) = fixture();
    let mut store = BindingStore::default();
    store.decode(envelope(json!([binding()])));
    assert!(!store.execution_switch);
    store.execution_switch = true;
    store.save(&path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut restored = BindingStore::default();
    restored.load(&path);
    assert_eq!(restored.policy_revision, 7);
    assert!(restored.execution_switch);
    assert_eq!(
        restored.resolve("repo-a", "node-a", "agent-a"),
        store.resolve("repo-a", "node-a", "agent-a")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_future_and_conflicting_entries_are_preserved_without_authority() {
    let (root, path) = fixture();
    let mut future = binding();
    future["binding_id"] = json!("future");
    future["future_field"] = json!(true);
    let mut invalid = binding();
    invalid["binding_id"] = json!("invalid");
    invalid["target"]["agent_ids"] = json!(["*"]);
    let mut conflict = binding();
    conflict["credential_ref"] = json!("conflicting-ref");
    let entries = json!([binding(), conflict, future, invalid, 42]);
    let mut survivor = binding();
    survivor["binding_id"] = json!("binding-survivor");
    survivor["target"]["agent_ids"] = json!(["agent-survivor"]);
    let mut all_entries = entries.as_array().unwrap().clone();
    all_entries.push(survivor.clone());
    let mut raw = envelope(json!(all_entries));
    raw["extra_root_field"] = json!({"future":true});
    raw["unknown"] = json!([{"previously_retained":true}]);
    let mut store = BindingStore::default();
    store.decode(raw);
    assert!(store.resolve("repo-a", "node-a", "agent-a").is_none());
    store.save(&path).unwrap();
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["bindings"], json!([survivor]));
    assert!(store
        .resolve("repo-a", "node-a", "agent-survivor")
        .is_some());
    assert_eq!(saved["unknown"][0], json!({"previously_retained":true}));
    assert_eq!(
        saved["unknown"].as_array().unwrap()[1..],
        *entries.as_array().unwrap()
    );
    assert_eq!(saved["extra_root_field"], json!({"future":true}));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unknown_or_malformed_envelope_is_never_overwritten() {
    for raw in [
        json!({"schema":2,"bindings":[binding()]}),
        json!({"schema":1,"policy_revision":7,"bindings":{}}),
    ] {
        let (root, path) = fixture();
        fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        let before = fs::read(&path).unwrap();
        let mut store = BindingStore::default();
        store.load(&path);
        assert!(store.resolve("repo-a", "node-a", "agent-a").is_none());
        assert!(store.save(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn older_snapshot_cannot_lower_observed_or_persisted_revision() {
    let (root, path) = fixture();
    let mut store = BindingStore::default();
    store.decode(envelope(json!([binding()])));
    store.save(&path).unwrap();
    let mut older = envelope(json!([]));
    older["policy_revision"] = json!(6);
    store.decode(older.clone());
    assert_eq!(store.policy_revision, 7);
    assert!(store.resolve("repo-a", "node-a", "agent-a").is_some());
    let mut stale = BindingStore::default();
    stale.decode(older);
    assert!(stale.save(&path).is_err());
    let mut restarted = BindingStore::default();
    restarted.load(&path);
    assert_eq!(restarted.policy_revision, 7);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn revoked_and_ambiguous_targets_never_resolve() {
    let mut initial = BindingStore::default();
    initial.decode(envelope(json!([binding()])));
    assert!(initial.resolve("repo-a", "node-a", "agent-a").is_some());
    let mut revoked = binding();
    revoked["revoked"] = json!({"reason":"fixture revocation","principal":"fixture operator","operation_id":"fixture-op","revision":7});
    let mut store = BindingStore::default();
    store.decode(envelope(json!([revoked])));
    assert!(store.resolve("repo-a", "node-a", "agent-a").is_none());
    let mut other = binding();
    other["binding_id"] = json!("binding-b");
    store.decode(envelope(json!([binding(), other])));
    assert!(store.resolve("repo-a", "node-a", "agent-a").is_none());
}

#[test]
fn resolution_cannot_open_credential_reference_or_spawn_it() {
    if let Ok(reference) = std::env::var("FLOCK_BINDING_READ_CANARY") {
        let mut raw = binding();
        raw["credential_ref"] = json!(reference);
        let mut store = BindingStore::default();
        store.decode(envelope(json!([raw])));
        assert!(store.resolve("repo-a", "node-a", "agent-a").is_some());
        return;
    }
    let (root, _) = fixture();
    let fifo = root.join("read-canary");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: the NUL-terminated fixture path is owned for this call.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let mut child = crate::process::TracedCommand::new(std::env::current_exe().unwrap(), "profile-resolution-canary")
        .args(["--exact", "structured::profiles::bindings::tests::resolution_cannot_open_credential_reference_or_spawn_it"])
        .env("FLOCK_BINDING_READ_CANARY", &fifo).spawn_traced().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut opened = false;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if let Ok(mut writer) = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
        {
            opened = true;
            let _ = writer.write_all(b"synthetic credential canary");
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("credential-resolution child exceeded its fail ceiling");
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    assert!(status.success());
    assert!(!opened, "resolution opened the credential reference");
    // Opening this credential reference as a file fails; treating it as an endpoint
    // leaves an observable connection. Its executable canary records any spawn.
    // Unix socket paths need an explicitly short root on macOS.
    // guardrails-ok(hermetic): explicit short Unix socket root for macOS.
    let socket = std::path::PathBuf::from("/tmp").join(format!("pb-{}", std::process::id()));
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let executable = root.join("credential-canary");
    let marker = root.join("spawned");
    fs::write(
        &executable,
        format!("#!/bin/sh\n: > '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    for reference in [&socket, &executable] {
        let mut raw = binding();
        raw["credential_ref"] = json!(reference);
        let mut store = BindingStore::default();
        store.decode(envelope(json!([raw])));
        assert_eq!(
            store
                .resolve("repo-a", "node-a", "agent-a")
                .unwrap()
                .credential_ref,
            reference.to_str().unwrap()
        );
        assert!(store.resolve("repo-a", "node-a", "agent-sibling").is_none());
        assert!(store.resolve("repo-b", "node-a", "agent-a").is_none());
        assert!(store.resolve("repo-a", "node-b", "agent-a").is_none());
    }
    assert!(matches!(listener.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    assert!(!marker.exists());
    fs::remove_file(socket).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn restart_rejects_older_snapshot_using_durable_floor() {
    let (root, path) = fixture();
    fs::write(
        &path,
        serde_json::to_vec(&envelope(json!([binding()]))).unwrap(),
    )
    .unwrap();
    let original = crate::structured::profiles::ProfileAdmission::load(&path);
    assert_eq!(original.status(true)["policy_revision"], 7);
    let mut older = envelope(json!([binding()]));
    older["policy_revision"] = json!(6);
    older["bindings"][0]["revision"] = json!(6);
    older["execution_switch"] = json!(true);
    fs::write(&path, serde_json::to_vec(&older).unwrap()).unwrap();
    let bytes = fs::read(&path).unwrap();
    let mut restarted = None;
    let logs = crate::logging::capture_logs(|| {
        restarted = Some(crate::structured::profiles::ProfileAdmission::load(&path));
    });
    let restarted = restarted.unwrap();
    assert!(logs.contains("policy_revision_regressed"));
    assert!(logs.contains("store=6") && logs.contains("floor=7"));
    assert!(!logs.contains("fixture-token-ref") && !logs.contains("fixture-account"));
    assert_eq!(restarted.status(true)["policy_revision"], 7);
    assert_eq!(
        restarted.status(true)["policy_error"],
        json!({"code":"policy_revision_regressed","store":6,"floor":7})
    );
    assert_eq!(restarted.status(true)["bindings"], json!([]));
    assert_eq!(restarted.status(true)["execution_switch"], false);
    assert_eq!(restarted.status(false), json!({}));
    assert_eq!(
        restarted.admit().unwrap_err().code().as_str(),
        "policy_revision_regressed"
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn floor_survives_store_write_failure_and_blocks_stale_retry() {
    let (root, path) = fixture();
    let mut original = BindingStore::default();
    original.decode(envelope(json!([binding()])));
    original.save(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    let mut advanced = BindingStore::default();
    let mut newer = envelope(json!([binding()]));
    newer["policy_revision"] = json!(8);
    newer["bindings"][0]["revision"] = json!(8);
    advanced.decode(newer);
    let floor = path.with_extension("floor.json");
    let result = advanced.save_with_store_write(&path, |_, _| {
        assert_eq!(
            serde_json::from_slice::<u64>(&fs::read(&floor).unwrap()).unwrap(),
            8
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        Err(io::Error::other(
            "injected store-write failure after the floor commit",
        ))
    });
    assert!(result.is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let mut restarted = BindingStore::default();
    restarted.load(&path);
    assert_eq!(restarted.policy_revision, 8);
    assert_eq!(
        restarted.status(true)["policy_error"],
        json!({"code":"policy_revision_regressed","store":7,"floor":8})
    );
    assert!(restarted.resolve("repo-a", "node-a", "agent-a").is_none());
    assert!(original.save(&path).is_err());
    assert_eq!(
        serde_json::from_slice::<u64>(&fs::read(&floor).unwrap()).unwrap(),
        8
    );
    fs::remove_file(&path).unwrap();
    let missing = crate::structured::profiles::ProfileAdmission::load(&path);
    assert_eq!(missing.status(true)["policy_revision"], 8);
    assert_eq!(
        missing.admit().unwrap_err().code().as_str(),
        "policy_revision_unavailable"
    );
    assert!(!path.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn upgrade_initializes_private_floor_without_rewriting_store() {
    let (root, path) = fixture();
    let floor = path.with_extension("floor.json");
    let mut missing = BindingStore::default();
    missing.load(&path);
    assert_eq!(missing.policy_revision, 0);
    assert!(!floor.exists());
    let raw = serde_json::to_vec(&envelope(json!([binding()]))).unwrap();
    fs::write(&path, &raw).unwrap();
    let mut store = BindingStore::default();
    store.load(&path);
    assert_eq!(fs::read(&path).unwrap(), raw);
    assert_eq!(
        serde_json::from_slice::<u64>(&fs::read(&floor).unwrap()).unwrap(),
        7
    );
    assert_eq!(
        fs::metadata(&floor).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(store.resolve("repo-a", "node-a", "agent-a").is_some());
    fs::write(&floor, b"unknown floor version").unwrap();
    let invalid = crate::structured::profiles::ProfileAdmission::load(&path);
    assert_eq!(
        invalid.admit().unwrap_err().code().as_str(),
        "policy_revision_unavailable"
    );
    assert_eq!(invalid.status(true)["bindings"], json!([]));
    assert_eq!(fs::read(&floor).unwrap(), b"unknown floor version");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_saves_cannot_interleave_or_lower_the_acknowledged_floor() {
    use std::sync::mpsc;
    use std::time::Duration;
    let ceiling = Duration::from_secs(10);
    let (root, path) = fixture();
    let candidate = |revision| {
        let mut raw = envelope(json!([]));
        raw["policy_revision"] = json!(revision);
        let mut store = BindingStore::default();
        store.decode(raw);
        store
    };
    candidate(1).save(&path).unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (held_tx, held_rx) = mpsc::sync_channel(1);
    let held_path = path.clone();
    let held = std::thread::spawn(move || {
        let result = candidate(1).save_with_store_write(&held_path, |path, raw| {
            entered_tx.send(()).map_err(io::Error::other)?;
            release_rx
                .recv_timeout(ceiling * 3)
                .map_err(io::Error::other)?;
            write_private_json(path, raw)
        });
        held_tx.send(result).unwrap();
    });
    entered_rx
        .recv_timeout(ceiling)
        .expect("first writer must reach its commit fence");
    let (results_tx, results_rx) = mpsc::channel();
    let mut workers = Vec::new();
    for revision in 2..=8 {
        let results_tx = results_tx.clone();
        let path = path.clone();
        workers.push(std::thread::spawn(move || {
            results_tx
                .send((revision, candidate(revision).save(&path)))
                .unwrap();
        }));
    }
    drop(results_tx);
    let results: Result<Vec<_>, _> = (2..=8).map(|_| results_rx.recv_timeout(ceiling)).collect();
    let during = floor_revision(&path).unwrap();
    release_tx.send(()).unwrap();
    let held_result = held_rx
        .recv_timeout(ceiling)
        .expect("held writer must finish");
    // Check bounded completion before joining; a silent producer must fail here.
    let results = results.expect("every contender must refuse within the fail ceiling");
    held_result.unwrap();
    held.join().unwrap();
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(
        results.iter().all(|(_, result)| result.is_err()),
        "a contender interleaved with the held transaction"
    );
    assert_eq!(during, Some(1), "a refused contender advanced the floor");
    candidate(8).save(&path).unwrap();
    let highest_acknowledged = 8;
    assert!(floor_revision(&path).unwrap().unwrap() >= highest_acknowledged);
    let mut older = envelope(json!([]));
    older["policy_revision"] = json!(3);
    fs::write(&path, serde_json::to_vec(&older).unwrap()).unwrap();
    let restarted = crate::structured::profiles::ProfileAdmission::load(&path);
    assert_eq!(
        restarted.status(true)["policy_revision"],
        highest_acknowledged
    );
    assert_eq!(
        restarted.admit().unwrap_err().code().as_str(),
        "policy_revision_regressed"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn linked_store_floor_and_lock_never_confer_authority() {
    for extension in ["json", "floor.json", "lock"] {
        let (root, path) = fixture();
        let raw = serde_json::to_vec(&envelope(json!([binding()]))).unwrap();
        fs::write(&path, &raw).unwrap();
        let referent = root.join("referent");
        fs::write(
            &referent,
            if extension == "json" {
                raw.as_slice()
            } else {
                b"7"
            },
        )
        .unwrap();
        let linked = path.with_extension(extension);
        if linked.exists() {
            fs::remove_file(&linked).unwrap();
        }
        std::os::unix::fs::symlink(&referent, &linked).unwrap();
        let gate = crate::structured::profiles::ProfileAdmission::load(&path);
        assert_eq!(
            gate.admit().unwrap_err().code().as_str(),
            "policy_revision_unavailable",
            "followed {extension}"
        );
        assert_eq!(gate.status(true)["bindings"], json!([]));
        assert_eq!(gate.status(false), json!({}));
        assert!(fs::symlink_metadata(&linked)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read(&referent).unwrap(),
            if extension == "json" {
                raw.as_slice()
            } else {
                b"7"
            }
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn external_lock_holder_refuses_initialization_and_save_without_interleaving() {
    let (root, path) = fixture();
    let raw = serde_json::to_vec(&envelope(json!([binding()]))).unwrap();
    fs::write(&path, &raw).unwrap();
    let file = fs::File::create(path.with_extension("lock")).unwrap();
    let mut holder = crate::platform::file_lock::inherited_holder::Holder::lock(&file).unwrap();
    drop(file);
    let gate = crate::structured::profiles::ProfileAdmission::load(&path);
    let fault = gate.admit().unwrap_err().code().as_str();
    let mut candidate = BindingStore::default();
    candidate.decode(envelope(json!([binding()])));
    let refused = candidate.save(&path).is_err();
    let initialized = path.with_extension("floor.json").exists();
    holder.verify().unwrap();
    drop(holder);
    assert_eq!(fault, "policy_revision_unavailable");
    assert!(refused);
    assert!(
        !initialized,
        "floor initialized despite another process holding authority"
    );
    assert_eq!(fs::read(&path).unwrap(), raw);
    candidate.save(&path).unwrap();
    fs::remove_dir_all(root).unwrap();
}
