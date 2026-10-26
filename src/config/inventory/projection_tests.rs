use super::*;
use serde_json::{json, Value};

const CANDIDATE: &str = "PRIVATE2384_CANDIDATE_d932a";
const RUNNING: &str = "PRIVATE2384_RUNNING_78d13";
const SOURCE: &str = "PRIVATE2384_SOURCE_991ad";

fn snapshots_probe(catalog: &Catalog, candidate: &Value, running: &Value) -> Value {
    serde_json::to_value(projection::snapshots(catalog, candidate, running)).unwrap()
}
fn defaults_probe(catalog: &Catalog) -> Value {
    serde_json::to_value(projection::defaults(catalog).unwrap()).unwrap()
}
fn provenance_probe(catalog: &Catalog, path: &str) -> Value {
    let winner = projection::Source {
        layer: projection::Layer::BaseFile,
        identifier: SOURCE,
    };
    let shadowed = [projection::Source {
        layer: projection::Layer::Environment,
        identifier: CANDIDATE,
    }];
    serde_json::to_value(projection::provenance(catalog, path, &winner, &shadowed)).unwrap()
}
fn fixture(marker: &str) -> Value {
    json!({
        "structured":{"token_file":format!("/tmp/{marker}")},
        "terminal":{"default_shell":marker},
        "systemone":{"urls":[format!("https://example.invalid/?token={marker}")]},
        "checks":{"script":[{"program":marker,"args":[marker],"env":{(marker):marker},"timeout_secs":20}]},
        "spawn":{"env":{(marker):[marker]}},
        "ui":{"agent_aliases":{(marker):marker}}
    })
}
#[test]
fn security2384_s3a_candidate_running_nested_values_do_not_leak() {
    let catalog = core_catalog().unwrap();
    let mut output = Value::Null;
    let logs = crate::logging::capture_logs(|| {
        output = snapshots_probe(&catalog, &fixture(CANDIDATE), &fixture(RUNNING))
    });
    let text = output.to_string();
    assert!(!text.contains(CANDIDATE), "candidate marker leaked");
    assert!(!text.contains(RUNNING), "running marker leaked");
    assert!(!logs.contains(CANDIDATE) && !logs.contains(RUNNING));
}
#[test]
fn security2384_s3a_defaults_redact_environment_routing_names() {
    let text = defaults_probe(&core_catalog().unwrap()).to_string();
    assert!(
        !text.contains("CLAUDE_CONFIG_DIR"),
        "compiled routing names leaked"
    );
}
#[test]
fn security2384_s3a_provenance_never_carries_source_identifiers() {
    let catalog = core_catalog().unwrap();
    for path in ["ui.sidebar_width", "structured.token_file", SOURCE] {
        let text = provenance_probe(&catalog, path).to_string();
        assert!(!text.contains(SOURCE), "source identifier leaked");
        assert!(!text.contains(CANDIDATE), "shadowed identifier leaked");
    }
}
#[test]
fn security2384_s3a_unknown_keys_and_invalid_public_values_are_redacted() {
    let input = json!({(CANDIDATE):CANDIDATE,"ui":{"confirm_close":CANDIDATE,"sidebar_width":{"hidden":CANDIDATE}}});
    let text = snapshots_probe(&core_catalog().unwrap(), &input, &input).to_string();
    assert!(!text.contains(CANDIDATE), "unknown or invalid field leaked");
}

