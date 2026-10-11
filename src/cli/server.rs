#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::{EmptyParams, Method, Request, ServerLiveHandoffParams};

pub(super) fn run_server_command(args: &[String]) -> std::io::Result<Option<i32>> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        return Ok(None);
    };

    match subcommand {
        "stop" => server_stop(&args[1..]).map(Some),
        "live-handoff" => server_live_handoff(&args[1..]).map(Some),
        "--handoff-import" => Ok(None),
        "reload-config" => server_reload_config(&args[1..]).map(Some),
        "help" | "--help" | "-h" => {
            print_server_help();
            Ok(Some(0))
        }
        _ => {
            print_server_help();
            Ok(Some(2))
        }
    }
}

fn server_stop(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: flk server stop");
        return Ok(2);
    }

    super::send_ok_request(Method::ServerStop(EmptyParams::default()))
}

fn server_reload_config(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: flk server reload-config");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:server:reload-config".into(),
        method: Method::ServerReloadConfig(EmptyParams::default()),
    })?)
}

fn server_live_handoff(args: &[String]) -> std::io::Result<i32> {
    let Some(params) = parse_live_handoff_params(args) else {
        eprintln!(
            "usage: flk server live-handoff [--import-exe <path>] [--expected-protocol <n>] [--expected-version <version>] [--skip-undrained]"
        );
        return Ok(2);
    };

    let response = super::send_request(&Request {
        id: "cli:server:live-handoff".into(),
        method: Method::ServerLiveHandoff(params),
    })?;
    if response.get("error").is_some() {
        // #426: the lost-session refusal is the one failure whose message is
        // written to be read by a person, and it is the one where raw JSON is
        // actively unhelpful — the operator needs to be told that the obvious
        // next move cannot work and what does. Everything else keeps the
        // machine-readable shape.
        if let Some(message) = error_message_for_human(&response) {
            eprintln!("{message}");
            return Ok(1);
        }
        if let Some(guidance) = old_server_drain_guidance(&response) {
            eprintln!("{guidance}");
            return Ok(1);
        }
        let rendered = serde_json::to_string(&response).unwrap_or_else(|err| {
            format!(
                "{{\"error\":{{\"code\":\"render_failed\",\"message\":\"failed to render error response: {err}\"}}}}"
            )
        });
        eprintln!("{rendered}");
        return Ok(1);
    }

    for pane in skipped_undrained(&response) {
        eprintln!("{pane}");
    }
    eprintln!(
        "live handoff complete; server log: {}",
        crate::session::data_dir()
            .join("flock-server.log")
            .display()
    );
    Ok(0)
}

/// The prose message for the errors whose message was written for a person, or
/// `None` to keep the JSON rendering every other failure uses.
///
/// Keyed on the code rather than matched by text, so an unrelated failure can
/// never start being pretty-printed because its message happens to read like
/// one of these.
fn error_message_for_human(response: &serde_json::Value) -> Option<&str> {
    let error = response.get("error")?;
    match error.get("code")?.as_str()? {
        "session_broken" | "handoff_undrained" => error.get("message")?.as_str(),
        _ => None,
    }
}

/// The drain error as a server older than #1011 sends it: no pane named and no
/// `--skip-undrained` to honour, because the OLD server is the one that drains.
/// This tells the operator how to get off it without a restart that loses every
/// pane.
fn old_server_drain_guidance(response: &serde_json::Value) -> Option<String> {
    let error = response.get("error")?;
    if error.get("code")?.as_str()? != "handoff_failed" {
        return None;
    }
    let message = error.get("message")?.as_str()?;
    if !message.contains("timed out draining PTY writes before handoff") {
        return None;
    }
    let data_dir = crate::session::data_dir();
    Some(format!(
        "{message}\n\
         The running server predates per-pane drain reporting, so it cannot name the \
         stuck panes or honour --skip-undrained. To get off it without a restart:\n\
         \x20 1. Find the stuck panes: grep 'write returned zero bytes' {log} and \
         note each \"pane\":N (an internal number).\n\
         \x20 2. Map N to its public id: jq -r --arg n N '.workspaces[] | \
         select(.public_pane_numbers[$n]) | \"\\(.id):p\\(.public_pane_numbers[$n])\"' \
         {session} (pane numbers 1-9 read as-is; 10 is pA, 11 pB and so on).\n\
         \x20 3. Close those panes (in the TUI, or flk pane close <wX:pN>), then retry \
         flk server live-handoff. From this release on, a stuck pane is named in the \
         error and --skip-undrained hands off past it.",
        log = data_dir.join("flock-server.log").display(),
        session = data_dir.join("session.json").display(),
    ))
}

