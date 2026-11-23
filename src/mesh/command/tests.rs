use super::*;
use sha2::{Digest, Sha256};

struct Clock(std::io::Result<Duration>);
impl suspend_clock::Read for Clock {
    fn read(&self, _: suspend_clock::ReadSource) -> std::io::Result<Duration> {
        self.0
            .as_ref()
            .copied()
            .map_err(|_| std::io::Error::other("clock unavailable"))
    }
}
fn clock(ms: u64) -> Clock {
    Clock(Ok(Duration::from_millis(ms)))
}
fn six() -> (
    std::sync::MutexGuard<'static, ()>,
    crate::config::TestFlockEnvScrub,
) {
    let guard = crate::config::test_config_env_guard();
    std::env::set_var("FLOCK_TEST_MESH_VERSION", "6");
    std::env::set_var("FLOCK_TEST_MESH_MIN", "5");
    guard
}
fn body() -> Body {
    Body {
        home_node_id: NodeIdentity::fixture([7; 32]).node_id(),
        principal: "fixture-principal".into(),
        token_source: TokenSource::App,
        scopes: BTreeSet::from([Scope::Send]),
        destination_node_id: NodeIdentity::fixture([8; 32]).node_id(),
        target: Target::Agent("agent_hopper_command".into()),
        cmd_id: "fixture-command-1".into(),
        verb: "agent.send".into(),
        args_digest: Sha256::digest(b"fixture secret argument").into(),
        requested_scope: Scope::Send,
        budget_ms: BUDGET_MS,
    }
}
fn frame() -> Frame {
    Frame::originate(
        body(),
        &NodeIdentity::fixture([7; 32]),
        6,
        &Provenance::fixture(&body()),
    )
    .unwrap()
}

pub(crate) fn admission_frame(body: Body) -> Frame {
    let mut valid = body.clone();
    valid.home_node_id = NodeIdentity::fixture([7; 32]).node_id();
    valid.token_source = TokenSource::App;
    let mut frame = Frame::originate(
        valid.clone(),
        &NodeIdentity::fixture([7; 32]),
        6,
        &Provenance::fixture(&valid),
    )
    .unwrap();
    frame.envelope.body = serde_json::to_vec(&body).unwrap();
    sign::seal(&mut frame.envelope, &NodeIdentity::fixture([7; 32]));
    frame
}

#[test]
fn signed_round_trip_preserves_home_after_multiple_hops() {
    let (_lock, _scrub) = six();
    assert_eq!(hello::window(), (5, 6));
    let original = frame();
    let mut sent = original.clone();
    for residence in [10, 25, 90] {
        let mut hop = Hop::receive(sent, Some(Duration::from_millis(100))).unwrap();
        sent = hop.forward(6, &clock(100 + residence)).unwrap();
        sent = serde_json::from_slice(&serde_json::to_vec(&sent).unwrap()).unwrap();
        assert_eq!(sent.envelope.signature, original.envelope.signature);
        assert_eq!(sent.verify().unwrap(), body());
        assert_eq!(sign::verify(&sent.envelope), Ok(()));
    }
    assert_eq!(sent.elapsed_ms, 125);
    assert_eq!(sent.envelope.key.origin_node, body().home_node_id);
    let mut file = body();
    file.token_source = TokenSource::File;
    assert_eq!(
        Frame::originate(
            file.clone(),
            &NodeIdentity::fixture([7; 32]),
            6,
            &Provenance::fixture(&file)
        )
        .unwrap()
        .verify(),
        Ok(file)
    );
}

