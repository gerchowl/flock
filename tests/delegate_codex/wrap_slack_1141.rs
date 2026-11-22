use super::*;

/// An empty first frame permits Enter, but a lost prefix must forbid its retry.
#[test]
fn packed_visible_codex_suffix_does_not_authorize_an_enter_retry() {
    packed_suffix("codex");
}

#[test]
fn packed_visible_opencode_suffix_does_not_authorize_an_enter_retry() {
    packed_suffix("opencode");
}

fn packed_suffix(kind: &str) {
    let server = start_server();
    let script = r#"import os, sys, tty
from pathlib import Path
tty.setraw(0)
base = Path(__file__).parent.parent
cols = os.get_terminal_size(0).columns
kind = sys.argv[1]
width = cols - (3 if kind == 'opencode' else 2)
first = 'a' * (width - 2) + ' b'
second = 'c' * (width - 2) + ' d'
(base / 'width').write_text(str(width))
log = base / 'wrap-bytes'
log.write_bytes(b'')
def draw(packed=False, working=False):
    chrome = '• Working (1s • esc to interrupt)\r\n' if working else ''
    body = first + '\r\n  ' + second if packed else ''
    if kind == 'opencode':
        chrome = '■■■■⬝⬝ esc interrupt opencode\r\n' if working else ''
        body = first + '\r\n┃  ' + second if packed else 'Ask anything…'
        frame = 'opencode\r\nFRAME1141\r\n' + chrome + '┃\r\n┃  ' + body + '\r\n┃\r\n┃  Build test-model\r\n╹\r\ntab agents ctrl+p commands\r\n'
    else:
        frame = 'OpenAI Codex\r\nFRAME1141\r\n' + chrome + '› ' + body + '\r\n  ? for shortcuts\r\n'
    sys.stdout.write('\x1b[2J\x1b[H' + frame)
    sys.stdout.flush()
sys.stdout.write('\x1b[?2004h')
draw()
pending = b''
paste = False
enters = 0
while True:
    byte = os.read(0, 1)
    if not byte: raise SystemExit('input closed')
    with log.open('ab') as out: out.write(byte)
    if byte == b'\x1b' or pending:
        pending += byte
        if pending == b'\x1b[200~': paste = True; pending = b''
        elif pending == b'\x1b[201~': paste = False; pending = b''
        continue
    if byte == b'\r' and not paste:
        enters += 1
        draw(packed=True, working=enters > 1)
"#;
    let python = server.base.join("raw").join(kind);
    fs::create_dir_all(python.parent().unwrap()).unwrap();
    fs::write(&python, script).unwrap();
    let binary = bin_dir(&server.base).join(kind);
    fs::write(
        &binary,
        format!("#!/bin/sh\nexec python3 '{}' {kind}\n", python.display()),
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let ws = operator_workspace(&server);
    let started = request(
        &server,
        &serde_json::json!({"id":"start","method":"agent.start","params":{
        "name":"wrap1141","workspace_id":ws,"argv":[kind],"focus":true}})
        .to_string(),
    );
    assert!(started.get("error").is_none(), "{started}");
    let pane = started["result"]["agent"]["pane_id"].as_str().unwrap();
    let painted = request(&server, &serde_json::json!({"id":"painted","method":"pane.wait_for_output","params":{
        "pane_id":pane,"source":"recent","match":{"type":"substring","value":"FRAME1141"},"timeout_ms":5000}}).to_string());
    assert!(painted.get("error").is_none(), "{painted}");
    let ready = cli(
        &server,
        &[
            "agent",
            "wait",
            pane,
            "--status",
            "settled",
            "--settle",
            SETTLE,
            "--timeout",
            "5000",
        ],
    );
    assert!(ready.status.success(), "{}", stderr(&ready));
    let width: usize = fs::read_to_string(server.base.join("width"))
        .unwrap()
        .parse()
        .unwrap();
    // Codex packs cols - 2; OpenCode packs cols - 3. Its extra border
    // column leaves the production cols - 2 bound conservative.
    let text = format!("X {} b {} d", "a".repeat(width - 2), "c".repeat(width - 2));
    let mut child = cli_spawn(&server, &["agent", "send", "--json", pane, &text]);
    let status = exited_within(&mut child, WITHIN);
    let sent = finish(child);
    assert!(status.is_some(), "guarded send exceeded its fail ceiling");
    let response = stdout_json(&sent);
    assert_eq!(sent.status.code(), Some(8), "{response} {}", stderr(&sent));
    assert_eq!(response["result"]["outcome"], "unconfirmed", "{response}");
    assert_eq!(response["result"]["reason"], "owned_composer_not_visible");
    assert_eq!(
        fs::read(server.base.join("wrap-bytes")).unwrap(),
        [b"\x1b[200~".as_slice(), text.as_bytes(), b"\x1b[201~\r"].concat(),
        "paste once, first Enter once, never retry onto the incomplete suffix"
    );
    for format in ["text", "ansi"] {
        let capture = cli(
            &server,
            &[
                "pane", "read", pane, "--source", "recent", "--format", format,
            ],
        );
        assert!(capture.status.success(), "{}", stderr(&capture));
        let screen = stdout(&capture);
        assert!(screen.contains("FRAME1141"));
        assert!(screen.contains(&format!("{} b", "a".repeat(width - 2))));
        assert!(screen.contains(&format!("{} d", "c".repeat(width - 2))));
    }
}
