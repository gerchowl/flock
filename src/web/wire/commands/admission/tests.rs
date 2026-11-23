use super::*;
use crate::{
    structured::permissions::auth::{gateway, tokens::Scope},
    web::wire::commands::Command,
};
use serde_json::{json, Value};
use std::sync::{atomic::Ordering, Arc};
use tokio::sync::Notify;

fn run(future: impl std::future::Future<Output = ()>) {
    let (_guard, _scrub) = crate::config::test_config_env_guard();
    crate::web::wire::tests::runtime(future);
}
pub(crate) async fn issue(
    verb: &str,
    args: Value,
    target: &str,
    workspace: &str,
) -> GrantAdmission {
    let (root, store, generation, issuance) = gateway::claim_fixture();
    let owner = Ownership::new(root.path().to_owned(), Arc::new(Notify::new()));
    owner.acquire().await.unwrap();
    let phase = owner.gate().load(Ordering::Acquire);
    let claim = request(
        Arc::new(store),
        generation,
        issuance,
        verb,
        args,
        target,
        workspace,
    );
    let (admission, grant) = GrantAdmission::claim(&owner, phase, claim).await.unwrap();
    assert!(
        matches!(admission, journal::Admission::Granted { state, .. }
        if matches!(*state, journal::Admission::Claimed(_)))
    );
    assert!(
        std::fs::read_to_string(root.path().join("wire/commands.jsonl"))
            .unwrap()
            .contains("consumption")
    );
    owner.shutdown().await.unwrap();
    grant.expect("a new durable claim must mint a dispatch capability")
}
fn request(
    store: Arc<gateway::Store>,
    generation: u64,
    issuance: String,
    verb: &str,
    args: Value,
    target: &str,
    workspace: &str,
) -> journal::grants::Claim {
    let workspace = workspace.to_owned();
    journal::grants::Claim {
        store,
        generation,
        issuance,
        home: "a".repeat(64),
        principal: "client-1".into(),
        source: gateway::lifecycle::PrincipalKind::User,
        scopes: [Scope::Operate].into(),
        destination: "b".repeat(64),
        target: target.into(),
        command: Command {
            id: "sealed-claim".into(),
            verb: verb.into(),
            args,
            resume_only: false,
            read_only: false,
        },
        now: 100,
        environment: Box::new(move || (None, Some(workspace))),
    }
}
#[test]
fn only_new_durable_claim_mints_authority() {
    run(async {
        let (root, store, generation, issuance) = gateway::claim_fixture();
        let store = Arc::new(store);
        let owner = Ownership::new(root.path().to_owned(), Arc::new(Notify::new()));
        owner.acquire().await.unwrap();
        let phase = owner.gate().load(Ordering::Acquire);
        let claim = || {
            request(
                store.clone(),
                generation,
                issuance.clone(),
                "agent.stop",
                json!({"target":"agent_atlas_1"}),
                "agent_atlas_1",
                "w1",
            )
        };
        let (_, grant) = GrantAdmission::claim(&owner, phase, claim()).await.unwrap();
        let grant = grant.unwrap();
        assert_eq!(grant.home, "a".repeat(64));
        assert_eq!(grant.principal, "client-1");
        assert_eq!(grant.issuance, issuance);
        assert_eq!(grant.ordinal, 1);
        assert_eq!(
            grant.payload_digest,
            journal::digest(&claim().command).unwrap()
        );
        let (retry, grant) = GrantAdmission::claim(&owner, phase, claim()).await.unwrap();
        assert!(matches!(retry, journal::Admission::Granted { state, .. }
            if matches!(*state, journal::Admission::Pending(_))));
        assert!(
            grant.is_none(),
            "pending retry must never mint a second capability"
        );
        owner
            .submit(
                phase,
                journal::Operation::Finish {
                    key: ("a".repeat(64), "client-1".into(), "sealed-claim".into()),
                    admitted_ms: 100,
                    result: Ok(json!({"stopped":true})),
                },
            )
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        let (retry, grant) = GrantAdmission::claim(&owner, phase, claim()).await.unwrap();
        assert!(matches!(retry, journal::Admission::Granted { state, .. }
            if matches!(*state, journal::Admission::Replayed(_))));
        assert!(
            grant.is_none(),
            "cached outcome must never mint a second capability"
        );
        owner.shutdown().await.unwrap();
    });
}
#[test]
fn changed_binding_or_mismatched_start_membership_does_not_claim() {
    run(async {
        let (root, store, generation, issuance) = gateway::claim_fixture();
        let store = Arc::new(store);
        let owner = Ownership::new(root.path().to_owned(), Arc::new(Notify::new()));
        owner.acquire().await.unwrap();
        let phase = owner.gate().load(Ordering::Acquire);
        let bad = request(
            store.clone(),
            generation,
            issuance.clone(),
            "agent.stop",
            json!({"target":"agent_atlas_2"}),
            "agent_atlas_1",
            "w1",
        );
        assert!(matches!(
            GrantAdmission::claim(&owner, phase, bad).await,
            Err(ErrorCode::NotAuthorized)
        ));
        let bad = request(
            store,
            generation,
            issuance,
            "agent.start",
            json!({"name":"fresh", "runtime":"structured", "argv":["claude"], "workspace_id":"w1"}),
            "w1",
            "w2",
        );
        let (admission, grant) = GrantAdmission::claim(&owner, phase, bad).await.unwrap();
        assert!(matches!(
            admission,
            journal::Admission::Refused(gateway::lifecycle::Refusal::Target)
        ));
        assert!(grant.is_none());
        assert_eq!(
            std::fs::metadata(root.path().join("wire/commands.jsonl"))
                .unwrap()
                .len(),
            0
        );
        owner.shutdown().await.unwrap();
    });
}
