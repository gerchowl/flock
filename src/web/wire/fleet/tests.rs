use super::*;
use crate::structured::{launch::SessionConfigDir, registry::StructuredRuntime};
fn registry(root: &Path) -> Registry {
    Registry::restore(vec![Record {
        runtime: StructuredRuntime::Structured,
        agent_id: "fixture-agent".into(),
        name: None,
        harness: "claude".into(),
        session_id: "fixture-session".into(),
        cwd: root.to_owned(),
        model: None,
    }])
    .unwrap()
}
#[tokio::test]
async fn unavailable_observation_preserves_rows_and_fresh_readers_can_recover() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let (stop, stopped) = watch::channel(false);
        let wake = Arc::new(Notify::new());
        let client = Client::new(wake.clone(), stopped);
        let registry = registry(root.path());
        let workers = Workers::new(wake);
        client.observe(&registry, &workers, root.path(), None, None);
        let mut cut = client.subscribe(None).await.unwrap();
        for _ in 0..3 {
            cut.next("old").await.unwrap();
        }
        let path = root.path().join("fleet.json");
        let before = std::fs::read(&path).unwrap();
        client.updates.send_replace(Some(Arc::new((
            path.clone(),
            Err(crate::api::error_code::ErrorCode::SessionBusy),
        ))));
        assert!(
            matches!(client.subscribe(Some(2)).await, Err(e) if e.kind()==io::ErrorKind::WouldBlock)
        );
        assert!(cut.next("old").await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        client.observe(&registry, &workers, root.path(), None, None);
        let mut fresh = client.subscribe(Some(2)).await.unwrap();
        assert!(matches!(
            fresh.next("fresh").await.unwrap(),
            ServerFrame::Gap {
                to: 2,
                snapshot_to: Some(3),
                ..
            }
        ));
        stop.send_replace(true);
        client.closed().await;
    })
    .await
    .expect("fleet owner stayed silent");
}
#[tokio::test]
async fn cancelled_cut_does_not_allocate_a_checkpoint() {
    tokio::time::timeout(Duration::from_secs(5), async {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (stop, stopped) = watch::channel(false); let wake = Arc::new(Notify::new());
    let client = Client::new(wake.clone(), stopped);
    let registry = registry(root.path()); let workers = Workers::new(wake);
    client.observe(&registry, &workers, root.path(), None, None);
    let (reply, cancelled) = oneshot::channel(); drop(cancelled);
    client.requests.try_send((None,reply)).unwrap();
    assert!(matches!(client.subscribe(Some(100)).await, Err(e) if e.kind()==io::ErrorKind::InvalidInput));
    let stored:serde_json::Value=serde_json::from_slice(&std::fs::read(root.path().join("fleet.json")).unwrap()).unwrap();
    assert_eq!(stored["high_water"], 1);
    stop.send_replace(true); client.closed().await;
    }).await.expect("fleet owner stayed silent");
}
#[test]
fn oversized_permission_row_is_lagged_before_it_enters_the_live_ring() {
    let delta = Delta {
        seq: 10,
        event: crate::structured::fleet_lite::FleetDelta::Upsert {
            row: crate::structured::fleet_lite::FleetAgentRow {
                id: "fixture-agent".into(),
                label: "Example".into(),
                state: crate::api::agent_state::AgentState::unknown(),
                activity: None,
                route: None,
                node_id: None,
                project: None,
                waiting_on_human: vec!["r".repeat(MAX_MESSAGE)],
            },
        },
    };
    let frame = event(delta, 42, 10).unwrap();
    assert!(matches!(
        frame.as_ref(),
        ServerFrame::Lagged { skipped_to: 10, .. }
    ));
    assert!(super::super::output::fits(&frame));
}
#[tokio::test]
async fn preparing_worker_is_unavailable_until_its_real_permission_book_is_ready() {
    tokio::time::timeout(Duration::from_secs(5), async {
        use crate::structured::workers::tests::Case as Worker;
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let (stop, stopped) = watch::channel(false);
        let wake = Arc::new(Notify::new());
        let client = Client::new(wake.clone(), stopped);
        let mut registry = Registry::default();
        let mut workers = Workers::new(wake.clone());
        let (mut worker, launch, factory) =
            Worker::prepare(&mut registry, 2226, "state-reducer", vec![]);
        let (entered, started) = oneshot::channel();
        let (release, proceed) = std::sync::mpsc::channel();
        worker.generation = workers
            .start(&mut registry, launch, move |record| {
                entered.send(()).unwrap();
                proceed
                    .recv_timeout(Duration::from_secs(5))
                    .expect("profile producer disappeared or stayed silent");
                factory(record)
            })
            .unwrap();
        started.await.unwrap();
        assert!(workers.runtime(&worker.id, worker.generation).is_none());
        client.observe(&registry, &workers, root.path(), None, None);
        assert!(
            matches!(client.subscribe(None).await, Err(e) if e.kind()==io::ErrorKind::WouldBlock)
        );
        assert!(!root.path().join("fleet.json").exists());
        release.send(()).unwrap();
        let now = Instant::now();
        worker.ready(&mut registry, &mut workers, &wake, now).await;
        client.observe(&registry, &workers, root.path(), None, None);
        let mut cut = client.subscribe(None).await.unwrap();
        assert!(matches!(
            cut.next("ready").await.unwrap(),
            ServerFrame::Gap { .. }
        ));
        worker
            .finish(&mut registry, &mut workers, &wake, now, None)
            .await;
        stop.send_replace(true);
        client.closed().await;
    })
    .await
    .expect("fleet preparation producer stayed silent");
}

