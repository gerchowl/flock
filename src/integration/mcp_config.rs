//! MCP registration is separate from the hook adapters. Existing entries belong
//! to their configuration owner and are never rewritten by integration install.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::api::schema::IntegrationTarget;

fn config_path(target: IntegrationTarget) -> io::Result<Option<PathBuf>> {
    Ok(match target {
        IntegrationTarget::Claude => {
            let dir = super::claude_dir()?;
            Some(
                if std::env::var_os(super::CLAUDE_CONFIG_DIR_ENV_VAR)
                    .is_some_and(|value| !value.is_empty())
                {
                    dir.join(".claude.json")
                } else {
                    super::home_dir()?.join(".claude.json")
                },
            )
        }
        IntegrationTarget::Codex => Some(super::codex_dir()?.join("config.toml")),
        IntegrationTarget::Opencode => {
            let dir = super::opencode_dir()?;
            Some(if dir.join("opencode.jsonc").is_file() {
                dir.join("opencode.jsonc")
            } else {
                dir.join("opencode.json")
            })
        }
        _ => None,
    })
}

pub(super) fn externally_owned(target: IntegrationTarget) -> bool {
    config_path(target)
        .ok()
        .flatten()
        .is_some_and(|path| externally_owned_at(&path))
}

fn externally_owned_at(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_symlink() || metadata.permissions().readonly()
    })
}

pub(super) fn install(target: IntegrationTarget, originally_owned: bool) -> Option<String> {
    let result = config_path(target).and_then(|path| match path {
        Some(path) => install_at(
            &path,
            target,
            originally_owned,
            super::launch::stable_launch_path,
        ),
        None => Ok(None),
    });
    registration_outcome(target, result)
}

fn registration_outcome(
    target: IntegrationTarget,
    result: io::Result<Option<String>>,
) -> Option<String> {
    match result {
        Ok(message) => message,
        Err(err) => Some(format!(
            "{} MCP registration deferred: {err}; hooks remain installed. Merge a flock MCP entry through the configuration owner using an absolute profile/user-local flk path with args mcp serve",
            super::integration_target_label(target)
        )),
    }
}

