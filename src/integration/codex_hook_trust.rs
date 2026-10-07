use std::{io, path::Path};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

// Codex 0.160.1 hashes the canonical JSON of its normalized TOML identity.
// Optional unset fields disappear in TOML, while async defaults to false.
pub(super) fn trust_flock_hook(
    config: &str,
    source: &Path,
    hooks_file: &Value,
    command: &str,
) -> io::Result<String> {
    let source = source.canonicalize()?;
    let mut config: toml::Value = toml::from_str(config).map_err(io::Error::other)?;
    let Some(groups) = hooks_file["hooks"]["SessionStart"].as_array() else {
        return Err(io::Error::other("missing Codex SessionStart hooks"));
    };
    for (group_index, group) in groups.iter().enumerate() {
        let Some(handlers) = group["hooks"].as_array() else {
            continue;
        };
        for (handler_index, handler) in handlers.iter().enumerate() {
            if handler["type"] != "command" || handler["command"] != command {
                continue;
            }
            let mut normalized = json!({
                "type": "command", "command": command,
                "timeout": handler["timeout"].as_u64().unwrap_or(600).max(1),
                "async": handler["async"].as_bool().unwrap_or(false)
            });
            if let Some(status) = handler["statusMessage"].as_str() {
                normalized["statusMessage"] = json!(status);
            }
            if let Some(limit) = handler["additionalContextLimit"]
                .as_u64()
                .filter(|v| *v != 2500)
            {
                normalized["additionalContextLimit"] = json!(limit);
            }
            let mut identity = json!({"event_name": "session_start", "hooks": [normalized]});
            if let Some(matcher) = group["matcher"].as_str() {
                identity["matcher"] = json!(matcher);
            }
            let bytes = serde_json::to_vec(&identity)?;
            let hex: String = Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let hash = format!("sha256:{hex}");
            let key = format!(
                "{}:session_start:{group_index}:{handler_index}",
                source.display()
            );
            let table = config
                .as_table_mut()
                .ok_or_else(|| io::Error::other("invalid Codex config"))?;
            let hooks = table
                .entry("hooks")
                .or_insert_with(|| toml::Value::Table(Default::default()));
            let hooks = hooks
                .as_table_mut()
                .ok_or_else(|| io::Error::other("invalid Codex hooks table"))?;
            let state = hooks
                .entry("state")
                .or_insert_with(|| toml::Value::Table(Default::default()));
            let state = state
                .as_table_mut()
                .ok_or_else(|| io::Error::other("invalid Codex hook state"))?;
            let hook = state
                .entry(key)
                .or_insert_with(|| toml::Value::Table(Default::default()));
            hook.as_table_mut()
                .ok_or_else(|| io::Error::other("invalid Codex hook trust"))?
                .insert("trusted_hash".into(), toml::Value::String(hash));
        }
    }
    toml::to_string_pretty(&config).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_codex_01601_hooks_list() {
        let dir = std::env::temp_dir().join(format!("flock-codex-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("hooks.json");
        std::fs::write(&source, "{}").unwrap();
        let hooks = json!({"hooks": {"SessionStart": [{"hooks": [
            {"type": "command", "command": "echo unrelated", "timeout": 10}
        ]}]}});
        let config = trust_flock_hook("", &source, &hooks, "echo unrelated").unwrap();
        let config: toml::Value = toml::from_str(&config).unwrap();
        let key = format!(
            "{}:session_start:0:0",
            source.canonicalize().unwrap().display()
        );
        assert_eq!(
            config["hooks"]["state"][&key]["trusted_hash"].as_str(),
            Some("sha256:5f33ee0064530d6b4e94468620e5dde2753e2e433c015812d9cb5073296e6239")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
