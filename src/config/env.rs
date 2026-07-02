//! ADR-0002 phase (d): generic `FLOCK_<UPPER_SNAKE_PATH>` env override layer.
//!
//! The layer synthesizes a TOML table by reading `FLOCK_<UPPER_SNAKE_PATH>`
//! for every scalar leaf of `Config::default()` and merging it BELOW the file
//! layer (env < base < overlay). Env is the per-invocation poke; the file is
//! the source of truth.
//!
//! Merge order recommendation (ADR-0002 open question 1): `env < base <
//! overlay`. To swap for the alternative `overlay < env`, change the ONE
//! merge-order line in `io::load_with_overlay` (see the comment there).
//!
//! # Name mapping
//!
//! For a scalar leaf at path `a.b.c`, the env var name is
//! `FLOCK_A_B_C` (segment-uppercase, `-`→`_`). Unset env vars are ignored.
//! A set value parses by the leaf's TOML type; a type mismatch surfaces as a
//! diagnostic and the value is skipped (so the lower layer keeps).
//!
//! # Blocklist
//!
//! Paths under `BLOCKLIST_PREFIXES` are file-only — a set env var yields an
//! "ignored" diagnostic. Prefixes are segment-aligned: `keys` blocks
//! `keys.help` but not `keysfoo`. Reasons are documented alongside the list.
//!
//! # Deprecated aliases
//!
//! `FLOCK_DISABLE_SOUND=1|true` → `ui.sound.enabled = false`.
//! `FLOCK_HOST_NAME=<x>`        → `name = <x>`.
//! Each emits a deprecation diagnostic once per load. The generic layer is
//! the primary mapping; aliases are kept for one release.

use toml::Value;

use super::Config;

/// Path prefixes that stay file-only. A path is blocklisted if it equals or
/// starts with any of these prefixes (segment-aligned).
///
/// Reasons:
/// - `keys` / `keys.*`         — keybindings are a structured tree
///   (`BindingConfig::One | Many`, `[[keys.command]]` array-of-tables) the
///   scalars-only env layer can't express.
/// - `peers`                   — `[[peers]]` is an array-of-tables the env
///   layer can't append to.
/// - `theme.custom`            — nested optional table with many optional
///   leaves; env would only ever produce partial theme fragments.
/// - `ui.agent_aliases`        — `HashMap<String, String>`: no per-key env
///   mapping under the flat `FLOCK_<UPPER>` scheme.
/// - `ui.disk_path`            — resolved RELATIVE to the config directory;
///   an env value would leak that resolution across environments.
/// - `ui.sound.path`, `ui.sound.done_path`, `ui.sound.request_path`,
///   `ui.sound.all_clear_path` — same relative-to-config concern as
///   `ui.disk_path`; the ADR calls these `sound.custom` paths.
const BLOCKLIST_PREFIXES: &[&str] = &[
    "keys",
    "peers",
    "theme.custom",
    "ui.agent_aliases",
    "ui.disk_path",
    "ui.sound.path",
    "ui.sound.done_path",
    "ui.sound.request_path",
    "ui.sound.all_clear_path",
];

fn is_blocklisted(path: &str) -> bool {
    BLOCKLIST_PREFIXES
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}.")))
}

fn env_var_name_for(path: &[String]) -> String {
    let mut name = String::from("FLOCK");
    for segment in path {
        name.push('_');
        for ch in segment.chars() {
            if ch == '-' {
                name.push('_');
            } else {
                name.push(ch.to_ascii_uppercase());
            }
        }
    }
    name
}

/// Parse an env-var value by the leaf's TOML type. Returns the parsed value or
/// a diagnostic string on failure.
fn parse_env_value(schema_leaf: &Value, raw: &str) -> Result<Value, String> {
    match schema_leaf {
        Value::String(_) => Ok(Value::String(raw.to_string())),
        Value::Boolean(_) => raw
            .parse::<bool>()
            .map(Value::Boolean)
            .map_err(|err| format!("invalid bool: {err}")),
        Value::Integer(_) => raw
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|err| format!("invalid integer: {err}")),
        Value::Float(_) => raw
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|err| format!("invalid float: {err}")),
        Value::Datetime(_) => Ok(Value::String(raw.to_string())),
        Value::Array(_) | Value::Table(_) => {
            Err("non-scalar leaves are not env-mappable".to_string())
        }
    }
}