#[test]
fn home_target_payload_and_budget_substitution_fail_signature_verification() {
    let (_lock, _scrub) = six();
    let original = frame();
    let changes: Vec<fn(&mut Body)> = vec![
        |b| b.home_node_id = NodeIdentity::fixture([9; 32]).node_id(),
        |b| b.principal.push('x'),
        |b| b.destination_node_id = NodeIdentity::fixture([9; 32]).node_id(),
        |b| b.target = Target::Agent("agent_kiln_command".into()),
        |b| b.cmd_id.push('x'),
        |b| b.verb = "agent.interrupt".into(),
        |b| b.args_digest[0] ^= 1,
        |b| b.requested_scope = Scope::Operate,
        |b| {
            b.scopes.insert(Scope::Operate);
        },
        |b| b.token_source = TokenSource::File,
        |b| b.budget_ms += 1,
    ];
    for change in changes {
        let mut altered = original.clone();
        let mut body = altered.verify().unwrap();
        change(&mut body);
        altered.envelope.body = serde_json::to_vec(&body).unwrap();
        assert_eq!(altered.verify(), Err("invalid_signature"));
    }
    let mut changed = original.clone();
    changed.envelope.key.origin_node = body().destination_node_id;
    assert_eq!(changed.verify(), Err("origin_mismatch"));
    let mut changed = original;
    changed.envelope.target_agent.push('x');
    assert_eq!(changed.verify(), Err("invalid_signature"));
    assert_eq!(
        Frame::originate(
            body(),
            &NodeIdentity::fixture([9; 32]),
            6,
            &Provenance::fixture(&body())
        ),
        Err("origin_mismatch")
    );
}

#[test]
fn serialized_frames_and_refusals_carry_no_credentials_or_raw_arguments() {
    let (_lock, _scrub) = six();
    let original = frame();
    let refused = original
        .refused(&NodeIdentity::fixture([8; 32]), "unsupported_for_agent")
        .unwrap();
    assert_eq!(refused.kind, Kind::Receipt);
    assert_eq!(sign::verify(&refused), Ok(()));
    assert_eq!(refused.request_key, Some(original.envelope.key.clone()));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&refused.body).unwrap(),
        serde_json::json!({"state":"refused","detail":"unsupported_for_agent"})
    );
    for envelope in [&original.envelope, &refused] {
        let bytes = serde_json::to_vec(envelope).unwrap();
        for secret in [
            "fixture-client-bearer",
            "fixture-ssh-private-key",
            "fixture-destination-token",
            "fixture-operator-credential",
            "fixture secret argument",
        ] {
            let encoded = serde_json::to_vec(secret.as_bytes()).unwrap();
            assert!(!bytes.windows(secret.len()).any(|v| v == secret.as_bytes()));
            assert!(!bytes
                .windows(encoded.len() - 2)
                .any(|v| v == &encoded[1..encoded.len() - 1]));
            assert!(!envelope
                .body
                .windows(secret.len())
                .any(|v| v == secret.as_bytes()));
        }
        assert!(envelope.return_binding.collection_token.is_empty());
    }
    let bytes = serde_json::to_vec(&original).unwrap();
    assert!(!bytes
        .windows(b"fixture secret argument".len())
        .any(|v| v == b"fixture secret argument"));
    let mut untrusted = serde_json::to_value(body()).unwrap();
    untrusted["bearer"] = serde_json::json!("fixture-client-bearer");
    assert!(serde_json::from_value::<Body>(untrusted).is_err());
}

#[test]
fn version_five_refuses_before_emission_and_downgrade_never_holds_commands() {
    let (_lock, _scrub) = six();
    assert_eq!(
        Frame::originate(
            body(),
            &NodeIdentity::fixture([7; 32]),
            5,
            &Provenance::fixture(&body())
        ),
        Err("unsupported_for_agent")
    );
    let mut hop = Hop::receive(frame(), Some(Duration::ZERO)).unwrap();
    let mut sends = Vec::new();
    let result = hop.forward(5, &clock(10));
    if let Ok(frame) = &result {
        sends.push(frame.clone());
    }
    assert!(sends.is_empty());
    let code = result.unwrap_err();
    assert_eq!(code, "unsupported_for_agent");
    let refusal = hop
        .frame
        .refused(&NodeIdentity::fixture([8; 32]), code)
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&refusal.body).unwrap()["detail"],
        "unsupported_for_agent"
    );
    assert_eq!(hop.forward(6, &clock(30)), Err("message_expired"));
    std::env::remove_var("FLOCK_TEST_MESH_VERSION");
    std::env::remove_var("FLOCK_TEST_MESH_MIN");
    assert_eq!(hello::window(), (5, 5));
    assert_eq!(
        Frame::originate(
            body(),
            &NodeIdentity::fixture([7; 32]),
            6,
            &Provenance::fixture(&body())
        ),
        Err("unsupported_for_agent")
    );
    assert_eq!(hop.frame.verify(), Err("unsupported_for_agent"));
    let value = serde_json::to_value(&hop.frame).unwrap();
    assert_eq!(version_gate::check(&value, 5), Err("command".into()));
}

