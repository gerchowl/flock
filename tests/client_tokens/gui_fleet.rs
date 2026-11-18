//! Scripted model exercised through the real structured runtime, without a browser.
use super::*;

struct Cleanup<'a>(&'a Server);

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = support::command::output(
            self.0.cli_command(&["server", "stop"]),
            None,
            support::command::CLI_LIMIT,
        );
        let _ = self.0.owner.wait_exited(support::command::CLI_LIMIT);
    }
}

fn cli(server: &Server, args: &[&str]) -> Value {
    let mut arguments = vec!["--json"];
    arguments.extend_from_slice(args);
    let output = support::command::output(
        server.cli_command(&arguments),
        None,
        support::command::CLI_LIMIT,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn next(socket: &mut WebSocket, stage: &str) -> Value {
    let deadline = std::time::Instant::now() + support::command::CLI_LIMIT;
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "wire producer stayed silent during {stage}"
        );
        let (opcode, bytes) = socket.receive();
        assert_eq!(opcode, 1, "wire closed during {stage}: {bytes:?}");
        let frame: Value = serde_json::from_slice(&bytes).unwrap();
        if frame["t"] == "ping" {
            send(socket, json!({"t":"pong", "nonce":frame["nonce"]}));
        } else {
            assert_ne!(frame["t"], "subfail", "{frame}");
            return frame;
        }
    }
}

fn send(socket: &mut WebSocket, frame: Value) {
    socket.send(1, frame.to_string().as_bytes());
}

