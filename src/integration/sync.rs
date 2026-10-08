//! Explicit reconciliation of installed integrations and owned MCP launch fields.
use std::{fs, io, path::Path};

use crate::api::schema::IntegrationTarget;
use serde_json::value::RawValue;

pub(crate) struct Outcome {
    pub message: String,
    pub failed: bool,
}

fn outcome(label: &str, path: &Path, state: &str, detail: &str) -> Outcome {
    Outcome {
        message: format!("{label}: {state} ({}): {detail}", path.display()),
        failed: state == "failed",
    }
}

/// Only the selected environment profiles are considered. No home/profile scan.
pub(crate) fn run(dry_run: bool) -> io::Result<Vec<Outcome>> {
    let launch = super::launch::stable_launch_path()?;
    let mut outcomes = Vec::new();
    for status in super::installed_integration_statuses() {
        if !matches!(
            status.target,
            IntegrationTarget::Claude | IntegrationTarget::Codex | IntegrationTarget::Opencode
        ) || status.state == super::IntegrationStatusKind::NotInstalled
        {
            continue;
        }
        let target = status.target;
        let label = super::integration_target_label(target);
        let mut paths = vec![status.path.clone()];
        let dir = match target {
            IntegrationTarget::Claude => Some(super::claude_dir()?),
            IntegrationTarget::Codex => Some(super::codex_dir()?),
            IntegrationTarget::Opencode => Some(super::opencode_dir()?),
            _ => status.path.parent().map(Path::to_owned),
        };
        if let Some(dir) = &dir {
            paths.push(dir.clone());
            for name in ["settings.json", "hooks.json", "config.toml"] {
                paths.push(dir.join(name));
            }
        }
        if paths.iter().any(|path| protected(path)) {
            let fragment = hook_fragment(target, &status.path)?;
            outcomes.push(outcome(
                label,
                &status.path,
                "skipped-declarative",
                &format!("merge through your configuration owner: {fragment}"),
            ));
            continue;
        }
        let drift = match hook_drift(target, &status.path) {
            Ok(drift) => drift,
            Err(err) => {
                outcomes.push(outcome(label, &status.path, "failed", &err.to_string()));
                continue;
            }
        };
        if status.state == super::IntegrationStatusKind::Current && !drift {
            outcomes.push(outcome(
                label,
                &status.path,
                "already-current",
                "hook assets and registration",
            ));
            continue;
        }
        if dry_run {
            outcomes.push(outcome(
                label,
                &status.path,
                "updated",
                "would refresh installed hooks",
            ));
            continue;
        }
        // A sync does not opt a user into trusting hooks. Existing trust state
        // is retained by the Codex adapter.
        let result = match target {
            IntegrationTarget::Claude => super::install_claude().map(|_| ()),
            IntegrationTarget::Codex => super::install_codex_with_trust(false).map(|_| ()),
            IntegrationTarget::Opencode => super::install_opencode().map(|_| ()),
            _ => super::install_target_with_hook_trust(target, false).map(|_| ()),
        };
        outcomes.push(match result {
            Ok(()) => outcome(label, &status.path, "updated", "refreshed installed hooks"),
            Err(err) => outcome(label, &status.path, "failed", &err.to_string()),
        });
    }
    for target in [
        IntegrationTarget::Claude,
        IntegrationTarget::Codex,
        IntegrationTarget::Opencode,
    ] {
        if let Some(path) = super::mcp_config::config_path(target)? {
            outcomes.push(sync_mcp_at(&path, target, &launch, dry_run));
        }
    }
    Ok(outcomes)
}

fn hook_fragment(target: IntegrationTarget, path: &Path) -> io::Result<String> {
    if target == IntegrationTarget::Claude {
        return super::integration_manifest(target).map(|value| value.to_string());
    }
    if target == IntegrationTarget::Codex {
        let command = format!(
            "bash {} session",
            super::shell_single_quote(&path.display().to_string())
        );
        let hooks = serde_json::json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":command,"timeout":10}]}]}});
        return Ok(format!("hooks.json: {hooks}; config.toml: features.hooks = true; retain hook trust and install the current flock-agent-state.sh through the owner"));
    }
    Ok(format!(
        "install the current flock-agent-state.js through your owner at {}",
        path.display()
    ))
}

