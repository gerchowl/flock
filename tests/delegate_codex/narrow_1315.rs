use super::*;

#[test]
fn codex_narrow_1315_at_15_columns() {
    eprintln!("#1315 {}", exercise_narrow_1315(false));
}

#[test]
fn codex_narrow_1315_with_clipped_model_status() {
    eprintln!("#1315 {}", exercise_narrow_1315(true));
}

fn exercise_narrow_1315(model_status: bool) -> serde_json::Value {
    let cols: u16 = 15;
    let server = start_server_with_rows(40);
    operator_workspace(&server);
    let hint: String = "Ask Codex to do anything"
        .chars()
        .take(usize::from(cols - 3))
        .collect();
    let program = format!(
        r#"import os, sys, select, signal
base = {base:?}
hint = {hint:?}
def paint(stage):
    size = os.get_terminal_size(0)
    with open(base + '/size', 'w') as file:
        file.write(str(size.lines) + ' ' + str(size.columns))
    if stage == 'working':
        return
    if stage == 'initial':
        body = '\x1b[2mAsk Codex to do anything\x1b[0m'
    elif stage == 'hint':
        body = '\x1b[2m' + hint + '\x1b[0m'
    elif stage == 'absent':
        body = ''
    elif stage == 'draft':
        body = hint
    elif stage == 'dimdraft':
        body = '\x1b[2mreal draft\x1b[0m'
    elif stage == 'shortcutdraft':
        body = '\n  ? for shortcu'
    elif stage == 'modeldraft':
        body = '\n  GPT-6.1-Sol …'
    elif stage == 'mixed':
        body = '\x1b[2m' + hint + '\x1b[0m draft'
    else:
        raise RuntimeError(stage)
    footer = '? for shortcuts'[:max(0, size.columns - 2)]
    if stage == 'absent':
        footer = '? for '
    status = '\n  GPT-6.1-Sol …' if {model_status} and stage != 'initial' else ''
    if stage == 'shortcutdraft':
        status = ''
        footer = ''
    sys.stdout.write('\x1b[3J\x1b[2J\x1b[HOpenAI Codex\nF' + stage[:2] + str(size.columns) + '\n› ' + body + status + '\n  ' + footer + '\n')
    sys.stdout.flush()
resize_read, resize_write = os.pipe()
signal.signal(signal.SIGWINCH, lambda *_: os.write(resize_write, b'r'))
stage = 'initial'
paint(stage)
line = bytearray()
with open(base + '/bytes', 'ab', buffering=0) as received:
    while True:
        ready, _, _ = select.select([0, resize_read], [], [])
        if resize_read in ready:
            os.read(resize_read, 4096)
            paint(stage)
        if 0 not in ready:
            continue
        byte = os.read(0, 1)
        if not byte:
            break
        received.write(byte)
        if byte != b'\n':
            line.extend(byte)
            continue
        text = line.decode('utf-8')
        line.clear()
        if text in ('hint', 'draft', 'dimdraft', 'mixed', 'modeldraft', 'shortcutdraft', 'absent'):
            stage = text
            paint(stage)
        elif text.startswith('Read '):
            stage = 'working'
            with open(base + '/accepted', 'a') as accepted:
                accepted.write(text + '\n')
            sys.stdout.write('\x1b[3J\x1b[2J\x1b[HOpenAI Codex\nFRAME-working\n• Working (1s • esc to interrupt)\n› \n')
            sys.stdout.flush()
        else:
            raise RuntimeError('unexpected input: ' + repr(text))
"#,
        base = server.base.to_string_lossy(),
        hint = hint,
        model_status = if model_status { "True" } else { "False" },
    );
    let program_path = server.base.join("fake-codex.py");
    fs::write(&program_path, program).unwrap();
    fs::write(
        bin_dir(&server.base).join("codex"),
        format!(
            "#!/bin/sh\nstty -echo -icanon\npython3 '{}'\n",
            program_path.display()
        ),
    )
    .unwrap();
    let first = brief(&server, "first.md", "first\n");
    let mut child = start_codex(
        &server,
        "narrow1315",
        &first,
        &["--json", "--ready-timeout", "5000"],
    );
    let pane = delegate_pane(&server, "narrow1315");
    report_session(&server, &pane);
    let exit = exited_within(&mut child, WITHIN).expect("initial submit completion");
    let out = finish(child);
    assert_eq!(exit.code(), Some(0), "{} {}", stdout(&out), stderr(&out));
    let agent = agent_get(&server, "narrow1315").unwrap();
    let pane = agent["pane_id"].as_str().unwrap();
    let client_path = server.socket.with_file_name("flock-client.sock");
    support::wait_for_socket(&client_path, Duration::from_secs(5));
    let mut client = UnixStream::connect(client_path).unwrap();
    let (_, error) =
        support::client_handshake(&mut client, support::PROTOCOL_VERSION, cols + 2, 40).unwrap();
    assert!(error.is_none(), "{error:?}");
    let focused = request(&server, &serde_json::json!({"id":"focus","method":"workspace.focus","params":{"workspace_id":agent["workspace_id"]}}).to_string());
    assert!(focused.get("error").is_none(), "{focused}");
    client
        .write_all(&support::frame_message(
            &[
                support::encode_varint_u32(3),
                support::encode_varint_u16(cols + 2),
                support::encode_varint_u16(40),
                support::encode_varint_u32(8),
                support::encode_varint_u32(16),
            ]
            .concat(),
        ))
        .unwrap();
    client.flush().unwrap();
    paint_narrow_1315(&server, pane, "hint");
    let size = fs::read_to_string(server.base.join("size")).unwrap();
    assert_eq!(size, format!("31 {cols}"));
    let mut captures = serde_json::Map::new();
    for format in ["text", "ansi"] {
        let out = cli(
            &server,
            &[
                "pane", "read", pane, "--source", "recent", "--format", format,
            ],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        captures.insert(format.into(), stdout(&out).into());
    }
    let sent = send_narrow_1315(&server, "second.md");
    assert!(
        sent.status.success(),
        "cols={cols}: {} {}",
        stdout(&sent),
        stderr(&sent)
    );
    let accepted = read_lines(&server.base.join("accepted"));
    assert_eq!(accepted.len(), 2, "both briefs must reach Enter");
    assert_eq!(accepted[0], expected_line(&first));
    assert_eq!(
        accepted[1],
        expected_line(server.base.join("briefs/second.md").to_str().unwrap())
    );
    for stage in ["draft", "dimdraft", "mixed", "modeldraft", "shortcutdraft"] {
        paint_narrow_1315(&server, pane, stage);
        let before = fs::read(server.base.join("bytes")).unwrap();
        let refused = send_narrow_1315(&server, "refused.md");
        assert_eq!(
            refused.status.code(),
            Some(10),
            "{stage}: {} {}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains(if stage == "shortcutdraft" {
                "unknown_composer"
            } else {
                "input_not_empty"
            }),
            "{}",
            stderr(&refused)
        );
        assert_eq!(
            fs::read(server.base.join("bytes")).unwrap(),
            before,
            "{stage}: refused send wrote bytes"
        );
        assert_eq!(read_lines(&server.base.join("accepted")).len(), 2);
    }
    paint_narrow_1315(&server, pane, "absent");
    let before = fs::read(server.base.join("bytes")).unwrap();
    let refused = cli(
        &server,
        &[
            "--json",
            "agent",
            "send",
            "narrow1315",
            "--submit",
            "blocked",
        ],
    );
    assert_eq!(
        refused.status.code(),
        Some(10),
        "{} {}",
        stdout(&refused),
        stderr(&refused)
    );
    let error: serde_json::Value = serde_json::from_str(&stdout(&refused)).unwrap();
    assert_eq!(error["error"]["code"], "unknown_composer", "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("15-column, 31-row"),
        "{error}"
    );
    assert_eq!(error["error"]["data"]["columns"], 15, "{error}");
    assert_eq!(error["error"]["data"]["rows"], 31, "{error}");
    assert!(error["error"]["data"]["bottom_rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row.as_str().unwrap().contains("? for")));
    assert!(error.get("result").is_none(), "{error}");
    assert_eq!(fs::read(server.base.join("bytes")).unwrap(), before);
    serde_json::json!({"size":size,"captures":captures,"accepted":2,"draft_refusals":5,"refusal_bytes":0,"unknown_error":error})
}

fn paint_narrow_1315(server: &Server, pane: &str, stage: &str) {
    let sent = request(server, &serde_json::json!({"id":"control","method":"pane.send_text","params":{"pane_id":pane,"text":format!("{stage}\n"),"raw":true}}).to_string());
    assert!(sent.get("error").is_none(), "{sent}");
    let painted = request(server, &serde_json::json!({"id":"painted","method":"pane.wait_for_output","params":{"pane_id":pane,"source":"recent","match":{"type":"substring","value":format!("F{}15", &stage[..2])},"timeout_ms":5000}}).to_string());
    assert!(painted.get("error").is_none(), "{painted}");
    let settled = cli(
        server,
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
    assert!(
        settled.status.success(),
        "{} {}",
        stdout(&settled),
        stderr(&settled)
    );
}
fn send_narrow_1315(server: &Server, filename: &str) -> Output {
    let b = brief(server, filename, "continue\n");
    let mut child = cli_spawn(
        server,
        &[
            "delegate",
            "send",
            "narrow1315",
            "--brief",
            &b,
            "--settle",
            SETTLE,
            "--ready-timeout",
            "5000",
            "--json",
        ],
    );
    exited_within(&mut child, WITHIN).expect("delegate send completion");
    finish(child)
}
