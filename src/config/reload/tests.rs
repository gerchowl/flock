use super::*;
use std::collections::BTreeSet;

/// Covers optional scalars too: default serialization can omit future fields.
#[test]
fn every_declared_config_field_has_exactly_one_reload_classification() {
    let source = syn::parse_file(include_str!("../model.rs")).unwrap();
    let fields: BTreeSet<_> = source
        .items
        .into_iter()
        .find_map(|item| match item {
            syn::Item::Struct(item) if item.ident == "Config" => Some(
                item.fields
                    .into_iter()
                    .map(|field| field.ident.unwrap().to_string())
                    .collect(),
            ),
            _ => None,
        })
        .expect("actual Config declaration");
    let classified: BTreeSet<_> = LIVE
        .iter()
        .chain(RESTART)
        .map(|name| name.to_string())
        .collect();
    assert_eq!(
        classified.len(),
        LIVE.len() + RESTART.len(),
        "no duplicate classifications"
    );
    assert_eq!(
        fields, classified,
        "every new config field needs an explicit reload decision"
    );
}

#[test]
fn section_reporting_is_typed_changed_only_and_value_free() {
    let current = Config::default();
    assert_eq!(plan(&current, &current, &current, &[]).unwrap(), []);
    let mut incoming = current.clone();
    incoming.structured.profile = "profile-content-canary".into();
    incoming.fleet.max_concurrent_agents = 3;
    incoming.security.agent_account = super::super::model::AgentAccount::None;
    incoming.sessions.stall_ms = 5000;
    let result = plan(&current, &current, &incoming, &["structured".into()]).unwrap();
    for (section, status) in [
        ("structured", SectionStatus::Invalid),
        ("fleet", SectionStatus::Applied),
        ("security", SectionStatus::RestartRequired),
        ("sessions", SectionStatus::RestartRequired),
    ] {
        assert_eq!(
            result.iter().find(|r| r.section == section).unwrap().status,
            status
        );
    }
    let wire = serde_json::to_string(&result).unwrap();
    assert!(!wire.contains("profile-content-canary"));
    assert!(wire.contains("restart_required"));
    assert!(wire.contains("invalid"));
}

#[test]
fn legacy_reload_responses_default_to_no_section_details() {
    let result: crate::api::schema::ResponseResult = serde_json::from_value(serde_json::json!({
        "type":"config_reload", "status":"applied", "diagnostics":[]
    }))
    .unwrap();
    assert!(
        matches!(result, crate::api::schema::ResponseResult::ConfigReload {sections, ..} if sections.is_empty())
    );
}

/// A live reader does not clear the warning for a still-cached startup consumer.
#[test]
fn restart_required_survives_repeated_reload_until_process_restart() {
    let startup = Config::default();
    let mut desired = startup.clone();
    desired.name = "identity.example.test".into();
    desired.icon = Some("satellite".into());
    for section in ["name", "icon"] {
        let sections = plan(&desired, &startup, &desired, &[]).unwrap();
        assert_eq!(
            sections
                .iter()
                .find(|s| s.section == section)
                .unwrap()
                .status,
            SectionStatus::RestartRequired
        );
    }
    assert!(plan(&desired, &desired, &desired, &[]).unwrap().is_empty());
}
