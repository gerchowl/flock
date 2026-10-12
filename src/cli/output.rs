//! Output selection and the error envelope (ADR-0029 §3, §4, #1024).
//!
//! `--format json|text`, and its synonym `--json`, are global: [`take_global`]
//! lifts them out of argv before the verb, or among the verb's flags, never
//! from a verb's free text ([`FREE_TEXT`]) or after a `--`. Under JSON an error
//! is the §4 envelope on STDOUT, `{"id", "error": {"code", "message", "retry",
//! "exit", "data"}}`, and one human line on stderr. It prints once, last
//! ([`finish`]), so `exit` is the process's exit status; `id` is `null` for a
//! failure that answers no request. Without JSON nothing changes (owner
//! decision 3): the error stays the one stderr line it always was.
//!
//! Known limit: only errors reported through [`error`] and [`failure`] gain
//! the envelope; usage lines and a verb's own text failures do not (#1150).

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output: the error envelope and its human line are the product surface"
)]

use std::cell::{Cell, RefCell};
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
    /// The first error response reported in this invocation, printed by
    /// [`finish`] with the exit status the process returns.
    static PENDING: RefCell<Option<Value>> = const { RefCell::new(None) };
}

/// Whether the caller asked for JSON (`--format json` or `--json`).
pub(crate) fn json() -> bool {
    FORMAT.get() == Format::Json
}

/// Set the invocation's format. Called once per invocation, from `maybe_run`.
pub(crate) fn set(format: Format) {
    FORMAT.set(format);
    PENDING.set(None);
}

/// Whether an error envelope is already waiting for [`finish`]. A failure
/// found after it (the skew diagnosis, an io error) adds its human line but
/// no second document.
pub(crate) fn envelope_pending() -> bool {
    PENDING.with_borrow(Option::is_some)
}

/// Print the pending envelope, if any, with the exit status the invocation
/// returns. Called once, by `maybe_run`, as the invocation ends.
pub(crate) fn finish(exit_code: i32) {
    if let Some(response) = PENDING.take() {
        println!("{}", envelope(&response, exit_code));
    }
}

/// A verb whose trailing words are free text, typed into a pane or stored as
/// a label verbatim: the verb, its operand count (a target, an id), its own
/// flags, and its flags that take a value. The text starts at the first word
/// after the operands that is not one of its flags.
type FreeText = (
    [&'static str; 2],
    usize,
    &'static [&'static str],
    &'static [&'static str],
);

/// Every verb that joins its trailing arguments into text. `msg send` and
/// `msg reply` are not here: they read options anywhere in their text.
const FREE_TEXT: &[FreeText] = &[
    (
        ["pane", "run"],
        1,
        &["--if-input-empty"],
        &["--if-session", "--min-age-secs"],
    ),
    (["pane", "send-text"], 1, &[], &[]),
    (["pane", "send-keys"], 1, &[], &[]),
    (["agent", "send"], 1, &["--submit", "--no-submit"], &[]),
    (
        ["pane", "arm-self-compact"],
        0,
        &["--abort"],
        &["--pane", "--instructions", "--instructions-file"],
    ),
    (["agent", "rename"], 1, &[], &[]),
    (["pane", "rename"], 1, &[], &[]),
    (["tab", "rename"], 1, &[], &[]),
    (["workspace", "rename"], 1, &[], &[]),
];

/// Split the global output flags off `args` (argv, program name first).
/// Returns the remaining argv and the format, or a usage message for a bad or
/// conflicting value.
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
    kept.extend(rest.next().cloned());
    let mut verb: Vec<&str> = Vec::new();
    let mut grammar: Option<FreeText> = None;
    let mut operands = 0;
    while let Some(arg) = rest.next() {
        let (_, want, flags, value_flags) = grammar.unwrap_or(([""; 2], 0, &[], &[]));
        let own_flag = flags.contains(&arg.as_str()) || value_flags.contains(&arg.as_str());
        if arg == "--" || (grammar.is_some() && operands == want && !own_flag) {
            // A `--`, or the verb's free text: the rest is the verb's.
            kept.push(arg.clone());
            kept.extend(rest.cloned());
            break;
        }
        let value = match arg.as_str() {
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
                    if value_flags.contains(&arg.as_str()) {
                        kept.extend(rest.next().cloned());
                    } else if arg.starts_with('-') || verb.len() == 2 && grammar.is_none() {
                        // A verb flag, or a word of a verb without free text.
                    } else if grammar.is_some() {
                        operands += 1;
                    } else {
                        verb.push(arg);
                        grammar = FREE_TEXT
                            .iter()
                            .find(|text| text.0[..] == verb[..])
                            .copied();
                    }
                    continue;
                }
            },
        };
        // `pane read` and `agent read` own `--format`.
        let reads = matches!(verb[..], ["pane" | "agent", "read"]);
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
    eprintln!("{}", human_line(code, message));
    PENDING.with_borrow_mut(|pending| {
        pending.get_or_insert_with(|| response.clone());
    });
    exit_code
}

