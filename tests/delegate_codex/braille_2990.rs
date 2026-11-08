use super::*;

#[test]
fn codex_braille_2990_delegate_start_send_and_draft_refusal() {
    let server = start_server_with_rows(40);
    operator_workspace(&server);
    for (name, contents) in [
        (
            "start.txt",
            include_str!("../fixtures/codex-braille-2990/start.txt"),
        ),
        (
            "send.txt",
            include_str!("../fixtures/codex-braille-2990/send.txt"),
        ),
        (
            "quoted-hint-draft.txt",
            include_str!("../fixtures/codex-braille-2990/quoted-hint-draft.txt"),
        ),
        (
            "multiline-braille-draft.txt",
            include_str!("../fixtures/codex-braille-2990/multiline-braille-draft.txt"),
        ),
        (
            "hint-tail-draft.txt",
            include_str!("../fixtures/codex-braille-2990/hint-tail-draft.txt"),
        ),
        (
            "harness.py",
            include_str!("../fixtures/codex-braille-2990/harness.py"),
        ),
    ] {
        fs::write(server.base.join(name), contents).unwrap();
    }
    fs::write(
        bin_dir(&server.base).join("codex"),
        format!(
            "#!/bin/sh\nstty -echo -icanon || exit 43\npython3 '{}' '{}'\n",
            server.base.join("harness.py").display(),
            server.base.display()
        ),
    )
    .unwrap();
    let first = brief(&server, "first.md", "first\n");
    let mut child = start_codex(
        &server,
        "braille2990",
        &first,
        &["--json", "--ready-timeout", "5000"],
    );
    let pane = delegate_pane(&server, "braille2990");
    report_session(&server, &pane);
    let exit =
        exited_within(&mut child, WITHIN).expect("start must finish within the fixture bound");
    let out = finish(child);
    assert_eq!(exit.code(), Some(0), "{} {}", stdout(&out), stderr(&out));
    assert_eq!(
        read_lines(&server.base.join("accepted")),
        [expected_line(&first)]
    );
    paint_braille_2990(&server, &pane, "send");
    for format in ["text", "ansi"] {
        let out = cli(
            &server,
            &[
                "pane", "read", &pane, "--source", "recent", "--format", format,
            ],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(stdout(&out).contains("Ask Codex to do anything"));
    }
    let status = cli(&server, &["delegate", "status", "braille2990", "--json"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let status = stdout_json(&status);
    assert!(
        matches!(status["agent_status"].as_str(), Some("idle" | "done")),
        "{status}"
    );
    let sent = send_braille_2990(&server, "second.md");
    assert!(sent.status.success(), "{} {}", stdout(&sent), stderr(&sent));
    assert_eq!(
        read_lines(&server.base.join("accepted")),
        [
            expected_line(&first),
            expected_line(server.base.join("briefs/second.md").to_str().unwrap()),
        ]
    );
    for stage in [
        "draft",
        "braille",
        "quoted-hint-draft",
        "multiline-braille-draft",
        "hint-tail-draft",
    ] {
        paint_braille_2990(&server, &pane, stage);
        let before = fs::read(server.base.join("bytes")).unwrap();
        let refused = send_braille_2990(&server, "refused.md");
        assert_eq!(
            refused.status.code(),
            Some(10),
            "{stage}: {} {}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("input_not_empty"),
            "{}",
            stderr(&refused)
        );
        assert_eq!(fs::read(server.base.join("bytes")).unwrap(), before);
        assert_eq!(read_lines(&server.base.join("accepted")).len(), 2);
    }
}

fn paint_braille_2990(server: &Server, pane: &str, stage: &str) {
    let sent = request(server, &serde_json::json!({"id":"control", "method":"pane.send_text", "params":{"pane_id":pane,"text":format!("{stage}\n"),"raw":true}}).to_string());
    assert!(sent.get("error").is_none(), "{sent}");
    let painted = request(server, &serde_json::json!({"id":"painted", "method":"pane.wait_for_output", "params":{"pane_id":pane,"source":"recent","match":{"type":"substring","value":format!("FRAME-{stage}")},"timeout_ms":5000}}).to_string());
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

fn send_braille_2990(server: &Server, filename: &str) -> Output {
    let b = brief(server, filename, "continue\n");
    let mut child = cli_spawn(
        server,
        &[
            "delegate",
            "send",
            "braille2990",
            "--brief",
            &b,
            "--settle",
            SETTLE,
            "--ready-timeout",
            "5000",
            "--json",
        ],
    );
    exited_within(&mut child, WITHIN).expect("send must finish within the fixture bound");
    finish(child)
}
