//! Drive sync through the CLI against explicitly selected sandbox profiles.
// Raw subprocesses are the integration harness, outside the production funnel.
#![allow(clippy::disallowed_methods)]
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "flk-sync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for dir in ["codex", "claude", ".config/opencode"] {
            fs::create_dir_all(path.join(dir)).unwrap();
        }
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_flk"))
            .args(args)
            .env_clear()
            .env("HOME", &self.0)
            .env("CODEX_HOME", self.0.join("codex"))
            .env("CLAUDE_CONFIG_DIR", self.0.join("claude"))
            .env("FLOCK_SOCKET_PATH", self.0.join("no-server.sock"))
            .env("FLOCK_CONFIG_PATH", self.0.join("flock.toml"))
            .output()
            .unwrap()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn integration_sync_cli_refreshes_selected_profiles_and_preserves_trust() {
    let sandbox = Sandbox::new();
    let install = sandbox.run(&["integration", "install", "codex", "--no-trust-hooks"]);
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let codex = sandbox.0.join("codex");
    let config = codex.join("config.toml");
    let raw = fs::read_to_string(&config).unwrap();
    let mut doc: toml_edit::DocumentMut = raw.parse().unwrap();
    doc["mcp_servers"]["flock"]["command"] =
        toml_edit::value("/nix/store/fixture-old-flock/bin/flk");
    doc["mcp_servers"]["flock"]["env"]["KEEP"] = toml_edit::value("yes");
    doc["hooks"]["state"]["fiction"]["trusted_hash"] = toml_edit::value("sha256:keep");
    let before = doc.to_string();
    fs::write(&config, &before).unwrap();
    let hook = codex.join("flock-agent-state.sh");
    fs::write(&hook, "# FLOCK_INTEGRATION_VERSION=0\n").unwrap();
    let hooks_before = fs::read(codex.join("hooks.json")).unwrap();
    let opencode = sandbox.0.join(".config/opencode/opencode.jsonc");
    fs::write(&opencode, "invalid JSON").unwrap();
    let claude = sandbox.0.join("claude/.claude.json");
    let claude_raw = r#"{"mcpServers":{"flock":{"command":"/nix/store/fixture/bin/flk","args":["mcp","serve"]}}}"#;
    fs::write(&claude, claude_raw).unwrap();
    // A different, unselected profile must never be discovered or touched.
    let unselected = sandbox.0.join("unselected");
    fs::create_dir_all(&unselected).unwrap();
    fs::write(unselected.join(".claude.json"), claude_raw).unwrap();
    let plan = sandbox.run(&["integration", "sync", "--dry-run"]);
    assert_eq!(plan.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&config).unwrap(), before);
    assert_eq!(
        fs::read_to_string(&hook).unwrap(),
        "# FLOCK_INTEGRATION_VERSION=0\n"
    );
    assert_eq!(fs::read_to_string(&claude).unwrap(), claude_raw);
    let sync = sandbox.run(&["integration", "sync"]);
    assert_eq!(
        sync.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&sync.stderr)
    );
    let output = String::from_utf8_lossy(&sync.stdout);
    assert!(output.contains("codex: updated"), "{output}");
    assert!(output.contains("opencode: failed"), "{output}");
    assert!(output.contains("agent restart"));
    assert!(output.contains("live-handoff"));
    assert_ne!(fs::read_to_string(&claude).unwrap(), claude_raw);
    assert_eq!(
        fs::read_to_string(unselected.join(".claude.json")).unwrap(),
        claude_raw
    );
    let after = fs::read_to_string(&config).unwrap();
    let doc: toml_edit::DocumentMut = after.parse().unwrap();
    assert_eq!(
        doc["mcp_servers"]["flock"]["env"]["KEEP"].as_str(),
        Some("yes")
    );
    assert_eq!(
        doc["hooks"]["state"]["fiction"]["trusted_hash"].as_str(),
        Some("sha256:keep")
    );
    assert_eq!(doc["hooks"]["state"].as_table_like().unwrap().len(), 1);
    assert_eq!(fs::read(codex.join("hooks.json")).unwrap(), hooks_before);
    fs::remove_file(&opencode).unwrap();
    let again = sandbox.run(&["integration", "sync"]);
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("codex: already-current"));
    assert_eq!(fs::read_to_string(&config).unwrap(), after);
    assert!(!sandbox.0.join("no-server.sock").exists());
}

#[test]
fn integration_sync_cli_skips_declarative_hook_configs() {
    let sandbox = Sandbox::new();
    let install = sandbox.run(&["integration", "install", "codex", "--no-trust-hooks"]);
    assert!(install.status.success());
    let codex = sandbox.0.join("codex");
    let hook = codex.join("flock-agent-state.sh");
    fs::write(&hook, "# FLOCK_INTEGRATION_VERSION=0\n").unwrap();
    let config = codex.join("config.toml");
    let original = fs::read(&config).unwrap();
    let owner = sandbox.0.join("owner.toml");
    fs::rename(&config, &owner).unwrap();
    std::os::unix::fs::symlink(&owner, &config).unwrap();
    let sync = sandbox.run(&["integration", "sync"]);
    assert!(sync.status.success());
    assert!(String::from_utf8_lossy(&sync.stdout).contains("codex: skipped-declarative"));
    assert_eq!(fs::read(&owner).unwrap(), original);
    assert_eq!(
        fs::read_to_string(&hook).unwrap(),
        "# FLOCK_INTEGRATION_VERSION=0\n"
    );
    assert!(fs::symlink_metadata(config)
        .unwrap()
        .file_type()
        .is_symlink());
}
