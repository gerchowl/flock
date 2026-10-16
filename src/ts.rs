//! Generated socket API and Claude adapter contracts (ADR-0036 §6).
//!
//! This module is test-only: normal binaries do not contain an export command.
//! `just gen-ts` explicitly rewrites the files; the drift test only reads them.

use std::any::TypeId;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use ts_rs::{Config, TS};

/// Render the selected roots and their transitive dependencies once each.
#[derive(Default)]
pub(crate) struct Collector {
    seen: HashSet<TypeId>,
    files: BTreeMap<PathBuf, String>,
}

impl Collector {
    /// Include a type and recursively visit the dependencies ts-rs reports.
    pub(crate) fn add<T: TS + 'static + ?Sized>(&mut self) {
        if !self.seen.insert(TypeId::of::<T>()) {
            return;
        }
        if let Some(path) = T::output_path() {
            let rendered = T::export_to_string(&Config::default()).expect("render TS contract");
            let rendered = rendered
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            if let Some(previous) = self.files.insert(path.clone(), rendered.clone()) {
                assert_eq!(previous, rendered, "TS export path collision: {path:?}");
            }
        }
        T::visit_dependencies(self);
    }
}

impl ts_rs::TypeVisitor for Collector {
    fn visit<T: TS + 'static + ?Sized>(&mut self) {
        self.add::<T>();
    }
}

fn contracts() -> BTreeMap<PathBuf, String> {
    use crate::api::{question::*, schema::*};
    let mut collector = Collector::default();
    collector.add::<Question>();
    collector.add::<QuestionForm>();
    collector.add::<QuestionCloseReason>();
    collector.add::<AgentInfo>();
    collector.add::<FleetAgentInfo>();
    collector.add::<WorkspaceInfo>();
    collector.add::<PaneInfo>();
    crate::structured::claude::ts_events(&mut collector);
    collector.files
}

fn generated_paths(root: &std::path::Path) -> Vec<PathBuf> {
    fn walk(root: &std::path::Path, paths: &mut Vec<PathBuf>) {
        if !root.exists() {
            return;
        }
        for entry in std::fs::read_dir(root).expect("read generated directory") {
            let path = entry.expect("read generated entry").path();
            if path.is_dir() {
                walk(&path, paths);
            } else if path.extension().is_some_and(|ext| ext == "ts") {
                paths.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    walk(root, &mut paths);
    paths.sort();
    paths
}

#[test]
fn wire_ts_types_are_current() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/client/src/gen");
    let expected = contracts();
    assert_eq!(
        generated_paths(&root),
        expected
            .keys()
            .map(|path| root.join(path))
            .collect::<Vec<_>>(),
        "TS file inventory drift: run `just gen-ts`"
    );
    for (path, rendered) in expected {
        assert_eq!(
            std::fs::read_to_string(root.join(&path)).ok().as_deref(),
            Some(rendered.as_str()),
            "TS drift in {}: run `just gen-ts`",
            path.display()
        );
    }
}

#[test]
#[ignore = "explicit generation through just gen-ts"]
fn generate_wire_ts_types() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/client/src/gen");
    let expected = contracts();
    for path in generated_paths(&root) {
        if !expected.contains_key(path.strip_prefix(&root).expect("generated path")) {
            std::fs::remove_file(path).expect("remove stale generated type");
        }
    }
    for (path, rendered) in expected {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().expect("export parent"))
            .expect("create TS directory");
        std::fs::write(target, rendered).expect("write TS contract");
    }
}

#[test]
fn json_numbers_and_explicit_nulls_have_wire_types() {
    let files = contracts();
    for (path, rendered) in &files {
        assert!(!rendered.contains("bigint"), "{}", path.display());
    }
    let assistant = &files[&PathBuf::from("Assistant.ts")];
    assert!(
        assistant.contains("session_id?: string | null"),
        "{assistant}"
    );
    assert!(
        assistant.contains("thinking_duration_ms?: number | null"),
        "{assistant}"
    );
    let tokens = &files[&PathBuf::from("ThinkingTokens.ts")];
    assert!(tokens.contains("estimated_tokens: number"), "{tokens}");
    assert!(
        tokens.contains("estimated_tokens_delta: number"),
        "{tokens}"
    );
}
