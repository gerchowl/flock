use super::RemoteSubject;

#[test]
fn remote_identity_pairs_round_trip_without_join_collisions() {
    let first = RemoteSubject {
        home: "home.example:part".into(),
        principal: "leaf".into(),
    };
    let second = RemoteSubject {
        home: "home.example".into(),
        principal: "part:leaf".into(),
    };
    assert_eq!(
        format!("{}:{}", first.home, first.principal),
        format!("{}:{}", second.home, second.principal)
    );
    assert_ne!(first.parent_key(), second.parent_key());
    for subject in [
        first,
        second,
        RemoteSubject {
            home: "home.example".into(),
            principal: "quoted\"principal:[]".into(),
        },
    ] {
        let key = subject.parent_key();
        assert!(!crate::terminal::AgentId::is_well_formed(&key));
        assert_eq!(RemoteSubject::from_parent_key(&key), Some(subject));
    }
}
#[test]
fn remote_parent_keys_reject_raw_or_unrecognized_encodings() {
    for value in [
        "home.example:principal",
        "agent_atlas_1",
        "null",
        r#"["unknown",{"home":"home.example","principal":"caller"}]"#,
        r#"["mesh-grant-v1",{"home":"","principal":"caller"}]"#,
        r#"["mesh-grant-v1",{"home":"home.example","principal":""}]"#,
        r#"["mesh-grant-v1",{"home":"home.example","principal":"caller","extra":true}]"#,
    ] {
        assert!(RemoteSubject::from_parent_key(value).is_none(), "{value}");
    }
}
