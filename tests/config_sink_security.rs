//! CLI probes use an isolated config and never contact a running server.
mod support;
use std::fs;
use support::environment::Command;

fn rejects_without_echoing(args: &[&str]) {
    let root = std::env::temp_dir().join(format!(
        "config-sink-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let path = root.join("SECRET_PATH_2384.toml");
    fs::write(&path, "[ui]\nconfirm_close='SECRET_VALUE_2384' trailing").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .env_remove("FLOCK_SOCKET_PATH")
        .env_remove("FLOCK_PANE_ID")
        .env_remove("FLOCK_AGENT_ID")
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", &root)
        .env("FLOCK_CONFIG_PATH", &path)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "[ui]\nconfirm_close='SECRET_VALUE_2384' trailing"
    );
    fs::remove_dir_all(root).unwrap();
    assert_eq!(output.status.code(), Some(1));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains("SECRET_"), "{text}");
    assert!(text.contains("invalid_toml"), "{text}");
}
#[test]
fn security2384_s3b1_channel_set_cli() {
    rejects_without_echoing(&["channel", "set", "stable"]);
}
#[test]
fn security2384_s3b1_reset_keys_cli() {
    rejects_without_echoing(&["config", "reset-keys"]);
}

#[test]
fn security2384_producers_cli_check_retains_safe_context() {
    let root = std::env::temp_dir().join(format!(
        "config-producers-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let path = root.join("config.toml");
    fs::write(
        &path,
        "[keys]\nprefix='SECRET_PREFIX_2384'\n[ui.sound]\npath='SECRET_SOUND_2384.wav'\n[spawn.env]\nSECRET_AGENT_2384=['SECRET_ENV_2384']\n[[checks.script]]\nname='SECRET_CHECK_2384'\nprogram='SECRET_PROGRAM_2384'\n[[channels]]\nname='SECRET_CHANNEL_2384!'\nowner_node='node-a.example'\ntarget='fleet'",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", &root)
        .env("FLOCK_CONFIG_PATH", &path)
        .args(["config", "check", "--json"])
        .output()
        .unwrap();
    fs::remove_dir_all(root).unwrap();
    assert_eq!(output.status.code(), Some(1), "loaded with warnings");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains("SECRET_"), "{text}");
    for field in [
        "keys.prefix",
        "ui.sound.path",
        "spawn.env",
        "checks.script[].program",
        "channels[]",
    ] {
        assert!(text.contains(field), "missing {field}: {text}");
    }
    assert!(
        text.contains("entry_index=0") && text.contains("using fallback"),
        "{text}"
    );
}
