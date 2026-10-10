use std::collections::HashMap;
use std::path::Path;

use super::model::{KeyDiagnostic, KeyDiagnosticKind};
use super::{config_overlay_path, config_path, Config};

/// Source warnings are independent of merging: even a shadowed obsolete key
/// needs to be deleted from the file that declares it.
pub(crate) fn removed_config_warnings() -> Vec<String> {
    [config_path(), config_overlay_path()]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok().map(|text| (path, text)))
        .flat_map(|(path, text)| super::io::removed_key_diagnostics(&text, path))
        .map(|diagnostic| diagnostic.message)
        .collect()
}

pub(crate) fn file_key_diagnostics() -> Vec<KeyDiagnostic> {
    [config_path(), config_overlay_path()]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok().map(|text| (path, text)))
        .flat_map(|(path, text)| key_diagnostics(&text, path))
        .collect()
}

fn key_diagnostics(text: &str, path: &Path) -> Vec<KeyDiagnostic> {
    let mut diagnostics = super::io::removed_key_diagnostics(text, path);
    let Ok(document) = toml_edit::Document::parse(text) else {
        return diagnostics;
    };
    let mut locations = HashMap::new();
    collect_locations(document.as_item(), "", text, &mut locations);
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return diagnostics;
    };
    let mut unknown = Vec::new();
    for (key, value) in &table {
        // An unknown top-level table gets the loader's section wording,
        // including the [ui.toast] hint, rather than a second warning (#860).
        if let Some(message) = super::io::unknown_top_level_section_diagnostic(key, value) {
            if let Some(line) = locations.get(key) {
                diagnostics.push(KeyDiagnostic {
                    kind: KeyDiagnosticKind::Unknown,
                    key: key.clone(),
                    message: format!("{}:{line}: {message}", path.display()),
                });
            }
            continue;
        }
        // Deserialize sections independently so one invalid section cannot hide
        // unknown keys in another. Serde owns the schema, including aliases.
        unknown_keys(std::slice::from_ref(key), value, &mut unknown);
    }
    for key in unknown {
        let Some(line) = locations.get(&key) else {
            continue;
        };
        let canonical = key
            .split('.')
            .filter(|part| part.parse::<usize>().is_err())
            .collect::<Vec<_>>()
            .join(".");
        if diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == KeyDiagnosticKind::Removed && diagnostic.key == canonical
        }) {
            continue;
        }
        diagnostics.push(KeyDiagnostic {
            kind: KeyDiagnosticKind::Unknown,
            message: format!("{}:{line}: unknown {key} setting ignored", path.display()),
            key: canonical,
        });
    }
    diagnostics
}

/// Collect the dotted paths serde ignores under `value`, which sits at `path`.
/// Serde stops at the first type error, so a table that fails is retried one
/// key at a time: a bad value then hides nothing but itself (#860).
fn unknown_keys(path: &[String], value: &toml::Value, found: &mut Vec<String>) {
    let mut section = value.clone();
    for part in path.iter().rev() {
        section = toml::Value::Table([(part.clone(), section)].into_iter().collect());
    }
    let result: Result<Config, _> = serde_ignored::deserialize(section, |ignored| {
        let key = ignored
            .to_string()
            .split('.')
            .filter(|part| *part != "?")
            .collect::<Vec<_>>()
            .join(".");
        if !found.contains(&key) {
            found.push(key);
        }
    });
    if result.is_err() {
        let Some(table) = value.as_table() else {
            return;
        };
        for (key, child) in table {
            let mut child_path = path.to_vec();
            child_path.push(key.clone());
            unknown_keys(&child_path, child, found);
        }
    }
}

fn collect_locations(
    item: &toml_edit::Item,
    prefix: &str,
    text: &str,
    locations: &mut HashMap<String, usize>,
) {
    if let Some(table) = item.as_table_like() {
        for (key, value) in table.iter() {
            let path = if prefix.is_empty() {
                key.to_string()
            } else {
                format!("{prefix}.{key}")
            };
            if let Some(span) = table.key(key).and_then(|key| key.span()) {
                locations.insert(
                    path.clone(),
                    text[..span.start].bytes().filter(|b| *b == b'\n').count() + 1,
                );
            }
            collect_locations(value, &path, text, locations);
        }
    } else if let Some(array) = item.as_array_of_tables() {
        for (index, table) in array.iter().enumerate() {
            collect_locations(
                &toml_edit::Item::Table(table.clone()),
                &format!("{prefix}.{index}"),
                text,
                locations,
            );
        }
    } else if let Some(array) = item.as_array() {
        for (index, value) in array.iter().enumerate() {
            collect_locations(
                &toml_edit::Item::Value(value.clone()),
                &format!("{prefix}.{index}"),
                text,
                locations,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    fn messages(text: &str) -> Vec<String> {
        super::key_diagnostics(text, std::path::Path::new("fixture.toml"))
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect()
    }

    #[test]
    fn unknown_keys_have_source_locations_including_peer_arrays() {
        let diagnostics = messages(
            "future = true\n[msg]\nfuture_setting = true\n[[peers]]\nname='nodea'\nfuture_peer=1\nsummary_command='false'\n",
        );
        assert!(
            diagnostics.contains(&"fixture.toml:1: unknown future setting ignored".to_string()),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics.contains(
                &"fixture.toml:3: unknown msg.future_setting setting ignored".to_string()
            ),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics.contains(
                &"fixture.toml:6: unknown peers.0.future_peer setting ignored".to_string()
            ),
            "{diagnostics:?}"
        );
        assert_eq!(diagnostics.len(), 4, "{diagnostics:?}");
    }

    /// Serde stops at the first type error, so a bad value ahead of an unknown
    /// key in the same table used to hide the unknown key (#860).
    #[test]
    fn a_type_error_does_not_hide_unknown_keys_in_the_same_table() {
        let diagnostics = messages(
            "[msg]\nenabled = \"yes\"\nfuture_setting = true\n[ui.toast]\ndelay_seconds = \"soon\"\nfuture_toast = 1\n",
        );
        assert_eq!(
            diagnostics,
            vec![
                "fixture.toml:3: unknown msg.future_setting setting ignored".to_string(),
                "fixture.toml:6: unknown ui.toast.future_toast setting ignored".to_string(),
            ]
        );
    }

    /// A misplaced `[toast]` is one mistake, so it gets one warning: the
    /// section hint, with its location (#860).
    #[test]
    fn an_unknown_top_level_table_gets_one_located_section_warning() {
        assert_eq!(
            messages("onboarding = false\n[toast]\ndelivery = \"flock\"\n"),
            vec![
                "fixture.toml:2: unknown config section [toast]; did you mean [ui.toast]? ignoring section"
                    .to_string()
            ]
        );
    }

    /// The hand-written and untagged deserializers must not make serde_ignored
    /// report keys they do read: the legacy `ui.toast.enabled`, both shapes of
    /// a keybinding, and the string-parsed `ui` settings.
    #[test]
    fn custom_deserializers_report_no_false_unknown_keys() {
        assert_eq!(
            messages(
                "[ui]\nright_click_passthrough_modifier = \"alt\"\n[ui.toast]\nenabled = true\ndelay_seconds = 2\n[ui.toast.flock]\nposition = \"bottom_right\"\n[keys]\nhelp = \"prefix+?\"\nsettings = [\"prefix+s\", \"f2\"]\n",
            ),
            Vec::<String>::new()
        );
    }
}
