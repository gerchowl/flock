use super::*;
use crate::{
    mesh::{
        command::{self, Frame, Target, TokenSource},
        identity::NodeIdentity,
    },
    structured::launch::SessionConfigDir,
    web::wire::commands::Command,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

fn run(future: impl std::future::Future<Output = ()>) {
    let (_guard, _scrub) = crate::config::test_config_env_guard();
    std::env::set_var("FLOCK_TEST_MESH_VERSION", "6");
    std::env::set_var("FLOCK_TEST_MESH_MIN", "5");
    crate::web::wire::tests::runtime(future);
}
fn args_digest(args: &Value) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"flock-m1-args-v1\0");
    hash.update(serde_json::to_vec(args).unwrap());
    hash.finalize().into()
}
fn pair(wall_ms: u64, elapsed_ms: u64) -> ClockPair {
    ClockPair {
        domain: "fixture-boot".into(),
        wall_ms,
        elapsed_ms,
    }
}
fn destination() -> Destination {
    Destination {
        node: "b".repeat(64),
        read: Box::new(|_| {
            Ok((
                Some(pair(11000, 21000)),
                Some("w1".into()),
                Some(Duration::from_millis(21000)),
            ))
        }),
    }
}
struct Fixture {
    _root: SessionConfigDir,
    owner: Ownership,
    store: Arc<gateway::Store>,
    generation: u64,
    body: Body,
    command: Command,
}
impl Fixture {
    async fn new(agent: bool, uses: Option<u64>, timed: bool) -> Self {
        let (root, store, generation) = gateway::attested_fixture(agent, uses, timed);
        let owner = Ownership::new(root.path().to_owned(), Arc::new(Notify::new()));
        owner.acquire().await.unwrap();
        let command = Command {
            id: "verified-command".into(),
            verb: "agent.stop".into(),
            args: json!({"target":"agent_atlas_1"}),
            resume_only: false,
            read_only: false,
        };
        let body = Body {
            home_node_id: NodeIdentity::fixture([7; 32]).node_id(),
            principal: if agent {
                "agent_atlas_subject"
            } else {
                "client-1"
            }
            .into(),
            token_source: if agent {
                TokenSource::Agent
            } else {
                TokenSource::App
            },
            scopes: [crate::api::client_tokens::Scope::Operate].into(),
            destination_node_id: "b".repeat(64),
            target: Target::Agent("agent_atlas_1".into()),
            cmd_id: command.id.clone(),
            verb: command.verb.clone(),
            args_digest: args_digest(&command.args),
            requested_scope: crate::api::client_tokens::Scope::Operate,
            budget_ms: command::BUDGET_MS,
        };
        Self {
            _root: root,
            owner,
            store: Arc::new(store),
            generation,
            body,
            command,
        }
    }
    fn frame(&self) -> Frame {
        command::tests::admission_frame(self.body.clone())
    }
    async fn submit(
        &self,
        destination: Destination,
    ) -> (journal::Admission, Option<GrantAdmission>) {
        self.submit_frame(self.frame(), self.command.clone(), destination)
            .await
    }
    async fn submit_frame(
        &self,
        frame: Frame,
        command: Command,
        destination: Destination,
    ) -> (journal::Admission, Option<GrantAdmission>) {
        let ingress = Ingress {
            frame,
            since: Some(Duration::ZERO),
            command,
        };
        admit(
            &self.owner,
            ingress,
            self.store.clone(),
            self.generation,
            destination,
        )
        .await
        .unwrap()
    }
    async fn refused(&self, dest: Destination, reason: Refusal) {
        let observation = self.owner.observe_writes_test();
        let (admission, grant) = self.submit(dest).await;
        assert!(matches!(admission, journal::Admission::Refused(actual) if actual == reason));
        assert!(grant.is_none());
        assert_eq!(observation.sample().appended, 0);
    }
}
fn claimed(result: (journal::Admission, Option<GrantAdmission>), ordinal: u64) {
    let (admission, grant) = result;
    assert!(
        matches!(admission, journal::Admission::Granted { consumption, state }
        if consumption.ordinal == ordinal && matches!(*state, journal::Admission::Claimed(_)))
    );
    assert!(grant.is_some());
}
#[test]
fn users_and_agents_once() {
    run(async {
        for agent in [false, true] {
            let mut f = Fixture::new(agent, Some(1), false).await;
            claimed(f.submit(destination()).await, 1);
            f.command.id = "second".into();
            f.body.cmd_id = f.command.id.clone();
            f.refused(destination(), Refusal::Exhausted).await;
            f.owner.shutdown().await.unwrap();
        }
    });
}
#[test]
fn users_and_agents_count() {
    run(async {
        for agent in [false, true] {
            let mut f = Fixture::new(agent, Some(2), false).await;
            for ordinal in 1..=2 {
                f.command.id = ordinal.to_string();
                f.body.cmd_id = f.command.id.clone();
                claimed(f.submit(destination()).await, ordinal);
            }
            f.command.id = "third".into();
            f.body.cmd_id = f.command.id.clone();
            f.refused(destination(), Refusal::Exhausted).await;
            f.owner.shutdown().await.unwrap();
        }
    });
}
#[test]
fn users_and_agents_expiry() {
    run(async {
        for agent in [false, true] {
            let mut f = Fixture::new(agent, None, true).await;
            claimed(f.submit(destination()).await, 1);
            f.command.id = "expired".into();
            f.body.cmd_id = f.command.id.clone();
            let mut dest = destination();
            dest.read = Box::new(|_| {
                Ok((
                    Some(pair(20000, 30000)),
                    Some("w1".into()),
                    Some(Duration::from_millis(21000)),
                ))
            });
            f.refused(dest, Refusal::Expired).await;
            f.owner.shutdown().await.unwrap();
        }
    });
}
#[test]
fn enrollment_refuses_without_claim() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        let mut dest = destination();
        dest.read = Box::new(|_| Err(Refusal::Enrollment));
        f.refused(dest, Refusal::Enrollment).await;
        real_destination_refusals(&f).await;
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn membership_refuses_without_claim() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        let mut dest = destination();
        dest.read = Box::new(|_| Ok((None, None, Some(Duration::from_millis(21000)))));
        f.refused(dest, Refusal::Target).await;
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn verb_policy_refuses_without_claim() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        let mut dest = destination();
        dest.read = Box::new(|_| Err(Refusal::Verb));
        f.refused(dest, Refusal::Verb).await;
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn destination_binding_refuses_without_claim() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        let mut dest = destination();
        dest.node = "c".repeat(64);
        f.refused(dest, Refusal::Binding).await;
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn payload_types_values_and_verb_mismatch_refuse() {
    run(async {
        let mut f = Fixture::new(false, Some(1), false).await;
        let observation = f.owner.observe_writes_test();
        f.command.args["extra"] = json!(1);
        f.body.args_digest = args_digest(&f.command.args);
        for value in [json!("1"), json!(1.0), json!(2)] {
            let mut cmd = f.command.clone();
            cmd.args["extra"] = value;
            let (result, authority) = f.submit_frame(f.frame(), cmd, destination()).await;
            assert!(matches!(
                result,
                journal::Admission::Refused(Refusal::Binding)
            ));
            assert!(authority.is_none());
        }
        let mut cmd = f.command.clone();
        cmd.verb = "agent.restart".into();
        let (result, authority) = f.submit_frame(f.frame(), cmd, destination()).await;
        assert!(matches!(
            result,
            journal::Admission::Refused(Refusal::Binding)
        ));
        assert!(authority.is_none());
        assert_eq!(observation.sample().appended, 0);
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn equivalent_json_retries_do_not_consume_again() {
    run(async {
        let mut f = Fixture::new(false, Some(1), false).await;
        f.command.args = serde_json::from_str(
            r#" { "extra": {"z":1,"a":[true,null]}, "target": "agent_atlas_1" } "#,
        )
        .unwrap();
        f.body.args_digest = args_digest(&f.command.args);
        claimed(f.submit(destination()).await, 1);
        f.command.args =
            serde_json::from_str(r#"{"target":"agent_atlas_1","extra":{"a":[true,null],"z":1}}"#)
                .unwrap();
        let (retry, grant) = f.submit(destination()).await;
        assert!(
            matches!(retry, journal::Admission::Granted { state, consumption }
            if consumption.ordinal == 1 && matches!(*state, journal::Admission::Pending(_)))
        );
        assert!(grant.is_none());
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn once_expiring_between_preflight_and_claim_refuses() {
    run(async {
        let f = Fixture::new(false, Some(1), true).await;
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let mut dest = destination();
        dest.read = Box::new(move |_| {
            let elapsed = if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                21000
            } else {
                30000
            };
            Ok((
                Some(pair(elapsed - 10000, elapsed)),
                Some("w1".into()),
                Some(Duration::from_millis(21000)),
            ))
        });
        f.refused(dest, Refusal::Expired).await;
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn concurrent_final_use_admits_exactly_one() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        let mut second = f.command.clone();
        second.id = "competitor".into();
        let mut body = f.body.clone();
        body.cmd_id = second.id.clone();
        let (one, two) = tokio::join!(
            f.submit(destination()),
            f.submit_frame(command::tests::admission_frame(body), second, destination()),
        );
        let outcomes = [one, two];
        assert_eq!(
            outcomes.iter().filter(|(_, grant)| grant.is_some()).count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|(a, _)| matches!(a, journal::Admission::Refused(Refusal::Exhausted)))
                .count(),
            1
        );
        f.owner.shutdown().await.unwrap();
    });
}

#[test]
fn file_tokens_refuse_without_claim() {
    run(async {
        let mut f = Fixture::new(false, Some(1), false).await;
        f.body.token_source = TokenSource::File;
        f.refused(destination(), Refusal::Principal).await;
        f.body.token_source = TokenSource::App;
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn forged_signature_refuses_without_claim() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        let observation = f.owner.observe_writes_test();
        let mut frame = serde_json::to_value(f.frame()).unwrap();
        frame["envelope"]["signature"][0] =
            json!(frame["envelope"]["signature"][0].as_u64().unwrap() ^ 1);
        let (result, authority) = f
            .submit_frame(
                serde_json::from_value(frame).unwrap(),
                f.command.clone(),
                destination(),
            )
            .await;
        assert!(matches!(
            result,
            journal::Admission::Refused(Refusal::Provenance)
        ));
        assert!(authority.is_none());
        assert_eq!(observation.sample().appended, 0);
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn signed_foreign_origin_refuses_without_claim() {
    run(async {
        let mut f = Fixture::new(false, Some(1), false).await;
        f.body.home_node_id = "c".repeat(64);
        f.refused(destination(), Refusal::Binding).await;
        f.body.home_node_id = NodeIdentity::fixture([7; 32]).node_id();
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn budget_expiring_after_preflight_refuses() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let mut dest = destination();
        dest.read = Box::new(move |_| {
            let ms = if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                29999
            } else {
                30000
            };
            Ok((None, Some("w1".into()), Some(Duration::from_millis(ms))))
        });
        f.refused(dest, Refusal::Budget).await;
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        claimed(f.submit(destination()).await, 1);
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn changed_target_and_payload_under_existing_key_refuse() {
    run(async {
        let f = Fixture::new(false, Some(1), false).await;
        claimed(f.submit(destination()).await, 1);
        for args in [
            json!({"target":"agent_atlas_2"}),
            json!({"target":"agent_atlas_1", "extra":true}),
        ] {
            let mut cmd = f.command.clone();
            cmd.args = args;
            let mut body = f.body.clone();
            body.args_digest = args_digest(&cmd.args);
            if let Some(target) = cmd.args["target"].as_str() {
                body.target = Target::Agent(target.into());
            }
            let ingress = Ingress {
                frame: command::tests::admission_frame(body),
                since: Some(Duration::ZERO),
                command: cmd,
            };
            assert!(matches!(
                admit(
                    &f.owner,
                    ingress,
                    f.store.clone(),
                    f.generation,
                    destination()
                )
                .await,
                Err(ErrorCode::InvalidParams)
            ));
        }
        f.owner.shutdown().await.unwrap();
    });
}
#[test]
fn clock_latch_refuses_timed_and_allows_untimed() {
    run(async {
        let f = Fixture::new(false, Some(2), true).await;
        claimed(f.submit(destination()).await, 1);
        let mut dest = destination();
        dest.read = Box::new(|_| {
            Ok((
                Some(pair(10500, 21500)),
                Some("w1".into()),
                Some(Duration::from_millis(21500)),
            ))
        });
        f.refused(dest, Refusal::Expired).await;
        f.refused(destination(), Refusal::Expired).await;
        let (generation, _) = gateway::claim_install(&f.store, "standing", None);
        // Add an attested untimed sibling through the fixture's operator path.
        gateway::attest_sibling(&f.store, "standing", &f.body.home_node_id);
        let mut ingress = Ingress {
            frame: f.frame(),
            since: Some(Duration::ZERO),
            command: f.command.clone(),
        };
        ingress.command.id = "untimed".into();
        let mut body = f.body.clone();
        body.cmd_id = ingress.command.id.clone();
        ingress.frame = command::tests::admission_frame(body);
        claimed(
            admit(
                &f.owner,
                ingress,
                f.store.clone(),
                generation,
                destination(),
            )
            .await
            .unwrap(),
            1,
        );
        f.owner.shutdown().await.unwrap();
    });
}

pub(crate) async fn dispatch_fixture(
    app: &mut crate::app::App,
    verb: &str,
    args: Value,
    target: &str,
) -> Value {
    let mut f = Fixture::new(false, Some(1), false).await;
    f.command.verb = verb.into();
    f.command.args = args;
    f.body.verb = verb.into();
    f.body.args_digest = args_digest(&f.command.args);
    f.body.target = if verb == "agent.start" {
        Target::Workspace(target.into())
    } else {
        Target::Agent(target.into())
    };
    let signer = NodeIdentity::fixture([7; 32]);
    crate::mesh::hello::with_store(|store| {
        store
            .put_pin(
                "home.example",
                &crate::mesh::store::IdentityPin {
                    node_id: signer.node_id(),
                    public_key: signer.public_key().to_vec(),
                },
            )
            .map_err(|e| e.to_string())
    })
    .unwrap();
    let ingress = Ingress {
        frame: f.frame(),
        since: crate::platform::suspend_clock::now(&crate::platform::suspend_clock::Kernel).ok(),
        command: f.command,
    };
    let result = execute(app, &f.owner, ingress).await;
    f.owner.shutdown().await.unwrap();
    match result.unwrap() {
        journal::Admission::Granted { state, .. } => match *state {
            journal::Admission::Replayed(frame) => serde_json::to_value(frame).unwrap(),
            _ => panic!("new execution must persist its result"),
        },
        journal::Admission::Refused(reason) => panic!("destination refused: {reason:?}"),
        _ => panic!("expected a granted execution"),
    }
}

async fn real_destination_refusals(f: &Fixture) {
    let _mesh = crate::mesh::runtime_store::TestStore::new();
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = crate::app::App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.node_id = Some("b".repeat(64));
    let ingress = || Ingress {
        frame: f.frame(),
        since: crate::platform::suspend_clock::now(&crate::platform::suspend_clock::Kernel).ok(),
        command: f.command.clone(),
    };
    let observation = f.owner.observe_writes_test();
    assert!(matches!(
        execute(&mut app, &f.owner, ingress()).await.unwrap(),
        journal::Admission::Refused(Refusal::Enrollment)
    ));
    let signer = NodeIdentity::fixture([7; 32]);
    crate::mesh::hello::with_store(|store| {
        store
            .put_pin(
                "home.example",
                &crate::mesh::store::IdentityPin {
                    node_id: signer.node_id(),
                    public_key: signer.public_key().to_vec(),
                },
            )
            .map_err(|e| e.to_string())
    })
    .unwrap();
    assert!(matches!(
        execute(&mut app, &f.owner, ingress()).await.unwrap(),
        journal::Admission::Refused(Refusal::Target)
    ));
    let mut ws = crate::workspace::Workspace::test_new("policy-target");
    ws.id = "w1".into();
    app.state.workspaces = vec![ws];
    app.state.config.fleet.max_spawn_depth = 0;
    let mut cmd = f.command.clone();
    cmd.id = "policy-refused".into();
    cmd.verb = "agent.start".into();
    cmd.args =
        json!({"name":"refused", "runtime":"structured", "argv":["claude"], "workspace_id":"w1"});
    let mut body = f.body.clone();
    body.cmd_id = cmd.id.clone();
    body.verb = cmd.verb.clone();
    body.target = Target::Workspace("w1".into());
    body.args_digest = args_digest(&cmd.args);
    let request = Ingress {
        frame: command::tests::admission_frame(body),
        since: crate::platform::suspend_clock::now(&crate::platform::suspend_clock::Kernel).ok(),
        command: cmd,
    };
    assert!(matches!(
        execute(&mut app, &f.owner, request).await.unwrap(),
        journal::Admission::Refused(Refusal::Verb)
    ));
    assert_eq!(observation.sample().appended, 0);
}