#[test]
fn fleet_frame_bound_reserves_the_maximum_escaped_subscription_id() {
    let mut row = crate::structured::fleet_lite::FleetAgentRow {
        id: "fixture-agent".into(),
        label: "Example".into(),
        state: crate::api::agent_state::AgentState::unknown(),
        activity: None,
        route: None,
        node_id: None,
        project: None,
        waiting_on_human: vec![String::new()],
    };
    let envelope = ServerFrame::Ev {
        sub_id: String::new(),
        stream: "fleet".into(),
        seq: Some(1),
        ts_ms: 0,
        ev: WireEvent::Json(
            serde_json::to_value(crate::structured::fleet_lite::FleetDelta::Upsert {
                row: row.clone(),
            })
            .unwrap(),
        ),
    };
    let available = MAX_MESSAGE - serde_json::to_vec(&envelope).unwrap().len();
    row.waiting_on_human[0] = "r".repeat(available);
    let delta = |row| Delta {
        seq: 1,
        event: crate::structured::fleet_lite::FleetDelta::Upsert { row },
    };
    assert!(matches!(
        &*event(delta(row.clone()), 0, 1).unwrap(),
        ServerFrame::Lagged { .. }
    ));
    row.waiting_on_human[0].truncate(available - 6 * super::super::subscriptions::ID_BYTES);
    let mut accepted = (*event(delta(row), 0, 1).unwrap()).clone();
    let ServerFrame::Ev { sub_id, .. } = &mut accepted else {
        panic!("boundary row must fit")
    };
    *sub_id = "\0".repeat(super::super::subscriptions::ID_BYTES);
    assert!(super::super::output::encode(accepted).is_some());
}

#[tokio::test]
async fn non_utf8_observation_keeps_the_publisher_and_live_reader_running() {
    use std::os::unix::ffi::OsStringExt;
    tokio::time::timeout(Duration::from_secs(5), async {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let (stop, stopped) = watch::channel(false);
        let client = Client::new(Arc::new(Notify::new()), stopped);
        let normal = registry(root.path()).records().remove(0);
        let mut odd = normal.clone();
        odd.agent_id = "odd".into();
        odd.session_id = "odd-session".into();
        odd.cwd.push(std::ffi::OsString::from_vec(vec![0xff]));
        let state = crate::api::agent_state::AgentState::unknown();
        let mut local = Local {
            agents: vec![
                Agent(odd, state.clone(), vec![]),
                Agent(normal, state, vec![]),
            ],
            home: Some(crate::mesh::identity::NodeIdentity::fixture([72; 32]).node_id()),
            nodes: None,
        };
        // Exercise the typed publisher boundary. Registry JSON admission already
        // rejects non-UTF-8 paths, independently of this projection's contract.
        let path = root.path().join("fleet.json");
        client
            .updates
            .send_replace(Some(Arc::new((path.clone(), Ok(local.clone())))));
        let mut cut = client.subscribe(None).await.unwrap();
        let mut rows = Vec::new();
        loop {
            let frame = cut.next("odd-cwd").await.unwrap();
            if matches!(frame, ServerFrame::Synced { .. }) {
                break;
            }
            if let ServerFrame::Ev {
                ev: WireEvent::Json(row),
                ..
            } = frame
            {
                rows.push(row);
            }
        }
        assert_eq!(rows.len(), 4);
        let odd = &rows.iter().find(|r| r["row"]["id"] == "odd").unwrap()["row"];
        assert!(odd.get("project").is_none());
        assert_eq!(odd["node_id"], local.home.as_deref().unwrap());
        assert_eq!(
            rows.iter()
                .filter(|r| r["type"] == "fleet.project.upsert")
                .count(),
            1
        );
        local.agents.pop();
        client
            .updates
            .send_replace(Some(Arc::new((path, Ok(local)))));
        let mut removed = Vec::new();
        for _ in 0..2 {
            let ServerFrame::Ev {
                ev: WireEvent::Json(ev),
                ..
            } = cut.next("odd-cwd").await.unwrap()
            else {
                panic!("expected live removal");
            };
            removed.push(ev["type"].clone());
        }
        assert!(removed.contains(&serde_json::json!("fleet.remove")));
        assert!(removed.contains(&serde_json::json!("fleet.project.remove")));
        stop.send_replace(true);
        client.closed().await;
    })
    .await
    .expect("fleet observation producer stayed silent");
}
