use std::{
    cell::Cell,
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

use super::*;

const SECRET: &str = "s3cret-app-token";
const ISSUED: &str = r#"{"id":"mint","result":{"type":"client_token_issued","issued":{"info":{"name":"app:1","principal":"op","scopes":["read","send","answer","operate"],"expires_in_ms":43200000},"token":"s3cret-app-token"}}}"#;

fn refused(code: &str) -> Failure {
    Failure::Refused {
        code: code.into(),
        message: format!("{code} detail"),
    }
}

/// Mints against scripted replies; returns the outcome and how many requests ran.
fn mint_scripted(
    mut replies: Vec<Result<Minted, Failure>>,
    start: Result<(), String>,
) -> (Result<Minted, String>, usize, bool) {
    replies.reverse();
    let calls = Cell::new(0);
    let started = Cell::new(false);
    let outcome = mint_with(
        || {
            calls.set(calls.get() + 1);
            replies.pop().expect("an unscripted request")
        },
        || {
            started.set(true);
            start
        },
    );
    (outcome, calls.get(), started.get())
}

fn token() -> Result<Minted, Failure> {
    parse_reply(ISSUED)
}

#[test]
fn operator_only_names_the_pane_launch_and_the_remote_path() {
    let (outcome, calls, _) = mint_scripted(vec![Err(refused("operator_only"))], Ok(()));
    let message = outcome.unwrap_err();
    assert!(
        message.contains("inside a flk pane") && message.contains("#2293"),
        "{message}"
    );
    assert_eq!(calls, 1);
}

#[test]
fn peer_process_changed_is_retried_once_on_a_fresh_connection() {
    let (outcome, calls, _) =
        mint_scripted(vec![Err(refused("peer_process_changed")), token()], Ok(()));
    assert_eq!(outcome.unwrap().token.secret(), SECRET);
    assert_eq!(calls, 2);
    let twice = vec![
        Err(refused("peer_process_changed")),
        Err(refused("peer_process_changed")),
    ];
    let (outcome, calls, _) = mint_scripted(twice, Ok(()));
    assert!(outcome.unwrap_err().contains("could not bind the token"));
    assert_eq!(calls, 2);
}

#[test]
fn session_busy_is_reported_without_a_retry() {
    let (outcome, calls, _) = mint_scripted(vec![Err(refused("session_busy"))], Ok(()));
    assert!(outcome
        .unwrap_err()
        .contains("too many app sessions or is busy"));
    assert_eq!(calls, 1);
}

#[test]
fn invalid_params_and_internal_error_carry_the_servers_detail() {
    let (outcome, _, _) = mint_scripted(vec![Err(refused("invalid_params"))], Ok(()));
    let message = outcome.unwrap_err();
    assert!(
        message.contains("invalid_params detail") && message.contains("different versions"),
        "{message}"
    );
    let (outcome, _, _) = mint_scripted(vec![Err(refused("internal_error"))], Ok(()));
    assert_eq!(
        outcome.unwrap_err(),
        "The flk server could not mint a token (internal_error: internal_error detail)."
    );
}

#[test]
fn no_server_starts_one_then_mints() {
    let (outcome, calls, started) = mint_scripted(vec![Err(Failure::NoServer), token()], Ok(()));
    assert_eq!(outcome.unwrap().token.secret(), SECRET);
    assert_eq!((calls, started), (2, true));
    let (outcome, calls, _) = mint_scripted(
        vec![Err(Failure::NoServer)],
        Err("flk: No such file or directory".into()),
    );
    assert_eq!(
        outcome.unwrap_err(),
        "No flk server is running, and starting one failed: flk: No such file or directory"
    );
    assert_eq!(calls, 1);
    let (outcome, _, _) =
        mint_scripted(vec![Err(Failure::NoServer), Err(Failure::NoServer)], Ok(()));
    assert!(outcome.unwrap_err().contains("did not answer"));
}

#[test]
fn a_running_server_is_never_started_again() {
    let (_, _, started) = mint_scripted(vec![Err(refused("operator_only"))], Ok(()));
    assert!(!started);
}

#[test]
fn ptrace_scope_zero_or_unknown_refuses_the_mint() {
    assert!(ptrace_guard(|| Ok("1\n".into())).is_ok());
    assert!(ptrace_guard(|| Ok("3\n".into())).is_ok());
    let zero = ptrace_guard(|| Ok("0\n".into())).unwrap_err();
    assert!(
        zero.contains("ptrace_scope is 0") && zero.contains("#2293"),
        "{zero}"
    );
    let missing = ptrace_guard(|| Err(io::Error::from(io::ErrorKind::NotFound))).unwrap_err();
    assert!(missing.contains("is unreadable"), "{missing}");
    assert!(ptrace_guard(|| Ok("x".into()))
        .unwrap_err()
        .contains("not a number"));
}

#[test]
fn the_token_never_shows_in_debug_or_errors() {
    let issued = token();
    assert_eq!(
        format!("{issued:?}"),
        "Ok(Minted { token: Token(<redacted>), expires_in_ms: Some(43200000) })"
    );
    let broken = ISSUED.replace("client_token_issued", "other");
    let truncated = &ISSUED[..ISSUED.len() - 3];
    for line in [broken.as_str(), truncated] {
        let failure = parse_reply(line).unwrap_err();
        let (outcome, _, _) = mint_scripted(vec![Err(failure)], Ok(()));
        let message = outcome.unwrap_err();
        assert!(!message.contains(SECRET), "{message}");
        assert!(
            message.starts_with("Talking to the flk server failed: "),
            "{message}"
        );
    }
}

#[test]
fn the_socket_path_follows_flk() {
    let path = |vars: &[(&str, &str)]| {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        socket_path(|key| vars.get(key).cloned())
    };
    let home = ("HOME", "/h");
    assert_eq!(
        path(&[home]).unwrap(),
        PathBuf::from("/h/.config/flock/flock.sock")
    );
    assert_eq!(
        path(&[home, ("XDG_CONFIG_HOME", "/x")]).unwrap(),
        PathBuf::from("/x/flock/flock.sock")
    );
    assert_eq!(
        path(&[home, ("FLOCK_SOCKET_PATH", "/s.sock")]).unwrap(),
        PathBuf::from("/s.sock")
    );
    assert_eq!(
        path(&[home, ("FLOCK_SESSION", "work")]).unwrap(),
        PathBuf::from("/h/.config/flock/sessions/work/flock.sock")
    );
    for ignored in ["default", "..", "a/b"] {
        assert_eq!(
            path(&[home, ("FLOCK_SESSION", ignored)]).unwrap(),
            PathBuf::from("/h/.config/flock/flock.sock")
        );
    }
    assert!(path(&[]).is_err());
}

#[cfg(unix)]
#[test]
fn one_request_line_on_a_fresh_connection() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    let dir = std::env::temp_dir().join(format!("flock-desktop-mint-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("flock.sock");
    let _ = std::fs::remove_file(&path);
    assert!(matches!(unix::request(&path), Err(Failure::NoServer)));
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        (&stream)
            .write_all(format!("{ISSUED}\n").as_bytes())
            .unwrap();
        line
    });
    assert_eq!(unix::request(&path).unwrap().token.secret(), SECRET);
    assert_eq!(
        server.join().unwrap(),
        "{\"id\":\"mint\",\"method\":\"client.app.mint\",\"params\":{}}\n"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_reply_carries_expires_in_ms_when_the_server_reports_it() {
    assert_eq!(token().unwrap().expires_in_ms, Some(43_200_000));
    let older = ISSUED.replace(r#","expires_in_ms":43200000"#, "");
    assert_eq!(parse_reply(&older).unwrap().expires_in_ms, None);
}

#[test]
fn the_ws_socket_sits_beside_the_api_socket() {
    let ws = |api: &str| ws_socket_path(Path::new(api));
    assert_eq!(
        ws("/h/.config/flock/flock.sock"),
        PathBuf::from("/h/.config/flock/flock-ws.sock")
    );
    assert_eq!(ws("/run/custom.sock"), PathBuf::from("/run/custom-ws.sock"));
}