/// Report a failure the client found itself, with a registered `code`. It
/// answers no request, so its envelope's `id` is `null`. Without machine
/// output the message prints on stderr as it is.
pub(crate) fn failure(code: ErrorCode, message: &str, exit_code: i32) -> i32 {
    let response = json!({"id": null, "error": {"code": code.as_str(), "message": message}});
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
/// and exit status added to `error`, and `id: null` when it carries none.
fn envelope(response: &Value, exit_code: i32) -> Value {
    let mut envelope = response.clone();
    if let Some(fields) = envelope.as_object_mut() {
        fields.entry("id").or_insert(Value::Null);
    }
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

    /// A verb's free text is never scanned: the grammar says where it starts.
    #[test]
    fn free_text_passes_through_verbatim() {
        for args in [
            &["flk", "pane", "run", "1-1", "git", "log", "--format=%H"][..],
            &[
                "flk", "pane", "run", "1-1", "gh", "pr", "view", "--json", "state",
            ],
            &[
                "flk",
                "pane",
                "run",
                "1-1",
                "--if-session",
                "s",
                "--if-input-empty",
                "--json",
            ],
            &["flk", "pane", "send-text", "1-1", "--format", "yaml"],
            &[
                "flk",
                "agent",
                "send",
                "--no-submit",
                "w1",
                "--no-submit",
                "a",
                "--json",
            ],
            &["flk", "agent", "rename", "w1", "--json"],
            &[
                "flk",
                "pane",
                "arm-self-compact",
                "--pane",
                "1-1",
                "go",
                "--json",
            ],
            &[
                "flk",
                "pane",
                "arm-self-compact",
                "--instructions",
                "--json",
                "go",
            ],
        ] {
            assert_eq!(taken(args), (argv(args), Format::Default), "{args:?}");
        }
    }

    /// Before the free text, in the global or the verb-flag position, they
    /// are global flags.
    #[test]
    fn flags_before_the_free_text_are_global() {
        assert_eq!(
            taken(&["flk", "--json", "pane", "run", "1-1", "ls", "--json"]),
            (
                argv(&["flk", "pane", "run", "1-1", "ls", "--json"]),
                Format::Json
            )
        );
        assert_eq!(
            taken(&["flk", "pane", "run", "--format=json", "1-1", "ls"]),
            (argv(&["flk", "pane", "run", "1-1", "ls"]), Format::Json)
        );
        // Its text starts right after the verb: the flag goes before it.
        assert_eq!(
            taken(&["flk", "pane", "--json", "arm-self-compact", "--abort"]),
            (
                argv(&["flk", "pane", "arm-self-compact", "--abort"]),
                Format::Json
            )
        );
        // `msg send` reads its options anywhere in the text, `--json` among them.
        assert_eq!(
            taken(&["flk", "msg", "send", "w1", "hi", "--json"]),
            (argv(&["flk", "msg", "send", "w1", "hi"]), Format::Json)
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

    /// The envelope prints once, at the end, with the exit the process
    /// returns: a later skew override to 78 is what it says (#1024 r2).
    #[test]
    fn the_pending_envelope_carries_the_final_exit_and_an_id() {
        set(Format::Json);
        error(
            &json!({"id": "cli:x", "error": {"code": "agent_not_found", "message": "m"}}),
            1,
        );
        failure(ErrorCode::Transport, "later failure", 1);
        let pending = PENDING.with_borrow(Clone::clone).unwrap();
        assert_eq!(
            pending["error"]["code"], "agent_not_found",
            "the first one wins"
        );
        assert_eq!(envelope(&pending, 78)["error"]["exit"], 78);
        set(Format::Default);
        assert!(!envelope_pending());
        let client_side = json!({"error": {"code": "transport", "message": "m"}});
        assert_eq!(envelope(&client_side, 1)["id"], Value::Null);
        assert!(envelope(&client_side, 1)
            .as_object()
            .unwrap()
            .contains_key("id"));
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