fn field<'a>(node: &'a Value, key: &str) -> &'a Value {
    &node["data"]["fields"][key]
}
fn public_value(node: &Value) -> &Value {
    assert_eq!(node["kind"], "public");
    &node["data"]
}
#[test]
fn public_values_survive_in_sections_arrays_and_optional_entries() {
    let input = json!({
        "ui":{"confirm_close":true,"sidebar_width":42,"tab_mode":"workspace"},
        "channels":[{"name":CANDIDATE,"scopes":["notify",CANDIDATE,42]}],
        "peers":[{"name":CANDIDATE,"poll_interval_secs":null}],
        "checks":{"script":[{"program":CANDIDATE,"timeout_secs":23}]},
        "advanced":{"scrollback_lines":1024}
    });
    let safe = snapshots_probe(&core_catalog().unwrap(), &input, &input);
    let node = &safe["candidate"];
    assert_eq!(public_value(field(field(node, "ui"), "sidebar_width")), 42);
    assert_eq!(
        public_value(field(field(node, "ui"), "confirm_close")),
        true
    );
    assert_eq!(
        public_value(field(field(node, "ui"), "tab_mode")),
        "workspace"
    );
    let channel = &field(node, "channels")["data"][0];
    let scopes = field(channel, "scopes");
    assert_eq!(public_value(&scopes["data"][0]), "notify");
    assert_eq!(scopes["data"][1]["kind"], "redacted");
    assert_eq!(scopes["data"][2]["kind"], "redacted");
    assert!(public_value(field(
        &field(node, "peers")["data"][0],
        "poll_interval_secs"
    ))
    .is_null());
    assert_eq!(
        public_value(field(
            &field(field(node, "checks"), "script")["data"][0],
            "timeout_secs"
        )),
        23
    );
    assert_eq!(
        public_value(field(field(node, "advanced"), "scrollback_limit_bytes")),
        1024
    );
    assert!(!safe.to_string().contains(CANDIDATE));
}
#[test]
fn unknown_boundaries_shapes_and_conflicting_aliases_fail_closed() {
    let mut catalog = core_catalog().unwrap();
    catalog.fields.retain(|f| f.path != "ui.confirm_close");
    catalog
        .fields
        .iter_mut()
        .find(|f| f.path == "ui.sidebar_width")
        .unwrap()
        .value_class = Sensitivity::Opaque;
    let input = json!({"ui":{"confirm_close":CANDIDATE,"sidebar_width":CANDIDATE},"checks":{"script":[CANDIDATE]},"advanced":{"scrollback_lines":1,"scrollback_limit_bytes":2},(CANDIDATE):CANDIDATE});
    let safe = snapshots_probe(&catalog, &input, &json!({"ui":CANDIDATE}));
    let root = &safe["candidate"];
    assert_eq!(root["data"]["unclassified"], true);
    assert_eq!(field(root, "ui")["data"]["unclassified"], true);
    assert!(field(root, "ui")["data"]["fields"]
        .get("confirm_close")
        .is_none());
    assert_eq!(
        field(field(root, "ui"), "sidebar_width")["data"],
        "unclassified"
    );
    assert_eq!(
        field(field(root, "advanced"), "scrollback_limit_bytes")["data"],
        "invalid"
    );
    assert_eq!(
        field(field(root, "checks"), "script")["data"][0]["kind"],
        "redacted"
    );
    assert_eq!(field(&safe["running"], "ui")["kind"], "redacted");
    assert!(!safe.to_string().contains(CANDIDATE));
}
#[test]
fn content_kinds_distinguish_references_names_and_possible_literals() {
    let catalog = core_catalog().unwrap();
    for (path, kind) in [
        ("structured.token_file", ContentKind::CredentialReference),
        ("spawn.env", ContentKind::EnvironmentNames),
        ("terminal.default_shell", ContentKind::Executable),
        ("web.flock_bin", ContentKind::Executable),
        ("checks.issue_guard.gh_bin", ContentKind::Executable),
        ("checks.script[].program", ContentKind::Executable),
        ("checks.script[].env", ContentKind::LiteralCredentials),
        ("checks.script[].args", ContentKind::LiteralCredentials),
        ("checks.issue_guard.repos", ContentKind::Repository),
        ("systemone.urls", ContentKind::Endpoint),
        ("peers[].relay_command", ContentKind::Command),
        ("channels[].target", ContentKind::Identity),
    ] {
        let f = catalog.fields.iter().find(|f| f.path == path).unwrap();
        assert_eq!(f.content_kind, kind, "{path}");
        assert_ne!(f.value_class, Sensitivity::Public, "{path}");
    }
    let snapshot = snapshots_probe(&catalog, &fixture(CANDIDATE), &fixture(RUNNING));
    assert_eq!(
        field(field(&snapshot["candidate"], "structured"), "token_file")["data"]["sensitive"],
        "credential_reference"
    );
    assert_eq!(
        field(field(&snapshot["candidate"], "spawn"), "env")["data"]["sensitive"],
        "environment_names"
    );
}
#[test]
fn provenance_exposes_only_closed_layers_for_public_fields() {
    let catalog = core_catalog().unwrap();
    let safe = provenance_probe(&catalog, "ui.sidebar_width");
    assert_eq!(safe["field"], "ui.sidebar_width");
    assert_eq!(safe["origin"]["layers"]["winner"]["layer"], "base_file");
    assert_eq!(
        safe["origin"]["layers"]["shadowed"],
        json!([{"layer":"environment","identifier":"hidden"}])
    );
    let sensitive = provenance_probe(&catalog, "structured.token_file");
    assert_eq!(
        sensitive["origin"]["redacted"]["sensitive"],
        "credential_reference"
    );
    let unknown = provenance_probe(&catalog, SOURCE);
    assert!(unknown["field"].is_null());
    assert_eq!(unknown["origin"]["redacted"], "unclassified");
}
#[test]
fn defaults_cover_all_declared_contexts_without_credential_stores() {
    let catalog = core_catalog().unwrap();
    let defaults = defaults_probe(&catalog);
    assert_eq!(defaults.as_object().unwrap().len(), catalog.fields.len());
    assert_eq!(defaults["channels[].name"]["value"]["kind"], "required");
    assert_eq!(defaults["theme.custom.accent"]["parent_absent"], true);
    assert_eq!(
        defaults["session.restart.max_restarts"]["parent_absent"],
        false
    );
    assert_eq!(
        defaults["session.restart_agents<key>.max_restarts"]["parent_absent"],
        true
    );
    let input = json!({"channel_token_store":CANDIDATE,"client_token_store":RUNNING});
    let snapshot = snapshots_probe(&catalog, &input, &input).to_string();
    assert!(!snapshot.contains(CANDIDATE) && !snapshot.contains(RUNNING));
}
#[test]
fn file_contents_and_process_env_cannot_influence_the_projection() {
    let _guard = crate::config::test_config_env_guard();
    struct Env(&'static str, Option<std::ffi::OsString>);
    impl Drop for Env {
        fn drop(&mut self) {
            if let Some(value) = self.1.take() {
                std::env::set_var(self.0, value)
            } else {
                std::env::remove_var(self.0)
            }
        }
    }
    let _env = Env(
        "CONFIG2384_PROJECTION_SECRET",
        std::env::var_os("CONFIG2384_PROJECTION_SECRET"),
    );
    let _public_env = Env(
        "FLOCK_UI_SIDEBAR_WIDTH",
        std::env::var_os("FLOCK_UI_SIDEBAR_WIDTH"),
    );
    let dir = std::env::temp_dir().join(format!(
        "config2384-projection-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let reference = dir.join("credential-reference");
    std::fs::write(&reference, CANDIDATE).unwrap();
    std::env::set_var("CONFIG2384_PROJECTION_SECRET", CANDIDATE);
    std::env::set_var("FLOCK_UI_SIDEBAR_WIDTH", CANDIDATE);
    let input = json!({"structured":{"token_file":reference},"spawn":{"env":{"claude":["CONFIG2384_PROJECTION_SECRET"]}},"checks":{"script":[{"program":reference,"args":["${CONFIG2384_PROJECTION_SECRET}"],"env":{"TOKEN":"${CONFIG2384_PROJECTION_SECRET}"}}]}});
    let catalog = core_catalog().unwrap();
    let first = projection::snapshots(&catalog, &input, &input);
    let source = projection::Source {
        layer: projection::Layer::Environment,
        identifier: "FLOCK_UI_SIDEBAR_WIDTH",
    };
    let first_origin = projection::provenance(&catalog, "ui.sidebar_width", &source, &[]);
    std::fs::write(&reference, RUNNING).unwrap();
    std::env::set_var("CONFIG2384_PROJECTION_SECRET", RUNNING);
    std::env::set_var("FLOCK_UI_SIDEBAR_WIDTH", RUNNING);
    let second = projection::snapshots(&catalog, &input, &input);
    assert_eq!(first, second);
    let second_origin = projection::provenance(&catalog, "ui.sidebar_width", &source, &[]);
    assert_eq!(first_origin, second_origin);
    let output = format!(
        "{} {:?} {}",
        serde_json::to_string(&first).unwrap(),
        first,
        serde_json::to_string(&first_origin).unwrap()
    );
    for marker in [
        CANDIDATE,
        RUNNING,
        "credential-reference",
        "CONFIG2384_PROJECTION_SECRET",
    ] {
        assert!(!output.contains(marker), "{marker}");
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn real_config_serialization_enters_the_same_mandatory_boundary() {
    let mut candidate = crate::config::Config::default();
    candidate.structured.token_file = Some(std::path::PathBuf::from(CANDIDATE));
    candidate.terminal.default_shell = CANDIDATE.into();
    candidate.ui.confirm_close = false;
    candidate
        .ui
        .agent_aliases
        .insert(CANDIDATE.into(), CANDIDATE.into());
    candidate
        .checks
        .scripts
        .push(crate::checks::config::ScriptCheck {
            program: std::path::PathBuf::from(CANDIDATE),
            args: vec![CANDIDATE.into()],
            ..Default::default()
        });
    let mut running = candidate.clone();
    running.structured.token_file = Some(std::path::PathBuf::from(RUNNING));
    running.ui.confirm_close = true;
    let output = snapshots_probe(
        &core_catalog().unwrap(),
        &serde_json::to_value(candidate).unwrap(),
        &serde_json::to_value(running).unwrap(),
    );
    let text = output.to_string();
    assert!(!text.contains(CANDIDATE) && !text.contains(RUNNING));
    assert_eq!(
        public_value(field(field(&output["candidate"], "ui"), "confirm_close")),
        false
    );
    assert_eq!(
        public_value(field(field(&output["running"], "ui"), "confirm_close")),
        true
    );
}

#[test]
fn provenance_env_names_come_from_metadata_not_untrusted_identifiers() {
    let catalog = core_catalog().unwrap();
    for identifier in ["FLOCK_UI_SIDEBAR_WIDTH", SOURCE] {
        let origin = projection::Source {
            layer: projection::Layer::Environment,
            identifier,
        };
        let output = serde_json::to_value(projection::provenance(
            &catalog,
            "ui.sidebar_width",
            &origin,
            &[],
        ))
        .unwrap();
        let source = &output["origin"]["layers"]["winner"];
        if identifier == SOURCE {
            assert_eq!(source["identifier"], "hidden");
        } else {
            assert_eq!(
                source["identifier"]["environment"],
                "FLOCK_UI_SIDEBAR_WIDTH"
            );
        }
        assert!(!output.to_string().contains(SOURCE));
    }
}

#[test]
fn payload_representations_are_not_closed_public_string_choices() {
    let mut catalog = core_catalog().unwrap();
    catalog
        .representations
        .iter_mut()
        .find(|r| r.model == "RunAs")
        .unwrap()
        .members
        .push("Payload:String".into());
    let output = snapshots_probe(
        &catalog,
        &json!({"structured":{"run_as":"flk-agent"}}),
        &json!({}),
    );
    assert_eq!(
        field(field(&output["candidate"], "structured"), "run_as")["kind"],
        "redacted"
    );
}

#[test]
fn nested_schema_provenance_is_canonical_without_instance_keys() {
    let catalog = core_catalog().unwrap();
    let nested = provenance_probe(&catalog, "peers[].poll_interval_secs");
    assert_eq!(nested["field"], "peers[].poll_interval_secs");
    assert_eq!(nested["origin"]["layers"]["winner"]["layer"], "base_file");
    let alias = provenance_probe(&catalog, "advanced.scrollback_lines");
    assert_eq!(alias["field"], "advanced.scrollback_limit_bytes");
    let unknown = provenance_probe(
        &catalog,
        &format!("session.restart_agents.{CANDIDATE}.max_restarts"),
    );
    assert!(unknown["field"].is_null());
    assert!(!unknown.to_string().contains(CANDIDATE));
}
