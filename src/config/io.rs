use std::path::{Path, PathBuf};

use tracing::warn;

use super::{env, model::LoadedConfig, Config, CONFIG_PATH_ENV_VAR};

const KNOWN_TOP_LEVEL_CONFIG_KEYS: &[&str] = &[
    "advanced",
    "experimental",
    "keys",
    "onboarding",
    "name",
    "peers",
    "remote",
    "session",
    "slots",
    "terminal",
    "theme",
    "ui",
    "update",
    "web",
    "worktrees",
];

pub fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(dir).join(app_dir_name())
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(format!(".config/{}", app_dir_name()))
    } else {
        PathBuf::from(format!("/tmp/{}", app_dir_name()))
    }
}

pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(dir).join(app_dir_name())
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(format!(".local/state/{}", app_dir_name()))
    } else {
        PathBuf::from(format!("/tmp/{}-state", app_dir_name()))
    }
}

impl Config {
    /// Load the config for process start. ADR-0002 phase (b): a thin wrapper
    /// over `load_live_config`, so cold start and live reload share ONE code
    /// path, one precedence (base < overlay deep-merge), and one diagnostics
    /// contract — previously the overlay only applied on reload, and a full
    /// parse error dropped the WHOLE file to defaults where the live path
    /// keeps every valid section.
    pub fn load() -> LoadedConfig {
        match load_live_config() {
            Ok(mut loaded) => {
                // Field-level validation (keybinds, sound files, idle
                // thresholds) belongs to the startup report; the live path
                // leaves it to apply_live_config's own re-validation.
                let field_diagnostics = loaded.config.collect_diagnostics();
                loaded.diagnostics.extend(field_diagnostics);
                loaded
            }
            Err(diagnostics) => {
                // No running config exists at cold start, so the live path's
                // keep-current contract collapses to defaults + diagnostics.
                for diagnostic in &diagnostics {
                    // guardrails-ok(no-raw-trace-fields): migrate to the logging.rs facade (logging redesign)
                    warn!(diagnostic = %diagnostic, "config load error, using defaults");
                }
                LoadedConfig {
                    config: Self::default(),
                    diagnostics,
                    invalid_sections: Vec::new(),
                }
            }
        }
    }
}

pub(super) fn resolve_config_relative_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    config_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(path)
}

pub fn config_path() -> PathBuf {
    if let Ok(path) = std::env::var(CONFIG_PATH_ENV_VAR) {
        return PathBuf::from(path);
    }
    config_dir().join("config.toml")
}

/// Path to the optional user overlay (config.local.toml). Mirrors
/// `config_path()`'s precedence (FLOCK_CONFIG_PATH wins) but only the
/// file name changes -- the overlay always sits next to the base config.
/// When FLOCK_CONFIG_PATH is set, the overlay sits in the same directory
/// as that override; otherwise it lives in `config_dir()`.
pub fn config_overlay_path() -> PathBuf {
    if let Ok(path) = std::env::var(CONFIG_PATH_ENV_VAR) {
        let base = PathBuf::from(path);
        if let Some(parent) = base.parent() {
            return parent.join("config.local.toml");
        }
    }
    config_dir().join("config.local.toml")
}