fn hook_drift(target: IntegrationTarget, path: &Path) -> io::Result<bool> {
    let asset = match target {
        IntegrationTarget::Claude => Some(super::CLAUDE_HOOK_ASSET),
        IntegrationTarget::Codex => Some(super::CODEX_HOOK_ASSET),
        IntegrationTarget::Opencode => Some(super::OPENCODE_PLUGIN_ASSET),
        _ => None,
    };
    if let Some(asset) = asset {
        if fs::read_to_string(path)? != asset {
            return Ok(true);
        }
    }
    if target == IntegrationTarget::Codex {
        let dir = super::codex_dir()?;
        let config = match fs::read_to_string(dir.join("config.toml")) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err),
        };
        if super::build_codex_config_with_hooks(&config)? != config {
            return Ok(true);
        }
        let hooks = match fs::read_to_string(dir.join("hooks.json")) {
            Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw).map_err(io::Error::other)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(err) => return Err(err),
        };
        let command = format!(
            "bash {} session",
            super::shell_single_quote(&path.display().to_string())
        );
        return Ok(!hooks["hooks"]["SessionStart"]
            .as_array()
            .is_some_and(|groups| {
                groups.iter().any(|group| {
                    group["hooks"].as_array().is_some_and(|handlers| {
                        handlers.iter().any(|hook| {
                            hook["type"] == "command"
                                && hook["command"] == command
                                && hook["timeout"] == 10
                        })
                    })
                })
            }));
    }
    Ok(super::settings_drift_details(target).is_some())
}

fn protected(path: &Path) -> bool {
    super::mcp_config::externally_owned_at(path)
        || path.ancestors().any(|parent| {
            fs::metadata(parent).is_ok_and(|metadata| metadata.permissions().readonly())
                || parent
                    .canonicalize()
                    .is_ok_and(|resolved| super::launch::is_store_path(&resolved))
        })
}

fn sync_mcp_at(path: &Path, target: IntegrationTarget, launch: &Path, dry_run: bool) -> Outcome {
    let label = super::integration_target_label(target);
    let result = (|| -> io::Result<(&str, String)> {
        let original = match fs::read_to_string(path) {
            Ok(value) => value,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(("already-current", "no registered flock MCP entry".into()))
            }
            Err(err) => return Err(err),
        };
        let updated = rewrite_mcp(&original, target, launch)?;
        if updated == original {
            return Ok((
                "already-current",
                "MCP launch fields (custom entries preserved)".into(),
            ));
        }
        if protected(path) {
            let fragment = match target {
                IntegrationTarget::Codex => format!(
                    "mcp_servers.flock.command = {}",
                    serde_json::to_string(launch)?
                ),
                IntegrationTarget::Opencode => {
                    let doc = super::jsonc::parse(&updated)?;
                    serde_json::json!({"mcp":{"flock":{"command":doc["mcp"]["flock"]["command"]}}})
                        .to_string()
                }
                _ => serde_json::json!({"mcpServers":{"flock":{"command":launch}}}).to_string(),
            };
            return Ok(("skipped-declarative", format!("merge launch field through your Nix/configuration owner, preserving other fields: {fragment}")));
        }
        if dry_run {
            return Ok((
                "updated",
                format!("would set MCP launch path to {}", launch.display()),
            ));
        }
        if fs::read_to_string(path)? != original {
            return Err(io::Error::other("config changed during sync; retry"));
        }
        super::atomic_write::replace(path, updated.as_bytes())?;
        Ok((
            "updated",
            format!("MCP launch path set to {}", launch.display()),
        ))
    })();
    match result {
        Ok((state, detail)) => outcome(label, path, state, &detail),
        Err(err) => outcome(label, path, "failed", &err.to_string()),
    }
}

fn owned(command: &str, args: &[String]) -> bool {
    (command == "flk" || command == "flock" || super::launch::is_store_path(Path::new(command)))
        && Path::new(command)
            .file_name()
            .is_some_and(|name| name == "flk" || name == "flock")
        && args.first().is_some_and(|arg| arg == "mcp")
        && args.get(1).is_some_and(|arg| arg == "serve")
}

