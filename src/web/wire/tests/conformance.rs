//! Server selection follows authored coverage, including multi-label headers.
use super::*;
use crate::structured_wire::fixture::{ClientAction, FixtureCoverage, FixtureLine, FixtureStep};

fn fixture_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/test/fixtures/wire")
}
fn selected() -> Vec<(String, Vec<FixtureLine>)> {
    selected_from(&fixture_root())
}
fn selected_from(root: &std::path::Path) -> Vec<(String, Vec<FixtureLine>)> {
    let mut scripts = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        let lines = FixtureLine::parse_script(&fs::read_to_string(&path).unwrap()).unwrap();
        let FixtureStep::Given { covers, .. } = &lines[0].step else {
            panic!("{name}: missing coverage header")
        };
        assert!(!covers.is_empty(), "{name}: uncovered fixture");
        assert!(
            covers
                .iter()
                .enumerate()
                .all(|(i, label)| !covers[..i].contains(label)),
            "{name}: duplicate coverage"
        );
        // A new coverage variant must choose its server-selection policy here.
        if covers.iter().any(|label| match label {
            FixtureCoverage::S7Min => true,
            FixtureCoverage::S7 | FixtureCoverage::ClientOnly => false,
        }) {
            assert!(
                !covers.contains(&FixtureCoverage::ClientOnly),
                "{name}: conflicting coverage"
            );
            scenario(&name, &lines);
            scripts.push((name, lines));
        }
        // Other inventory entries explicitly cover full S7 or client-only behavior.
    }
    scripts.sort_by(|a, b| a.0.cmp(&b.0));
    scripts
}
#[derive(Debug, PartialEq)]
enum Scenario {
    ClientOpen,
    Scoped,
    Refusal,
}
fn scenario(name: &str, lines: &[FixtureLine]) -> Scenario {
    let command = lines.iter().any(|line| {
        matches!(
            &line.step,
            FixtureStep::Expect {
                frame: ClientFrame::Cmd { .. },
                ..
            } | FixtureStep::Client {
                action: ClientAction::Command { .. }
            }
        ) || matches!(&line.step, FixtureStep::Given { outbox, .. } if !outbox.is_empty())
    });
    let welcome = lines.iter().any(|line| {
        matches!(
            &line.step,
            FixtureStep::Send {
                frame: ServerFrame::Welcome { .. },
                ..
            }
        )
    });
    if command {
        assert!(welcome, "{name}: command fixture lacks welcome coverage");
        Scenario::ClientOpen
    } else if welcome {
        Scenario::Scoped
    } else {
        assert!(
            lines
                .iter()
                .any(|line| matches!(line.step, FixtureStep::TokenRefused { code: 4401 })),
            "{name}: uncovered minimal fixture"
        );
        Scenario::Refusal
    }
}

/// Configure only the child process: unrelated tests may read HOME/XDG without a lock.
/// The child runs exactly one test, so App's path readers and background threads see
/// the same sandbox for their entire lifetime, without any process-wide mutation.
fn sandbox_command(root: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FLOCK_") {
            command.env_remove(key);
        }
    }
    for key in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        command.env(key, root);
    }
    command.kill_on_drop(true);
    command
}
async fn client_open_server() -> (Server, Bearer) {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    // Only the isolated child calls this helper; its environment is immutable.
    let data = crate::session::data_dir();
    fs::create_dir_all(&data).unwrap();
    let server = Server::from_store(root, TokenStore::open(&data).unwrap(), |clock| {
        Arc::new(clock)
    })
    .await;
    let mut config = crate::config::Config::default();
    config.client.bind = Some(server.bind);
    let mut app = crate::app::App::new(
        &config,
        true,
        None,
        mpsc::unbounded_channel().1,
        crate::api::EventHub::default(),
    );
    // Kernel-attested operator, not a forged authority bit or fixture credential.
    let (peer, _client) = std::os::unix::net::UnixStream::pair().unwrap();
    let peer = crate::platform::peer::credentials(&peer);
    app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: crate::platform::peer::effective_uid(),
        dedicated: Ok(None).into(),
    };
    app.current_api_peer_pid = peer.pid;
    app.current_api_peer_birth = Some(peer.birth);
    app.current_api_peer_uid = Some(peer.uid);
    let response: Value =
        serde_json::from_str(&app.handle_api_request(crate::api::schema::Request {
            id: "conformance-client-open".into(),
            method: crate::api::schema::Method::ClientOpen(crate::api::schema::EmptyParams {}),
        }))
        .unwrap();
    let url = response["result"]["url"]
        .as_str()
        .expect("ClientOpen issues a browser handoff");
    let code = url.split_once("#code=").expect("ClientOpen handoff code").1;
    let bearer = app
        .client_codes
        .exchange(code, Instant::now())
        .expect("production exchange redeems ClientOpen once");
    assert!(app.client_codes.exchange(code, Instant::now()).is_none());
    (server, bearer)
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
    assert!(scripts
        .iter()
        .any(|(_, lines)| scenario("inventory", lines) == Scenario::Refusal));
}

