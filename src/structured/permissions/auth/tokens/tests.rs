use super::*;
use crate::app::App;
use crate::structured::{identity::UidPolicy, launch::SessionConfigDir};
use std::fs;
use std::os::unix::{
    fs::{symlink, MetadataExt, PermissionsExt},
    net::UnixStream,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

fn operator() -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    let (left, _right) = UnixStream::pair().unwrap();
    let peer = crate::platform::peer::credentials(&left);
    app.runtime_config.uid_policy = UidPolicy {
        operator: crate::platform::peer::effective_uid(),
        dedicated: Ok(None).into(),
    };
    app.current_api_peer_pid = peer.pid;
    app.current_api_peer_birth = Some(peer.birth);
    app.current_api_peer_uid = Some(peer.uid);
    app
}
fn scopes(values: &[Scope]) -> BTreeSet<Scope> {
    values.iter().copied().collect()
}
struct FixtureEntropy(u64);
impl Entropy for FixtureEntropy {
    fn fill(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        for chunk in bytes.chunks_mut(32) {
            let digest = Sha256::digest(self.0.to_le_bytes());
            chunk.copy_from_slice(&digest[..chunk.len()]);
            self.0 += 1;
        }
        Ok(())
    }
}
fn setup() -> (SessionConfigDir, TokenStore, App) {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let store = TokenStore::open_with_entropy(root.path(), FixtureEntropy(0)).unwrap();
    (root, store, operator())
}
fn issue(store: &TokenStore, app: &mut App, name: &str, values: &[Scope]) -> Issued {
    store
        .issue(&app.client_token_authority().unwrap(), name, scopes(values))
        .unwrap()
}
#[test]
fn kernel_operator_proof_refuses_missing_unknown_and_dedicated_callers() {
    let mut app = operator();
    assert!(app.client_token_authority().is_ok());
    app.current_api_peer_pid = None;
    assert!(matches!(
        app.client_token_authority(),
        Err(crate::api::error_code::ErrorCode::OperatorOnly)
    ));
    let mut app = operator();
    app.runtime_config.uid_policy.operator ^= 1;
    app.runtime_config.uid_policy.dedicated =
        Ok(Some(crate::platform::peer::effective_uid())).into();
    assert!(app.client_token_authority().is_err());
    let mut app = operator();
    app.current_api_peer_uid = Some(None);
    assert!(app.client_token_authority().is_err());
}
#[test]
fn issue_authenticate_list_and_private_persistence_survive_reopen() {
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read, Scope::Operate]);
    assert!(valid_hex(&issued.token.0, 64));
    assert!(valid_hex(&issued.info.principal, 32));
    assert_eq!(
        fs::metadata(root.path().join("client-tokens"))
            .unwrap()
            .mode()
            & 0o7777,
        0o700
    );
    assert_eq!(
        fs::metadata(root.path().join("client-tokens/browser.token"))
            .unwrap()
            .mode()
            & 0o7777,
        0o600
    );
    let reopened = TokenStore::open(root.path()).unwrap();
    assert_eq!(
        reopened.authenticate(&issued.token).unwrap().info,
        issued.info
    );
    let listed = reopened
        .list(&app.client_token_authority().unwrap())
        .unwrap();
    assert_eq!(
        listed,
        vec![Listed::Valid {
            entry: hex(b"browser.token"),
            info: issued.info.clone()
        }]
    );
    assert_eq!(issued.token.0, hex(&Sha256::digest(0u64.to_le_bytes())));
    assert_eq!(
        issued.info.principal,
        hex(&Sha256::digest(1u64.to_le_bytes())[..16])
    );
    assert!(!serde_json::to_string(&listed)
        .unwrap()
        .contains(&issued.token.0));
    assert!(!format!("{issued:?}").contains(&issued.token.0));
    assert!(
        matches!(store.issue(&app.client_token_authority().unwrap(), "browser", scopes(&[Scope::Send])), Err(TokenError::Io(e)) if e.kind()==io::ErrorKind::AlreadyExists)
    );
    assert_eq!(
        reopened.authenticate(&issued.token).unwrap().info,
        issued.info
    );
    assert!(fs::read_dir(root.path().join("client-tokens"))
        .unwrap()
        .all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp-")));
}
#[test]
fn known_digest_and_constant_time_comparison_are_exact() {
    assert_eq!(
        hex(&Sha256::digest(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let value = [7; 32];
    assert!(equal(&value, &value));
    for i in 0..32 {
        let mut other = value;
        other[i] ^= 1;
        assert!(!equal(&value, &other));
    }
    let (_root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    let mut wrong = issued.token.0.clone();
    wrong.replace_range(..1, if wrong.starts_with('0') { "1" } else { "0" });
    for token in [
        "".into(),
        wrong,
        "g".repeat(64),
        format!(" {}", issued.token.0),
        "F".repeat(64),
    ] {
        assert!(matches!(
            store.authenticate(&Bearer(token)),
            Err(TokenError::Unauthorized)
        ));
    }
}
#[test]
fn admission_uses_current_exact_scopes_and_the_sealed_permission_principal() {
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read, Scope::Operate]);
    let session = store.authenticate(&issued.token).unwrap();
    assert_eq!(
        store
            .admit(&session, Scope::Operate, |proof| proof
                .decision_subject()
                .map(str::to_owned))
            .unwrap(),
        Some(issued.info.principal.clone())
    );
    assert!(matches!(
        store.admit(&session, Scope::Send, |_| panic!("no send scope")),
        Err(TokenError::Scope)
    ));
    let path = root.path().join("client-tokens/browser.token");
    let mut record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record.scopes = scopes(&[Scope::Read]);
    fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(matches!(
        store.admit(&session, Scope::Operate, |_| panic!("stale scope")),
        Err(TokenError::Scope)
    ));
    assert_eq!(
        store
            .admit(&session, Scope::Read, |proof| proof
                .decision_subject()
                .map(str::to_owned))
            .unwrap(),
        None
    );
    assert_eq!(
        store.authenticate(&issued.token).unwrap().info.principal,
        issued.info.principal
    );
}
#[test]
fn revoked_replaced_and_reissued_tokens_never_admit_an_old_session() {
    let (root, store, mut app) = setup();
    let first = issue(&store, &mut app, "browser", &[Scope::Send]);
    let old = store.authenticate(&first.token).unwrap();
    store
        .revoke(&app.client_token_authority().unwrap(), "browser")
        .unwrap();
    assert!(matches!(
        store.admit(&old, Scope::Send, |_| panic!("revoked")),
        Err(TokenError::Revoked)
    ));
    assert!(matches!(
        store.authenticate(&first.token),
        Err(TokenError::Unauthorized)
    ));
    let second = issue(&store, &mut app, "browser", &[Scope::Send]);
    assert_ne!(first.token.0, second.token.0);
    assert_ne!(first.info.principal, second.info.principal);
    assert!(matches!(
        store.admit(&old, Scope::Send, |_| panic!("reissued")),
        Err(TokenError::Revoked)
    ));
    let session = store.authenticate(&second.token).unwrap();
    let path = root.path().join("client-tokens/browser.token");
    let mut record: Record = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record.token = Bearer("0".repeat(64));
    fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(matches!(
        store.admit(&session, Scope::Send, |_| panic!("replaced")),
        Err(TokenError::Revoked)
    ));
}
#[test]
fn unsafe_entries_match_nobody_but_do_not_deny_other_clients_or_operator_inspection() {
    let (root, store, mut app) = setup();
    for name in ["", "..", "../escape", "Upper", "a/b"] {
        assert!(matches!(
            store.issue(
                &app.client_token_authority().unwrap(),
                name,
                BTreeSet::new()
            ),
            Err(TokenError::Unsafe)
        ));
    }
    let good = issue(&store, &mut app, "good", &[Scope::Read]);
    let bad = issue(&store, &mut app, "bad", &[Scope::Read]);
    let dir = root.path().join("client-tokens");
    let file = dir.join("bad.token");
    let original = fs::read(&file).unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, &original).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&file).unwrap();
    symlink(&outside, &file).unwrap();
    let inspect = |store: &TokenStore, app: &mut App| {
        assert_eq!(store.authenticate(&good.token).unwrap().info, good.info);
        assert!(matches!(
            store.authenticate(&bad.token),
            Err(TokenError::Unauthorized)
        ));
        let listed = store.list(&app.client_token_authority().unwrap()).unwrap();
        assert!(listed.contains(&Listed::Unsafe {
            entry: hex(b"bad.token"),
            name: Some("bad.token".into())
        }));
        assert!(listed
            .iter()
            .any(|entry| matches!(entry, Listed::Valid{info, ..} if info == &good.info)));
        assert!(!serde_json::to_string(&listed)
            .unwrap()
            .contains(&bad.token.0));
    };
    inspect(&store, &mut app);
    fs::remove_file(&file).unwrap();
    fs::write(&file, &original).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    inspect(&store, &mut app);
    assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o644);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let mut oversized = original.clone();
    oversized.resize(4097, b' ');
    for corrupt in [
        oversized,
        format!("{{\"{}\":true}}", bad.token.0).into_bytes(),
        vec![b' '; 4097],
        b"{\"token\":\"bad\"}".to_vec(),
    ] {
        fs::write(&file, corrupt).unwrap();
        inspect(&store, &mut app);
    }
    for name in ["Upper.token", &format!("{}.token", "a".repeat(65))] {
        let name = std::ffi::OsString::from(name);
        let path = dir.join(&name);
        fs::write(&path, &original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        inspect(&store, &mut app);
        let listed = store.list(&app.client_token_authority().unwrap()).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let entry = hex(name.as_bytes());
        assert!(listed
            .iter()
            .any(|row| matches!(row, Listed::Unsafe {entry: id, ..} if id == &entry)));
        store
            .revoke_entry(&app.client_token_authority().unwrap(), &entry)
            .unwrap();
        assert!(!path.exists());
    }
    store
        .revoke(&app.client_token_authority().unwrap(), "bad")
        .unwrap();
    assert!(!file.exists());
    assert_eq!(fs::read(outside).unwrap(), original);
    fs::rename(&dir, root.path().join("saved")).unwrap();
    symlink(root.path().join("saved"), &dir).unwrap();
    assert!(store.authenticate(&good.token).is_err());
    assert!(store.list(&app.client_token_authority().unwrap()).is_err());
    assert!(TokenStore::open(root.path()).is_err());
}
#[test]
fn namespace_replacement_and_write_failure_do_not_redirect_or_issue_tokens() {
    let (root, store, mut app) = setup();
    fs::remove_file(root.path().join("client-tokens/.lock")).unwrap();
    fs::remove_dir(root.path().join("client-tokens")).unwrap();
    assert!(matches!(
        store.issue(
            &app.client_token_authority().unwrap(),
            "browser",
            BTreeSet::new()
        ),
        Err(TokenError::Io(_))
    ));
    assert!(root.path().is_dir());
    assert!(
        matches!(store.authenticate(&Bearer("0".repeat(64))), Err(TokenError::Io(error)) if error.kind() == io::ErrorKind::NotFound)
    );
    assert!(
        matches!(store.list(&app.client_token_authority().unwrap()), Err(TokenError::Io(error)) if error.kind() == io::ErrorKind::NotFound)
    );
    let replacement = TokenStore::open(root.path()).unwrap();
    assert!(matches!(
        store.issue(
            &app.client_token_authority().unwrap(),
            "browser",
            BTreeSet::new()
        ),
        Err(TokenError::Unsafe)
    ));
    assert!(replacement
        .list(&app.client_token_authority().unwrap())
        .unwrap()
        .is_empty());
}
#[test]
fn authentication_does_not_acquire_the_admission_lock() {
    let (_root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    let _guard = store.lock().unwrap();
    assert_eq!(store.authenticate(&issued.token).unwrap().info, issued.info);
    assert!(matches!(
        store.authenticate(&Bearer("0".repeat(64))),
        Err(TokenError::Unauthorized)
    ));
    assert!(store.list(&app.client_token_authority().unwrap()).is_ok());
}
#[test]
fn authentication_contention_cannot_make_revocation_busy() {
    let (_root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Send]);
    let session = store.authenticate(&issued.token).unwrap();
    let store = Arc::new(store);
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let (done_tx, done_rx) = mpsc::channel();
    let worker = {
        let store = store.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            // The first iteration proves the caller is active before revocation.
            assert!(matches!(
                store.authenticate(&Bearer("0".repeat(64))),
                Err(TokenError::Unauthorized)
            ));
            ready_tx.send(()).unwrap();
            while !stop.load(Ordering::Acquire) {
                assert!(matches!(
                    store.authenticate(&Bearer("0".repeat(64))),
                    Err(TokenError::Unauthorized)
                ));
            }
            done_tx.send(()).unwrap();
        })
    };
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let result = store.revoke(&app.client_token_authority().unwrap(), "browser");
    stop.store(true, Ordering::Release);
    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    result.unwrap();
    assert!(matches!(
        store.admit(&session, Scope::Send, |_| panic!("revoked")),
        Err(TokenError::Revoked)
    ));
}
#[test]
fn separate_handles_wait_for_admission_and_revocation_wins_after_it() {
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Send]);
    let session = store.authenticate(&issued.token).unwrap();
    let mut other = TokenStore::open(root.path()).unwrap();
    other.2 = helper_binary();
    let store = Arc::new(store);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = {
        let store = store.clone();
        let session = session.clone();
        std::thread::spawn(move || {
            let result = store.admit(&session, Scope::Send, |_| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            });
            done_tx.send(result).unwrap();
        })
    };
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    // Begin the real revocation while dispatch holds the OS lock. The staged
    // operation must wait, rather than fail Busy or delete an admitted record.
    let pending = other.begin_revoke("browser").unwrap();
    assert!(priority::pending(&other.0).unwrap());
    assert_eq!(store.authenticate(&issued.token).unwrap().info, issued.info);
    release_tx.send(()).unwrap();
    pending.finish().unwrap();
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    assert!(matches!(
        store.admit(&session, Scope::Send, |_| panic!("after revocation")),
        Err(TokenError::Revoked)
    ));
}
#[test]
fn revocation_lock_deadline_fails_without_removing_a_credential() {
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    let mut other = TokenStore::open(root.path()).unwrap();
    other.2 = helper_binary();
    let guard = store.lock().unwrap();
    assert!(
        matches!(other.revoke(&app.client_token_authority().unwrap(), "browser"), Err(TokenError::Io(error)) if error.kind() == io::ErrorKind::TimedOut)
    );
    drop(guard);
    assert_eq!(store.authenticate(&issued.token).unwrap().info, issued.info);
}
#[test]
fn open_sweeps_orphan_bearers_without_following_links() {
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    let directory = root.path().join("client-tokens");
    fs::copy(
        directory.join("browser.token"),
        directory.join(".tmp-orphan"),
    )
    .unwrap();
    let outside = root.path().join("preserve");
    fs::write(&outside, b"fixture").unwrap();
    symlink(&outside, directory.join(".tmp-link")).unwrap();
    let reopened = TokenStore::open(root.path()).unwrap();
    assert!(!directory.join(".tmp-orphan").exists());
    assert!(fs::symlink_metadata(directory.join(".tmp-link")).is_err());
    assert_eq!(fs::read(outside).unwrap(), b"fixture");
    assert_eq!(
        reopened.authenticate(&issued.token).unwrap().info,
        issued.info
    );
}
#[test]
fn same_uid_with_lease_ancestry_cannot_issue() {
    let (_root, store, mut app) = setup();
    assert!(app.client_token_authority().is_ok());
    let mut lease = app
        .session_leases
        .reserve("agent_atlas_token", "token-fixture", None)
        .unwrap();
    lease.bind(std::process::id()).unwrap();
    assert!(matches!(
        app.session_leases.caller(Some(std::process::id())),
        crate::structured::lease::Caller::Agent { .. }
    ));
    assert!(matches!(
        app.client_token_authority(),
        Err(crate::api::error_code::ErrorCode::OperatorOnly)
    ));
    assert!(store
        .list(&operator().client_token_authority().unwrap())
        .unwrap()
        .is_empty());
}