fn install_at(
    path: &Path,
    target: IntegrationTarget,
    originally_owned: bool,
    launch: impl FnOnce() -> io::Result<PathBuf>,
) -> io::Result<Option<String>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    let existing_entry = if target == IntegrationTarget::Codex {
        let doc: toml_edit::DocumentMut = content.parse().map_err(io::Error::other)?;
        doc.get("mcp_servers")
            .and_then(toml_edit::Item::as_table_like)
            .is_some_and(|servers| servers.contains_key("flock"))
    } else {
        read_json(&content, target)
            .ok()
            .and_then(|doc| {
                doc.get(if target == IntegrationTarget::Opencode {
                    "mcp"
                } else {
                    "mcpServers"
                })
                .cloned()
            })
            .is_some_and(|servers| servers.get("flock").is_some())
    };
    if existing_entry {
        return Ok(Some(format!(
            "preserved existing flock MCP entry in {}",
            path.display()
        )));
    }
    let launch = launch()?;
    let updated = if target == IntegrationTarget::Codex {
        add_toml_entry(&content, &launch)?
    } else {
        match add_json_entry(&content, &launch, target == IntegrationTarget::Opencode) {
            Ok(updated) => updated,
            Err(err) if path.extension().is_some_and(|ext| ext == "jsonc") => {
                return Ok(Some(format!(
                    "MCP config {} requires an owner-managed edit ({err}); use command {} with args mcp serve",
                    path.display(), launch.display()
                )));
            }
            Err(err) => return Err(err),
        }
    };
    let Some(updated) = updated else {
        return Ok(Some(format!(
            "preserved existing flock MCP entry in {}",
            path.display()
        )));
    };
    if originally_owned || externally_owned_at(path) {
        return Ok(Some(format!(
            "MCP config {} is externally owned; merge command {} with args mcp serve",
            path.display(),
            launch.display()
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Claude Code can update this file while installation is preparing an
    // entry. Re-read at the write boundary and defer rather than lose its edit.
    let latest = match fs::read_to_string(path) {
        Ok(latest) => latest,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    if latest != content {
        return Err(io::Error::other(
            "MCP config changed during registration; retry installation",
        ));
    }
    super::atomic_write::replace(path, updated.as_bytes())?;
    Ok(Some(format!(
        "registered flock MCP in {} using {}",
        path.display(),
        launch.display()
    )))
}

fn add_json_entry(content: &str, launch: &Path, opencode: bool) -> io::Result<Option<String>> {
    let mut doc: Value = if content.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(content).map_err(io::Error::other)?
    };
    let root = doc
        .as_object_mut()
        .ok_or_else(|| io::Error::other("MCP config must be an object"))?;
    let key = if opencode { "mcp" } else { "mcpServers" };
    let servers = root
        .entry(key)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| io::Error::other("MCP servers must be an object"))?;
    if servers.contains_key("flock") {
        return Ok(None);
    }
    let entry = if opencode {
        json!({"type": "local", "command": [launch, "mcp", "serve"], "enabled": true})
    } else {
        json!({"type": "stdio", "command": launch, "args": ["mcp", "serve"]})
    };
    // Borrow raw member text solely to locate the insertion point. Existing
    // objects retain their byte order and formatting without a global change
    // to serde_json's map ordering semantics.
    let original = if content.trim().is_empty() {
        "{}"
    } else {
        content
    };
    let members: std::collections::BTreeMap<String, &serde_json::value::RawValue> =
        serde_json::from_str(original).map_err(io::Error::other)?;
    let fragment = format!("\"flock\":{}", serde_json::to_string(&entry)?);
    let (offset, addition) = if let Some(raw) = members.get(key) {
        let start = raw.get().as_ptr() as usize - original.as_ptr() as usize;
        let end = raw
            .get()
            .rfind('}')
            .ok_or_else(|| io::Error::other("missing MCP object boundary"))?;
        (
            start + end,
            format!("{}{fragment}", if servers.is_empty() { "" } else { "," }),
        )
    } else {
        let end = original
            .rfind('}')
            .ok_or_else(|| io::Error::other("missing config object boundary"))?;
        (
            end,
            format!(
                "{}\"{key}\":{{{fragment}}}",
                if members.is_empty() { "" } else { "," }
            ),
        )
    };
    Ok(Some(format!(
        "{}{}{}",
        &original[..offset],
        addition,
        &original[offset..]
    )))
}

fn add_toml_entry(content: &str, launch: &Path) -> io::Result<Option<String>> {
    let mut doc: toml_edit::DocumentMut = content.parse().map_err(io::Error::other)?;
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| io::Error::other("MCP servers must be a table"))?;
    if servers.contains_key("flock") {
        return Ok(None);
    }
    let mut entry = toml_edit::Table::new();
    entry.insert(
        "command",
        toml_edit::value(launch.to_string_lossy().as_ref()),
    );
    let mut args = toml_edit::Array::new();
    args.push("mcp");
    args.push("serve");
    entry.insert("args", toml_edit::value(args));
    servers.insert("flock", toml_edit::Item::Table(entry));
    Ok(Some(doc.to_string()))
}

pub(crate) fn pinned_path_notices() -> Vec<String> {
    [IntegrationTarget::Claude, IntegrationTarget::Codex, IntegrationTarget::Opencode]
        .into_iter()
        .filter_map(|target| {
            let path = config_path(target).ok()??;
            let raw = match fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
                Err(err) => return Some(format!("{} MCP: unknown ({}: {err})", super::integration_target_label(target), path.display())),
            };
            let commands = match configured_commands(&raw, target) {
                Ok(commands) => commands,
                Err(err) => return Some(format!("{} MCP: unknown ({}: {err})", super::integration_target_label(target), path.display())),
            };
            let pinned: Vec<_> = commands.into_iter().filter(|(_, command)| super::launch::is_store_path(Path::new(command))).collect();
            if pinned.is_empty() {
                return None;
            }
            Some(format!("{} MCP: pinned store path in {}: {}; update the configuration owner to use a stable profile/user-local flk path, then reconnect the harness",
                super::integration_target_label(target), path.display(),
                pinned.iter().map(|(name, command)| format!("{name} = {command}")).collect::<Vec<_>>().join(", ")))
        }).collect()
}

