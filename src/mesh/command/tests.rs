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
    Frame::originate(body(), &NodeIdentity::fixture([7; 32]), 6).unwrap()
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
        Frame::originate(file.clone(), &NodeIdentity::fixture([7; 32]), 6)
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
        Frame::originate(body(), &NodeIdentity::fixture([9; 32]), 6),
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
        Frame::originate(body(), &NodeIdentity::fixture([7; 32]), 5),
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
        Frame::originate(body(), &NodeIdentity::fixture([7; 32]), 6),
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
        Frame::originate(start.clone(), &identity, 6)
            .unwrap()
            .verify(),
        Ok(start.clone())
    );
    start.target = Target::Agent("agent_hopper_command".into());
    assert_eq!(
        Frame::originate(start, &identity, 6),
        Err("invalid_envelope")
    );
    let mut invalid = body();
    invalid.budget_ms += 1;
    assert_eq!(
        Frame::originate(invalid, &identity, 6),
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
