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
    collector.add::<crate::api::structured_agent::StructuredAgentInfo>();
    collector.add::<FleetAgentInfo>();
    collector.add::<WorkspaceInfo>();
    collector.add::<PaneInfo>();
    crate::structured::claude::ts_events(&mut collector);
    collector.add::<crate::structured_wire::ClientFrame>();
    collector.add::<crate::structured_wire::ServerFrame>();
    collector.add::<crate::structured_wire::fixture::FixtureLine>();
    collector.add::<crate::structured_wire::FleetAgentRow>();
    collector.add::<crate::structured_wire::AgentDecide>();
    collector.add::<crate::structured_wire::PermissionDecision>();
    collector.add::<crate::structured_wire::FleetDelta>();
    collector.add::<crate::structured_wire::AgentDecideResult>();
    collector.add::<crate::structured_wire::AgentSendResult>();
    collector.add::<crate::structured_wire::AgentDecideErrorCode>();
    let mut files = collector.files;
    // Classification comes from the adapter, so new fixtures cannot silently be skipped.
    let mut fixtures = String::from(
        "// Generated from Rust-classified Claude fixtures by just gen-ts. Do not edit.\n\
         import type { KnownSessionEvent } from './KnownSessionEvent';\n",
    );
    for line in include_str!("../tests/fixtures/claude-session/events.jsonl").lines() {
        if !matches!(
            crate::structured::claude::SessionEvent::parse(line, "fixture"),
            crate::structured::claude::SessionEvent::Unknown { .. }
        ) {
            fixtures.push_str(&format!("({line}) satisfies KnownSessionEvent;\n"));
        }
    }
    files.insert(PathBuf::from("known-session-fixtures.ts"), fixtures);
    // Compile literal fixture inputs under the same strict TS config as contracts.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/client/test/fixtures/wire");
    let mut scripts: Vec<_> = std::fs::read_dir(root)
        .expect("fixture directory")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    scripts.sort();
    let mut permission_lifecycle = String::new();
    let mut examples = String::from(
        r#"// Generated from wire JSONL scripts by just gen-ts.
import type { FixtureLine } from "./FixtureLine";
import type { PermissionRecord } from "./PermissionRecord";
import type { AgentDecideResult } from "./AgentDecideResult";
export const fixtureExamples = {
"#,
    );
    for path in scripts {
        let name = path.file_stem().expect("fixture name").to_string_lossy();
        examples.push_str(&format!(
            "{}: [
",
            serde_json::to_string(&name).expect("fixture key")
        ));
        for line in std::fs::read_to_string(&path)
            .expect("fixture script")
            .lines()
        {
            let value: serde_json::Value = serde_json::from_str(line).expect("fixture JSON");
            if name == "permission-decision" {
                let frame = &value["frame"];
                let typed = if frame["t"] == "ev" {
                    let record: crate::structured_wire::PermissionRecord =
                        serde_json::from_value(frame["ev"].clone())
                            .expect("typed permission fixture");
                    Some((
                        serde_json::to_value(record).expect("permission JSON"),
                        "PermissionRecord",
                    ))
                } else if frame["t"] == "ack" {
                    let ack: crate::structured_wire::AgentDecideResult =
                        serde_json::from_value(frame["result"].clone())
                            .expect("typed decision ack");
                    Some((
                        serde_json::to_value(ack).expect("ack JSON"),
                        "AgentDecideResult",
                    ))
                } else {
                    None
                };
                if let Some((payload, ty)) = typed {
                    permission_lifecycle.push_str(&format!("({payload}) satisfies {ty},\n"));
                }
            }
            examples.push_str(&serde_json::to_string(&value).expect("fixture literal"));
            examples.push_str(
                ",
",
            );
        }
        examples.push_str(
            "],
",
        );
    }
    examples.push_str(
        "} satisfies Record<string, FixtureLine[]>;
",
    );
    examples.push_str("export const permissionLifecycle = [\n");
    examples.push_str(&permission_lifecycle);
    examples.push_str("] as const;\n");
    files.insert(PathBuf::from("FixtureExamples.ts"), examples);
    files
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
