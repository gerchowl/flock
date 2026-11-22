//! Drive sync through the CLI against explicitly selected sandbox profiles.
// Raw subprocesses are the integration harness, outside the production funnel.
#![allow(clippy::disallowed_methods)]
mod support;

use std::{fs, path::PathBuf, process::Output};

use support::environment::Command;

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
        fs::create_dir_all(path.join("profile/bin")).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_flk"), path.join("profile/bin/flk"))
            .unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        self.run_with_launch(args, false)
    }
    fn run_with_launch(&self, args: &[&str], store_only: bool) -> Output {
        let executable = if store_only {
            PathBuf::from(env!("CARGO_BIN_EXE_flk"))
        } else {
            self.0.join("profile/bin/flk")
        };
        let mut command = Command::new(executable);
        if store_only {
            // Simulate a store invocation/PATH without creating a Nix store.
            // The real test image must not be accepted as a fallback either.
            command.arg0("/nix/store/fixture-flock/bin/flk");
        }
        command
            .args(args)
            .env_clear()
            .env(
                "PATH",
                if store_only {
                    PathBuf::from("/nix/store/fixture-flock/bin")
                } else {
                    self.0.join("profile/bin")
                },
            )
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
    let plan_text = String::from_utf8_lossy(&plan.stdout);
    assert!(plan_text.starts_with("MCP launch path:"));
    assert!(plan_text.contains(": would-update"));
    assert!(!plan_text.contains(": updated"));
    let json_plan = sandbox.run(&["integration", "sync", "--dry-run", "--json"]);
    assert_eq!(json_plan.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&json_plan.stdout).unwrap();
    assert!(report["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|outcome| outcome["state"] == "would-update"));
    assert!(!report["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|outcome| outcome["state"] == "updated"));
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

#[test]
fn integration_sync_cli_store_only_launch_still_refreshes_hooks() {
    let sandbox = Sandbox::new();
    assert!(sandbox
        .run(&["integration", "install", "codex", "--no-trust-hooks"])
        .status
        .success());
    let hook = sandbox.0.join("codex/flock-agent-state.sh");
    fs::write(&hook, "# FLOCK_INTEGRATION_VERSION=0\n").unwrap();
    let output = sandbox.run_with_launch(&["integration", "sync"], true);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with("MCP launch path: unavailable"), "{text}");
    assert!(text.contains("codex: updated"), "{text}");
    assert!(text.contains("refusing to pin MCP configs to current executable"));
    for target in ["claude", "codex", "opencode"] {
        assert!(text.contains(&format!("{target}: failed")), "{text}");
    }
    assert_ne!(
        fs::read_to_string(&hook).unwrap(),
        "# FLOCK_INTEGRATION_VERSION=0\n"
    );
}

#[test]
fn integration_sync_cli_reports_each_claude_local_scope() {
    let sandbox = Sandbox::new();
    let path = sandbox.0.join("claude/.claude.json");
    let project = sandbox.0.join("project").display().to_string();
    let other = sandbox.0.join("other").display().to_string();
    let pinned = "/nix/store/fixture-flock/bin/flk";
    let raw = serde_json::json!({"projects":{
        project.clone():{"mcpServers":{"flock":{"command":pinned,"args":["mcp","serve"],"env":{"KEEP":"yes"}},"unrelated":{"command":pinned,"args":["mcp","serve"]}}},
        other.clone():{"mcpServers":{"flock":{"command":"flk","args":["mcp","serve"]}}}
    }}).to_string();
    fs::write(&path, &raw).unwrap();
    let output = sandbox.run(&["integration", "sync"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(&format!(
            "claude:{}#projects.{project}: updated",
            path.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "claude:{}#projects.{other}: already-current",
            path.display()
        )),
        "{text}"
    );
    let updated: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        updated["projects"][&project]["mcpServers"]["flock"]["command"],
        sandbox.0.join("profile/bin/flk").display().to_string()
    );
    assert_eq!(
        updated["projects"][&project]["mcpServers"]["unrelated"]["command"],
        pinned
    );
    assert_eq!(
        updated["projects"][&other]["mcpServers"]["flock"]["command"],
        "flk"
    );
}