#[test]
fn new_command_fixtures_are_covered_without_a_filename_allowlist() {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    fs::copy(
        fixture_root().join("agent-send.jsonl"),
        root.path().join("new-command.jsonl"),
    )
    .unwrap();
    let scripts = selected_from(root.path());
    assert_eq!(scripts.len(), 1);
    assert_eq!(scenario(&scripts[0].0, &scripts[0].1), Scenario::ClientOpen);
}
#[test]
fn uncovered_inventory_entries_are_rejected() {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let source = fs::read_to_string(fixture_root().join("clean.jsonl")).unwrap();
    fs::write(
        root.path().join("uncovered.jsonl"),
        source.replace("[\"s7-min\"]", "[]"),
    )
    .unwrap();
    assert!(std::panic::catch_unwind(|| selected_from(root.path())).is_err());
    fs::write(root.path().join("uncovered.jsonl"), "{\"v\":1,\"step\":\"given\",\"cursors\":{},\"outbox\":[],\"covers\":[\"s7-min\"]}\n{\"v\":1,\"step\":\"quiet\",\"ceiling_ms\":1}\n").unwrap();
    assert!(std::panic::catch_unwind(|| selected_from(root.path())).is_err());
}
#[test]
fn sandbox_command_configures_child_environment() {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let sandbox = sandbox_command(root.path());
    let mut expected: std::collections::BTreeMap<_, _> = [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ]
    .into_iter()
    .map(|key| {
        (
            std::ffi::OsString::from(key),
            Some(root.path().as_os_str().to_owned()),
        )
    })
    .collect();
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FLOCK_") {
            expected.insert(key, None);
        }
    }
    let actual = sandbox
        .as_std()
        .get_envs()
        .map(|(key, value)| (key.to_owned(), value.map(std::ffi::OsStr::to_owned)))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(actual, expected, "child paths and inherited FLOCK removals");
}

#[test]
fn selected_welcome_capabilities_match_the_actual_endpoint() {
    if std::env::var_os("FLOCK_WIRE_CONFORMANCE_CHILD").is_none() {
        let root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let mut child = sandbox_command(root.path());
        let test = concat!(
            module_path!(),
            "::selected_welcome_capabilities_match_the_actual_endpoint"
        );
        child.args(["--exact", test.split_once("::").unwrap().1, "--nocapture"]);
        child.env("FLOCK_WIRE_CONFORMANCE_CHILD", "1");
        runtime(async {
            let output = within(child.output()).await.unwrap();
            assert!(
                output.status.success(),
                "isolated conformance failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "child must execute the conformance test"
            );
        });
        return;
    }
    runtime(async {
        for (name, lines) in selected() {
            let kind = scenario(&name, &lines);
            let hello = lines
                .iter()
                .find_map(|line| match &line.step {
                    FixtureStep::Expect {
                        frame: frame @ ClientFrame::Hello { .. },
                        ..
                    } => Some(serde_json::to_value(frame).unwrap()),
                    _ => None,
                })
                .expect("selected fixture has a hello");
            if kind == Scenario::Refusal {
                let server = Server::new().await;
                let mut client = Socket::open(&server).await;
                let mut refused = hello;
                refused["token"] = json!("0".repeat(64));
                client.send(refused).await;
                assert!(
                    matches!(client.receive().await, Received::Close(4401)),
                    "{name}: actual auth refusal"
                );
                server.finish().await;
                continue;
            }
            let expected = lines
                .iter()
                .find_map(|line| match &line.step {
                    FixtureStep::Send {
                        frame: frame @ ServerFrame::Welcome { .. },
                        ..
                    } => Some(serde_json::to_value(frame).unwrap()),
                    _ => None,
                })
                .expect("scenario classification requires welcome");
            let (server, bearer) = if kind == Scenario::ClientOpen {
                client_open_server().await
            } else {
                (Server::new().await, Bearer("a".repeat(64)))
            };
            let scopes = if kind == Scenario::ClientOpen {
                let grants =
                    serde_json::to_value(crate::api::client_tokens::operator_scopes()).unwrap();
                assert_eq!(
                    expected["capabilities"], grants,
                    "{name}: ClientOpen grants"
                );
                grants
            } else {
                let path = server._root.path().join("client-tokens/one.token");
                let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                token["scopes"] = expected["capabilities"].clone();
                fs::write(path, serde_json::to_vec(&token).unwrap()).unwrap();
                expected["capabilities"].clone()
            };
            let mut client = Socket::open(&server).await;
            let actual = client
                .hello(hello["protocol"].as_u64().unwrap() as u32, &bearer.0)
                .await;
            server.finish().await;
            assert_eq!(
                actual["capabilities"], scopes,
                "{name}: granted welcome capabilities"
            );
            for field in ["protocol", "idempotency_window_ms"] {
                assert_eq!(actual[field], expected[field], "{name}: welcome.{field}");
            }
        }
    });
}

#[test]
fn replay_credential_without_answer_gets_typed_scope_refusal() {
    runtime(async {
        let mut server = Server::new().await;
        let path = server._root.path().join("client-tokens/one.token");
        let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        token["scopes"] = json!(["read", "send"]);
        fs::write(path, token.to_string()).unwrap();
        let (_, rx) = mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut socket = Socket::open(&server).await;
        assert_eq!(
            socket.hello(1, &"a".repeat(64)).await["capabilities"],
            json!(["read", "send"])
        );
        // Scope admission precedes target lookup; no agent or permission book is needed.
        socket.send(json!({"t":"cmd","cmd_id":"missing-answer","verb":"agent.decide","args":{},"resume_only":false})).await;
        let wake = server.wake.clone();
        let actual = within(async {
            loop {
                let notified = wake.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                Arc::get_mut(&mut server.core)
                    .unwrap()
                    .service_commands(&mut app);
                tokio::select! { biased;
                    frame = socket.text() => break frame,
                    _ = &mut notified => (),
                }
            }
        })
        .await;
        server.finish().await;
        assert_eq!(actual["cmd_id"], "missing-answer");
        assert_eq!(actual["ok"], false);
        assert_eq!(actual["error"]["code"], "scope");
        assert_eq!(actual["error"]["retry"], "never");
    });
}