pub fn config_diagnostic_summary(diagnostics: &[String]) -> Option<String> {
    const MAX_VISIBLE_DIAGNOSTICS: usize = 4;

    if diagnostics.is_empty() {
        return None;
    }

    let mut lines: Vec<String> = diagnostics
        .iter()
        .take(MAX_VISIBLE_DIAGNOSTICS)
        .map(|diagnostic| diagnostic.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let hidden = diagnostics.len().saturating_sub(MAX_VISIBLE_DIAGNOSTICS);
    if hidden > 0 {
        lines.push(format!("and {hidden} more config warnings"));
    }
    Some(lines.join("\n"))
}

pub fn load_live_config() -> Result<LoadedConfig, Vec<String>> {
    let path = config_path();
    let base = if path.exists() {
        Some(
            std::fs::read_to_string(&path)
                .map_err(|err| vec![format!("config read error: {err}; keeping current config")])?,
        )
    } else {
        None
    };

    let overlay_path = config_overlay_path();
    let overlay = if overlay_path.exists() {
        match std::fs::read_to_string(&overlay_path) {
            Ok(content) => Some(content),
            Err(err) => {
                // Surface the overlay read failure as a non-fatal diagnostic
                // and continue with just the base config; matches the
                // per-section keep-current behaviour rather than dropping
                // the running config wholesale.
                let diagnostic = format!(
                    "overlay read error at {}: {err}; keeping base config",
                    overlay_path.display()
                );
                return load_with_overlay_diagnostic(base.as_deref(), Some(diagnostic));
            }
        }
    } else {
        None
    };

    load_with_overlay(base.as_deref(), overlay.as_deref(), &overlay_path)
}

/// Load the base config with the user overlay (`config.local.toml`)
/// DEEP-MERGED on top (#45).
///
/// Both base and overlay are parsed to TOML tables and merged structurally:
/// the overlay's scalars and whole values WIN, nested tables merge
/// recursively, and `[[peers]]` arrays APPEND. This lets the overlay OVERRIDE
/// a base-set scalar (e.g. `ui.tab_mode`) — the previous text-concatenation
/// approach could only ADD keys the base never set, because re-declaring a
/// `[section]` was a duplicate-key parse error.
///
/// A malformed overlay falls back to base-only with a diagnostic; the running
/// config is never dropped, preserving the per-section keep-current contract.
/// A malformed BASE keeps the current config (propagated as `Err`), unchanged.
fn load_with_overlay(
    base: Option<&str>,
    overlay: Option<&str>,
    overlay_path: &Path,
) -> Result<LoadedConfig, Vec<String>> {
    // ADR-0002 phase (d): the generic FLOCK_<UPPER_SNAKE> env layer sits
    // BELOW the file. Merge order below: env < base < overlay.
    //
    // ADR-0002 open question 1 documents the alternative (overlay < env,
    // preserving the muscle memory that FLOCK_HOST_NAME/FLOCK_DISABLE_SOUND
    // always win). To flip precedence, swap the ONE merge-order line
    // marked `ADR-0002 open question 1` below — the env table would then
    // be deep-merged INTO the base+overlay merge instead of starting the
    // merge.
    let mut env_diagnostics = Vec::new();
    let mut merged = env::env_override_table(&mut env_diagnostics);

    if let Some(b) = base {
        let base_table = parse_config_table(b)?;
        // ADR-0002 open question 1: base (file) beats env.
        deep_merge_tables(&mut merged, base_table);
    }

    let Some(overlay) = overlay else {
        // No overlay: env+base or env+defaults. Env diagnostics ride along.
        let mut loaded = load_live_config_from_table(merged)?;
        loaded.diagnostics.splice(0..0, env_diagnostics);
        return Ok(loaded);
    };

    match parse_config_table(overlay) {
        Ok(overlay_table) => {
            // ADR-0002 open question 1: overlay beats base beats env.
            deep_merge_tables(&mut merged, overlay_table);
            let mut loaded = load_live_config_from_table(merged)?;
            loaded.diagnostics.splice(0..0, env_diagnostics);
            Ok(loaded)
        }
        Err(diagnostics) => {
            // The overlay itself is unparseable; fall back to env+base and
            // surface the overlay failure as a non-fatal diagnostic.
            let overlay_diag = format!(
                "overlay at {} broke parse: {}; ignoring overlay",
                overlay_path.display(),
                diagnostics.join("; ")
            );
            let mut loaded = load_live_config_from_table(merged)?;
            loaded.diagnostics.splice(0..0, env_diagnostics);
            loaded.diagnostics.push(overlay_diag);
            Ok(loaded)
        }
    }
}

fn load_with_overlay_diagnostic(
    base: Option<&str>,
    overlay_diagnostic: Option<String>,
) -> Result<LoadedConfig, Vec<String>> {
    // Route through load_with_overlay so the env layer applies uniformly. The
    // caller ended up here because it couldn't even READ the overlay file —
    // the overlay content is None; the diagnostic is threaded through.
    let mut env_diagnostics = Vec::new();
    let mut merged = env::env_override_table(&mut env_diagnostics);

    if let Some(b) = base {
        let base_table = parse_config_table(b)?;
        deep_merge_tables(&mut merged, base_table);
    }

    let mut loaded = load_live_config_from_table(merged)?;
    loaded.diagnostics.splice(0..0, env_diagnostics);
    if let Some(diag) = overlay_diagnostic {
        loaded.diagnostics.push(diag);
    }
    Ok(loaded)
}

/// Parse TOML text into the top-level config table, mapping a parse error or
/// a non-table top level to the keep-current diagnostic.
fn parse_config_table(content: &str) -> Result<toml::map::Map<String, toml::Value>, Vec<String>> {
    // toml 1.x: `str::parse::<Value>` (FromStr) parses a single value, not a
    // document — use the serde document parser for the whole config file.
    match toml::from_str::<toml::Value>(content) {
        Ok(toml::Value::Table(table)) => Ok(table),
        Ok(_) => Err(vec![
            "config parse error: top-level config must be a table; keeping current config"
                .to_string(),
        ]),
        Err(err) => Err(vec![format!(
            "config parse error: {err}; keeping current config"
        )]),
    }
}

/// Deep-merge `overlay` into `base` (#45). Overlay scalars and whole values
/// WIN over the base (so `config.local.toml` can OVERRIDE a base-set scalar
/// like `ui.tab_mode`); nested tables merge recursively; arrays (e.g.
/// `[[peers]]`) APPEND (base entries first, then overlay) — preserving the
/// prior "overlay adds peers" behaviour while newly allowing scalar override.
fn deep_merge_tables(
    base: &mut toml::map::Map<String, toml::Value>,
    overlay: toml::map::Map<String, toml::Value>,
) {
    for (key, overlay_value) in overlay {
        match (base.get_mut(&key), overlay_value) {
            (Some(toml::Value::Table(base_table)), toml::Value::Table(overlay_table)) => {
                deep_merge_tables(base_table, overlay_table);
            }
            (Some(toml::Value::Array(base_array)), toml::Value::Array(overlay_array)) => {
                base_array.extend(overlay_array);
            }
            (_, overlay_value) => {
                base.insert(key, overlay_value);
            }
        }
    }
}

#[cfg(test)]
fn load_live_config_from_str(content: &str) -> Result<LoadedConfig, Vec<String>> {
    load_live_config_from_table(parse_config_table(content)?)
}

fn load_live_config_from_table(
    table: toml::map::Map<String, toml::Value>,
) -> Result<LoadedConfig, Vec<String>> {
    let mut config = Config::default();
    let mut diagnostics = unknown_top_level_section_diagnostics(&table);
    let mut invalid_sections = Vec::new();

    if let Some(value) = table.get("onboarding") {
        match value.clone().try_into::<Option<bool>>() {
            Ok(onboarding) => config.onboarding = onboarding,
            Err(err) => diagnostics.push(format!(
                "invalid onboarding setting: {err}; keeping current onboarding state"
            )),
        }
    }

    if let Some(value) = table.get("name") {
        match value.clone().try_into::<String>() {
            Ok(name) => config.name = name,
            Err(err) => diagnostics.push(format!(
                "invalid name setting: {err}; keeping current node name"
            )),
        }
    }

    load_live_section(
        &table,
        "theme",
        "theme config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.theme = section,
    );
    load_live_section(
        &table,
        "keys",
        "keybinding config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.keys = section,
    );
    load_live_section(
        &table,
        "terminal",
        "terminal config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.terminal = section,
    );
    load_live_section(
        &table,
        "session",
        "session config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.session = section,
    );
    load_live_section(
        &table,
        "update",
        "update config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.update = section,
    );
    load_live_section(
        &table,
        "ui",
        "ui config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.ui = section,
    );
    load_live_section(
        &table,
        "advanced",
        "advanced config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.advanced = section,
    );
    load_live_section(
        &table,
        "worktrees",
        "worktree config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.worktrees = section,
    );
    load_live_section(
        &table,
        "experimental",
        "experimental config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.experimental = section,
    );
    load_live_section(
        &table,
        "remote",
        "remote config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.remote = section,
    );
    load_live_section(
        &table,
        "peers",
        "peers config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.peers = section,
    );
    load_live_section(
        &table,
        "slots",
        "slots config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.slots = section,
    );
    load_live_section(
        &table,
        "web",
        "web config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.web = section,
    );
    validate_peers(&mut config.peers, &mut diagnostics);

    Ok(LoadedConfig {
        config,
        diagnostics,
        invalid_sections,
    })
}

/// Drop unusable `[[peers]]` entries (no name) and duplicate names, keeping
/// the first occurrence; each drop produces a diagnostic.
fn validate_peers(
    peers: &mut Vec<crate::config::model::PeerConfig>,
    diagnostics: &mut Vec<String>,
) {
    let mut seen = std::collections::HashSet::new();
    peers.retain(|peer| {
        if peer.name.trim().is_empty() {
            diagnostics.push("invalid [[peers]] entry: missing name; entry ignored".to_string());
            return false;
        }
        if !seen.insert(peer.name.clone()) {
            diagnostics.push(format!(
                "duplicate [[peers]] name \"{}\"; later entry ignored",
                peer.name
            ));
            return false;
        }
        true
    });
}

fn unknown_top_level_section_diagnostics(
    table: &toml::map::Map<String, toml::Value>,
) -> Vec<String> {
    table
        .iter()
        .filter_map(|(key, value)| unknown_top_level_section_diagnostic(key, value))
        .collect()
}

fn unknown_top_level_section_diagnostic(key: &str, value: &toml::Value) -> Option<String> {
    if KNOWN_TOP_LEVEL_CONFIG_KEYS.contains(&key) {
        return None;
    }

    let header = if value.is_table() {
        format!("[{key}]")
    } else if value
        .as_array()
        .is_some_and(|items| !items.is_empty() && items.iter().all(toml::Value::is_table))
    {
        format!("[[{key}]]")
    } else {
        return None;
    };

    if key == "toast" {
        Some(format!(
            "unknown config section {header}; did you mean [ui.toast]? ignoring section"
        ))
    } else {
        Some(format!("unknown config section {header}; ignoring section"))
    }
}

fn load_live_section<T>(
    table: &toml::map::Map<String, toml::Value>,
    section: &'static str,
    label: &str,
    diagnostics: &mut Vec<String>,
    invalid_sections: &mut Vec<String>,
    apply: impl FnOnce(T),
) where
    T: serde::de::DeserializeOwned,
{
    let Some(value) = table.get(section) else {
        return;
    };

    match value.clone().try_into::<T>() {
        Ok(section_config) => apply(section_config),
        Err(err) => {
            diagnostics.push(format!(
                "invalid {label}: {err}; keeping current {section} settings"
            ));
            invalid_sections.push(section.to_string());
        }
    }
}

pub(crate) fn upsert_top_level_bool(content: &str, key: &str, value: bool) -> String {
    let replacement = format!("{key} = {value}");
    let mut lines: Vec<String> = content.lines().map(|line| line.to_string()).collect();
    let mut in_section = false;

    for line in &mut lines {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = true;
            continue;
        }
        if in_section {
            continue;
        }
        if trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}=")) {
            *line = replacement.clone();
            return lines.join("\n") + "\n";
        }
    }

    if lines.is_empty() {
        format!("{replacement}\n")
    } else {
        format!("{replacement}\n{}\n", lines.join("\n").trim_end())
    }
}