#[test]
fn residence_including_pause_reroute_and_recovery_never_renews_budget() {
    let (_lock, _scrub) = six();
    let mut hop = Hop::receive(frame(), Some(Duration::from_millis(100))).unwrap();
    assert_eq!(hop.forward(6, &clock(200)).unwrap().elapsed_ms, 100);
    assert_eq!(hop.forward(6, &clock(200)).unwrap().elapsed_ms, 100);
    assert_eq!(hop.forward(6, &clock(30_099)).unwrap().elapsed_ms, 29_999);
    assert_eq!(hop.forward(6, &clock(30_100)), Err("message_expired"));
    assert_eq!(hop.forward(6, &clock(30_101)), Err("message_expired"));
    let persisted = serde_json::from_slice(&serde_json::to_vec(&frame()).unwrap()).unwrap();
    let mut recovered = Hop::receive(persisted, None).unwrap();
    assert_eq!(recovered.forward(6, &clock(0)), Err("message_expired"));
    assert_eq!(recovered.forward(6, &clock(10)), Err("message_expired"));
    let mut backwards = Hop::receive(frame(), Some(Duration::from_millis(100))).unwrap();
    assert_eq!(backwards.forward(6, &clock(99)), Err("message_expired"));
    assert_eq!(backwards.forward(6, &clock(101)), Err("message_expired"));
    let mut missing = Hop::receive(frame(), Some(Duration::ZERO)).unwrap();
    assert_eq!(
        missing.forward(6, &Clock(Err(std::io::Error::other("clock unavailable")))),
        Err("message_expired")
    );
    assert_eq!(missing.forward(6, &clock(1)), Err("message_expired"));
    let mut overflow = frame();
    overflow.elapsed_ms = u64::MAX;
    assert_eq!(overflow.verify(), Err("message_expired"));
    let mut huge = Hop::receive(frame(), Some(Duration::ZERO)).unwrap();
    assert_eq!(
        huge.forward(6, &Clock(Ok(Duration::MAX))),
        Err("message_expired")
    );
}

#[test]
fn workspace_start_is_explicit_and_budget_policy_is_fixed() {
    let (_lock, _scrub) = six();
    let mut start = body();
    start.verb = "agent.start".into();
    start.target = Target::Workspace("w1".into());
    start.scopes = BTreeSet::from([Scope::Operate]);
    start.requested_scope = Scope::Operate;
    let identity = NodeIdentity::fixture([7; 32]);
    assert_eq!(
        Frame::originate(start.clone(), &identity, 6, &Provenance::fixture(&start))
            .unwrap()
            .verify(),
        Ok(start.clone())
    );
    start.target = Target::Agent("agent_hopper_command".into());
    assert_eq!(
        Frame::originate(start.clone(), &identity, 6, &Provenance::fixture(&start)),
        Err("invalid_envelope")
    );
    let mut invalid = body();
    invalid.budget_ms += 1;
    assert_eq!(
        Frame::originate(
            invalid.clone(),
            &identity,
            6,
            &Provenance::fixture(&invalid)
        ),
        Err("invalid_envelope")
    );
}

