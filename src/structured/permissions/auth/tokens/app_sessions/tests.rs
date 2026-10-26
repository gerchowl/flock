use super::super::tests::{helper_binary, setup};
use super::*;
use crate::platform::process_exit::tests::{readable, Peer};
use std::{os::fd::AsRawFd, sync::mpsc};
struct Manual(Mutex<Instant>);
impl Clock for Manual {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}
impl Manual {
    fn advance(&self, time: Duration) {
        *self.0.lock().unwrap() += time;
    }
}
fn fixture() -> (
    crate::structured::launch::SessionConfigDir,
    Arc<TokenStore>,
    Store,
    Arc<Manual>,
    crate::app::App,
) {
    let (root, mut files, app) = setup();
    files.2 = helper_binary();
    let files = Arc::new(files);
    let clock = Arc::new(Manual(Mutex::new(Instant::now())));
    let store = Store::new(files.clone(), clock.clone());
    (root, files, store, clock, app)
}
#[test]
fn private_issue_is_fixed_operator_scoped_memory_only_and_renews_fresh() {
    let (root, files, store, _clock, mut app) = fixture();
    let authority = app.client_token_authority().unwrap();
    let first = store.issue(&authority).unwrap();
    assert_eq!(
        first.info.scopes,
        BTreeSet::from([Scope::Read, Scope::Send, Scope::Answer, Scope::Operate])
    );
    assert!(files.list(&authority).unwrap().is_empty());
    assert!(matches!(
        files.authenticate(&first.token),
        Err(TokenError::Unauthorized)
    ));
    assert!(!std::fs::read_dir(root.path().join("client-tokens"))
        .unwrap()
        .any(|e| e.unwrap().path().extension().is_some_and(|e| e == "token")));
    let session = store.authenticate(&first.token).unwrap();
    assert_eq!(session.info, first.info);
    assert!(store
        .admit(&session, Scope::Operate, |t| t.decision_subject().is_some())
        .unwrap());
    let renewed = store.issue(&authority).unwrap();
    assert_ne!(renewed.token, first.token);
    assert_ne!(renewed.info.principal, first.info.principal);
    assert!(store.authenticate(&first.token).is_ok());
}
#[test]
fn fixed_ttl_rejects_at_the_exact_boundary_and_notifies_after_sweep() {
    let (_root, _files, store, clock, mut app) = fixture();
    let issued = store.issue(&app.client_token_authority().unwrap()).unwrap();
    let session = store.authenticate(&issued.token).unwrap();
    let expires = session.expires;
    clock.advance(TTL - Duration::from_nanos(1));
    assert!(store.admit(&session, Scope::Read, |_| ()).is_ok());
    assert_eq!(
        store.authenticate(&issued.token).unwrap().expires,
        expires,
        "lookup is not renewal"
    );
    clock.advance(Duration::from_nanos(1));
    assert!(matches!(
        store.authenticate(&issued.token),
        Err(TokenError::Revoked)
    ));
    assert!(matches!(
        store.admit(&session, Scope::Send, |_| panic!("expired dispatch")),
        Err(TokenError::Revoked)
    ));
    assert!(!*session.revoked.borrow());
    store.sweep().unwrap();
    assert!(*session.revoked.borrow());
}
#[test]
fn real_peer_exit_invalidates_a_retained_session_and_reclaims_it() {
    let (_root, _files, store, _clock, mut app) = fixture();
    let mut child = Peer::spawn();
    app.current_api_peer_pid = child.credentials.pid;
    app.current_api_peer_uid = Some(child.credentials.uid);
    app.current_api_peer_birth = Some(child.credentials.birth);
    let issued = store.issue(&app.client_token_authority().unwrap()).unwrap();
    let session = store.authenticate(&issued.token).unwrap();
    child.exit();
    assert!(readable(
        session.process.as_raw_fd(),
        Duration::from_secs(5)
    ));
    assert!(matches!(
        store.admit(&session, Scope::Operate, |_| panic!("exited peer")),
        Err(TokenError::Revoked)
    ));
    store.sweep().unwrap();
    assert!(*session.revoked.borrow());
    assert!(store.entries.lock().unwrap().is_empty());
}
#[test]
fn peer_failures_are_typed_without_creating_any_credential() {
    let (_root, _files, store, _clock, mut app) = fixture();
    app.current_api_peer_birth = Some(None);
    let error = store
        .issue(&app.client_token_authority().unwrap())
        .unwrap_err();
    assert_eq!(
        error.peer_code(),
        Some(crate::api::error_code::ErrorCode::PeerProcessChanged)
    );
    assert!(store.entries.lock().unwrap().is_empty());
}
#[test]
fn restart_owner_and_handoff_fence_never_reuse_the_memory_domain() {
    let (_root, files, store, clock, mut app) = fixture();
    let issued = store.issue(&app.client_token_authority().unwrap()).unwrap();
    let session = store.authenticate(&issued.token).unwrap();
    store.revoke_all().unwrap();
    assert!(*session.revoked.borrow());
    assert!(matches!(
        store.admit(&session, Scope::Read, |_| panic!("old owner")),
        Err(TokenError::Revoked)
    ));
    let successor = Store::new(files.clone(), clock.clone());
    assert!(successor.authenticate(&issued.token).is_err());
    let second = successor
        .issue(&app.client_token_authority().unwrap())
        .unwrap();
    let observed = successor.authenticate(&second.token).unwrap();
    drop(successor);
    assert!(
        observed.revoked.has_changed().is_err(),
        "dropping the sole owner closes revocation evidence"
    );
    let restarted = Store::new(files, clock);
    assert!(restarted.authenticate(&second.token).is_err());
}
#[test]
fn expired_entries_release_the_fixed_capacity_and_old_handles_stay_revoked() {
    let (_root, _files, store, clock, mut app) = fixture();
    let authority = app.client_token_authority().unwrap();
    for _ in 0..CAPACITY {
        store.issue(&authority).unwrap();
    }
    assert!(matches!(
        store.issue(&authority),
        Err(IssueError::Store(TokenError::Busy))
    ));
    clock.advance(TTL);
    store.issue(&authority).unwrap();
    assert_eq!(store.entries.lock().unwrap().len(), 1);
}
#[test]
fn app_admissions_are_shared_but_keep_the_file_mutation_fence() {
    let (_root, files, store, _clock, mut app) = fixture();
    let issued = store.issue(&app.client_token_authority().unwrap()).unwrap();
    let session = store.authenticate(&issued.token).unwrap();
    let store = Arc::new(store);
    let (entered, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let worker = std::thread::spawn({
        let store = store.clone();
        let session = session.clone();
        move || {
            store.admit(&session, Scope::Read, |_| {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
            })
        }
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    let exclusive_refused = matches!(files.lock(), Err(TokenError::Busy));
    let shared_admitted = store.admit(&session, Scope::Operate, |_| ()).is_ok();
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert!(
        exclusive_refused,
        "app dispatch did not hold the shared file lock"
    );
    assert!(
        shared_admitted,
        "independent shared app admissions must coexist"
    );
}
#[test]
fn revocation_intent_fences_both_sources_and_waits_for_existing_callbacks() {
    let (_root, files, store, _clock, mut app) = fixture();
    let issued = store.issue(&app.client_token_authority().unwrap()).unwrap();
    let session = store.authenticate(&issued.token).unwrap();
    let file = files
        .issue(
            &app.client_token_authority().unwrap(),
            "browser",
            operator_scopes(),
        )
        .unwrap();
    let file_session = files.authenticate(&file.token).unwrap();
    let store = Arc::new(store);
    let (entered, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let worker = std::thread::spawn({
        let store = store.clone();
        let session = session.clone();
        move || {
            store.admit(&session, Scope::Read, |_| {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
            })
        }
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut pending = store.mutation().unwrap();
    assert!(priority::pending(&files.0).unwrap());
    let app_refused = matches!(
        store.admit(&session, Scope::Read, |_| ()),
        Err(TokenError::Busy)
    );
    let file_refused = matches!(
        files.admit(&file_session, Scope::Read, |_| ()),
        Err(TokenError::Busy)
    );
    std::thread::scope(|scope| {
        let (waiting, started) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        pending.pending.notify_waiting(waiting);
        let digest = session.digest;
        let revoke = scope.spawn(move || {
            done.send(pending.finish(|entries| Store::remove(entries, &digest)))
                .unwrap();
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let still_waiting = matches!(finished.try_recv(), Err(mpsc::TryRecvError::Empty));
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        finished
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        revoke.join().unwrap();
        assert!(
            still_waiting,
            "revocation returned while dispatch was in flight"
        );
    });
    assert!(
        app_refused && file_refused,
        "revocation intent must fence both sources"
    );
    assert!(matches!(
        store.admit(&session, Scope::Read, |_| ()),
        Err(TokenError::Revoked)
    ));
    assert!(files.admit(&file_session, Scope::Read, |_| ()).is_ok());
}

#[test]
fn exact_revocation_keeps_a_renewed_credential_independent() {
    let (_root, _files, store, _clock, mut app) = fixture();
    let authority = app.client_token_authority().unwrap();
    let first = store.issue(&authority).unwrap();
    let second = store.issue(&authority).unwrap();
    let first_session = store.authenticate(&first.token).unwrap();
    let second_session = store.authenticate(&second.token).unwrap();
    store.revoke(&first_session).unwrap();
    assert!(*first_session.revoked.borrow());
    assert!(!*second_session.revoked.borrow());
    assert!(matches!(
        store.revoke(&first_session),
        Err(TokenError::Revoked)
    ));
    assert!(matches!(
        store.admit(&first_session, Scope::Read, |_| ()),
        Err(TokenError::Revoked)
    ));
    assert!(store.admit(&second_session, Scope::Read, |_| ()).is_ok());
}

#[test]
fn revoke_all_cannot_cross_an_inflight_admission() {
    let (_root, _files, store, _clock, mut app) = fixture();
    let issued = store.issue(&app.client_token_authority().unwrap()).unwrap();
    let session = store.authenticate(&issued.token).unwrap();
    let store = Arc::new(store);
    let (entered, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let worker = std::thread::spawn({
        let store = store.clone();
        let session = session.clone();
        move || {
            store.admit(&session, Scope::Read, |_| {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
            })
        }
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    // The existing acquisition ceiling fails while the earlier callback remains live.
    let result = store.revoke_all();
    let signalled_early = *session.revoked.borrow();
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert!(matches!(result, Err(TokenError::Io(e)) if e.kind() == io::ErrorKind::TimedOut));
    assert!(!signalled_early);
    assert!(store.admit(&session, Scope::Read, |_| ()).is_ok());
    store.revoke_all().unwrap();
    assert!(*session.revoked.borrow());
    assert!(matches!(
        store.admit(&session, Scope::Read, |_| ()),
        Err(TokenError::Revoked)
    ));
}

#[test]
fn a_watch_for_another_peer_cannot_be_substituted_for_the_mint_authority() {
    let (_root, _files, store, _clock, mut app) = fixture();
    let peer = Peer::spawn();
    let watch = ExitWatch::arm(peer.credentials).unwrap();
    let result = store.issue_watched(&app.client_token_authority().unwrap(), watch);
    assert!(matches!(result, Err(IssueError::PeerProcessChanged(_))));
    assert!(store.entries.lock().unwrap().is_empty());
}

#[test]
fn an_exited_prearmed_peer_cannot_receive_a_credential() {
    let (_root, _files, store, _clock, mut app) = fixture();
    let mut peer = Peer::spawn();
    app.current_api_peer_pid = peer.credentials.pid;
    app.current_api_peer_uid = Some(peer.credentials.uid);
    app.current_api_peer_birth = Some(peer.credentials.birth);
    let watch = ExitWatch::arm(peer.credentials).unwrap();
    let authority = app.client_token_authority().unwrap();
    peer.exit();
    assert!(readable(watch.as_raw_fd(), Duration::from_secs(5)));
    assert!(matches!(
        store.issue_watched(&authority, watch),
        Err(IssueError::PeerProcessChanged(_))
    ));
    assert!(store.entries.lock().unwrap().is_empty());
}
