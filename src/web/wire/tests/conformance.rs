//! Server selection follows authored coverage, including multi-label headers.
use super::*;
use crate::structured_wire::fixture::{FixtureCoverage, FixtureLine, FixtureStep};

fn selected() -> Vec<(String, Vec<FixtureLine>)> {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/test/fixtures/wire");
    let mut scripts = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let lines = FixtureLine::parse_script(&fs::read_to_string(&path).unwrap()).unwrap();
        if matches!(&lines[0].step, FixtureStep::Given { covers, .. } if covers.contains(&FixtureCoverage::S7Min))
        {
            scripts.push((
                path.file_stem().unwrap().to_str().unwrap().to_owned(),
                lines,
            ));
        }
    }
    scripts.sort_by(|a, b| a.0.cmp(&b.0));
    scripts
}

#[test]
fn server_conformance_selection_contains_no_client_only_fixture() {
    let scripts = selected();
    assert!(!scripts.is_empty());
    assert!(scripts.iter().all(|(name, lines)| {
        name != "fleet-lite"
            && matches!(&lines[0].step, FixtureStep::Given { covers, .. } if !covers.contains(&FixtureCoverage::ClientOnly))
    }));
    for required in ["heartbeat-missed", "unknown-subscription"] {
        assert!(
            scripts.iter().any(|(name, _)| name == required),
            "multi-label header {required}"
        );
    }
}

#[test]
fn selected_welcome_capabilities_match_the_actual_endpoint() {
    runtime(async {
        for (name, lines) in selected() {
            let Some(expected) = lines.iter().find_map(|line| match &line.step {
                FixtureStep::Send {
                    frame: frame @ ServerFrame::Welcome { .. },
                    ..
                } => Some(serde_json::to_value(frame).unwrap()),
                _ => None,
            }) else {
                continue;
            };
            let server = Server::new().await;
            let mut client = Socket::open(&server).await;
            let actual = client.hello(1, &"a".repeat(64)).await;
            server.finish().await;
            // Opaque server identities differ; operation capabilities are semantic.
            for field in ["protocol", "capabilities", "idempotency_window_ms"] {
                assert_eq!(actual[field], expected[field], "{name}: welcome.{field}");
            }
        }
    });
}
