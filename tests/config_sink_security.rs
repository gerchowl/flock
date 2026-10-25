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