fn insert_at_path(root: &mut toml::map::Map<String, Value>, path: &[String], value: Value) {
    let Some((last, prefix)) = path.split_last() else {
        return;
    };
    let mut cursor = root;
    for segment in prefix {
        let entry = cursor
            .entry(segment.clone())
            .or_insert_with(|| Value::Table(toml::map::Map::new()));
        let Value::Table(next) = entry else {
            return;
        };
        cursor = next;
    }
    cursor.insert(last.clone(), value);
}

fn path_has_value(root: &toml::map::Map<String, Value>, path: &[String]) -> bool {
    let Some((last, prefix)) = path.split_last() else {
        return false;
    };
    let mut cursor: &toml::map::Map<String, Value> = root;
    for segment in prefix {
        let Some(Value::Table(next)) = cursor.get(segment) else {
            return false;
        };
        cursor = next;
    }
    cursor.contains_key(last)
}

fn walk_scalar_leaves<F>(
    schema: &toml::map::Map<String, Value>,
    path: &mut Vec<String>,
    visit: &mut F,
) where
    F: FnMut(&[String], &Value),
{
    for (key, value) in schema {
        path.push(key.clone());
        match value {
            Value::Table(t) => walk_scalar_leaves(t, path, visit),
            Value::Array(_) => {
                // Arrays (e.g. [[peers]], [[keys.command]], cjk_ime_agents) are
                // not env-mappable; the blocklist backs this up for structured
                // arrays, and Vec<String> leaves are intentionally left to the
                // file (a single FLOCK_* value can't express list semantics).
            }
            _ => visit(path, value),
        }
        path.pop();
    }
}

/// Build the env-override table (ADR-0002 phase d) from `FLOCK_*` reads,
/// appending any diagnostics (unparseable values, blocklisted paths, deprecated
/// aliases) to `diagnostics`.
///
/// The returned table sits BELOW the file layer in `load_with_overlay`.
pub(super) fn env_override_table(diagnostics: &mut Vec<String>) -> toml::map::Map<String, Value> {
    let mut env_table = toml::map::Map::new();

    // 1. Walk Config::default() and pull FLOCK_A_B_C for every scalar leaf.
    let schema = toml::Value::try_from(Config::default())
        .expect("Config::default() serializes to TOML (ADR-0002 phase g)");
    let schema_table = schema
        .as_table()
        .expect("Config serializes to a top-level table")
        .clone();

    let mut path_scratch: Vec<String> = Vec::new();
    let mut visits: Vec<(Vec<String>, Value)> = Vec::new();
    walk_scalar_leaves(&schema_table, &mut path_scratch, &mut |path, leaf| {
        visits.push((path.to_vec(), leaf.clone()));
    });

    for (path, leaf) in visits {
        let joined = path.join(".");
        let env_name = env_var_name_for(&path);
        let Ok(raw) = std::env::var(&env_name) else {
            continue;
        };
        if is_blocklisted(&joined) {
            diagnostics.push(format!(
                "env {env_name} ignored: {joined} is file-only (structured or path-resolved); \
                 set it in config.toml instead"
            ));
            continue;
        }
        match parse_env_value(&leaf, &raw) {
            Ok(parsed) => insert_at_path(&mut env_table, &path, parsed),
            Err(err) => diagnostics.push(format!(
                "env {env_name}={raw:?} for {joined}: {err}; using lower config layer"
            )),
        }
    }

    // 2. Deprecated aliases (ADR-0002 phase d, one release). Each emits a
    //    deprecation diagnostic when SET regardless of whether the generic
    //    reads picked up the equivalent path — the deprecation is the point.
    apply_deprecated_aliases(&mut env_table, diagnostics);

    env_table
}

