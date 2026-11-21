//! Exercise the producer seam directly; journal admission remains P2.
use super::*;
use crate::structured::{
    history::Item,
    publication::causal::{Ack, Cause},
};

async fn synced(case: &mut Case, socket: &mut Socket, id: &str) -> u64 {
    socket
        .send(json!({"t":"sub", "sub_id":id,"stream":format!("agent/{}",case.worker.id)}))
        .await;
    loop {
        let Received::Text(frame) = case.frame(socket).await else {
            panic!("text required")
        };
        assert_ne!(frame["t"], "subfail");
        if frame["t"] == "synced" {
            return frame["seq"].as_u64().unwrap();
        }
    }
}
fn sender(case: &Case, id: u64) -> mpsc::Sender<output::Delivery> {
    case.server
        .core
        .shared
        .clients
        .lock()
        .unwrap()
        .get(&id)
        .unwrap()
        .data
        .clone()
}
fn bind(case: &Case, id: u64, ack: Ack) -> Ack {
    case.server
        .core
        .shared
        .clients
        .lock()
        .unwrap()
        .get_mut(&id)
        .unwrap()
        .causal
        .bind(ack)
        .unwrap()
}

fn ack(lease: Ack) -> output::Delivery {
    output::Delivery {
        frame: wire_commands::ack("causal-command".into(), Ok(json!({}))),
        active: None,
        delivered: None,
        causal: Some(lease),
    }
}
async fn trace(vanish: bool) {
    let mut case = Case::new(if vanish { 183196 } else { 183195 }).await;
    let args = case.prompt().await;
    let mut origin = case.open().await;
    let origin_id = *case
        .server
        .core
        .shared
        .clients
        .lock()
        .unwrap()
        .keys()
        .next()
        .unwrap();
    let origin_data = sender(&case, origin_id);
    let mut observer = case.open().await;
    let observer_id = *case
        .server
        .core
        .shared
        .clients
        .lock()
        .unwrap()
        .keys()
        .find(|id| **id != origin_id)
        .unwrap();
    let observer_data = sender(&case, observer_id);
    let cursor = synced(&mut case, &mut origin, "origin").await;
    assert_eq!(synced(&mut case, &mut observer, "observer").await, cursor);
    let source = case
        .app
        .structured_workers
        .history(&case.app.state.structured_agents, &case.worker.id)
        .unwrap()
        .clone();
    let (cause, lease) = Cause::hold();
    let lease = bind(&case, origin_id, lease);
    let joined = (!vanish).then(|| bind(&case, observer_id, cause.join().unwrap()));
    let runtime = case
        .app
        .structured_workers
        .runtime(&case.worker.id, case.worker.generation)
        .unwrap();
    let store = &case.server.core.shared.store;
    let session = store.authenticate(&Bearer("a".repeat(64))).unwrap();
    store
        .admit(
            &session,
            crate::structured::permissions::auth::tokens::Scope::Answer,
            |token| {
                runtime.agent_decide_caused(
                    &crate::structured::permissions::Principal::Token(token),
                    &serde_json::from_value(args).unwrap(),
                    cause.clone(),
                )
            },
        )
        .unwrap()
        .unwrap();
    // Poll real durable bytes, not state constructed by the test.
    within(async {
        loop {
            if case.events().iter().any(|r| {
                r["type"] == "flk_permission_request"
                    && r["request_id"] == "first-request"
                    && r["state"] == "closed"
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let mut cut = source.subscribe(Some(cursor)).unwrap();
    assert!(matches!(cut.page().unwrap(), Item::Synced(n) if n == cursor));
    // The persisted tail must not change the new cut or live continuation.
    assert!(tokio::time::timeout(Duration::from_millis(50), async {
        loop {
            if !matches!(cut.page().unwrap(), Item::Pending) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_err());
    let mut fresh = case.open().await;
    assert_eq!(synced(&mut case, &mut fresh, "fresh").await, cursor);
    for socket in [&mut origin, &mut observer, &mut fresh] {
        socket.send(json!({"t":"ping","nonce":"held-probe"})).await;
        let Received::Text(frame) = case.frame(socket).await else {
            panic!("pong required")
        };
        assert_eq!(frame, json!({"t":"pong","nonce":"held-probe"}));
    }
    if vanish {
        drop(origin);
        within(async {
            while case
                .server
                .core
                .shared
                .clients
                .lock()
                .unwrap()
                .contains_key(&origin_id)
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
        // Teardown releases the registered waiter while the unsent token still lives.
        within(cause.released()).await;
        drop(lease);
    } else {
        origin_data.send(ack(lease)).await.unwrap();
        let Received::Text(frame) = case.frame(&mut origin).await else {
            panic!("ack required")
        };
        assert_eq!(frame["t"], "ack");
    }
    if let Some(joined) = joined {
        // A duplicate on another socket retains the hold after the origin flush.
        assert!(matches!(cut.page().unwrap(), Item::Pending));
        observer_data.send(ack(joined)).await.unwrap();
        let Received::Text(frame) = case.frame(&mut observer).await else {
            panic!("joined ack required")
        };
        assert_eq!(frame["t"], "ack");
    }
    within(cause.released()).await;
    for socket in [&mut observer, &mut fresh] {
        let Received::Text(frame) = case.frame(socket).await else {
            panic!("closed event required")
        };
        assert_eq!(frame["t"], "ev");
        assert_eq!(frame["seq"], cursor + 1);
        assert_eq!(frame["ev"]["state"], "closed");
    }
    case.stop().await;
}
#[test]
fn causal_history_is_private_to_origin_observer_and_fresh_subscription_until_ack() {
    runtime(trace(false));
}
#[test]
fn causal_origin_loss_releases_publication_without_leaking_the_hold() {
    runtime(trace(true));
}
