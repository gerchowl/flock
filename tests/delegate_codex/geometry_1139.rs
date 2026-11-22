use super::*;

#[test]
fn background_spawn_attached_80() {
    eprintln!("#1608 {}", geometry_probe_1139(80, false));
}
#[test]
fn background_spawn_detached_80() {
    eprintln!("#1608 {}", geometry_probe_1139(80, true));
}
#[test]
fn background_spawn_attached_15() {
    eprintln!("#1608 {}", geometry_probe_1139(15, false));
}
#[test]
fn background_spawn_detached_15() {
    eprintln!("#1608 {}", geometry_probe_1139(15, true));
}

fn geometry_probe_1139(cols: u16, detach: bool) -> serde_json::Value {
    let server = start_server_with_rows(40);
    let client_path = server.socket.with_file_name("flock-client.sock");
    support::wait_for_socket(&client_path, Duration::from_secs(5));
    let mut client = UnixStream::connect(client_path).unwrap();
    let (version, error) =
        support::client_handshake(&mut client, support::PROTOCOL_VERSION, cols, 40).unwrap();
    assert_eq!(version, support::PROTOCOL_VERSION);
    assert!(error.is_none(), "{error:?}");
    let ws = operator_workspace(&server);
    let panes = request(
        &server,
        &serde_json::json!({"id":"panes","method":"pane.list","params":{"workspace_id":ws}})
            .to_string(),
    );
    let root = panes["result"]["panes"][0]["pane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = server.base.join("operator-size");
    let observer = server.base.join("geometry-observer.py");
    fs::write(
        &observer,
        include_str!("../fixtures/geometry-1608-observer.py"),
    )
    .unwrap();
    let full_columns = if cols == 15 { 13 } else { 52 };
    let command = format!(
        "python3 '{}' '{}' {full_columns} || printf '\\106\\101\\111\\114\\061\\066\\060\\070\\012'",
        observer.display(), path.display()
    );
    let sent = request(&server, &serde_json::json!({"id":"observer","method":"pane.send_text","params":{"pane_id":root,"text":command,"raw":true}}).to_string());
    assert!(sent.get("error").is_none(), "{sent}");
    geometry_enter_1608(&server, &root);
    geometry_output_1608(&server, &root, "ARM1608");
    for (stage, marker) in [("first", "SP1"), ("second", "SZA")] {
        let split = request(&server, &serde_json::json!({"id":"split","method":"pane.split","params":{"workspace_id":ws,"target_pane_id":root,"direction":"right","focus":false}}).to_string());
        assert!(split.get("error").is_none(), "{split}");
        let measured = request(&server, &serde_json::json!({"id":"observe-split","method":"pane.send_text","params":{"pane_id":root,"text":stage,"raw":true}}).to_string());
        assert!(measured.get("error").is_none(), "{measured}");
        geometry_enter_1608(&server, &root);
        geometry_output_1608(&server, &root, marker);
    }
    let operator_size = fs::read_to_string(&path).unwrap();
    if detach {
        support::send_detach(&mut client).unwrap();
        assert!(support::wait_for_disconnect(&mut client, Duration::from_secs(5)).unwrap());
    }
    let script = "#!/bin/sh\nstty -echo\nstty size > \"$1\"\nprintf 'BOOT1139\\n'\ni=0\nwhile IFS= read -r stage; do\n[ \"$stage\" = size ] || exit 42\ni=$((i+1))\nstty size > \"$1\"\nprintf 'SIZE%d\\n' \"$i\"\ndone\n";
    fs::write(bin_dir(&server.base).join("codex"), script).unwrap();
    let size_path = server.base.join("new-size");
    let started = request(&server, &serde_json::json!({"id":"start","method":"agent.start","params":{"name":"geometry1139","cwd":work_dir(&server),"argv":["codex",size_path.to_str().unwrap()],"focus":false}}).to_string());
    assert!(started.get("error").is_none(), "{started}");
    let pane = started["result"]["agent"]["pane_id"].as_str().unwrap();
    probe_output_1139(&server, pane, "BOOT1139");
    let birth = fs::read_to_string(&size_path).unwrap();
    let sent = request(&server, &serde_json::json!({"id":"measure","method":"pane.send_text","params":{"pane_id":pane,"text":"size\n","raw":true}}).to_string());
    assert!(sent.get("error").is_none(), "{sent}");
    probe_output_1139(&server, pane, "SIZE1");
    let current = fs::read_to_string(size_path).unwrap();
    let actual_cols: u16 = birth.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert_eq!(
        actual_cols,
        if cols == 15 && !detach { 13 } else { 52 },
        "client={cols} detach={detach}: {birth}"
    );
    assert_eq!(
        current, birth,
        "background PTY must retain its allocated geometry"
    );
    let split_cols: u16 = operator_size
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        split_cols < actual_cols,
        "new background pane must not inherit the active split width: client={cols}, detach={detach}, operator={operator_size:?}, birth={birth:?}, current={current:?}"
    );
    serde_json::json!({"client_columns":cols,"detached":detach,"operator_size":operator_size.trim(),"background_birth":birth.trim(),"background_current":current.trim(),"native_events":fs::read_to_string(path.with_file_name("operator-size.events")).unwrap()})
}