/// Write a key = value pair in a TOML section (creates section if missing).
pub fn upsert_section_value(content: &str, section: &str, key: &str, value: &str) -> String {
    upsert_section_raw(content, section, key, value)
}

pub fn upsert_section_bool(content: &str, section: &str, key: &str, value: bool) -> String {
    upsert_section_raw(content, section, key, &value.to_string())
}

pub fn remove_section_key(content: &str, section: &str, key: &str) -> String {
    let header = format!("[{section}]");
    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    let mut in_section = false;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed == header;
            result.push(line.to_string());
            i += 1;
            continue;
        }

        if in_section
            && (trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}=")))
        {
            i += 1;
            continue;
        }

        result.push(line.to_string());
        i += 1;
    }

    result.join("\n") + "\n"
}

pub fn remove_keybinding_config_sections(content: &str) -> (String, bool) {
    let mut result = Vec::new();
    let mut removed = false;
    let mut skipping_key_section = false;
    let mut in_table = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(table_name) = toml_table_header_name(trimmed) {
            in_table = true;
            skipping_key_section = is_keys_table_name(table_name);
            if skipping_key_section {
                removed = true;
                continue;
            }
        } else if skipping_key_section || (!in_table && is_top_level_keys_assignment(trimmed)) {
            removed = true;
            continue;
        }

        result.push(line.to_string());
    }

    let mut updated = result.join("\n");
    if content.ends_with('\n') || !updated.is_empty() {
        updated.push('\n');
    }
    (updated, removed)
}