/// One line per pane whose pending input `--skip-undrained` dropped.
fn skipped_undrained(response: &serde_json::Value) -> Vec<String> {
    let Some(panes) = response
        .get("result")
        .and_then(|result| result.get("skipped_undrained"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    panes
        .iter()
        .map(|pane| {
            let id = pane
                .get("pane_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let bytes = pane
                .get("pending_bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            format!("skipped undrained pane {id}: dropped {bytes} pending bytes")
        })
        .collect()
}

fn parse_live_handoff_params(args: &[String]) -> Option<ServerLiveHandoffParams> {
    let mut params = ServerLiveHandoffParams::default();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--skip-undrained" {
            params.skip_undrained = true;
            idx += 1;
            continue;
        }
        let (flag, value) = if let Some((flag, value)) = arg.split_once('=') {
            (flag, Some(value.to_string()))
        } else {
            let value = args.get(idx + 1).cloned();
            idx += 1;
            (arg.as_str(), value)
        };
        let value = value?;
        match flag {
            "--import-exe" => params.import_exe = Some(value),
            "--expected-protocol" => {
                params.expected_protocol = Some(value.parse().ok()?);
            }
            "--expected-version" => params.expected_version = Some(value),
            _ => return None,
        }
        idx += 1;
    }
    Some(params)
}

fn print_server_help() {
    eprintln!("flk server commands:");
    eprintln!("  flk server                run as headless server");
    eprintln!("  flk server stop           stop the running server via the API socket");
    eprintln!("  flk server live-handoff   hand off live panes to a new local server");
    eprintln!("  flk server reload-config  reload config.toml in the running server");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_handoff_params_parse_remote_update_fields() {
        let args = vec![
            "--import-exe".to_string(),
            "/home/me/.local/bin/flk".to_string(),
            "--expected-protocol=9".to_string(),
            "--expected-version".to_string(),
            "0.6.2".to_string(),
        ];

        let params = parse_live_handoff_params(&args).expect("params");

        assert_eq!(
            params.import_exe.as_deref(),
            Some("/home/me/.local/bin/flk")
        );
        assert_eq!(params.expected_protocol, Some(9));
        assert_eq!(params.expected_version.as_deref(), Some("0.6.2"));
    }

    #[test]
    fn live_handoff_params_parse_skip_undrained_flag() {
        let args = vec![
            "--skip-undrained".to_string(),
            "--expected-version".to_string(),
            "1.2.0".to_string(),
        ];

        let params = parse_live_handoff_params(&args).expect("params");

        assert!(params.skip_undrained);
        assert_eq!(params.expected_version.as_deref(), Some("1.2.0"));
        assert!(
            !parse_live_handoff_params(&[])
                .expect("params")
                .skip_undrained
        );
    }

    #[test]
    fn undrained_refusal_prints_as_prose() {
        let response = serde_json::json!({
            "id": "cli:server:live-handoff",
            "error": {
                "code": "handoff_undrained",
                "message": "timed out draining PTY writes before handoff: w1:p2 (3 bytes pending)."
            }
        });

        assert_eq!(
            error_message_for_human(&response),
            Some("timed out draining PTY writes before handoff: w1:p2 (3 bytes pending).")
        );
        assert!(old_server_drain_guidance(&response).is_none());
    }

    #[test]
    fn old_server_drain_error_gets_recovery_guidance() {
        let response = serde_json::json!({
            "id": "cli:server:live-handoff",
            "error": {
                "code": "handoff_failed",
                "message": "timed out draining PTY writes before handoff"
            }
        });

        let guidance = old_server_drain_guidance(&response).expect("guidance");

        assert!(guidance.starts_with("timed out draining PTY writes before handoff\n"));
        assert!(guidance.contains("write returned zero bytes"));
        assert!(guidance.contains("public_pane_numbers"));
        assert!(guidance.contains("flk pane close"));
        let other = serde_json::json!({
            "error": {"code": "handoff_failed", "message": "handoff import refused: protocol"}
        });
        assert!(old_server_drain_guidance(&other).is_none());
    }

    #[test]
    fn skipped_undrained_lists_each_dropped_pane() {
        let response = serde_json::json!({
            "id": "cli:server:live-handoff",
            "result": {
                "type": "live_handoff",
                "skipped_undrained": [
                    {"pane_id": "w1:p2", "pending_bytes": 3},
                    {"pane_id": "w2:p6", "pid": 7, "pending_bytes": 17}
                ]
            }
        });

        assert_eq!(
            skipped_undrained(&response),
            vec![
                "skipped undrained pane w1:p2: dropped 3 pending bytes".to_string(),
                "skipped undrained pane w2:p6: dropped 17 pending bytes".to_string(),
            ]
        );
        let ok = serde_json::json!({"result": {"type": "ok"}});
        assert!(skipped_undrained(&ok).is_empty());
    }
}