fn geometry_enter_1608(server: &Server, pane: &str) {
    let sent = request(server, &serde_json::json!({"id":"geometry-enter","method":"pane.send_keys","params":{"pane_id":pane,"keys":["Enter"]}}).to_string());
    assert!(sent.get("error").is_none(), "{sent}");
}

fn geometry_output_1608(server: &Server, pane: &str, marker: &str) {
    let pattern = |text: &str| {
        text.chars()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join("\\s*")
    };
    let output = request(server, &serde_json::json!({"id":"geometry-stage","method":"pane.wait_for_output","params":{"pane_id":pane,"source":"recent","match":{"type":"regex","value":format!("{}|{}", pattern(marker), pattern("FAIL1608"))},"timeout_ms":5000}}).to_string());
    assert!(output.get("error").is_none(), "{output}; pane={}", request(server, &serde_json::json!({"id":"geometry-diagnostic","method":"pane.read","params":{"pane_id":pane,"source":"recent","format":"text"}}).to_string()));
    let observed: String = output["result"]["read"]["text"]
        .as_str()
        .unwrap()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        !observed.contains("FAIL1608") && observed.contains(marker),
        "observer stage {marker}: {output}"
    );
}

fn probe_output_1139(server: &Server, pane: &str, needle: &str) {
    let output = request(server, &serde_json::json!({"id":"output","method":"pane.wait_for_output","params":{"pane_id":pane,"source":"recent","match":{"type":"regex","value":needle.chars().map(|c| c.to_string()).collect::<Vec<_>>().join("\\s*")},"timeout_ms":5000}}).to_string());
    assert!(output.get("error").is_none(), "{output}");
}