fn toml_table_header_name(trimmed: &str) -> Option<&str> {
    if let Some(name) = trimmed
        .strip_prefix("[[")
        .and_then(|value| value.strip_suffix("]]"))
    {
        return Some(name.trim());
    }
    trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .map(str::trim)
}

fn is_keys_table_name(name: &str) -> bool {
    name == "keys" || name.starts_with("keys.")
}

fn is_top_level_keys_assignment(trimmed: &str) -> bool {
    trimmed.starts_with("keys ") || trimmed.starts_with("keys=") || trimmed.starts_with("keys.")
}

fn upsert_section_raw(content: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let assignment = format!("{key} = {value}");
    let lines: Vec<&str> = content.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    let mut found_section = false;
    let mut inserted = false;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed == header {
            found_section = true;
            result.push(line.to_string());
            i += 1;

            while i < lines.len() {
                let current = lines[i];
                let current_trimmed = current.trim();
                if current_trimmed.starts_with('[') && current_trimmed.ends_with(']') {
                    if !inserted {
                        result.push(assignment.clone());
                        inserted = true;
                    }
                    break;
                }

                if current_trimmed.starts_with(&format!("{key} "))
                    || current_trimmed.starts_with(&format!("{key}="))
                {
                    result.push(assignment.clone());
                    inserted = true;
                } else {
                    result.push(current.to_string());
                }
                i += 1;
            }

            continue;
        }

        result.push(line.to_string());
        i += 1;
    }

    if !found_section {
        if !result.is_empty() && !result.last().is_some_and(|line| line.trim().is_empty()) {
            result.push(String::new());
        }
        result.push(header);
        result.push(assignment);
    } else if !inserted {
        result.push(assignment);
    }

    result.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_top_level_bool_replaces_existing_value() {
        let content = "onboarding = true\n[keys]\nprefix = \"ctrl+b\"\n";
        let updated = upsert_top_level_bool(content, "onboarding", false);
        assert!(updated.contains("onboarding = false"));
        assert!(!updated.contains("onboarding = true"));
    }

    #[test]
    fn upsert_section_bool_adds_missing_section() {
        let updated = upsert_section_bool("", "ui.toast", "enabled", true);
        assert!(updated.contains("[ui.toast]"));
        assert!(updated.contains("enabled = true"));
    }

    #[test]
    fn remove_section_key_removes_matching_key_from_section() {
        let content =
            "[ui.toast]\nenabled = true\ndelivery = \"flock\"\n[ui.sound]\nenabled = true\n";
        let updated = remove_section_key(content, "ui.toast", "enabled");
        assert!(!updated.contains("[ui.toast]\nenabled = true"));
        assert!(updated.contains("delivery = \"flock\""));
        assert!(updated.contains("[ui.sound]\nenabled = true"));
    }

    #[test]
    fn config_diagnostic_summary_keeps_multiple_warnings_visible() {
        let diagnostics = vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
            "four".to_string(),
            "five".to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("one\ntwo\nthree\nfour\nand 1 more config warnings")
        );
    }

    #[test]
    fn load_live_config_parses_session_section() {
        let loaded = load_live_config_from_str(
            r#"
[session]
resume_agents_on_restore = true
"#,
        )
        .unwrap();

        assert!(loaded.config.session.resume_agents_on_restore);
        assert!(loaded.diagnostics.is_empty());
        assert!(loaded.invalid_sections.is_empty());
    }

    #[test]
    fn load_live_config_reads_top_level_node_name() {
        let loaded = load_live_config_from_str("name = \"mba22\"\n").unwrap();
        assert_eq!(loaded.config.name, "mba22");
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert!(loaded.invalid_sections.is_empty());
    }

    #[test]
    fn overlay_can_set_node_name_when_base_omits_it() {
        // The centrally-managed-box case (#42): the base config.toml is a
        // read-only symlink with no `name`, and the user sets it in the
        // writable config.local.toml overlay. It must take effect.
        let _lock = crate::config::test_config_env_lock().lock().unwrap();
        let dir = unique_test_dir("flock-overlay-node-name");
        let base = dir.join("config.toml");
        let overlay = dir.join("config.local.toml");
        std::fs::write(&base, "[ui]\nsidebar_row_gap = 1\n").unwrap();
        std::fs::write(&overlay, "name = \"ksb\"\n").unwrap();

        let loaded = load_live_config_with_base(&base);

        assert_eq!(loaded.config.name, "ksb");
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    }

    #[test]
    fn load_live_config_parses_peers_entries() {
        let loaded = load_live_config_from_str(
            r#"
[[peers]]
name = "anvil"

[[peers]]
name = "sage"
ssh = "sage.tail22bd7c.ts.net"
"#,
        )
        .unwrap();

        assert_eq!(loaded.config.peers.len(), 2);
        assert_eq!(loaded.config.peers[0].name, "anvil");
        assert_eq!(loaded.config.peers[0].ssh_target(), "anvil");
        assert_eq!(
            loaded.config.peers[1].ssh_target(),
            "sage.tail22bd7c.ts.net"
        );
        assert!(loaded.diagnostics.is_empty());
    }

    #[test]
    fn load_live_config_drops_invalid_peer_entries() {
        let loaded = load_live_config_from_str(
            r#"
[[peers]]
ssh = "nameless"

[[peers]]
name = "anvil"

[[peers]]
name = "anvil"
ssh = "anvil-dev"
"#,
        )
        .unwrap();

        assert_eq!(loaded.config.peers.len(), 1);
        assert_eq!(loaded.config.peers[0].ssh_target(), "anvil");
        assert_eq!(loaded.diagnostics.len(), 2);
        assert!(loaded.diagnostics[0].contains("missing name"));
        assert!(loaded.diagnostics[1].contains("duplicate"));
    }

    #[test]
    fn load_live_config_warns_about_unknown_top_level_sections() {
        let loaded = load_live_config_from_str(
            r#"
[toast]
delivery = "system"

[ui.toast]
delivery = "flock"
"#,
        )
        .unwrap();

        assert_eq!(
            loaded.diagnostics,
            vec!["unknown config section [toast]; did you mean [ui.toast]? ignoring section"]
        );
        assert!(loaded.invalid_sections.is_empty());
        assert_eq!(
            loaded.config.ui.toast.delivery,
            super::super::ToastDelivery::Flock
        );
    }

    #[test]
    fn load_live_config_warns_about_unknown_bogus_section() {
        let loaded = load_live_config_from_str(
            r#"
[bogus]
key = "value"

[ui.toast]
delivery = "flock"
"#,
        )
        .unwrap();

        assert_eq!(
            loaded.diagnostics,
            vec!["unknown config section [bogus]; ignoring section"]
        );
        assert!(loaded.invalid_sections.is_empty());
    }

    #[test]
    fn load_live_config_does_not_warn_for_fully_valid_config() {
        let loaded = load_live_config_from_str(
            r#"
onboarding = false

[ui.toast]
delivery = "flock"

[[peers]]
name = "anvil"
"#,
        )
        .unwrap();

        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert!(loaded.invalid_sections.is_empty());
    }

    #[test]
    fn load_live_config_does_not_warn_about_unknown_top_level_scalar_values() {
        let loaded = load_live_config_from_str(
            r#"
plugin = []

[ui.toast]
delivery = "flock"
"#,
        )
        .unwrap();

        assert!(loaded.diagnostics.is_empty());
        assert_eq!(
            loaded.config.ui.toast.delivery,
            super::super::ToastDelivery::Flock
        );
    }

    #[test]
    fn startup_config_load_warns_about_unknown_top_level_sections() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "flock-config-unknown-section-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