#[test]
fn fractional_residence_is_charged_and_signed_foreign_home_is_rejected() {
    let (_lock, _scrub) = six();
    let mut hop = Hop::receive(frame(), Some(Duration::ZERO)).unwrap();
    assert_eq!(
        hop.forward(6, &Clock(Ok(Duration::from_nanos(1))))
            .unwrap()
            .elapsed_ms,
        1
    );
    let identity = NodeIdentity::fixture([7; 32]);
    let mut claimed = frame();
    let mut payload = body();
    payload.home_node_id = NodeIdentity::fixture([8; 32]).node_id();
    claimed.envelope.body = serde_json::to_vec(&payload).unwrap();
    sign::seal(&mut claimed.envelope, &identity);
    assert_eq!(claimed.verify(), Err("origin_mismatch"));
    let mut credential = frame();
    credential.envelope.return_binding.collection_token = b"fixture-destination-token".to_vec();
    sign::seal(&mut credential.envelope, &identity);
    assert_eq!(credential.verify(), Err("origin_mismatch"));
}

#[test]
fn hub_preparation_returns_a_signed_refusal_instead_of_a_deferred_command() {
    let (_lock, _scrub) = six();
    let hub = NodeIdentity::fixture([8; 32]);
    let mut hop = Hop::receive(frame(), Some(Duration::ZERO)).unwrap();
    let Prepared::Refused(receipt) = hop.prepare(5, &clock(20), &hub).unwrap() else {
        panic!("old next hop received a command")
    };
    assert_eq!(receipt.kind, Kind::Receipt);
    assert_eq!(sign::verify(&receipt), Ok(()));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&receipt.body).unwrap()["detail"],
        "unsupported_for_agent"
    );
    let Prepared::Refused(receipt) = hop.prepare(6, &clock(30), &hub).unwrap() else {
        panic!("refused command was revived")
    };
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&receipt.body).unwrap()["detail"],
        "message_expired"
    );
    let mut fresh = Hop::receive(frame(), Some(Duration::ZERO)).unwrap();
    let Prepared::Send(outbound) = fresh.prepare(6, &clock(20), &hub).unwrap() else {
        panic!("compatible next hop refused a live command")
    };
    assert_eq!(outbound.elapsed_ms, 20);
    assert_eq!(outbound.verify(), Ok(body()));
    let mut mismatched = frame();
    mismatched.envelope.target_agent = "agent_kiln_command".into();
    sign::seal(&mut mismatched.envelope, &NodeIdentity::fixture([7; 32]));
    assert_eq!(mismatched.verify(), Err("invalid_envelope"));
}

