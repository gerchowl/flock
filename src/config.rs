use crossterm::event::{KeyCode, KeyModifiers};

mod env;
mod io;
mod keybinds;
pub(crate) mod model;
mod sound;
mod theme;

pub use self::{
    io::{
        config_diagnostic_summary, config_dir, config_overlay_path, config_path, load_live_config,
        remove_keybinding_config_sections, remove_section_key, state_dir, upsert_section_bool,
        upsert_section_value,
    },
    keybinds::{
        format_key_combo, normalize_key_combo, terminal_key_matches_combo, ActionKeybinds,
        BindingConfig, CommandKeybindConfig, CustomCommandAction, CustomCommandKeybind,
        IndexedKeybind, Keybinds, LiveKeybindConfig,
    },
    model::{
        validated_prompt_float_lines, validated_sidebar_bounds, validated_sidebar_pane_gap,
        validated_sidebar_row_gap, Config, ConfigReloadReport, ConfigReloadStatus, FileDropMode,
        KeysConfig, NewTerminalCwdConfig, PanelScopeConfig, PeerConfig, ServerStateMarkConfig,
        ShellModeConfig, TabModeConfig, ToastClipboardPosition, ToastConfig, ToastDelivery,
        ToastFlockPosition, UpdateChannelConfig,
    },
    sound::SoundConfig,
    theme::{parse_color, CustomThemeColors, ThemeConfig},
};

pub(crate) use self::io::upsert_top_level_bool;

pub const CONFIG_PATH_ENV_VAR: &str = "FLOCK_CONFIG_PATH";
pub const DEFAULT_SCROLLBACK_LIMIT_BYTES: usize = 10_000_000;
pub const DEFAULT_MOUSE_SCROLL_LINES: usize = 3;
pub const DEFAULT_MOBILE_WIDTH_THRESHOLD: u16 = 64;
pub const DEFAULT_SIDEBAR_ROW_GAP: u16 = 1;
pub const MAX_SIDEBAR_ROW_GAP: u16 = 3;
pub const DEFAULT_SIDEBAR_PANE_GAP: u16 = 0;
pub const MAX_SIDEBAR_PANE_GAP: u16 = 4;
pub const DEFAULT_PROMPT_FLOAT_LINES: u16 = 3;
pub const MAX_PROMPT_FLOAT_LINES: u16 = 10;

#[cfg(test)]
pub(crate) fn app_dir_name() -> &'static str {
    io::app_dir_name()
}

#[cfg(test)]
pub(crate) fn test_config_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Acquire the config-env serialization lock AND neutralize ambient `FLOCK_*`
/// environment variables that the config env-layer consumes.
///
/// Without this, an ambient alias exported by the surrounding shell — a dev box
/// or a CI runner that exports `FLOCK_HOST_NAME` — feeds the config env layer a
/// stray value (and a deprecation diagnostic) that has nothing to do with the
/// config under test: reloads come back `Partial`, overlay `name` gets
/// overridden, `diagnostics.is_empty()` assertions trip. The failures are
/// invisible in isolation on machines that don't export the var and shuffle
/// across tests/platforms per CI run. Every config-env test holds this guard so
/// its view of the environment is hermetic. Held for the test's scope.
#[cfg(test)]
pub(crate) fn test_config_env_guard() -> (std::sync::MutexGuard<'static, ()>, TestFlockEnvScrub) {
    let lock = test_config_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let scrub = TestFlockEnvScrub::new();
    (lock, scrub)
}

/// RAII scrub of every `FLOCK_*` env var except `FLOCK_CONFIG_PATH` (which the
/// reload tests set explicitly). Removes them on construction, and on drop
/// clears anything the test added before restoring the original set — so no
/// `FLOCK_*` mutation leaks between serialized config-env tests.
#[cfg(test)]
pub(crate) struct TestFlockEnvScrub {
    saved: Vec<(String, std::ffi::OsString)>,
}

#[cfg(test)]
impl TestFlockEnvScrub {
    fn scrubbable() -> Vec<String> {
        std::env::vars_os()
            .filter_map(|(k, _)| {
                let ks = k.to_string_lossy();
                (ks.starts_with("FLOCK_") && ks != "FLOCK_CONFIG_PATH").then(|| ks.into_owned())
            })
            .collect()
    }

    fn new() -> Self {
        let saved: Vec<(String, std::ffi::OsString)> = Self::scrubbable()
            .into_iter()
            .filter_map(|k| std::env::var_os(&k).map(|v| (k, v)))
            .collect();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }
        Self { saved }
    }
}

#[cfg(test)]
impl Drop for TestFlockEnvScrub {
    fn drop(&mut self) {
        for k in Self::scrubbable() {
            std::env::remove_var(k);
        }
        for (k, v) in &self.saved {
            std::env::set_var(k, v);
        }
    }
}

impl Config {
    pub fn should_show_onboarding(&self) -> bool {
        self.onboarding.unwrap_or(true)
    }

    pub fn prefix_key(&self) -> (KeyCode, KeyModifiers) {
        self.validated_keybinds().1
    }

    /// Parsed keybinds for Flock actions.
    pub fn keybinds(&self) -> Keybinds {
        self.validated_keybinds().3
    }

    pub fn collect_diagnostics(&self) -> Vec<String> {
        let (prefix_diag, _, keybind_diags, _) = self.validated_keybinds();
        prefix_diag
            .into_iter()
            .chain(keybind_diags)
            .chain(self.ui.sound.diagnostics())
            .chain(self.ui.idle.diagnostics())
            .chain(self.gossip.diagnostics())
            .collect()
    }

    pub fn live_keybinds(&self) -> Result<LiveKeybindConfig, Vec<String>> {
        let (prefix_diag, prefix, keybind_diags, keybinds) = self.validated_keybinds();
        let diagnostics: Vec<String> = prefix_diag.into_iter().chain(keybind_diags).collect();
        if diagnostics.is_empty() {
            Ok(LiveKeybindConfig { prefix, keybinds })
        } else {
            Err(diagnostics)
        }
    }

    pub(crate) fn local_keybindings_profile_toml(&self) -> Result<String, toml::ser::Error> {
        let mut keys = self.keys.clone();
        keys.command.clear();

        #[derive(serde::Serialize)]
        struct KeysProfile {
            keys: KeysConfig,
        }

        toml::to_string_pretty(&KeysProfile { keys })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_keybindings_profile_includes_defaults_and_excludes_commands() {
        let config: Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
new_tab = "prefix+t"

[[keys.command]]
key = "prefix+g"
command = "lazygit"
"#,
        )
        .unwrap();

        let profile = config.local_keybindings_profile_toml().unwrap();
        assert!(profile.contains("[keys]"));
        assert!(profile.contains("prefix = \"ctrl+a\""));
        assert!(profile.contains("new_tab = \"prefix+t\""));
        assert!(profile.contains("next_tab = \"prefix+n\""));
        assert!(!profile.contains("lazygit"));
        assert!(!profile.contains("command ="));
        assert!(!profile.contains("[[keys.command]]"));
    }
}