#[test]
fn detached_delegate_starts_sends_twice_and_resumes_at_current_geometry() {
    let server = start_server_with_rows(40);
    let client_path = server.socket.with_file_name("flock-client.sock");
    support::wait_for_socket(&client_path, Duration::from_secs(5));
    let mut client = UnixStream::connect(client_path).unwrap();
    let (_, error) =
        support::client_handshake(&mut client, support::PROTOCOL_VERSION, 15, 40).unwrap();
    assert!(error.is_none(), "{error:?}");
    operator_workspace(&server);
    support::send_detach(&mut client).unwrap();
    assert!(support::wait_for_disconnect(&mut client, Duration::from_secs(5)).unwrap());
    let script = format!(
        "#!/bin/sh\n\
         stty -echo\n\
         stty size >> '{base}/sizes'\n\
         n=$(wc -l < '{base}/sizes' | tr -d ' ')\n\
         idle() {{ printf '\\033[3J\\033[2J\\033[HOpenAI Codex\\nLAUNCH-%s\\n› \\033[2mAsk Codex to do anything\\033[0m\\n  ? for shortcuts\\n' \"$n\"; }}\n\
         idle\n\
         while IFS= read -r line; do\n\
           case \"$line\" in\n\
             finish1139) idle ;;\n\
             *) printf '%s\\n' \"$line\" >> '{base}/accepted'; printf '\\033[3J\\033[2J\\033[HOpenAI Codex\\n• Working (1s • esc to interrupt)\\n› \\n' ;;\n\
           esac\n\
         done\n", base=server.base.display());
    fs::write(bin_dir(&server.base).join("codex"), script).unwrap();
    let b = brief(&server, "task.md", "do it\n");
    let mut expected_briefs = vec![expected_line(&b)];
    let mut child = start_codex(
        &server,
        "geometry1139",
        &b,
        &["--json", "--ready-timeout", "5000"],
    );
    let exit = exited_within(&mut child, WITHIN).expect("delegate start completion");
    let out = finish(child);
    assert_eq!(exit.code(), Some(0), "{} {}", stdout(&out), stderr(&out));
    let agent = agent_get(&server, "geometry1139").unwrap();
    let pane = agent["pane_id"].as_str().unwrap();
    report_session(&server, pane);
    for round in 2..=3 {
        let reset = request(&server, &serde_json::json!({"id":"finish","method":"pane.send_text","params":{"pane_id":pane,"text":"finish1139\n","raw":true}}).to_string());
        assert!(reset.get("error").is_none(), "{reset}");
        probe_output_1139(&server, pane, "LAUNCH-1");
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
        assert!(
            ready.status.success(),
            "{} {}",
            stdout(&ready),
            stderr(&ready)
        );
        for format in ["text", "ansi"] {
            let captured = cli(
                &server,
                &[
                    "pane", "read", pane, "--source", "recent", "--format", format,
                ],
            );
            assert!(captured.status.success(), "{}", stderr(&captured));
            eprintln!("#1139 delegate idle {format}: {}", stdout(&captured));
        }
        let b = brief(&server, &format!("round-{round}.md"), "continue\n");
        expected_briefs.push(expected_line(&b));
        let mut sender = cli_spawn(
            &server,
            &[
                "delegate",
                "send",
                "geometry1139",
                "--brief",
                &b,
                "--settle",
                SETTLE,
                "--ready-timeout",
                "5000",
                "--json",
            ],
        );
        let exit = exited_within(&mut sender, WITHIN).expect("delegate send completion");
        let sent = finish(sender);
        assert_eq!(exit.code(), Some(0), "{} {}", stdout(&sent), stderr(&sent));
        eprintln!("#1139 send round {round}: {}", stdout(&sent));
    }
    let accepted = read_lines(&server.base.join("accepted"));
    assert_eq!(
        accepted.len(),
        expected_briefs.len(),
        "each brief must reach Enter"
    );
    for (actual, expected) in accepted.iter().zip(&expected_briefs) {
        assert!(
            actual.contains(expected),
            "expected {expected:?}, got {actual:?}"
        );
    }
    let hibernated = request(
        &server,
        &serde_json::json!({"id":"hibernate","method":"agent.hibernate","params":{"target":pane}})
            .to_string(),
    );
    assert!(hibernated.get("error").is_none(), "{hibernated}");
    let resumed = request(
        &server,
        &serde_json::json!({"id":"resume","method":"agent.resume","params":{"target":pane}})
            .to_string(),
    );
    assert!(resumed.get("error").is_none(), "{resumed}");
    probe_output_1139(&server, pane, "LAUNCH-2");
    let sizes = read_lines(&server.base.join("sizes"));
    assert_eq!(sizes.len(), 2, "{sizes:?}");
    for size in &sizes {
        assert_eq!(size.split_whitespace().nth(1), Some("52"), "{sizes:?}");
    }
    eprintln!("#1139 real delegate start + two sends + resume: sizes={sizes:?}, accepted=3");
}

#[test]
fn cold_detached_spawn_gets_its_first_runtime_resize() {
    let server = start_server();
    let size_path = server.base.join("cold-size");
    fs::write(bin_dir(&server.base).join("codex"), "#!/bin/sh\nstty -echo\nprintf 'BOOT1139\\n'\nwhile IFS= read -r stage; do\n[ \"$stage\" = size ] || exit 42\nstty size > \"$1\"\nprintf 'SIZE1\\n'\ndone\n").unwrap();
    let started = request(&server, &serde_json::json!({"id":"cold-start","method":"agent.start","params":{"name":"cold1139","cwd":work_dir(&server),"argv":["codex",size_path.to_str().unwrap()],"focus":false}}).to_string());
    assert!(started.get("error").is_none(), "{started}");
    let pane = started["result"]["agent"]["pane_id"].as_str().unwrap();
    probe_output_1139(&server, pane, "BOOT1139");
    let sent = request(&server, &serde_json::json!({"id":"size","method":"pane.send_text","params":{"pane_id":pane,"text":"size\n","raw":true}}).to_string());
    assert!(sent.get("error").is_none(), "{sent}");
    probe_output_1139(&server, pane, "SIZE1");
    assert_eq!(
        fs::read_to_string(size_path).unwrap().trim(),
        "20 52",
        "first virtual render must establish the real inner rectangle"
    );
}