/// Exercise the signed envelope from a real token-store admission callback.
pub(crate) fn authenticated_token_round_trip(
    token: &crate::structured::permissions::auth::tokens::CommandToken<'_>,
    principal: &str,
    app: bool,
) {
    let (_lock, _scrub) = six();
    let proof = Provenance::from_token(token);
    let (_, _, scopes) = token.command_facts();
    let mut payload = body();
    payload.principal = principal.into();
    payload.scopes = scopes.clone();
    payload.token_source = if app {
        TokenSource::App
    } else {
        TokenSource::File
    };
    let identity = NodeIdentity::fixture([7; 32]);
    let signed = Frame::originate(payload.clone(), &identity, 6, &proof).unwrap();
    assert_eq!(signed.verify(), Ok(payload.clone()));
    for change in [
        |b: &mut Body| b.principal.push('x'),
        |b: &mut Body| {
            b.scopes.clear();
        },
        |b: &mut Body| b.token_source = TokenSource::Agent,
    ] {
        let mut altered = signed.clone();
        let mut b = payload.clone();
        change(&mut b);
        altered.envelope.body = serde_json::to_vec(&b).unwrap();
        assert_eq!(altered.verify(), Err("invalid_signature"));
        assert_eq!(
            Frame::originate(b, &identity, 6, &proof),
            Err("not_authorized")
        );
    }
    if !app {
        let mut promoted = payload.clone();
        promoted.token_source = TokenSource::App;
        assert_eq!(
            Frame::originate(promoted, &identity, 6, &proof),
            Err("not_authorized")
        );
    }
    for verb in ["agent.start", "agent.stop", "agent.restart", "agent.resume"] {
        let mut lifecycle = payload.clone();
        lifecycle.verb = verb.into();
        lifecycle.requested_scope = Scope::Operate;
        if verb == "agent.start" {
            lifecycle.target = Target::Workspace("w1".into());
        }
        if app {
            assert!(Frame::originate(lifecycle, &identity, 6, &proof).is_ok());
        } else {
            assert_eq!(
                Frame::originate(lifecycle.clone(), &identity, 6, &proof),
                Err("not_authorized")
            );
            let mut forged = signed.clone();
            forged.envelope.body = serde_json::to_vec(&lifecycle).unwrap();
            forged.envelope.intent = verb.into();
            if verb == "agent.start" {
                forged.envelope.target_agent.clear();
            }
            sign::seal(&mut forged.envelope, &identity);
            assert_eq!(forged.verify(), Err("not_authorized"));
        }
    }
    std::env::remove_var("FLOCK_TEST_MESH_VERSION");
    std::env::remove_var("FLOCK_TEST_MESH_MIN");
    assert_eq!(hello::window(), (5, 5));
    assert_eq!(
        Frame::originate(payload, &identity, 6, &proof),
        Err("unsupported_for_agent")
    );
    assert_eq!(signed.verify(), Err("unsupported_for_agent"));
}