fn apply_deprecated_aliases(
    env_table: &mut toml::map::Map<String, Value>,
    diagnostics: &mut Vec<String>,
) {
    // FLOCK_DISABLE_SOUND=1|true → ui.sound.enabled = false.
    if let Ok(raw) = std::env::var("FLOCK_DISABLE_SOUND") {
        diagnostics.push(
            "FLOCK_DISABLE_SOUND is deprecated (one-release alias); \
             use FLOCK_UI_SOUND_ENABLED=false or set [ui.sound] enabled = false in config"
                .to_string(),
        );
        let normalized = raw.trim().to_ascii_lowercase();
        if matches!(normalized.as_str(), "1" | "true") {
            let path = ["ui".to_string(), "sound".to_string(), "enabled".to_string()];
            if !path_has_value(env_table, &path) {
                insert_at_path(env_table, &path, Value::Boolean(false));
            }
        }
        // Any other value (including empty) is a no-op — historically ANY
        // presence disabled sound, but the twelve-factor design routes through
        // ui.sound.enabled, so silence for out-of-vocabulary values matches
        // FLOCK_UI_SOUND_ENABLED's own parse behavior.
    }

    // FLOCK_HOST_NAME=<x> → name = <x> (empty is ignored, matching the
    // pre-migration behavior at src/app/api/peers.rs:283).
    if let Ok(raw) = std::env::var("FLOCK_HOST_NAME") {
        diagnostics.push(
            "FLOCK_HOST_NAME is deprecated (one-release alias); \
             use FLOCK_NAME=<x> or set name = \"<x>\" in config"
                .to_string(),
        );
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let path = ["name".to_string()];
            if !path_has_value(env_table, &path) {
                insert_at_path(env_table, &path, Value::String(trimmed.to_string()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_live_config, CONFIG_PATH_ENV_VAR};
    use std::path::{Path, PathBuf};

    /// RAII env guard: sets `key=value` on new, restores prior value on drop.
    /// Every env-var test here holds `test_config_env_lock` for serialization,
    /// and every guard drops before the lock releases — so tests that DON'T
    /// touch env aren't perturbed by a lingering setting.
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prev }
        }

        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
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

    /// `load_live_config()` with `FLOCK_CONFIG_PATH` pointed at `base`.
    /// Restores the prior value on drop.
    fn load_with_base(base: &Path) -> super::super::model::LoadedConfig {
        let _guard = EnvGuard::set(CONFIG_PATH_ENV_VAR, base);
        load_live_config().unwrap()
    }

    /// `load_live_config()` with NO config files present (env-only).
    fn load_env_only() -> super::super::model::LoadedConfig {
        let dir = unique_test_dir("flock-env-only");
        let path = dir.join("config.toml");
        // Path must not exist for the env-only case.
        let _guard = EnvGuard::set(CONFIG_PATH_ENV_VAR, &path);
        load_live_config().unwrap()
    }

    fn poisoned_lock<'a>(m: &'a std::sync::Mutex<()>) -> std::sync::MutexGuard<'a, ()> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ADR-0002 phase (d) test names track the spec exactly.

    #[test]
    fn env_var_applies_over_defaults() {
        let _lock = poisoned_lock(crate::config::test_config_env_lock());
        let _e = EnvGuard::set("FLOCK_UI_SIDEBAR_ROW_GAP", "3");
        // No FLOCK_DISABLE_SOUND / FLOCK_HOST_NAME leakage from prior tests.
        let _s = EnvGuard::unset("FLOCK_DISABLE_SOUND");
        let _h = EnvGuard::unset("FLOCK_HOST_NAME");

        let loaded = load_env_only();

        assert_eq!(
            loaded.config.ui.sidebar_row_gap, 3,
            "env-set scalar must apply when no file is present"
        );
        assert!(
            loaded.diagnostics.is_empty(),
            "no diagnostics for a valid env value, got {:?}",
            loaded.diagnostics
        );
    }

    #[test]
    fn base_file_beats_env() {
        // env=3, base file sets 1 — file WINS (env < base < overlay).
        let _lock = poisoned_lock(crate::config::test_config_env_lock());
        let _e = EnvGuard::set("FLOCK_UI_SIDEBAR_ROW_GAP", "3");
        let _s = EnvGuard::unset("FLOCK_DISABLE_SOUND");
        let _h = EnvGuard::unset("FLOCK_HOST_NAME");
        let dir = unique_test_dir("flock-env-base-beats");
        let base = dir.join("config.toml");
        std::fs::write(&base, "[ui]\nsidebar_row_gap = 1\n").unwrap();

        let loaded = load_with_base(&base);

        assert_eq!(
            loaded.config.ui.sidebar_row_gap, 1,
            "base file must beat env"
        );
    }

    #[test]
    fn overlay_beats_base_beats_env() {
        // env=3, base=1, overlay=2 → overlay wins; strict order env < base <
        // overlay.
        let _lock = poisoned_lock(crate::config::test_config_env_lock());
        let _e = EnvGuard::set("FLOCK_UI_SIDEBAR_ROW_GAP", "3");
        let _s = EnvGuard::unset("FLOCK_DISABLE_SOUND");
        let _h = EnvGuard::unset("FLOCK_HOST_NAME");
        let dir = unique_test_dir("flock-env-overlay-beats");
        let base = dir.join("config.toml");
        let overlay = dir.join("config.local.toml");
        std::fs::write(&base, "[ui]\nsidebar_row_gap = 1\n").unwrap();
        std::fs::write(&overlay, "[ui]\nsidebar_row_gap = 2\n").unwrap();

        let loaded = load_with_base(&base);

        assert_eq!(
            loaded.config.ui.sidebar_row_gap, 2,
            "overlay must beat base which beats env"
        );
    }

    #[test]
    fn env_var_nested_path() {
        // FLOCK_UI_IDLE_ENABLE=false toggles [ui.idle] enable = false.
        let _lock = poisoned_lock(crate::config::test_config_env_lock());
        let _e = EnvGuard::set("FLOCK_UI_IDLE_ENABLE", "false");
        let _s = EnvGuard::unset("FLOCK_DISABLE_SOUND");
        let _h = EnvGuard::unset("FLOCK_HOST_NAME");

        let loaded = load_env_only();

        assert!(
            !loaded.config.ui.idle.enable,
            "nested FLOCK_UI_IDLE_ENABLE must reach ui.idle.enable"
        );
    }

    #[test]
    fn env_var_invalid_type_becomes_diagnostic() {
        // "abc" won't parse as u16 — expect a diagnostic and the lower layer
        // (default) keeps.
        let _lock = poisoned_lock(crate::config::test_config_env_lock());
        let _e = EnvGuard::set("FLOCK_UI_SIDEBAR_ROW_GAP", "abc");
        let _s = EnvGuard::unset("FLOCK_DISABLE_SOUND");
        let _h = EnvGuard::unset("FLOCK_HOST_NAME");

        let loaded = load_env_only();

        assert_eq!(
            loaded.config.ui.sidebar_row_gap,
            crate::config::DEFAULT_SIDEBAR_ROW_GAP,
            "invalid env parses to a diagnostic; default value keeps"
        );
        assert!(
            loaded
                .diagnostics
                .iter()
                .any(|d| d.contains("FLOCK_UI_SIDEBAR_ROW_GAP") && d.contains("invalid")),
            "expected an invalid-value diagnostic, got {:?}",
            loaded.diagnostics
        );
    }

    #[test]
    fn env_var_blocklisted_ignored_with_diagnostic() {
        // FLOCK_KEYS_PREFIX targets a blocklisted path (structured keybinds).
        // The env value is ignored; the diagnostic explains why.
        let _lock = poisoned_lock(crate::config::test_config_env_lock());
        let _e = EnvGuard::set("FLOCK_KEYS_PREFIX", "alt+z");
        let _s = EnvGuard::unset("FLOCK_DISABLE_SOUND");
        let _h = EnvGuard::unset("FLOCK_HOST_NAME");

        let loaded = load_env_only();

        assert_eq!(
            loaded.config.keys.prefix, "ctrl+b",
            "blocklisted path stays at default; env is ignored"
        );
        assert!(
            loaded
                .diagnostics
                .iter()
                .any(|d| d.contains("FLOCK_KEYS_PREFIX") && d.contains("ignored")),
            "expected an ignored-blocklist diagnostic, got {:?}",
            loaded.diagnostics
        );
    }

    #[test]
    fn deprecated_flock_disable_sound_alias_disables_and_warns() {
        // FLOCK_DISABLE_SOUND=1 → ui.sound.enabled = false via the env layer,
        // plus a deprecation diagnostic. The kill switch must still work end
        // to end: SoundConfig::allows(None) returns false.
        let _lock = poisoned_lock(crate::config::test_config_env_lock());
        let _e = EnvGuard::set("FLOCK_DISABLE_SOUND", "1");
        let _h = EnvGuard::unset("FLOCK_HOST_NAME");

        let loaded = load_env_only();

        assert!(
            !loaded.config.ui.sound.enabled,
            "FLOCK_DISABLE_SOUND=1 must resolve to ui.sound.enabled=false"
        );
        assert!(
            !loaded.config.ui.sound.allows(None),
            "kill switch end-to-end: allows(None) is the play() gate at call sites"
        );
        assert!(
            loaded
                .diagnostics
                .iter()
                .any(|d| d.contains("FLOCK_DISABLE_SOUND") && d.contains("deprecated")),
            "expected deprecation diagnostic, got {:?}",
            loaded.diagnostics
        );
    }

    #[test]
    fn deprecated_flock_disable_sound_below_file() {
        // The alias is at the env layer, so `[ui.sound] enabled = true` in
        // config.toml BEATS FLOCK_DISABLE_SOUND=1. This is the twelve-factor
        // flip ADR-0002 open question 1 documents; the muscle-memory of
        // "FLOCK_DISABLE_SOUND always wins" no longer holds.
        let _lock = poisoned_lock(crate::config::test_config_env_lock());
        let _e = EnvGuard::set("FLOCK_DISABLE_SOUND", "1");
        let _h = EnvGuard::unset("FLOCK_HOST_NAME");
        let dir = unique_test_dir("flock-env-disable-sound-below");
        let base = dir.join("config.toml");
        std::fs::write(&base, "[ui.sound]\nenabled = true\n").unwrap();

        let loaded = load_with_base(&base);

        assert!(
            loaded.config.ui.sound.enabled,
            "file WINS over env alias — the source-of-truth flip in ADR-0002 open question 1"
        );
    }

    #[test]
    fn deprecated_flock_host_name_alias_sets_name_and_warns() {
        let _lock = poisoned_lock(crate::config::test_config_env_lock());
        let _e = EnvGuard::set("FLOCK_HOST_NAME", "alice");
        let _s = EnvGuard::unset("FLOCK_DISABLE_SOUND");

        let loaded = load_env_only();

        assert_eq!(
            loaded.config.name, "alice",
            "FLOCK_HOST_NAME must land on Config.name via the alias"
        );
        assert!(
            loaded
                .diagnostics
                .iter()
                .any(|d| d.contains("FLOCK_HOST_NAME") && d.contains("deprecated")),
            "expected deprecation diagnostic, got {:?}",
            loaded.diagnostics
        );
    }

    #[test]
    fn env_var_name_mapping_uppercases_and_underscores() {
        assert_eq!(
            env_var_name_for(&["ui".to_string(), "idle".to_string(), "enable".to_string()]),
            "FLOCK_UI_IDLE_ENABLE"
        );
        assert_eq!(
            env_var_name_for(&["ui".to_string(), "sidebar_row_gap".to_string()]),
            "FLOCK_UI_SIDEBAR_ROW_GAP"
        );
        // Dashes in path segments (rare — Rust struct fields are snake_case,
        // but the spec calls for the transform) collapse to underscores.
        assert_eq!(
            env_var_name_for(&["a-b".to_string(), "c".to_string()]),
            "FLOCK_A_B_C"
        );
    }

    #[test]
    fn blocklist_matches_segment_aligned_prefixes() {
        assert!(is_blocklisted("keys"));
        assert!(is_blocklisted("keys.prefix"));
        assert!(is_blocklisted("keys.command.0.key"));
        assert!(is_blocklisted("peers"));
        assert!(is_blocklisted("theme.custom"));
        assert!(is_blocklisted("theme.custom.accent"));
        assert!(is_blocklisted("ui.sound.path"));
        // Segment-aligned: "keysfoo" is NOT blocked by "keys".
        assert!(!is_blocklisted("keysfoo"));
        assert!(!is_blocklisted("ui.sound.enabled"));
        assert!(!is_blocklisted("theme.name"));
    }
}
