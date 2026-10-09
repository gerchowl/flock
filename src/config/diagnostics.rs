use std::collections::HashMap;
use std::path::Path;

use super::{config_overlay_path, config_path, Config};

/// Source warnings are independent of merging: even a shadowed obsolete key
/// needs to be deleted from the file that declares it.
pub(crate) fn removed_config_warnings() -> Vec<String> {
    [config_path(), config_overlay_path()]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok().map(|text| (path, text)))
        .flat_map(|(path, text)| super::io::removed_key_diagnostics(&text, path))
        .collect()
}

pub(crate) fn file_key_diagnostics() -> Vec<String> {
    [config_path(), config_overlay_path()]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok().map(|text| (path, text)))
        .flat_map(|(path, text)| key_diagnostics(&text, path))
        .collect()
}

fn key_diagnostics(text: &str, path: &Path) -> Vec<String> {
    let mut diagnostics = super::io::removed_key_diagnostics(text, path);
    let Ok(document) = toml_edit::Document::parse(text) else {
        return diagnostics;
    };
    let mut locations = HashMap::new();
    collect_locations(document.as_item(), "", text, &mut locations);
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return diagnostics;
    };
    // Deserialize sections independently so one invalid section cannot hide
    // unknown keys in another. Serde owns the schema, including aliases.
    for (key, value) in table {
        let section = toml::Value::Table([(key, value)].into_iter().collect());
        let _: Result<Config, _> = serde_ignored::deserialize(section, |ignored| {
            let key = ignored.to_string();
            let key = key
                .split('.')
                .filter(|part| *part != "?")
                .collect::<Vec<_>>()
                .join(".");
            let Some(line) = locations.get(&key) else {
                return;
            };
            let canonical = key
                .split('.')
                .filter(|part| part.parse::<usize>().is_err())
                .collect::<Vec<_>>()
                .join(".");
            if diagnostics
                .iter()
                .any(|message| message.contains(&format!("{canonical} was removed")))
            {
                return;
            }
            diagnostics.push(format!(
                "{}:{line}: unknown {key} setting ignored",
                path.display()
            ));
        });
    }
    diagnostics
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
    #[test]
    fn unknown_keys_have_source_locations_including_peer_arrays() {
        let diagnostics = super::key_diagnostics(
            "future = true\n[msg]\nfuture_setting = true\n[[peers]]\nname='nodea'\nfuture_peer=1\nsummary_command='false'\n",
            std::path::Path::new("fixture.toml"),
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
}