fn read_json(raw: &str, target: IntegrationTarget) -> io::Result<Value> {
    if target == IntegrationTarget::Opencode {
        super::jsonc::parse(raw)
    } else {
        serde_json::from_str(raw).map_err(io::Error::other)
    }
}

fn configured_commands(raw: &str, target: IntegrationTarget) -> io::Result<Vec<(String, String)>> {
    if target == IntegrationTarget::Codex {
        let doc: toml_edit::DocumentMut = raw.parse().map_err(io::Error::other)?;
        return Ok(doc
            .get("mcp_servers")
            .and_then(toml_edit::Item::as_table_like)
            .map(|servers| {
                servers
                    .iter()
                    .filter_map(|(name, entry)| {
                        let entry = entry.as_table_like()?;
                        let command = entry.get("command")?.as_str()?;
                        let flock = name == "flock"
                            || entry
                                .get("args")
                                .and_then(toml_edit::Item::as_array)
                                .is_some_and(|args| {
                                    args.get(0).and_then(toml_edit::Value::as_str) == Some("mcp")
                                        && args.get(1).and_then(toml_edit::Value::as_str)
                                            == Some("serve")
                                });
                        flock.then(|| (name.to_owned(), command.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default());
    }
    let doc = read_json(raw, target)?;
    let opencode = target == IntegrationTarget::Opencode;
    Ok(doc
        .get(if opencode { "mcp" } else { "mcpServers" })
        .and_then(Value::as_object)
        .map(|servers| {
            servers
                .iter()
                .filter_map(|(name, entry)| {
                    let (command, args) = if opencode {
                        let parts = entry.get("command")?.as_array()?;
                        (parts.first()?.as_str()?, parts.get(1..).unwrap_or_default())
                    } else {
                        (
                            entry.get("command")?.as_str()?,
                            entry
                                .get("args")
                                .and_then(Value::as_array)
                                .map(Vec::as_slice)
                                .unwrap_or_default(),
                        )
                    };
                    let flock = name == "flock"
                        || (args.first().and_then(Value::as_str) == Some("mcp")
                            && args.get(1).and_then(Value::as_str) == Some("serve"));
                    flock.then(|| (name.to_owned(), command.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_registration_preserves_json_key_order_and_existing_bytes() {
        let launch = crate::test_support::unique_temp_path("ordered-flk");
        for original in [
            r#"{"zRoot":1,"mcpServers":{"zServer":{"command":"z"},"aServer":{"command":"a"}},"aRoot":2}"#,
            r#"{"zRoot":{"mcpServers":"incidental"},"aRoot":2}"#,
            r#"{"mcpServers":{},"zRoot":1,"aRoot":2}"#,
        ] {
            let updated = add_json_entry(original, &launch, false).unwrap().unwrap();
            assert!(updated.find("zRoot").unwrap() < updated.find("aRoot").unwrap());
            assert!(
                updated.contains(r#""zRoot":1"#)
                    || updated.contains(r#""zRoot":{"mcpServers":"incidental"}"#)
            );
            if original.contains("zServer") {
                assert!(updated.find("zServer").unwrap() < updated.find("aServer").unwrap());
                assert!(updated.find("aServer").unwrap() < updated.find("flock").unwrap());
            }
            let parsed: Value = serde_json::from_str(&updated).unwrap();
            assert_eq!(
                parsed["mcpServers"]["flock"]["command"],
                launch.to_str().unwrap()
            );
        }
    }

    #[test]
    fn mcp_registration_defers_store_only_parse_and_concurrent_write_failures() {
        let dir = crate::test_support::unique_temp_path("deferred-mcp");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let target = IntegrationTarget::Opencode;
        let pinned = Path::new("/nix/store/fixture-flock/bin/flk");
        let result = install_at(&path, target, false, || {
            super::super::launch::select_launch_path(Some(pinned), pinned, &[])
        });
        let guidance = registration_outcome(target, result).unwrap();
        assert!(guidance.contains("no stable flk launch path"));
        assert!(!path.exists());
        for content in ["{ // owner's comments\n \"mcp\": {} }", "invalid JSON"] {
            fs::write(&path, content).unwrap();
            let result = install_at(&path, target, false, || Ok(dir.join("flk")));
            assert!(registration_outcome(target, result)
                .unwrap()
                .contains("MCP registration deferred"));
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }
        fs::write(&path, "{}").unwrap();
        let changed = r#"{"claudeWrites":"keep"}"#;
        let result = install_at(&path, IntegrationTarget::Claude, false, || {
            fs::write(&path, changed)?;
            Ok(dir.join("flk"))
        });
        assert!(registration_outcome(IntegrationTarget::Claude, result)
            .unwrap()
            .contains("changed during registration"));
        assert_eq!(fs::read_to_string(&path).unwrap(), changed);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mcp_registrations_use_stable_path_and_preserve_custom_targets() {
        let dir = crate::test_support::unique_temp_path("mcp-registration");
        let launch = dir.as_path().join("profile/bin/flk");
        for opencode in [false, true] {
            let original = r#"{"other":true}"#;
            let updated = add_json_entry(original, &launch, opencode)
                .unwrap()
                .unwrap();
            let target = if opencode {
                IntegrationTarget::Opencode
            } else {
                IntegrationTarget::Claude
            };
            assert_eq!(
                configured_commands(&updated, target).unwrap(),
                vec![("flock".into(), launch.display().to_string())]
            );
            assert!(
                add_json_entry(&updated, &dir.as_path().join("different"), opencode)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                serde_json::from_str::<Value>(&updated).unwrap()["other"],
                true
            );
        }
        let original = "# keep this comment\nmodel = 'fixture'\n";
        let updated = add_toml_entry(original, &launch).unwrap().unwrap();
        assert!(updated.starts_with(original));
        assert_eq!(
            configured_commands(&updated, IntegrationTarget::Codex).unwrap(),
            vec![("flock".into(), launch.display().to_string())]
        );
        assert!(add_toml_entry(&updated, &dir.as_path().join("different"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn mcp_store_pins_detected_in_each_harness_and_alias() {
        let pinned = "/nix/store/fixture-flock/bin/flk";
        for (raw, target) in [
            (
                json!({"mcpServers":{"alias":{"command":pinned,"args":["mcp","serve"]}}})
                    .to_string(),
                IntegrationTarget::Claude,
            ),
            (
                json!({"mcp":{"flock":{"command":[pinned,"mcp","serve"]}}}).to_string(),
                IntegrationTarget::Opencode,
            ),
            (
                format!("[mcp_servers.flock]\ncommand = '{pinned}'\nargs = ['mcp', 'serve']\n"),
                IntegrationTarget::Codex,
            ),
        ] {
            let commands = configured_commands(&raw, target).unwrap();
            assert_eq!(commands.len(), 1);
            assert!(super::super::launch::is_store_path(Path::new(
                &commands[0].1
            )));
        }
        let custom = json!({"mcpServers":{"flock":{"command":"custom","env":{"KEEP":"yes"},"args":["special"]}}}).to_string();
        assert!(add_json_entry(
            &custom,
            Path::new("/nix/store/fixture-flock/bin/flk"),
            false
        )
        .unwrap()
        .is_none());
    }
}
