use super::*;
use crate::platform::process_exit::tests::{readable, Peer};
use std::{os::fd::AsRawFd, time::Duration};
fn app() -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: crate::platform::peer::effective_uid(),
        dedicated: Ok(None).into(),
    };
    app
}
fn refuses(app: &mut App, peer: Credentials, code: ErrorCode) {
    assert_eq!(app.admit_local_peer(peer).err(), Some(code));
}
#[test]
fn kernel_peer_admission_is_independent_of_the_current_api_request() {
    let mut app = app();
    let child = Peer::spawn();
    app.current_api_peer_pid = None;
    app.current_api_peer_uid = Some(None);
    app.current_api_peer_birth = Some(None);
    let admitted = app.admit_local_peer(child.credentials).unwrap();
    assert!(admitted.process().matches(child.credentials));
    assert_eq!(app.current_api_peer_pid, None);
    assert_eq!(app.current_api_peer_uid, Some(None));
    assert_eq!(app.current_api_peer_birth, Some(None));
}
#[test]
fn strict_uid_pid_birth_and_exit_fail_closed_with_registered_reasons() {
    let mut app = app();
    let mut child = Peer::spawn();
    let original = child.credentials;
    for uid in [None, original.uid.map(|uid| uid.wrapping_add(1))] {
        refuses(
            &mut app,
            Credentials {
                uid,
                birth: None,
                ..original
            },
            ErrorCode::OperatorOnly,
        );
    }
    for facts in [
        Credentials {
            pid: None,
            ..original
        },
        Credentials {
            birth: None,
            ..original
        },
        Credentials {
            birth: original.birth.map(|n| n.wrapping_add(1)),
            ..original
        },
    ] {
        refuses(&mut app, facts, ErrorCode::PeerProcessChanged);
    }
    app.runtime_config.uid_policy.dedicated = Ok(original.uid).into();
    refuses(&mut app, original, ErrorCode::OperatorOnly);
    app.runtime_config.uid_policy.dedicated = Ok(None).into();
    let admitted = app.admit_local_peer(original).unwrap();
    child.exit();
    assert!(readable(
        admitted.process().as_raw_fd(),
        Duration::from_secs(5)
    ));
    refuses(&mut app, original, ErrorCode::PeerProcessChanged);
}
#[tokio::test]
async fn real_pane_agent_and_mesh_origins_cannot_become_local_operators() {
    let mut app = app();
    let child = Peer::spawn();
    app.inbound
        .attach(
            child.credentials.pid.unwrap(),
            child.credentials.birth.unwrap(),
            crate::platform::process_start_time,
        )
        .unwrap();
    refuses(&mut app, child.credentials, ErrorCode::OperatorOnly);
    drop(app);
    let mut app = self::app();
    let mut lease = app
        .session_leases
        .reserve("fixture-agent", "fixture-session", None)
        .unwrap();
    lease.bind_test_peer(child.credentials.pid.unwrap());
    refuses(&mut app, child.credentials, ErrorCode::OperatorOnly);
    drop(lease);
    drop(app);
    let mut app = self::app();
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("operator-pane")];
    app.state.ensure_test_terminals();
    let pane = app.state.workspaces[0].focused_pane_id().unwrap();
    crate::test_support::attest_pane_caller(&mut app, pane);
    let pid = app.current_api_peer_pid.unwrap();
    let pane_peer = Credentials {
        pid: Some(pid),
        uid: Some(crate::platform::peer::effective_uid()),
        birth: crate::platform::process_start_time(pid),
    };
    refuses(&mut app, pane_peer, ErrorCode::OperatorOnly);
    refuses(
        &mut app,
        Credentials {
            birth: None,
            ..pane_peer
        },
        ErrorCode::OperatorOnly,
    );
}

#[test]
fn exit_during_ancestry_refuses_the_unreaped_peer() {
    let mut app = app();
    let mut child = Peer::spawn();
    let credentials = child.credentials;
    let mut exited = false;
    let result = app.admit_local_peer_using(credentials, |pid| {
        let facts = crate::platform::live_process_facts(pid);
        if Some(pid) == credentials.pid && !exited {
            assert!(facts.is_some());
            child.exit_unreaped();
            exited = true;
        }
        facts
    });
    assert!(
        exited,
        "the real peer must exit after its kernel watch was armed"
    );
    assert_eq!(result.err(), Some(ErrorCode::PeerProcessChanged));
}

#[test]
fn incomplete_ancestry_refuses_an_otherwise_live_peer() {
    let mut app = app();
    let child = Peer::spawn();
    let mut read = false;
    let result = app.admit_local_peer_using(child.credentials, |_| {
        read = true;
        None
    });
    assert!(read, "admission must request ancestry evidence");
    assert_eq!(result.err(), Some(ErrorCode::OperatorOnly));
}