#[test]
fn scripted_model_stream_permission_and_resume_through_real_runtime() {
    let server =
        Server::with_setup("#!/bin/sh\nprintf '{}\\n'\n", None, |root, directory| {
            let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .map(|directory| directory.join("python3"))
                .find(|path| path.is_file())
                .expect("python3 fixture interpreter");
            let fixture = fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/gui-fleet/claude.py"
            ))
            .expect("GUI fleet fake model");
            let executable = root.join("tools/claude");
            fs::write(&executable, format!("#!{}\n{fixture}", python.display())).unwrap();
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
            let home = root.join("c/home");
            fs::create_dir_all(&home).unwrap();
            fs::write(home.join("gui-fleet.json"), serde_json::to_vec(&json!([
            {"expect":"message", "text":"fixture message"},
            {"emit":"stream", "chunks":["first partial", " then second"]},
            {"expect":"gate", "name":"partial-observed"},
            {"emit":"permission", "request_id":"gui-permission", "input":{"command":"true"}},
            {"expect":"decision", "behavior":"allow"},
            {"emit":"reply", "text":"permission accepted"},
            {"expect":"message", "text":"continue fixture"},
            {"emit":"reply", "text":"continued after restart"}
        ])).unwrap()).unwrap();
            let token = root.join("token");
            fs::write(&token, "fixture-token-not-a-credential").unwrap();
            fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
            let config = directory.join("config.toml");
            let mut contents = fs::read_to_string(&config).unwrap();
            contents.push_str(&format!(
                "\n[structured]\nrun_as = 'operator'\nprofile = 'fixture'\ntoken_file = {}\n",
                json!(token)
            ));
            fs::write(config, contents).unwrap();
        })
        .unwrap();
    let _cleanup = Cleanup(&server);
    let created = cli(
        &server,
        &[
            "agent",
            "start",
            "gui-fixture",
            "--runtime",
            "structured",
            "--cwd",
            server.root.to_str().unwrap(),
            "--",
            "claude",
        ],
    );
    let agent = created["result"]["agent"]["agent_id"].as_str().unwrap();
    let session = created["result"]["agent"]["session_id"].clone();
    // The shipped native view consumes live partials; the wire replays durable events.
    let mut native = UnixStream::connect(server.socket.with_file_name("a-client.sock")).unwrap();
    let (_, error) =
        support::client_handshake(&mut native, support::PROTOCOL_VERSION, 120, 50).unwrap();
    assert!(error.is_none(), "{error:?}");
    cli(&server, &["agent", "focus", agent]);
    screen_contains(&mut native, "Send at idle");
    cli(&server, &["agent", "send", agent, "fixture message"]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    screen_contains(&mut native, "first partial then second");
    let issued = cli(
        &server,
        &[
            "client",
            "token",
            "add",
            "fixture-wire",
            "--scopes",
            "read,send,answer,operate",
            "--show-token",
        ],
    );
    let mut socket = WebSocket::open(&endpoint(&server));
    let (opcode, welcome) = socket.hello(issued["result"]["issued"]["token"].as_str().unwrap());
    assert_eq!(opcode, 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&welcome).unwrap()["t"],
        "welcome"
    );
    send(
        &mut socket,
        json!({"t":"sub", "sub_id":"history", "stream":format!("agent/{agent}")}),
    );
    let mut gate = UnixStream::connect(
        server
            .root
            .join("c/home")
            .join(format!("g-{}", session.as_str().unwrap())),
    )
    .unwrap();
    gate.set_write_timeout(Some(support::command::CLI_LIMIT))
        .unwrap();
    writeln!(gate, "\"partial-observed\"").unwrap();
    drop(gate);
    let request = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "permission producer stayed silent"
        );
        assert!(!server.owner.has_exited().unwrap(), "server exited");
        let frame = next(&mut socket, "permission request");
        let event = &frame["ev"];
        if event["type"] == "flk_permission_request" && event["state"] == "open" {
            break event.clone();
        }
    };
    send(
        &mut socket,
        json!({"t":"cmd", "cmd_id":"decision", "verb":"agent.decide", "resume_only":false,
        "args":{"agent":agent,"request_id":request["request_id"],"request_seq":request["request_seq"],"decision":"allow_once"}}),
    );
    let mut acknowledged = false;
    let mut completed = false;
    while !acknowledged || !completed {
        assert!(
            std::time::Instant::now() < deadline,
            "decision producer stayed silent"
        );
        let frame = next(&mut socket, "permission decision");
        if frame["t"] == "ack" {
            assert_eq!(frame["ok"], true, "{frame}");
            acknowledged = true;
        }
        if frame["ev"]["type"] == "result" {
            assert_eq!(frame["ev"]["result"], "permission accepted");
            completed = true;
        }
    }
    drop(socket);
    cli(&server, &["agent", "stop", agent]);
    loop {
        let status = cli(&server, &["agent", "status", agent]);
        if status["result"]["agent"]["lifecycle"] == "offline" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stop did not finish: {status}"
        );
    }
    let restarted = cli(
        &server,
        &[
            "agent",
            "start",
            "gui-fixture",
            "--runtime",
            "structured",
            "--",
            "claude",
        ],
    );
    assert_eq!(restarted["result"]["agent"]["session_id"], session);
    assert_eq!(restarted["result"]["agent"]["agent_id"], agent);
    loop {
        let status = cli(&server, &["agent", "status", agent]);
        if status["result"]["agent"]["lifecycle"] == "live" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "resumed worker did not become live: {status}"
        );
        assert!(
            !server.owner.has_exited().unwrap(),
            "server exited during resume"
        );
    }
    let expected =
        json!([{"session_id":session,"resume":false},{"session_id":session,"resume":true}]);
    loop {
        let launches: Value = serde_json::from_slice(
            &fs::read(server.root.join("c/home/gui-fleet-launches.json")).unwrap(),
        )
        .unwrap();
        if launches == expected {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "resumed model did not launch: {launches}"
        );
        assert!(
            !server.owner.has_exited().unwrap(),
            "server exited during model resume"
        );
    }
    let diagnostic = server.root.join("c/home/gui-fleet-error.json");
    assert!(
        !diagnostic.exists(),
        "resumed model failed: {}",
        fs::read_to_string(&diagnostic).unwrap_or_default()
    );
    assert_eq!(
        cli(&server, &["agent", "status", agent])["result"]["agent"]["session_id"],
        session
    );
    cli(&server, &["agent", "send", agent, "continue fixture"]);
    screen_contains(&mut native, "continued after restart");
    cli(&server, &["agent", "stop", agent]);
}

// Wire tuples mirror FrameData and CellData, as in the other native client probes.
type Cell = (String, u32, u32, u16, bool, Option<u32>);
type Cursor = (u16, u16, bool, u8);
type Frame = (Vec<Cell>, u16, u16, Option<Cursor>, Vec<String>, Vec<u8>);

fn screen_contains(native: &mut UnixStream, wanted: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut last = String::new();
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "native producer never rendered {wanted}: {last}"
        );
        let (variant, bytes) = support::read_framed_message(native)
            .expect("native producer must render before the ceiling");
        if variant != 1 {
            continue;
        }
        let (frame, consumed): (Frame, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(consumed, bytes.len());
        let text = frame
            .0
            .chunks(usize::from(frame.1))
            .map(|row| row.iter().map(|cell| cell.0.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains(wanted) {
            return;
        }
        last = text;
    }
}