fn rewrite_mcp(raw: &str, target: IntegrationTarget, launch: &Path) -> io::Result<String> {
    if target == IntegrationTarget::Codex {
        let mut doc: toml_edit::DocumentMut = raw.parse().map_err(io::Error::other)?;
        let Some(entry) = doc
            .get_mut("mcp_servers")
            .and_then(toml_edit::Item::as_table_like_mut)
            .and_then(|servers| servers.get_mut("flock"))
            .and_then(toml_edit::Item::as_table_like_mut)
        else {
            return Ok(raw.to_owned());
        };
        let args = entry
            .get("args")
            .and_then(toml_edit::Item::as_array)
            .map(|args| {
                args.iter()
                    .map(|arg| arg.as_str().unwrap_or_default().to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let Some(command) = entry.get("command").and_then(toml_edit::Item::as_str) else {
            return Ok(raw.to_owned());
        };
        if !owned(command, &args) || Path::new(command) == launch {
            return Ok(raw.to_owned());
        }
        let mut value = toml_edit::Value::from(launch.to_string_lossy().as_ref());
        if let Some(previous) = entry.get("command").and_then(toml_edit::Item::as_value) {
            *value.decor_mut() = previous.decor().clone();
        }
        entry.insert("command", toml_edit::Item::Value(value));
        return Ok(doc.to_string());
    }
    let opencode = target == IntegrationTarget::Opencode;
    let masked = if opencode {
        jsonc_mask(raw)?
    } else {
        raw.to_owned()
    };
    let members: std::collections::BTreeMap<String, &RawValue> =
        serde_json::from_str(&masked).map_err(io::Error::other)?;
    let Some(servers) = members.get(if opencode { "mcp" } else { "mcpServers" }) else {
        return Ok(raw.to_owned());
    };
    let servers: std::collections::BTreeMap<String, &RawValue> =
        serde_json::from_str(servers.get()).map_err(io::Error::other)?;
    let Some(entry) = servers.get("flock") else {
        return Ok(raw.to_owned());
    };
    let fields: std::collections::BTreeMap<String, &RawValue> =
        serde_json::from_str(entry.get()).map_err(io::Error::other)?;
    let Some(command) = fields.get("command") else {
        return Ok(raw.to_owned());
    };
    let (leaf, command, args) = if opencode {
        let parts: Vec<&RawValue> =
            serde_json::from_str(command.get()).map_err(io::Error::other)?;
        let Some(leaf) = parts.first() else {
            return Ok(raw.to_owned());
        };
        let command: String = serde_json::from_str(leaf.get()).map_err(io::Error::other)?;
        let args = parts
            .iter()
            .skip(1)
            .map(|part| serde_json::from_str::<String>(part.get()).map_err(io::Error::other))
            .collect::<io::Result<Vec<_>>>()?;
        (*leaf, command, args)
    } else {
        let args: Vec<String> = fields
            .get("args")
            .map(|value| serde_json::from_str(value.get()))
            .transpose()
            .map_err(io::Error::other)?
            .unwrap_or_default();
        (
            *command,
            serde_json::from_str::<String>(command.get()).map_err(io::Error::other)?,
            args,
        )
    };
    if !owned(&command, &args) || Path::new(&command) == launch {
        return Ok(raw.to_owned());
    }
    // As in the registration splice, borrow raw JSON solely to locate the
    // changed token. All other bytes, including JSONC comments, survive.
    let start = leaf.get().as_ptr() as usize - masked.as_ptr() as usize;
    let mut updated = raw.to_owned();
    updated.replace_range(
        start..start + leaf.get().len(),
        &serde_json::to_string(launch)?,
    );
    Ok(updated)
}

/// Replace JSONC syntax with equal-length whitespace so raw token offsets
/// still refer to the owner's original bytes, including Unicode comments.
fn jsonc_mask(raw: &str) -> io::Result<String> {
    let mut bytes = raw.as_bytes().to_vec();
    let mut index = 0;
    let mut quoted = false;
    while index < bytes.len() {
        if quoted {
            if bytes[index] == b'\\' {
                index += 2;
                continue;
            }
            if bytes[index] == b'"' {
                quoted = false;
            }
        } else if bytes[index] == b'"' {
            quoted = true;
        } else if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                bytes[index] = b' ';
                index += 1;
            }
            continue;
        } else if bytes[index..].starts_with(b"/*") {
            let start = index;
            index += 2;
            while index < bytes.len() && !bytes[index..].starts_with(b"*/") {
                index += 1;
            }
            if index == bytes.len() {
                return Err(io::Error::other("unterminated JSONC comment"));
            }
            index += 2;
            bytes[start..index].fill(b' ');
            continue;
        }
        index += 1;
    }
    quoted = false;
    index = 0;
    while index < bytes.len() {
        if quoted && bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == b'"' {
            quoted = !quoted;
        }
        if !quoted
            && bytes[index] == b','
            && bytes[index + 1..]
                .iter()
                .find(|byte| !byte.is_ascii_whitespace())
                .is_some_and(|byte| *byte == b'}' || *byte == b']')
        {
            bytes[index] = b' ';
        }
        index += 1;
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_mcp_preserves_every_byte_except_owned_launch_token() {
        let launch = crate::test_support::unique_temp_path("sync-profile").join("bin/flk");
        let pinned = "/nix/store/fixture-old-flock/bin/flk";
        let replacement = serde_json::to_string(&launch).unwrap();
        for (raw, target) in [
            (
                format!(
                    r#"{{"z":1,"mcpServers":{{"other":{{"command":"keep"}},"flock":{{"command":"{pinned}","args":["mcp","serve","--channels"],"env":{{"KEEP":"yes"}},"permissions":["allow"]}}}},"a":2}}"#
                ),
                IntegrationTarget::Claude,
            ),
            (
                format!(
                    r#"{{ // ü owner comment
"mcp":{{"flock":{{"type":"local","command":["{pinned}","mcp","serve","--channels",],"environment":{{"KEEP":"yes"}},}},}},}}"#
                ),
                IntegrationTarget::Opencode,
            ),
        ] {
            let updated = rewrite_mcp(&raw, target, &launch).unwrap();
            assert_eq!(updated, raw.replace(&format!("\"{pinned}\""), &replacement));
            assert_eq!(rewrite_mcp(&updated, target, &launch).unwrap(), updated);
        }
        let raw = format!("# owner\n[mcp_servers.flock]\ncommand = '{pinned}' # launch\nargs = ['mcp', 'serve', '--channels']\nenv = {{ KEEP = 'yes' }}\n[hooks.state.fixture]\ntrusted_hash = 'sha256:keep'\n");
        let updated = rewrite_mcp(&raw, IntegrationTarget::Codex, &launch).unwrap();
        assert_eq!(updated, raw.replace(&format!("'{pinned}'"), &replacement));
        assert_eq!(
            rewrite_mcp(&updated, IntegrationTarget::Codex, &launch).unwrap(),
            updated
        );
    }

    #[test]
    fn sync_mcp_preserves_custom_entries_and_unregistered_aliases() {
        let launch = crate::test_support::unique_temp_path("sync-profile");
        for raw in [
            r#"{"mcpServers":{"alias":{"command":"/nix/store/fixture/bin/flk","args":["mcp","serve"]}}}"#,
            r#"{"mcpServers":{"flock":{"command":"custom","args":["mcp","serve"]}}}"#,
            r#"{"mcpServers":{"flock":{"command":"flk","args":["special"]}}}"#,
        ] {
            assert_eq!(
                rewrite_mcp(raw, IntegrationTarget::Claude, &launch).unwrap(),
                raw
            );
        }
    }

    #[test]
    fn sync_mcp_dry_run_declarative_and_failures_never_write() {
        let dir = crate::test_support::unique_temp_path("sync-mcp-files");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let launch = dir.join("profile/bin/flk");
        let raw = r#"{"mcpServers":{"flock":{"command":"/nix/store/fixture/bin/flk","args":["mcp","serve"]}}}"#;
        fs::write(&path, raw).unwrap();
        assert!(sync_mcp_at(&path, IntegrationTarget::Claude, &launch, true)
            .message
            .contains("would set"));
        assert_eq!(fs::read_to_string(&path).unwrap(), raw);
        let link = dir.join("declarative.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let report = sync_mcp_at(&link, IntegrationTarget::Claude, &launch, false);
        assert!(report.message.contains("skipped-declarative"));
        assert!(report.message.contains("mcpServers"));
        assert_eq!(fs::read_to_string(&path).unwrap(), raw);
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        fs::write(&path, "invalid").unwrap();
        assert!(sync_mcp_at(&path, IntegrationTarget::Claude, &launch, false).failed);
        assert_eq!(fs::read_to_string(&path).unwrap(), "invalid");
        fs::write(&path, raw).unwrap();
        assert!(
            sync_mcp_at(&path, IntegrationTarget::Claude, &launch, false)
                .message
                .contains("updated")
        );
        assert!(
            sync_mcp_at(&path, IntegrationTarget::Claude, &launch, false)
                .message
                .contains("already-current")
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
