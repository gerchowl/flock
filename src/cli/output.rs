//! Output selection and the error envelope (ADR-0029 §3, §4, #1024).
//!
//! `--format json|text` is global, and `--json` is its synonym: [`take_global`]
//! lifts them out of argv before any verb parses it, so every verb accepts
//! them in any position before a `--`. A verb that has a text form reads
//! [`json`] where it used to read its own `--json`; a verb that only prints
//! JSON keeps doing so under `--format text`. `pane read` and `agent read`
//! own a `--format text|ansi` for the screen they return, so after those verbs
//! only `--format json` is the global flag.
//!
//! Under machine output (`--format json` or `--json`) an error prints on
//! STDOUT as the envelope ADR-0029 §4 defines, `{"id", "error": {"code",
//! "message", "retry", "exit", "data"}}`, with a one-line human message on
//! stderr, so a pipe into a JSON parser always has a document to read. Without
//! machine output nothing changes (owner decision 3 on ADR-0029): the error
//! goes to stderr as the one line it always was.
//!
//! Known limit: only errors reported through [`error`] and [`failure`] gain
//! the envelope. Usage lines and a verb's own text failures still print on
//! stderr alone; #1150 tracks moving them.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output: the error envelope and its human line are the product surface"
)]

use std::cell::Cell;
use std::fmt::Display;

use serde_json::{json, Value};

use super::exit;
use crate::api::error_code::ErrorCode;

/// The output a caller asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    /// No flag given: each verb prints what it always printed.
    Default,
    Json,
    Text,
}