#[test]
fn fifo_token_is_refused_without_waiting_for_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    let path = root.path().join("client-tokens/browser.token");
    fs::remove_file(&path).unwrap();
    let good = issue(&store, &mut app, "good", &[Scope::Read]);
    let raw = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: terminated sandbox fixture path, never a shared entry.
    assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o600) }, 0);
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut app = operator();
        assert_eq!(store.authenticate(&good.token).unwrap().info, good.info);
        assert!(store.list(&app.client_token_authority().unwrap()).unwrap().iter().any(|row| matches!(row, Listed::Unsafe {name: Some(name), ..} if name == "browser.token")));
        tx.send(store.authenticate(&issued.token).map(|_| ()))
            .unwrap();
    });
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(TokenError::Unauthorized)
    ));
    worker.join().unwrap();
}

#[test]
fn open_cannot_sweep_an_active_issue_temporary() {
    let (root, store, _app) = setup();
    let guard = store.lock().unwrap();
    let temporary = root.path().join("client-tokens/.tmp-active");
    fs::write(&temporary, b"fixture").unwrap();
    assert!(matches!(
        TokenStore::open(root.path()),
        Err(TokenError::Busy)
    ));
    assert!(temporary.exists());
    drop(guard);
    TokenStore::open(root.path()).unwrap();
    assert!(!temporary.exists());
}

