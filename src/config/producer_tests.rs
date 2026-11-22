//! Producer ingress probes: returned diagnostics and immediate logs must agree.
use super::*;

fn probe(input: &str, marker: &str) {
    let mut messages = Vec::new();
    let logs = crate::logging::capture_logs(|| {
        let loaded = io::load_live_config_from_str(input).unwrap();
        messages.extend(loaded.diagnostics);
        messages.extend(loaded.config.collect_diagnostics());
        for message in &messages {
            crate::logging::config_diagnostic(message);
        }
    });
    assert!(!messages.is_empty(), "probe must reach a diagnostic");
    assert!(
        !format!("{messages:?} {logs}").contains(marker),
        "{messages:?} {logs}"
    );
}
macro_rules! case {
    ($name:ident, $text:expr, $marker:expr) => {
        #[test]
        fn $name() {
            probe($text, $marker);
        }
    };
}
case!(
    security2384_producer_prefix,
    "[keys]\nprefix='SECRET_PREFIX_2384'",
    "SECRET_PREFIX_2384"
);
case!(
    security2384_producer_action,
    "[keys]\nnew_tab='SECRET_ACTION_2384'",
    "SECRET_ACTION_2384"
);
case!(
    security2384_producer_navigate,
    "[keys]\nnavigate_workspace_up='SECRET_NAV_2384'",
    "SECRET_NAV_2384"
);
case!(
    security2384_producer_legacy,
    "[keys.indexed]\ntabs='SECRET_LEGACY_2384'",
    "SECRET_LEGACY_2384"
);
case!(
    security2384_producer_command,
    "[[keys.command]]\nkey='PROBE_PROBE_2384'\ncommand='SECRET_EXEC_2384'",
    "PROBE_PROBE_2384"
);
case!(
    security2384_producer_sound_format,
    "[ui.sound]\npath='SECRET_SOUND_2384.wav'",
    "SECRET_SOUND_2384"
);
case!(
    security2384_producer_sound_missing,
    "[ui.sound]\ndone_path='SECRET_MISSING_2384.mp3'",
    "SECRET_MISSING_2384"
);
case!(
    security2384_producer_spawn_unknown,
    "[spawn.env]\nSECRET_AGENT_2384=['X']",
    "SECRET_AGENT_2384"
);
case!(
    security2384_producer_spawn_alias,
    "[spawn.env]\nclaude=[]\nclaude-code=[]",
    "claude-code"
);
case!(
    security2384_producer_spawn_key,
    "[spawn.env]\nclaude=['SECRET_ENV_2384=*']",
    "SECRET_ENV_2384"
);
case!(
    security2384_producer_script_program,
    "[[checks.script]]\nname='SECRET_SCRIPT_2384'\nprogram='SECRET_PROGRAM_2384'",
    "SECRET_"
);
case!(security2384_producer_script_duplicate, "[[checks.script]]\nname='SECRET_DUP_2384'\nprogram='/bin/sh'\n[[checks.script]]\nname='SECRET_DUP_2384'\nprogram='/bin/sh'", "SECRET_DUP_2384");
case!(security2384_producer_script_bounds, "[[checks.script]]\nname='SECRET_BOUNDS_2384'\nprogram='/bin/sh'\ninterval_secs=0\ntimeout_secs=0\ndebounce=0", "SECRET_BOUNDS_2384");
case!(security2384_producer_cron_duplicate, "[[checks.cron]]\nname='SECRET_CRON_2384'\nexpr='* * * * *'\n[[checks.cron]]\nname='SECRET_CRON_2384'\nexpr='* * * * *'", "SECRET_CRON_2384");
case!(
    security2384_producer_channel_name,
    "[[channels]]\nname='SECRET_CHANNEL_2384!'\nowner_node='node-a.example'\ntarget='fleet'",
    "SECRET_CHANNEL_2384"
);
case!(
    security2384_producer_channel_owner,
    "[[channels]]\nname='SECRET_OWNER_2384'\nowner_node=''\ntarget='fleet'",
    "SECRET_OWNER_2384"
);
case!(security2384_producer_channel_duplicate, "[[channels]]\nname='SECRET_DUPCHAN_2384'\nowner_node='node-a.example'\ntarget='fleet'\n[[channels]]\nname='SECRET_DUPCHAN_2384'\nowner_node='node-a.example'\ntarget='fleet'", "SECRET_DUPCHAN_2384");
#[test]
fn security2384_producer_channel_target() {
    let error =
        crate::channels::ChannelTarget::try_from("SECRET_TARGET_2384".to_owned()).unwrap_err();
    assert!(!error.contains("SECRET_TARGET_2384"), "{error}");
    assert!(
        error.contains("fleet") && error.contains("repo:<name>") && error.contains("agent:<id>")
    );
}
#[test]
fn security2384_producer_sound_directory() {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let path = root.path().join("SECRET_DIRECTORY_2384.mp3");
    std::fs::create_dir(&path).unwrap();
    let config = Config {
        ui: model::UiConfig {
            sound: SoundConfig {
                path: Some(path),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let diagnostics = config.collect_diagnostics();
    assert!(diagnostics.iter().any(|d| d.contains("ui.sound.path")));
    assert!(!diagnostics.join(" ").contains("SECRET_DIRECTORY_2384"));
}

case!(
    security2384_producer_spawn_wildcard,
    "[spawn.env]\nclaude=['SECRET_WILDCARD_2384*']",
    "SECRET_WILDCARD_2384"
);
case!(
    security2384_producer_spawn_whitespace,
    "[spawn.env]\nclaude=['SECRET_WHITESPACE_2384 X']",
    "SECRET_WHITESPACE_2384"
);
case!(
    security2384_producer_navigate_range,
    "[keys]\nnavigate_workspace_up='ctrl+1..9'",
    "ctrl+1..9"
);

#[test]
fn usability2384_producer_indices_schema_and_static_hints_survive() {
    let loaded = io::load_live_config_from_str("[[keys.command]]\nkey='SECRET_2384'\ncommand='SECRET_COMMAND_2384'\n[[channels]]\nname='SECRET_CHANNEL_2384!'\nowner_node='node-a.example'\ntarget='fleet'\n[spawn.env]\nclaude=['GOOD', 'SECRET_2384*']\n[[checks.script]]\nname='SECRET_2384'\nprogram='bad'\ninterval_secs=0").unwrap();
    let text = loaded
        .diagnostics
        .into_iter()
        .chain(loaded.config.collect_diagnostics())
        .collect::<Vec<_>>()
        .join(" ");
    for part in [
        "key=keys.command[].key",
        "entry_index=0",
        "key=channels[]",
        "needs 1-64",
        "key=spawn.env",
        "item_index=1",
        "only exact names",
        "key=checks.script[].program",
        "~/.flock/checks/",
        "expected interval_secs >= 10",
    ] {
        assert!(text.contains(part), "missing {part}: {text}");
    }
    assert!(!text.contains("SECRET_"), "{text}");
    let mut registry_messages = Vec::new();
    let logs = crate::logging::capture_logs(|| {
        let config: Config = toml::from_str("[[keys.command]]\nkey='ctrl+é'\ncommand='one'\n[[keys.command]]\nkey='ctrl+é'\ncommand='two'").unwrap();
        registry_messages = config.collect_diagnostics();
    });
    let text = format!("{registry_messages:?} {logs}");
    assert!(
        text.contains("entry_index=1")
            && text.contains("kept_entry_index=0")
            && text.contains("kept keys.command[].key"),
        "{text}"
    );
    assert!(!text.contains('é'), "{text}");
}

#[test]
fn usability2384_legacy_modifiers_and_empty_command_hints() {
    let config: Config = toml::from_str(
        "[keys.indexed]\ntabs='PROBE_PROBE_2384'\n[[keys.command]]\nkey='ctrl+x'\ncommand=' '",
    )
    .unwrap();
    let messages = config.collect_diagnostics();
    assert!(
        messages.iter().any(|d| d.contains("keys.indexed.tabs")
            && d.contains("expected modifier combination")
            && !d.contains("1..9")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|d| d.contains("keys.command[].command")
            && d.contains("entry_index=0")
            && d.contains("disabling custom command")),
        "{messages:?}"
    );
    assert!(
        messages.iter().all(|d| !d.contains("PROBE_PROBE_2384")),
        "{messages:?}"
    );
}