#[test]
fn scoped_admission_binds_profile_ceiling_identity_and_revocation() {
    use crate::structured::{
        profiles::ProfileAdmission,
        registry::{BindingIdentity, ProfileIdentity, Record, Registry, StructuredRuntime},
    };
    let (_lock, _scrub) = six();
    let root = crate::test_support::TempDir::socket();
    let path = root.path().join("profile-bindings.json");
    let mut profiles = ProfileAdmission::load(&path);
    let (_leases, lease, scope, _) =
        crate::api::scoped::tests::live_scope_for_agent("agent_hopper_scoped");
    let crate::structured::lease::Caller::Agent { agent, .. } = scope.caller() else {
        panic!("not authenticated")
    };
    let mut record = Record {
        runtime: StructuredRuntime::Structured,
        agent_id: agent.clone(),
        name: None,
        harness: "claude".into(),
        session_id: "fixture-session".into(),
        harness_established: true,
        profile_identity: None,
        cwd: root.path().into(),
        model: None,
    };
    let mut registry = Registry::default();
    registry.register(record.clone()).unwrap();
    let proof = Provenance::from_scoped(&scope, &mut profiles, &registry).unwrap();
    let mut payload = body();
    payload.principal = agent.clone();
    payload.token_source = TokenSource::Agent;
    payload.scopes = [Scope::Read].into();
    payload.requested_scope = Scope::Read;
    payload.verb = "agent.read".into();
    let identity = NodeIdentity::fixture([7; 32]);
    assert_eq!(
        Frame::originate(payload.clone(), &identity, 6, &proof)
            .unwrap()
            .verify(),
        Ok(payload.clone())
    );
    let mut widened = payload.clone();
    widened.scopes.insert(Scope::Operate);
    assert_eq!(
        Frame::originate(widened, &identity, 6, &proof),
        Err("not_authorized")
    );
    let mut impersonated = payload.clone();
    impersonated.principal = "agent_kiln_other".into();
    assert_eq!(
        Frame::originate(impersonated, &identity, 6, &proof),
        Err("not_authorized")
    );
    let read_plan = profiles.plan_bind(7, serde_json::from_value(serde_json::json!({
        "action":"bind", "binding_id":"fixture-binding",
        "target":{"repo_id":"fixture-repo", "node_ids":["node.example"], "agent_ids":[agent]},
        "account_alias":"fixture-account", "credential_ref":"unopened-reference",
        "credential_generation":1, "bundle_digests":["fixture-digest"], "grants":["profile-select-only"]
    })).unwrap()).unwrap();
    profiles
        .bind(
            7,
            read_plan["plan"].as_str().unwrap(),
            read_plan["expect"].as_str().unwrap(),
        )
        .unwrap();
    record.profile_identity = Some(ProfileIdentity {
        profile: "fixture".into(),
        binding: Some(BindingIdentity {
            binding_id: "fixture-binding".into(),
            revision: 1,
        }),
    });
    registry = Registry::default();
    registry.register(record.clone()).unwrap();
    let readonly = Provenance::from_scoped(&scope, &mut profiles, &registry).unwrap();
    assert!(Frame::originate(payload.clone(), &identity, 6, &readonly).is_ok());
    let mut elevated = payload.clone();
    elevated.scopes.insert(Scope::Operate);
    assert_eq!(
        Frame::originate(elevated, &identity, 6, &readonly),
        Err("not_authorized")
    );
    let plan = profiles.plan_bind(7, serde_json::from_value(serde_json::json!({"action":"bind", "binding_id":"fixture-binding", "target":{"repo_id":"fixture-repo", "node_ids":["node.example"], "agent_ids":[agent]}, "account_alias":"fixture-account", "credential_ref":"unopened-reference", "credential_generation":1, "bundle_digests":["fixture-digest"], "grants":["profile-select-only"], "mesh_scopes":["read", "operate"]})).unwrap()).unwrap();
    profiles
        .bind(
            7,
            plan["plan"].as_str().unwrap(),
            plan["expect"].as_str().unwrap(),
        )
        .unwrap();
    assert!(Provenance::from_scoped(&scope, &mut profiles, &registry).is_err());
    record.profile_identity = Some(ProfileIdentity {
        profile: "fixture".into(),
        binding: Some(BindingIdentity {
            binding_id: "fixture-binding".into(),
            revision: 2,
        }),
    });
    registry = Registry::default();
    registry.register(record.clone()).unwrap();
    let proof = Provenance::from_scoped(&scope, &mut profiles, &registry).unwrap();
    payload.scopes.insert(Scope::Operate);
    payload.verb = "agent.stop".into();
    payload.requested_scope = Scope::Operate;
    let signed = Frame::originate(payload.clone(), &identity, 6, &proof).unwrap();
    assert_eq!(signed.verify(), Ok(payload.clone()));
    for change in [
        |b: &mut Body| b.principal = "agent_kiln_other".into(),
        |b: &mut Body| {
            b.scopes.insert(Scope::Answer);
        },
        |b: &mut Body| b.token_source = TokenSource::App,
    ] {
        let mut tampered = signed.clone();
        let mut b = payload.clone();
        change(&mut b);
        tampered.envelope.body = serde_json::to_vec(&b).unwrap();
        assert_eq!(tampered.verify(), Err("invalid_signature"));
    }
    // Resume checks the original profile pin rather than accepting a new ceiling.
    assert!(record
        .admit_profile(record.profile_identity.as_ref())
        .is_ok());
    let mut changed = record.profile_identity.clone().unwrap();
    changed.binding.as_mut().unwrap().revision += 1;
    assert!(record.admit_profile(Some(&changed)).is_err());
    assert_eq!(
        profiles
            .mesh_scopes(record.profile_identity.as_ref())
            .unwrap(),
        payload.scopes
    );
    let (_, _child_lease, child_scope, _) =
        crate::api::scoped::tests::live_scope_for_agent("agent_kiln_child");
    let mut child = record.clone();
    child.agent_id = "agent_kiln_child".into();
    child.session_id = "child-session".into();
    registry.register(child).unwrap();
    let child_proof = Provenance::from_scoped(&child_scope, &mut profiles, &registry).unwrap();
    payload.principal = "agent_kiln_child".into();
    assert_eq!(
        Frame::originate(payload.clone(), &identity, 6, &proof),
        Err("not_authorized")
    );
    assert_eq!(
        Frame::originate(payload.clone(), &identity, 6, &child_proof)
            .unwrap()
            .verify(),
        Ok(payload)
    );
    // Neither the parent nor the fork imports profile-selection grants into A1.
    let grants = crate::structured::permissions::auth::gateway::Store::open(root.path()).unwrap();
    use crate::structured::permissions::auth::gateway::{
        lifecycle::{Decision, Evaluation, PrincipalKind, Refusal},
        Request, Target as GrantTarget,
    };
    let scopes = [Scope::Read, Scope::Operate].into();
    let consumed = std::collections::BTreeMap::new();
    for principal in [agent.as_str(), "agent_kiln_child"] {
        let request = Request {
            home: &identity.node_id(),
            principal,
            scopes: &scopes,
            verb: "agent.stop",
            target: GrantTarget::Agent("agent_hopper_command"),
        };
        let context = Evaluation {
            request: &request,
            source: PrincipalKind::Agent,
            destination: &NodeIdentity::fixture([8; 32]).node_id(),
            clock: None,
            consumed: &consumed,
        };
        assert_eq!(
            grants.evaluate(&context, 0, |_| Some("w1".into())).unwrap(),
            Decision::Refused(Refusal::NoGrant)
        );
    }
    let authenticated_body = signed.verify().unwrap();
    drop(lease);
    assert_eq!(
        Frame::originate(authenticated_body, &identity, 6, &proof),
        Err("not_authorized")
    );
    profiles
        .revoke(
            7,
            crate::structured::profiles::bindings::transactions::RevokeRequest {
                binding: "fixture-binding".into(),
                reason: "fixture revocation".into(),
            },
        )
        .unwrap();
    assert!(Provenance::from_scoped(&child_scope, &mut profiles, &registry).is_err());
    assert!(Provenance::from_scoped(&scope, &mut profiles, &registry).is_err());
    assert!(Provenance::from_scoped(
        &crate::api::scoped::tests::unknown_credential(&scope),
        &mut profiles,
        &registry
    )
    .is_err());
}