#[test]
fn non_utf8_directory_entries_are_lossless_unsafe_rows_not_scan_errors() {
    use std::os::unix::ffi::OsStringExt;
    let (_root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    // APFS rejects non-UTF-8 file creation. Drive the production directory-name
    // decoder and entry loader directly, together with an actual valid record.
    let bad = Entry::from_name(std::ffi::OsString::from_vec(b"\xff.token".to_vec()));
    assert!(bad.name.is_none());
    assert_eq!(bad.id, "ff2e746f6b656e");
    let mut entries = store.0.names().unwrap();
    entries.push(bad);
    let loaded: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.id.ends_with("2e746f6b656e"))
        .map(|entry| store.load_entry(entry))
        .collect();
    assert!(loaded
        .iter()
        .any(|(entry, record)| entry.id == "ff2e746f6b656e" && record.is_none()));
    assert!(loaded.iter().any(|(_, record)| record
        .as_ref()
        .is_some_and(|(_, record)| record.token.0 == issued.token.0)));
    assert_eq!(store.authenticate(&issued.token).unwrap().info, issued.info);
}

#[test]
fn listed_entry_identifiers_cannot_revoke_outside_the_token_namespace_or_its_lock() {
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    let outside = root.path().join("outside.token");
    fs::write(&outside, b"fixture").unwrap();
    for entry in [
        String::new(),
        "z0".into(),
        "0".into(),
        hex(b"../outside.token"),
        hex(b".lock"),
        hex(b".tmp-active"),
        hex(b"browser.token\0"),
    ] {
        assert!(matches!(
            store.revoke_entry(&app.client_token_authority().unwrap(), &entry),
            Err(TokenError::Unsafe)
        ));
    }
    assert_eq!(fs::read(outside).unwrap(), b"fixture");
    assert_eq!(store.authenticate(&issued.token).unwrap().info, issued.info);
    store.lock().unwrap();
}