thread_local! {
    static FORMAT: Cell<Format> = const { Cell::new(Format::Default) };
    static ENVELOPE_PRINTED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the caller asked for JSON (`--format json` or `--json`).
pub(crate) fn json() -> bool {
    FORMAT.get() == Format::Json
}

/// Set the invocation's format. Called once per invocation, from `maybe_run`.
pub(crate) fn set(format: Format) {
    FORMAT.set(format);
    ENVELOPE_PRINTED.set(false);
}

/// Whether an error envelope already went to stdout in this invocation. A
/// failure found after that (the skew diagnosis, an io error) adds its human
/// line but no second document.
pub(crate) fn envelope_printed() -> bool {
    ENVELOPE_PRINTED.get()
}

/// Split the global output flags off `args`. Everything after a `--` is left
/// alone (an agent's argv, a command to run). Returns the remaining argv and
/// the format, or a usage message for a bad or conflicting value.
pub(crate) fn take_global(args: &[String]) -> Result<(Vec<String>, Format), String> {
    let mut kept: Vec<String> = Vec::with_capacity(args.len());
    let mut format = Format::Default;
    let mut choose = |next: Format| match format {
        Format::Default => {
            format = next;
            Ok(())
        }
        current if current == next => Ok(()),
        _ => Err("--json / --format json and --format text cannot be combined".to_string()),
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let value = match arg.as_str() {
            "--" => {
                kept.push(arg.clone());
                kept.extend(rest.cloned());
                break;
            }
            "--json" => {
                choose(Format::Json)?;
                continue;
            }
            "--format" => rest
                .next()
                .ok_or("--format needs a value: json or text")?
                .clone(),
            _ => match arg.strip_prefix("--format=") {
                Some(value) => value.to_string(),
                None => {
                    kept.push(arg.clone());
                    continue;
                }
            },
        };
        // argv[0], then the verb: `pane read` and `agent read` own `--format`.
        let reads = matches!(kept.get(1..3), Some([group, verb])
            if matches!(group.as_str(), "pane" | "agent") && verb == "read");
        match value.as_str() {
            "json" => choose(Format::Json)?,
            _ if reads => kept.extend(["--format".to_string(), value]),
            "text" => choose(Format::Text)?,
            other => return Err(format!("--format takes json or text, not {other:?}")),
        }
    }
    Ok((kept, format))
}

/// The retry class an envelope carries: the error code's own (ADR-0029 §4),
/// or, for a code the registry does not know (an older server's), the class
/// of the exit status the call returns.
pub(crate) fn retry_for(code: &str, exit_code: i32) -> exit::Retry {
    match ErrorCode::from_code(code) {
        Some(code) => code.retry(),
        None => exit_retry(exit_code),
    }
}

/// The retry class of an exit status: its first entry in the registry, so a
/// shared code (1 is also `CONFIG_CHECK_ADVISORY`) reads as its general case.
pub(crate) fn exit_retry(exit_code: i32) -> exit::Retry {
    exit::ALL
        .iter()
        .find(|entry| entry.code == exit_code)
        .map_or(exit::Retry::Maybe, |entry| entry.retry)
}

/// Report an error response (`{"id", "error": {"code", "message", …}}`) and
/// return `exit_code`. Without machine output the response prints on stderr
/// as it always did.
pub(crate) fn error(response: &Value, exit_code: i32) -> i32 {
    error_as(
        response,
        serde_json::to_string(response).unwrap_or_default(),
        exit_code,
    )
}

/// [`error`] for a verb whose stderr line without machine output is not the
/// whole response (`pane read` prints the `error` object alone).
pub(crate) fn error_as(response: &Value, default_line: impl Display, exit_code: i32) -> i32 {
    if !json() {
        eprintln!("{default_line}");
        return exit_code;
    }
    let error = &response["error"];
    let code = error["code"].as_str().unwrap_or("internal_error");
    let message = error["message"].as_str().unwrap_or(code);
    // A recorded skew failure turns this call's exit into 78 on the way out
    // (`maybe_run`), so the envelope says 78 now.
    let gap = ErrorCode::ServerVersionGap;
    let exit_code = if code == gap.as_str() && crate::api::compatibility::failure_pending() {
        gap.exit()
    } else {
        exit_code
    };
    eprintln!("{}", human_line(code, message));
    if !ENVELOPE_PRINTED.replace(true) {
        println!("{}", envelope(response, exit_code));
    }
    exit_code
}

/// Report a failure the client found itself, with a registered `code`.
/// Without machine output the message prints on stderr as it is.
pub(crate) fn failure(code: ErrorCode, message: &str, exit_code: i32) -> i32 {
    let response = json!({"error": {"code": code.as_str(), "message": message}});
    error_as(&response, message, exit_code)
}

/// A send's effect failure (`unconfirmed`, `abandoned`; exit 8 or 9) is a
/// result, not an error: under machine output it is the stdout document,
/// otherwise it stays on stderr.
pub(crate) fn effect(response: &Value, exit_code: i32) -> i32 {
    if json() {
        println!("{response}");
    } else {
        eprintln!("{response}");
    }
    exit_code
}

/// The registered code for an io error that ended a verb.
pub(crate) fn io_error_code(err: &std::io::Error) -> ErrorCode {
    use std::io::ErrorKind::*;
    match err.kind() {
        TimedOut | WouldBlock => ErrorCode::Timeout,
        NotFound | ConnectionRefused | ConnectionReset | ConnectionAborted | BrokenPipe
        | UnexpectedEof | AddrNotAvailable => ErrorCode::Transport,
        _ => ErrorCode::InternalError,
    }
}

/// The envelope: the response as the server sent it, with the retry class
/// and exit status added to `error`.
fn envelope(response: &Value, exit_code: i32) -> Value {
    let mut envelope = response.clone();
    if let Some(error) = envelope.get_mut("error").and_then(Value::as_object_mut) {
        let code = error.get("code").and_then(Value::as_str).unwrap_or("");
        let retry = retry_for(code, exit_code).as_str();
        error.insert("retry".into(), retry.into());
        error.insert("exit".into(), exit_code.into());
    }
    envelope
}

/// One line however many the message has: `error: <code>: <message>`.
fn human_line(code: &str, message: &str) -> String {
    let message: Vec<&str> = message.lines().map(str::trim).collect();
    format!("error: {code}: {}", message.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    fn taken(args: &[&str]) -> (Vec<String>, Format) {
        take_global(&argv(args)).unwrap()
    }

    #[test]
    fn json_and_format_are_global_in_any_position_before_a_double_dash() {
        let want = (argv(&["flk", "agent", "get", "w"]), Format::Json);
        assert_eq!(taken(&["flk", "agent", "get", "w", "--json"]), want);
        assert_eq!(taken(&["flk", "--json", "agent", "get", "w"]), want);
        assert_eq!(
            taken(&["flk", "agent", "--format", "json", "get", "w"]),
            want
        );
        assert_eq!(taken(&["flk", "agent", "get", "w", "--format=json"]), want);
        assert_eq!(
            taken(&["flk", "delegate", "status", "d", "--format", "text"]),
            (argv(&["flk", "delegate", "status", "d"]), Format::Text)
        );
        assert_eq!(
            taken(&["flk", "agent", "start", "a", "--", "claude", "--json"]),
            (
                argv(&["flk", "agent", "start", "a", "--", "claude", "--json"]),
                Format::Default
            )
        );
    }

    #[test]
    fn the_read_verbs_keep_their_own_screen_format() {
        assert_eq!(
            taken(&["flk", "pane", "read", "p", "--format", "text", "--json"]),
            (
                argv(&["flk", "pane", "read", "p", "--format", "text"]),
                Format::Json
            )
        );
        assert_eq!(
            taken(&["flk", "agent", "read", "a", "--format=ansi"]),
            (
                argv(&["flk", "agent", "read", "a", "--format", "ansi"]),
                Format::Default
            )
        );
        assert_eq!(
            taken(&["flk", "pane", "read", "p", "--format", "json"]),
            (argv(&["flk", "pane", "read", "p"]), Format::Json)
        );
    }

    #[test]
    fn a_bad_or_conflicting_format_is_a_usage_error() {
        for args in [
            &["flk", "agent", "get", "w", "--format"][..],
            &["flk", "agent", "get", "w", "--format", "yaml"],
            &["flk", "agent", "get", "w", "--json", "--format", "text"],
        ] {
            assert!(take_global(&argv(args)).is_err(), "{args:?}");
        }
    }

    #[test]
    fn the_envelope_adds_retry_and_exit_from_the_registries() {
        let response = json!({"id": "cli:agent:get", "error": {
            "code": "agent_not_found", "message": "agent nosuch not found", "data": {"k": 1}}});
        assert_eq!(
            envelope(&response, exit::FAILURE),
            json!({"id": "cli:agent:get", "error": {
                "code": "agent_not_found", "message": "agent nosuch not found",
                "data": {"k": 1}, "retry": "never", "exit": 1}})
        );
        // A refusal a send-like verb exits 10 on keeps its own class.
        let refused = json!({"error": {"code": "not_settled", "message": "m"}});
        assert_eq!(envelope(&refused, exit::REFUSED)["error"]["retry"], "later");
        // An older server's unregistered code takes the exit status's class.
        let unknown = json!({"error": {"code": "unknown_method", "message": "m"}});
        assert_eq!(envelope(&unknown, exit::FAILURE)["error"]["retry"], "maybe");
        assert_eq!(envelope(&unknown, exit::USAGE)["error"]["retry"], "never");
    }

    #[test]
    fn the_human_line_is_one_line() {
        assert_eq!(
            human_line("agent_not_found", "agent nosuch\n  not found\n"),
            "error: agent_not_found: agent nosuch not found"
        );
    }

    #[test]
    fn io_errors_map_to_registered_codes() {
        let kind = |kind| io_error_code(&std::io::Error::from(kind));
        assert_eq!(kind(std::io::ErrorKind::TimedOut), ErrorCode::Timeout);
        assert_eq!(kind(std::io::ErrorKind::NotFound), ErrorCode::Transport);
        assert_eq!(
            kind(std::io::ErrorKind::ConnectionRefused),
            ErrorCode::Transport
        );
        assert_eq!(kind(std::io::ErrorKind::Other), ErrorCode::InternalError);
    }
}