#[test]
fn argument_identity_is_canonical_typed_and_domain_separated() {
    let a: serde_json::Value =
        serde_json::from_str(r#" { "z": [true, null], "a": {"b":1,"a":"x"} } "#).unwrap();
    let b: serde_json::Value =
        serde_json::from_str(r#"{"a":{"a":"x","b":1},"z":[true,null]}"#).unwrap();
    assert_eq!(
        canonical_json(&a).unwrap(),
        br#"{"a":{"a":"x","b":1},"z":[true,null]}"#
    );
    assert_eq!(args_digest(&a).unwrap(), args_digest(&b).unwrap());
    let mut bytes = b"flock-m1-args-v1\0".to_vec();
    bytes.extend(canonical_json(&a).unwrap());
    assert_eq!(
        args_digest(&a).unwrap(),
        <[u8; 32]>::from(Sha256::digest(bytes))
    );
    assert_ne!(
        args_digest(&a).unwrap(),
        <[u8; 32]>::from(Sha256::digest(canonical_json(&a).unwrap()))
    );
    for changed in [
        serde_json::json!({"a":{"b":"1","a":"x"}, "z":[true,null]}),
        serde_json::json!({"a":{"b":1.0,"a":"x"}, "z":[true,null]}),
        serde_json::json!({"a":{"b":2,"a":"x"}, "z":[true,null]}),
    ] {
        assert_ne!(args_digest(&a).unwrap(), args_digest(&changed).unwrap());
    }
}
#[test]
fn file_provenance_allows_only_known_nonlifecycle_verbs() {
    let (_lock, _scrub) = six();
    for verb in ["agent.fork", "agent.reload", "agent.future"] {
        let mut body = body();
        body.token_source = TokenSource::File;
        body.verb = verb.into();
        body.scopes.insert(Scope::Operate);
        body.requested_scope = Scope::Operate;
        assert_eq!(admission_frame(body).verify(), Err("not_authorized"));
    }
}
