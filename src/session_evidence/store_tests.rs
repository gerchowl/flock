use super::*;
use serde_json::json;
use std::os::unix::fs::{symlink, PermissionsExt};

#[derive(Clone, Copy, Debug)]
enum Case {
    ConflictingMeta,
    SymlinkedDuplicate,
    SelfSymlink,
    HardLink,
    BrokenSymlink,
    UnreadableDir,
    TwoStores,
    PrefixId,
    UppercaseIdentity,
    ReusedStaleId,
    Malformed,
    LateConflict,
}

#[test]
fn adversarial_store_validation_table() {
    let mut failures = Vec::new();
    for agent in ["codex", "claude"] {
        for case in [
            Case::ConflictingMeta,
            Case::SymlinkedDuplicate,
            Case::SelfSymlink,
            Case::HardLink,
            Case::BrokenSymlink,
            Case::UnreadableDir,
            Case::TwoStores,
            Case::PrefixId,
            Case::UppercaseIdentity,
            Case::ReusedStaleId,
            Case::Malformed,
            Case::LateConflict,
        ] {
            let root = crate::test_support::TempDir::socket();
            let home = root.path().join("home");
            let profile = root.path().join("profile");
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_STATE_HOME", root.path().join("state"));
            std::env::set_var("CODEX_HOME", home.join(".codex"));
            let session = AgentSessionInfo {
                source: format!("flock:{agent}"),
                agent: agent.into(),
                kind: crate::agent_resume::AgentSessionRefKind::Id,
                value: "store-session".into(),
            };
            let base = if agent == "codex" {
                home.join(".codex/sessions")
            } else {
                home.join(".claude/projects")
            };
            let name = if agent == "codex" {
                "rollout-fixture-store-session.jsonl"
            } else {
                "store-session.jsonl"
            };
            let first = base.join("one").join(name);
            std::fs::create_dir_all(first.parent().unwrap()).unwrap();
            let row = |id: &str| {
                if agent == "codex" {
                    json!({"type":"session_meta","payload":{"id":id}})
                } else {
                    json!({"type":"user","sessionId":id})
                }
            };
            let body = format!("{}\n", row("store-session"));
            std::fs::write(&first, &body).unwrap();
            let second = base.join("two").join(name);
            let blocked = base.join("blocked");
            let mut expected = "refused";
            match case {
                Case::ConflictingMeta => {
                    std::fs::write(&first, format!("{body}{}\n", row("other-session"))).unwrap()
                }
                Case::SymlinkedDuplicate => {
                    let external = root.path().join("external.jsonl");
                    std::fs::write(&external, &body).unwrap();
                    std::fs::create_dir_all(second.parent().unwrap()).unwrap();
                    symlink(external, &second).unwrap();
                }
                Case::SelfSymlink | Case::HardLink => {
                    std::fs::create_dir_all(second.parent().unwrap()).unwrap();
                    if matches!(case, Case::HardLink) {
                        std::fs::hard_link(&first, &second).unwrap();
                    } else {
                        symlink(&first, &second).unwrap();
                    }
                    expected = "found";
                }
                Case::BrokenSymlink => {
                    std::fs::create_dir_all(second.parent().unwrap()).unwrap();
                    symlink(root.path().join("absent.jsonl"), &second).unwrap();
                }
                Case::UnreadableDir => {
                    std::fs::create_dir_all(&blocked).unwrap();
                    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000))
                        .unwrap();
                }
                Case::TwoStores => {
                    if agent == "claude" {
                        let project = profile.join("projects/fixture");
                        std::fs::create_dir_all(&project).unwrap();
                        std::fs::write(project.join(name), &body).unwrap();
                        crate::agent_resume::record_claude_config_dir(
                            "store-session",
                            profile.to_str().unwrap(),
                        );
                    } else {
                        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
                        std::fs::write(&second, &body).unwrap();
                    }
                }
                Case::PrefixId | Case::UppercaseIdentity => {
                    let id = if matches!(case, Case::PrefixId) {
                        "store-session-extra"
                    } else {
                        "STORE-SESSION"
                    };
                    std::fs::write(&first, format!("{}\n", row(id))).unwrap();
                    expected = "absent";
                }
                Case::ReusedStaleId => {
                    std::fs::create_dir_all(second.parent().unwrap()).unwrap();
                    std::fs::write(&second, &body).unwrap();
                    std::fs::File::open(&second)
                        .unwrap()
                        .set_modified(std::time::UNIX_EPOCH)
                        .unwrap();
                }
                Case::Malformed => std::fs::write(&first, format!("{body}invalid JSON\n")).unwrap(),
                Case::LateConflict => {
                    std::fs::write(
                        &first,
                        format!(
                            "{body}{}\n{}\n",
                            json!({"padding":"x".repeat(70000)}),
                            row("other-session")
                        ),
                    )
                    .unwrap();
                }
            }
            let result = checked_path(&session);
            if matches!(case, Case::UnreadableDir) {
                std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let actual = match &result {
                Ok(Some(_)) => "found",
                Ok(None) => "absent",
                Err(_) => "refused",
            };
            let pass = actual == expected;
            if !pass {
                failures.push(format!("{agent} {case:?}: {actual}: {result:?}"));
            }
        }
    }
    std::env::remove_var("CODEX_HOME");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
