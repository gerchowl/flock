use super::*;
use crate::platform::process_exit::tests::{readable, Peer};
use std::{os::fd::AsRawFd, time::Duration};
fn peer(app: &mut crate::app::App, peer: crate::platform::peer::Credentials) {
    app.current_api_peer_pid = peer.pid;
    app.current_api_peer_uid = Some(peer.uid);
    app.current_api_peer_birth = Some(peer.birth);
}
#[test]
fn process_bound_hello_and_registration_require_the_exact_minting_peer() {
    let (_root, files, mut app) = super::super::tests::setup();
    let store = Store::new(files, Arc::new(app_sessions::SystemClock));
    let a = Peer::spawn();
    let b = Peer::spawn();
    peer(&mut app, a.credentials);
    let local_a = app.admit_local_peer(a.credentials).unwrap();
    let local_b = app.admit_local_peer(b.credentials).unwrap();
    let minted = store
        .issue(
            &app.client_token_authority().unwrap(),
            local_a.process().clone(),
        )
        .unwrap();
    let matched = store
        .admit_peer(
            &minted.token,
            Some(&local_a),
            Duration::from_secs(5),
            |session| session.info,
        )
        .unwrap();
    assert_eq!(matched, minted.info);
    for refused in [Some(&local_b), None] {
        assert!(matches!(
            store.admit_peer(&minted.token, refused, Duration::from_secs(5), |_| panic!(
                "mismatched registration"
            )),
            Err(PeerAdmissionError::PeerProcessChanged)
        ));
    }
    let file = store
        .files
        .issue(
            &app.client_token_authority().unwrap(),
            "file-only-send",
            [Scope::Send].into(),
        )
        .unwrap();
    for origin in [Some(&local_a), None] {
        let info = store
            .admit_peer(&file.token, origin, Duration::from_secs(5), |session| {
                session.info
            })
            .unwrap();
        assert_eq!(info.scopes, [Scope::Send].into());
    }
}
#[test]
fn ready_peer_exit_prevents_registration_before_notifications_are_serviced() {
    let (_root, files, mut app) = super::super::tests::setup();
    let store = Store::new(files, Arc::new(app_sessions::SystemClock));
    let mut child = Peer::spawn();
    peer(&mut app, child.credentials);
    let local = app.admit_local_peer(child.credentials).unwrap();
    let minted = store
        .issue(
            &app.client_token_authority().unwrap(),
            local.process().clone(),
        )
        .unwrap();
    child.exit();
    assert!(readable(
        local.process().as_raw_fd(),
        Duration::from_secs(5)
    ));
    assert!(matches!(
        store.admit_peer(
            &minted.token,
            Some(&local),
            Duration::from_secs(5),
            |_| panic!("exited registration")
        ),
        Err(PeerAdmissionError::Token(TokenError::Revoked))
    ));
}
#[test]
fn registered_callback_retains_the_shared_revocation_fence() {
    let (_root, files, mut app) = super::super::tests::setup();
    let store = Store::new(files, Arc::new(app_sessions::SystemClock));
    let child = Peer::spawn();
    peer(&mut app, child.credentials);
    let local = app.admit_local_peer(child.credentials).unwrap();
    let minted = store
        .issue(
            &app.client_token_authority().unwrap(),
            local.process().clone(),
        )
        .unwrap();
    store
        .admit_peer(&minted.token, Some(&local), Duration::from_secs(5), |_| {
            let file = store.files.lock_file().unwrap();
            assert!(matches!(
                file.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
        })
        .unwrap();
}

#[test]
fn persistent_file_bearer_refuses_an_exited_local_peer() {
    let (_root, files, mut app) = super::super::tests::setup();
    let store = Store::new(files, Arc::new(app_sessions::SystemClock));
    let mut child = Peer::spawn();
    peer(&mut app, child.credentials);
    let local = app.admit_local_peer(child.credentials).unwrap();
    let file = store
        .files
        .issue(
            &app.client_token_authority().unwrap(),
            "file-exited-peer",
            [Scope::Send].into(),
        )
        .unwrap();
    child.exit_unreaped();
    assert!(
        store.authenticate(&file.token).is_ok(),
        "file bearer remains valid"
    );
    assert!(matches!(
        store.admit_peer(
            &file.token,
            Some(&local),
            Duration::from_secs(5),
            |_| panic!("exited file registration")
        ),
        Err(PeerAdmissionError::PeerProcessChanged)
    ));
}