#[test]
fn operator_browser_token_is_reused_with_all_explicit_grants() {
    let (_root, store, mut app) = setup();
    let authority = app.client_token_authority().unwrap();
    let first = store.operator_token(&authority).unwrap();
    let second = store.operator_token(&authority).unwrap();
    assert_eq!(first, second);
    let session = store.authenticate(&first).unwrap();
    assert_eq!(
        session.info.scopes,
        scopes(&[Scope::Read, Scope::Send, Scope::Answer, Scope::Operate])
    );
    store.revoke(&authority, "operator").unwrap();
    assert!(store.authenticate(&first).is_err());
    let replacement = store.operator_token(&authority).unwrap();
    assert_ne!(replacement, first);
}
#[test]
fn browser_open_does_not_widen_an_existing_operator_named_token() {
    let (_root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "operator", &[Scope::Read]);
    assert!(store
        .operator_token(&app.client_token_authority().unwrap())
        .is_err());
    assert_eq!(
        store.authenticate(&issued.token).unwrap().info.scopes,
        scopes(&[Scope::Read])
    );
}

fn helper_binary() -> std::path::PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("flk{}", std::env::consts::EXE_SUFFIX))
}
#[test]
fn revocation_intent_blocks_new_admissions_across_handles_without_starvation() {
    let (_root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Send]);
    let session = store.authenticate(&issued.token).unwrap();
    let intent = priority::Intent::begin(&store.0, store.random::<16>().unwrap()).unwrap();
    // The primary lock is free, so Busy proves intent priority rather than contention.
    assert!(store.lock_file().unwrap().try_lock().is_ok());
    for _ in 0..4096 {
        assert!(matches!(
            store.admit(&session, Scope::Send, |_| panic!(
                "admission overtook revocation"
            )),
            Err(TokenError::Busy)
        ));
    }
    drop(intent);
    assert!(store.admit(&session, Scope::Send, |_| ()).is_ok());
}
#[test]
fn looping_valid_admissions_cannot_make_operator_revocation_fail() {
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Send]);
    let session = store.authenticate(&issued.token).unwrap();
    let mut other = TokenStore::open(root.path()).unwrap();
    other.2 = helper_binary();
    let store = Arc::new(store);
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let worker = {
        let store = store.clone();
        let stop = stop.clone();
        let session = session.clone();
        std::thread::spawn(move || {
            assert!(store.admit(&session, Scope::Send, |_| ()).is_ok());
            tx.send(()).unwrap();
            while !stop.load(Ordering::Acquire) {
                match store.admit(&session, Scope::Send, |_| ()) {
                    Ok(()) | Err(TokenError::Busy) | Err(TokenError::Revoked) => (),
                    other => panic!("unexpected admission: {other:?}"),
                }
            }
        })
    };
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let result = other.revoke(&app.client_token_authority().unwrap(), "browser");
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    result.unwrap();
    assert!(matches!(
        store.admit(&session, Scope::Send, |_| panic!("revoked callback")),
        Err(TokenError::Revoked)
    ));
}

