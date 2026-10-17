//! Operator client management against an isolated real server and Unix socket.
#![allow(clippy::disallowed_methods)] // Fixture drives the compiled CLI and owned PTY.
mod support;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use support::environment::Command;

struct Server {
    owner: support::server_startup::ServerStartup,
    root: PathBuf,
    config: PathBuf,
    socket: PathBuf,
}
impl Server {
    fn start() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(format!("/tmp/hct-{}-{unique}", std::process::id()));
        let config = root.join("c");
        let runtime = root.join("r");
        let socket = root.join("a.sock");
        let directory = config.join(support::paths::app_dir_name());
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("config.toml"), "onboarding = false\n[security]\nagent_account = 'none'\n[client]\nbind = '127.0.0.1:7682'\n").unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_flk"));
        cmd.arg("server");
        for (key, _) in std::env::vars().filter(|(k, _)| k.starts_with("FLOCK_")) {
            cmd.env_remove(key);
        }
        for (key, value) in support::environment::isolated_env(&config, &runtime) {
            cmd.env(key, value);
        }
        cmd.env("FLOCK_LOG", "flk=debug,flock=debug");
        cmd.env("FLOCK_SOCKET_PATH", &socket);
        cmd.env("SHELL", "/bin/sh");
        support::environment::assert_pty_isolated(&cmd);
        let child = support::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
        drop(pair.slave);
        let mut owner = support::server_startup::ServerStartup::new(
            pair.master,
            child,
            &socket,
            Duration::from_secs(30),
        )
        .unwrap();
        owner.wait_responsive().unwrap();
        Self {
            owner,
            root,
            config,
            socket,
        }
    }
    fn cli_command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_flk"));
        cmd.args(args);
        for (key, _) in std::env::vars().filter(|(k, _)| k.starts_with("FLOCK_")) {
            cmd.env_remove(key);
        }
        for (key, value) in support::environment::isolated_env(&self.config, &self.root.join("r")) {
            cmd.env(key, value);
        }
        cmd.env("FLOCK_SOCKET_PATH", &self.socket);
        cmd
    }
    fn cli(&self, args: &[&str]) -> std::process::Output {
        self.cli_command(args).output().unwrap()
    }
    fn api(&self, method: &str, params: Value) -> Value {
        let mut socket = UnixStream::connect(&self.socket).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        writeln!(
            socket,
            "{}",
            json!({"id":"fixture","method":method,"params":params})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(socket).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    fn directory(&self) -> PathBuf {
        self.config
            .join(support::paths::app_dir_name())
            .join("client-tokens")
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.owner.stop();
        support::cleanup_test_base(&self.root);
    }
}

#[test]
fn client_token_cli_socket_roundtrip_private_files_and_fragment_url() {
    let server = Server::start();
    let added = server.cli(&[
        "client",
        "token",
        "add",
        "phone",
        "--scopes",
        "read,send",
        "--json",
    ]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let added: Value = serde_json::from_slice(&added.stdout).unwrap();
    let token = added["result"]["issued"]["token"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(
        fs::metadata(server.directory())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(server.directory().join("phone.token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let listed = server.cli(&["client", "token", "list", "--json"]);
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["result"]["entries"][0]["info"]["name"], "phone");
    assert!(!listed.to_string().contains(token));
    let open = server.cli(&["client", "open", "--print"]);
    assert!(
        open.status.success(),
        "{}",
        String::from_utf8_lossy(&open.stderr)
    );
    let url = String::from_utf8(open.stdout).unwrap();
    let code = url
        .trim()
        .strip_prefix("http://127.0.0.1:7682/#code=")
        .unwrap();
    assert_eq!(code.len(), 64);
    assert!(code
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert!(!url.contains('?'));
    assert!(!url.contains(token));
    let before = fs::read(server.directory().join("operator.token")).unwrap();
    let printed = server.cli(&["client", "open", "--print", "--json"]);
    assert!(
        printed.status.success(),
        "status: {}; {}",
        printed.status,
        String::from_utf8_lossy(&printed.stderr)
    );
    let printed = String::from_utf8(printed.stdout).unwrap();
    assert!(printed.starts_with("http://127.0.0.1:7682/#code="));
    assert_eq!(printed.lines().count(), 1);
    assert!(!printed.contains('{'));
    assert_eq!(
        fs::read(server.directory().join("operator.token")).unwrap(),
        before
    );
    let removed = server.api("client.token.rm", json!({"name":"phone"}));
    assert!(removed.get("result").is_some(), "{removed}");
    assert!(!server.directory().join("phone.token").exists());
    let list = server.api("client.token.list", json!({}));
    assert_eq!(list["result"]["entries"].as_array().unwrap().len(), 1);
    let logs = fs::read_dir(server.config.join(support::paths::app_dir_name()))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|s| s == "log"))
        .map(|e| fs::read_to_string(e.path()).unwrap())
        .collect::<String>();
    assert!(
        logs.contains("client.open"),
        "fixture must capture real request diagnostics"
    );
    assert!(!logs.contains(token));
    assert!(!logs.contains(code));
}

#[test]
fn token_add_requires_terminal_stdout_or_explicit_secret_output() {
    use std::io::{IsTerminal, Read};
    let server = Server::start();
    let args = ["client", "token", "add", "piped", "--scopes", "read"];
    let refused = server.cli(&args);
    assert_eq!(refused.status.code(), Some(2));
    assert!(refused.stdout.is_empty());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--show-token"));
    assert!(!server.directory().join("piped.token").exists());

    let disclosed = server.cli(&[
        "client",
        "token",
        "add",
        "explicit",
        "--scopes",
        "read",
        "--show-token",
    ]);
    assert!(
        disclosed.status.success(),
        "{}",
        String::from_utf8_lossy(&disclosed.stderr)
    );
    let explicit: Value = serde_json::from_slice(&disclosed.stdout).unwrap();
    let stored: Value =
        serde_json::from_slice(&fs::read(server.directory().join("explicit.token")).unwrap())
            .unwrap();
    assert_eq!(explicit["result"]["issued"]["token"], stored["token"]);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let terminal = fs::OpenOptions::new()
        .write(true)
        .open(pair.master.tty_name().unwrap())
        .unwrap();
    assert!(terminal.is_terminal());
    let mut command =
        server.cli_command(&["client", "token", "add", "terminal", "--scopes", "read"]);
    command.stdout(terminal);
    let output = command.output().unwrap();
    drop(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stages = support::readiness::Stages::new(Duration::from_secs(10));
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut bytes = Vec::new();
    while !bytes.contains(&b'\n') {
        stages
            .readable(&[pair.master.as_raw_fd().unwrap()])
            .unwrap();
        let mut buffer = [0; 1024];
        let count = reader.read(&mut buffer).unwrap();
        assert!(
            count > 0,
            "terminal output closed before the issuance response"
        );
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 4096, "unexpected oversized terminal output");
    }
    // Keep the slave open until its buffered output is consumed on both platforms.
    drop(pair.slave);
    let printed: Value = serde_json::from_slice(&bytes).unwrap();
    let stored: Value =
        serde_json::from_slice(&fs::read(server.directory().join("terminal.token")).unwrap())
            .unwrap();
    assert_eq!(printed["result"]["issued"]["token"], stored["token"]);
}

#[test]
fn client_management_inside_an_agent_pane_is_operator_only_exit_thirteen() {
    let server = Server::start();
    let created = server.api("workspace.create", json!({"label":"credential-fixture"}));
    let pane = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let command = format!("FLOCK_SOCKET_PATH='{}' '{}' client token add forbidden --scopes operate --json; printf 'CLIENT_EXIT=%s\\n' \"$?\"", server.socket.display(), env!("CARGO_BIN_EXE_flk"));
    let sent = server.api(
        "pane.send_input",
        json!({"pane_id":pane,"text":command,"keys":["Enter"]}),
    );
    assert!(sent.get("result").is_some(), "{sent}");
    let waited = server.api(
        "pane.wait_for_output",
        json!({"pane_id":pane,"source":"recent","match":{"type":"substring","value":"CLIENT_EXIT=13"},"timeout_ms":8000}),
    );
    assert!(waited.get("result").is_some(), "{waited}");
    let read = server.api(
        "pane.read",
        json!({"pane_id":pane,"source":"recent","lines":100,"strip_ansi":true}),
    );
    let text = read["result"]["read"]["text"].as_str().unwrap();
    assert!(text.contains("operator_only"), "{text}");
    assert!(text.contains("CLIENT_EXIT=13"), "{text}");
    assert!(!server.directory().join("forbidden.token").exists());
}