[[plugin]]
id = "example"

[ui.toast]
delivery = "system"
"#,
        )
        .unwrap();
        let previous = std::env::var_os(CONFIG_PATH_ENV_VAR);
        std::env::set_var(CONFIG_PATH_ENV_VAR, &path);

        let loaded = Config::load();

        match previous {
            Some(value) => std::env::set_var(CONFIG_PATH_ENV_VAR, value),
            None => std::env::remove_var(CONFIG_PATH_ENV_VAR),
        }
        let _ = std::fs::remove_file(path);

        assert_eq!(
            loaded.diagnostics,
            vec!["unknown config section [[plugin]]; ignoring section"]
        );
        assert_eq!(
            loaded.config.ui.toast.delivery,
            super::super::ToastDelivery::System
        );
    }

    #[test]
    fn remove_keybinding_config_sections_removes_keys_tables_only() {
        let content = r#"onboarding = false

[theme]
name = "catppuccin"

[keys]
prefix = "ctrl+a"
new_tab = "c"

[[keys.command]]
key = "g"
command = "lazygit"

[keys.indexed]
tabs = "ctrl"

[ui]
mouse_capture = false
"#;

        let (updated, removed) = remove_keybinding_config_sections(content);

        assert!(removed);
        assert!(updated.contains("onboarding = false"));
        assert!(updated.contains("[theme]\nname = \"catppuccin\""));
        assert!(updated.contains("[ui]\nmouse_capture = false"));
        assert!(!updated.contains("[keys]"));
        assert!(!updated.contains("[[keys.command]]"));
        assert!(!updated.contains("[keys.indexed]"));
        assert!(toml::from_str::<toml::Value>(&updated).is_ok());
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("{label}-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn overlay_introduces_new_section_when_base_omits_it() {
        let _lock = crate::config::test_config_env_lock().lock().unwrap();
        let dir = unique_test_dir("flock-overlay-new");
        let base = dir.join("config.toml");
        let overlay = dir.join("config.local.toml");
        std::fs::write(&base, "onboarding = false\n").unwrap();
        std::fs::write(&overlay, "[ui]\nsidebar_row_gap = 3\n").unwrap();

        let previous = std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR);
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &base);

        let loaded = load_live_config().unwrap();

        match previous {
            Some(value) => std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, value),
            None => std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR),
        }

        assert_eq!(loaded.config.ui.sidebar_row_gap, 3);
        assert_eq!(loaded.config.onboarding, Some(false));
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    }

    /// Run `load_live_config` with `CONFIG_PATH_ENV_VAR` pointed at `base`,
    /// restoring the prior env afterwards. The overlay sits beside the base
    /// (see `config_overlay_path`).
    fn load_live_config_with_base(base: &Path) -> LoadedConfig {
        let previous = std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR);
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, base);
        let loaded = load_live_config().unwrap();
        match previous {
            Some(value) => std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, value),
            None => std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR),
        }
        loaded
    }

    #[test]
    fn startup_config_load_applies_overlay() {
        // ADR-0002 phase (b): Config::load (cold start) previously read ONLY
        // the base — a field set in config.local.toml worked after
        // reload_config but not at startup. Startup and live reload must share
        // one code path and one precedence.
        let _lock = crate::config::test_config_env_lock().lock().unwrap();
        let dir = unique_test_dir("flock-startup-overlay");
        let base = dir.join("config.toml");
        let overlay = dir.join("config.local.toml");
        std::fs::write(&base, "[ui]\nsidebar_row_gap = 2\n").unwrap();
        std::fs::write(&overlay, "[ui]\nsidebar_row_gap = 9\n").unwrap();

        let previous = std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR);
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &base);
        let loaded = Config::load();
        match previous {
            Some(value) => std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, value),
            None => std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR),
        }

        assert_eq!(
            loaded.config.ui.sidebar_row_gap, 9,
            "the overlay must win at COLD START, not only on live reload"
        );
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    }

    #[test]
    fn startup_config_load_reports_broken_overlay_but_keeps_base() {
        let _lock = crate::config::test_config_env_lock().lock().unwrap();
        let dir = unique_test_dir("flock-startup-overlay-bad");
        let base = dir.join("config.toml");
        let overlay = dir.join("config.local.toml");
        std::fs::write(&base, "[ui]\nsidebar_row_gap = 2\n").unwrap();
        std::fs::write(&overlay, "this is not valid toml\n").unwrap();

        let previous = std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR);
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &base);
        let loaded = Config::load();
        match previous {
            Some(value) => std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, value),
            None => std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR),
        }

        assert_eq!(loaded.config.ui.sidebar_row_gap, 2, "base survives");
        assert!(
            loaded
                .diagnostics
                .iter()
                .any(|d| d.contains("config.local.toml")),
            "expected overlay diagnostic at startup, got {:?}",
            loaded.diagnostics
        );
    }

    #[test]
    fn every_config_top_level_field_is_known() {
        // Drift guard (ADR-0002 phase c): every top-level Config field must be
        // in KNOWN_TOP_LEVEL_CONFIG_KEYS, or a DEFAULT config would trip the
        // "unknown config section" diagnostic. This failed for `slots` and
        // `name` when the list was hand-kept.
        let table = toml::Value::try_from(Config::default()).unwrap();
        let table = table.as_table().unwrap();
        for key in table.keys() {
            assert!(
                KNOWN_TOP_LEVEL_CONFIG_KEYS.contains(&key.as_str()),
                "Config field '{key}' is missing from KNOWN_TOP_LEVEL_CONFIG_KEYS \
                 — add it there AND wire it into load_live_config_from_table"
            );
        }
    }

    #[test]
    fn slots_section_survives_live_reload() {
        // `[slots]` parsed at cold start but was silently DROPPED on live
        // reload (not wired into load_live_config_from_table) and warned as an
        // unknown section.
        let _lock = crate::config::test_config_env_lock().lock().unwrap();
        let dir = unique_test_dir("flock-slots-live");
        let base = dir.join("config.toml");
        std::fs::write(&base, "[slots]\nenabled = true\nmax = 7\n").unwrap();

        let loaded = load_live_config_with_base(&base);

        assert!(loaded.config.slots.enabled, "slots.enabled must survive");
        assert_eq!(loaded.config.slots.max, 7, "slots.max must survive");
        assert!(
            loaded.diagnostics.is_empty(),
            "a known section must not warn: {:?}",
            loaded.diagnostics
        );
    }

    #[test]
    fn overlay_overrides_base_set_scalar() {
        let _lock = crate::config::test_config_env_lock().lock().unwrap();
        let dir = unique_test_dir("flock-overlay-override");
        let base = dir.join("config.toml");
        let overlay = dir.join("config.local.toml");
        // The base already sets the scalar; the overlay re-sets it in the same
        // `[ui]` table. Deep-merge makes the overlay WIN (#45) — text
        // concatenation rejected this as a duplicate key.
        std::fs::write(&base, "[ui]\nsidebar_row_gap = 2\n").unwrap();
        std::fs::write(&overlay, "[ui]\nsidebar_row_gap = 9\n").unwrap();

        let loaded = load_live_config_with_base(&base);

        assert_eq!(loaded.config.ui.sidebar_row_gap, 9);
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    }

    #[test]
    fn unparseable_overlay_keeps_base_and_surfaces_diagnostic() {
        let _lock = crate::config::test_config_env_lock().lock().unwrap();
        let dir = unique_test_dir("flock-overlay-bad");
        let base = dir.join("config.toml");
        let overlay = dir.join("config.local.toml");
        std::fs::write(&base, "[ui]\nsidebar_row_gap = 2\n").unwrap();
        // Not valid TOML (a bare line with no `=`): the overlay is dropped and
        // the base survives intact, with a diagnostic.
        std::fs::write(&overlay, "this is not valid toml\n").unwrap();

        let loaded = load_live_config_with_base(&base);

        assert_eq!(loaded.config.ui.sidebar_row_gap, 2);
        assert!(
            loaded
                .diagnostics
                .iter()
                .any(|d| d.contains("overlay") && d.contains("config.local.toml")),
            "expected overlay diagnostic, got {:?}",
            loaded.diagnostics
        );
    }

    #[test]
    fn overlay_peers_append_to_base_peers() {
        let _lock = crate::config::test_config_env_lock().lock().unwrap();
        let dir = unique_test_dir("flock-overlay-peers");
        let base = dir.join("config.toml");
        let overlay = dir.join("config.local.toml");
        std::fs::write(&base, "[[peers]]\nname = \"anvil\"\n").unwrap();
        std::fs::write(&overlay, "[[peers]]\nname = \"sage\"\n").unwrap();

        let loaded = load_live_config_with_base(&base);

        let names: Vec<&str> = loaded
            .config
            .peers
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["anvil", "sage"], "overlay peers should append");
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    }

    #[test]
    fn remove_keybinding_config_sections_reports_noop_without_keys() {
        let content = "[ui]\nmouse_capture = true\n";
        let (updated, removed) = remove_keybinding_config_sections(content);
        assert!(!removed);
        assert_eq!(updated, content);
    }
}