#[test]
fn temporary_directory_is_unsafe_entry_not_a_store_outage() {
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    let path = root.path().join("client-tokens/.tmp-directory");
    fs::create_dir(&path).unwrap();
    let logs = crate::logging::capture_logs(|| {
        let reopened = TokenStore::open(root.path()).unwrap();
        assert_eq!(
            reopened.authenticate(&issued.token).unwrap().info,
            issued.info
        );
        assert!(reopened
            .list(&app.client_token_authority().unwrap())
            .is_ok());
    });
    assert!(path.is_dir());
    assert!(logs.contains(&hex(b".tmp-directory")));
}
#[test]
fn orphan_intent_and_invalid_dash_names_cannot_deny_or_issue() {
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    let name = format!(".revoke-{}", "a".repeat(32));
    let orphan = store.0.file(&name, true).unwrap();
    drop(orphan);
    assert!(!priority::pending(&store.0).unwrap());
    assert!(store
        .admit(
            &store.authenticate(&issued.token).unwrap(),
            Scope::Read,
            |_| ()
        )
        .is_ok());
    for name in ["--show-token", "-browser"] {
        assert!(matches!(
            store.issue(
                &app.client_token_authority().unwrap(),
                name,
                scopes(&[Scope::Read])
            ),
            Err(TokenError::Unsafe)
        ));
        assert!(!root
            .path()
            .join("client-tokens")
            .join(format!("{name}.token"))
            .exists());
    }
}

#[test]
fn priority_process_fixture() {
    let Some(root) = std::env::var_os("FLK_TOKEN_PRIORITY_FIXTURE") else {
        return;
    };
    assert!(matches!(
        TokenStore::open(Path::new(&root)),
        Err(TokenError::Busy)
    ));
}
#[test]
fn intent_is_observed_by_an_independent_process_with_no_primary_lock_held() {
    let (root, store, _app) = setup();
    let intent = priority::Intent::begin(&store.0, store.random::<16>().unwrap()).unwrap();
    let mut command = crate::process::TracedCommand::new(std::env::current_exe().unwrap(), "test");
    command.args([
        "--exact",
        "structured::permissions::auth::tokens::tests::priority_process_fixture",
        "--nocapture",
    ]);
    for (key, _) in std::env::vars().filter(|(k, _)| k.starts_with("FLOCK_")) {
        command.env_remove(key);
    }
    command.env("FLK_TOKEN_PRIORITY_FIXTURE", root.path());
    let output = command
        .output_traced_with_timeout(Duration::from_secs(10))
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(intent);
    assert!(store.lock().is_ok());
}
