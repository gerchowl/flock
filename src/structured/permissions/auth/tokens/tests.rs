use super::*;
use crate::app::App;
use crate::structured::{identity::UidPolicy, launch::SessionConfigDir};
use std::fs;
use std::os::unix::{
    fs::{symlink, MetadataExt, PermissionsExt},
    net::UnixStream,
};
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
fn setup() -> (SessionConfigDir, TokenStore, App) {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let store = TokenStore::open(root.path()).unwrap();
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
    assert_eq!(listed.as_slice(), std::slice::from_ref(&issued.info));
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
fn unsafe_paths_links_modes_and_corrupt_records_fail_without_leaking_content() {
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
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    let file = root.path().join("client-tokens/browser.token");
    let original = fs::read(&file).unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, &original).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&file).unwrap();
    symlink(&outside, &file).unwrap();
    assert!(matches!(
        store.authenticate(&issued.token),
        Err(TokenError::Unsafe)
    ));
    fs::remove_file(&file).unwrap();
    fs::write(&file, &original).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        store.authenticate(&issued.token),
        Err(TokenError::Unsafe)
    ));
    assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o644);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let mut oversized = original.clone();
    oversized.resize(4097, b' ');
    for corrupt in [
        oversized,
        format!("{{\"{}\":true}}", issued.token.0).into_bytes(),
        vec![b' '; 4097],
        b"{\"token\":\"bad\"}".to_vec(),
    ] {
        fs::write(&file, corrupt).unwrap();
        let error = store.authenticate(&issued.token).err().unwrap();
        assert!(matches!(error, TokenError::Unsafe));
        assert!(!format!("{error}").contains(&issued.token.0));
    }
    fs::rename(root.path().join("client-tokens"), root.path().join("saved")).unwrap();
    symlink(root.path().join("saved"), root.path().join("client-tokens")).unwrap();
    assert!(store.authenticate(&issued.token).is_err());
    assert!(TokenStore::open(root.path()).is_err());
}
#[test]
fn namespace_replacement_and_write_failure_do_not_redirect_or_issue_tokens() {
    let (root, store, mut app) = setup();
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
fn separate_handles_serialize_admission_with_operator_revocation_without_waiting() {
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Send]);
    let session = store.authenticate(&issued.token).unwrap();
    let store = Arc::new(store);
    let other = TokenStore::open(root.path()).unwrap();
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
    assert!(matches!(
        other.revoke(&app.client_token_authority().unwrap(), "browser"),
        Err(TokenError::Busy)
    ));
    release_tx.send(()).unwrap();
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    other
        .revoke(&app.client_token_authority().unwrap(), "browser")
        .unwrap();
    assert!(matches!(
        store.admit(&session, Scope::Send, |_| panic!("after revocation")),
        Err(TokenError::Revoked)
    ));
}

#[test]
fn private_issue_restores_owner_access_under_restrictive_umask() {
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
    let mut app = operator();
    // SAFETY: nextest isolates this process-local umask.
    let _mask = Mask(unsafe { libc::umask(0o777) });
    let store = TokenStore::open(root.path()).unwrap();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    assert_eq!(store.authenticate(&issued.token).unwrap().info, issued.info);
}

#[test]
fn fifo_token_is_refused_without_waiting_for_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    let (root, store, mut app) = setup();
    let issued = issue(&store, &mut app, "browser", &[Scope::Read]);
    let path = root.path().join("client-tokens/browser.token");
    fs::remove_file(&path).unwrap();
    let raw = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: terminated sandbox fixture path, never a shared entry.
    assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o600) }, 0);
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(store.authenticate(&issued.token).map(|_| ()))
            .unwrap();
    });
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(TokenError::Unsafe)
    ));
    worker.join().unwrap();
}
