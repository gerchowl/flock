#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::{
    Method, PaneAgentState, PaneArmSelfCompactParams, PaneClearHeaderFieldParams, PaneListParams,
    PaneMoveDestination, PaneMoveParams, PaneReadParams, PaneRenameParams, PaneReportAgentParams,
    PaneReportMetadataParams, PaneReportRecapParams, PaneReportReplyParams, PaneSendInputParams,
    PaneSendKeysParams, PaneSendTextParams, PaneSetHeaderFieldParams, PaneSplitParams, PaneTarget,
    ReadFormat, ReadSource, Request, SplitDirection,
};

pub(super) fn run_pane_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_pane_help();
        return Ok(2);
    };

    match subcommand {
        "list" => pane_list(&args[1..]),
        "get" => pane_get(&args[1..]),
        "read" => pane_read(&args[1..]),
        "rename" => pane_rename(&args[1..]),
        "split" => pane_split(&args[1..]),
        "move" => pane_move(&args[1..]),
        "close" => pane_close(&args[1..]),
        "send-text" => pane_send_text(&args[1..]),
        "send-keys" => pane_send_keys(&args[1..]),
        "arm-self-compact" => pane_arm_self_compact(&args[1..]),
        "report-agent" => pane_report_agent(&args[1..]),
        "report-metadata" => pane_report_metadata(&args[1..]),
        "report-recap" => pane_report_recap(&args[1..]),
        "report-reply" => pane_report_reply(&args[1..]),
        "set-field" => pane_set_field(&args[1..]),
        "clear-field" => pane_clear_field(&args[1..]),
        "run" => pane_run(&args[1..]),
        "help" | "--help" | "-h" => {
            print_pane_help();
            Ok(0)
        }
        _ => {
            print_pane_help();
            Ok(2)
        }
    }
}

fn pane_list(args: &[String]) -> std::io::Result<i32> {
    let mut workspace_id = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --workspace");
                    return Ok(2);
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:pane:list".into(),
        method: Method::PaneList(PaneListParams { workspace_id }),
    })?)
}

fn pane_get(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_pane_id) = args.first() else {
        eprintln!("usage: flk pane get <pane_id>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: flk pane get <pane_id>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:pane:get".into(),
        method: Method::PaneGet(PaneTarget {
            pane_id: super::normalize_pane_id(raw_pane_id),
        }),
    })?)
}

fn pane_rename(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_pane_id) = args.first() else {
        eprintln!("usage: flk pane rename <pane_id> <label>|--clear");
        return Ok(2);
    };
    if args.len() < 2 {
        eprintln!("usage: flk pane rename <pane_id> <label>|--clear");
        return Ok(2);
    }
    let label = if args.len() == 2 && args[1] == "--clear" {
        None
    } else {
        Some(args[1..].join(" "))
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:pane:rename".into(),
        method: Method::PaneRename(PaneRenameParams {
            pane_id: super::normalize_pane_id(raw_pane_id),
            label,
        }),
    })?)
}

const PANE_TARGET_HELP: &str = "Target: pane id, terminal id, or unique agent name/label. A pane id wins over a same-named agent.";

pub(super) const PANE_READ_USAGE: &str = "flk pane read <target> [--source visible|recent|recent-unwrapped|detection] [--lines N] [--format text|ansi] [--ansi]\n  Target: pane id, terminal id, or unique agent name/label. A pane id wins over a same-named agent.";
pub(super) const PANE_CLOSE_USAGE: &str = "flk pane close <target>\n  Target: pane id, terminal id, or unique agent name/label. A pane id wins over a same-named agent.";
pub(super) const PANE_SEND_TEXT_USAGE: &str = "flk pane send-text <target> <text>\n  Target: pane id, terminal id, or unique agent name/label. A pane id wins over a same-named agent.\n  Pastes text (bracketed when enabled), without sending Enter.\n  Use pane send-keys for control keys (Enter, C-c, Esc).";
pub(super) const PANE_SEND_KEYS_USAGE: &str = "flk pane send-keys <target> <key> [key ...]\n  Target: pane id, terminal id, or unique agent name/label. A pane id wins over a same-named agent.";

fn pane_read(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_pane_id) = args.first() else {
        eprintln!("usage: {PANE_READ_USAGE}");
        return Ok(2);
    };

    let pane_id = super::normalize_pane_id(raw_pane_id);
    let mut source = ReadSource::Recent;
    let mut lines = None;
    let mut format = ReadFormat::Text;
    let mut strip_ansi = true;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--source" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --source");
                    return Ok(2);
                };
                source = super::parse_read_source(value)?;
                index += 2;
            }
            "--lines" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --lines");
                    return Ok(2);
                };
                lines = Some(super::parse_u32_flag("--lines", value)?);
                index += 2;
            }
            "--format" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --format");
                    return Ok(2);
                };
                format = super::parse_read_format(value)?;
                index += 2;
            }
            "--ansi" => {
                format = ReadFormat::Ansi;
                index += 1;
            }
            "--raw" => {
                format = ReadFormat::Ansi;
                strip_ansi = false;
                index += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let response = super::send_request(&Request {
        id: "cli:pane:read".into(),
        method: Method::PaneRead(PaneReadParams {
            pane_id,
            source,
            lines,
            format,
            strip_ansi,
        }),
    })?;

    if let Some(error) = response.get("error") {
        eprintln!("{}", serde_json::to_string(error).unwrap());
        return Ok(1);
    }

    if let Some(text) = response["result"]["read"]["text"].as_str() {
        print!("{text}");
    }
    Ok(0)
}

fn pane_split(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_pane_id) = args.first() else {
        eprintln!(
            "usage: flk pane split <pane_id> --direction right|down [--cwd PATH] [--focus] [--no-focus]"
        );
        return Ok(2);
    };

    let pane_id = super::normalize_pane_id(raw_pane_id);
    let mut direction = None;
    let mut cwd = None;
    let mut focus = false;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--direction" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --direction");
                    return Ok(2);
                };
                direction = Some(super::parse_split_direction(value)?);
                index += 2;
            }
            "--cwd" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --cwd");
                    return Ok(2);
                };
                cwd = Some(value.clone());
                index += 2;
            }
            "--focus" => {
                focus = true;
                index += 1;
            }
            "--no-focus" => {
                focus = false;
                index += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let Some(direction) = direction else {
        eprintln!("missing required --direction");
        return Ok(2);
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:pane:split".into(),
        method: Method::PaneSplit(PaneSplitParams {
            workspace_id: None,
            target_pane_id: pane_id,
            direction,
            cwd,
            focus,
        }),
    })?)
}

fn pane_move(args: &[String]) -> std::io::Result<i32> {
    let params = match parse_pane_move_args(args) {
        Ok(params) => params,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:pane:move".into(),
        method: Method::PaneMove(params),
    })?)
}

fn parse_pane_move_args(args: &[String]) -> Result<PaneMoveParams, String> {
    let Some(raw_pane_id) = args.first() else {
        return Err(pane_move_usage());
    };
    if raw_pane_id.starts_with('-') {
        return Err(pane_move_usage());
    }

    let pane_id = super::normalize_pane_id(raw_pane_id);
    let mut tab_id = None;
    let mut new_tab = false;
    let mut new_workspace = false;
    let mut workspace_id = None;
    let mut target_pane_id = None;
    let mut split = None;
    let mut ratio = None;
    let mut label = None;
    let mut tab_label = None;
    let mut focus = true;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--tab" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --tab".into());
                };
                tab_id = Some(super::normalize_tab_id(value));
                index += 2;
            }
            "--new-tab" => {
                new_tab = true;
                index += 1;
            }
            "--new-workspace" => {
                new_workspace = true;
                index += 1;
            }
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --workspace".into());
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            "--target-pane" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --target-pane".into());
                };
                target_pane_id = Some(super::normalize_pane_id(value));
                index += 2;
            }
            "--split" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --split".into());
                };
                split = Some(parse_move_split_direction(value)?);
                index += 2;
            }
            "--ratio" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --ratio".into());
                };
                let parsed = value
                    .parse::<f32>()
                    .map_err(|_| format!("invalid ratio: {value}"))?;
                if !parsed.is_finite() {
                    return Err(format!("invalid ratio: {value}"));
                }
                ratio = Some(parsed);
                index += 2;
            }
            "--label" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --label".into());
                };
                label = Some(value.clone());
                index += 2;
            }
            "--tab-label" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --tab-label".into());
                };
                tab_label = Some(value.clone());
                index += 2;
            }
            "--focus" => {
                focus = true;
                index += 1;
            }
            "--no-focus" => {
                focus = false;
                index += 1;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    let destination_count =
        usize::from(tab_id.is_some()) + usize::from(new_tab) + usize::from(new_workspace);
    if destination_count != 1 {
        return Err(pane_move_usage());
    }

    let destination = if let Some(tab_id) = tab_id {
        let Some(split) = split else {
            return Err(pane_move_usage());
        };
        if workspace_id.is_some()
            || new_tab
            || new_workspace
            || label.is_some()
            || tab_label.is_some()
        {
            return Err(pane_move_usage());
        }
        PaneMoveDestination::Tab {
            tab_id,
            target_pane_id,
            split,
            ratio,
        }
    } else if new_tab {
        if split.is_some() || target_pane_id.is_some() || new_workspace || tab_label.is_some() {
            return Err(pane_move_usage());
        }
        PaneMoveDestination::NewTab {
            workspace_id,
            label,
        }
    } else {
        if split.is_some() || target_pane_id.is_some() || workspace_id.is_some() || new_tab {
            return Err(pane_move_usage());
        }
        PaneMoveDestination::NewWorkspace { label, tab_label }
    };

    Ok(PaneMoveParams {
        pane_id,
        destination,
        focus,
    })
}

fn pane_move_usage() -> String {
    "usage: flk pane move <pane_id> --tab <tab_id> --split right|down [--target-pane ID] [--ratio FLOAT] [--focus|--no-focus]\n       flk pane move <pane_id> --new-tab [--workspace ID] [--label TEXT] [--focus|--no-focus]\n       flk pane move <pane_id> --new-workspace [--label TEXT] [--tab-label TEXT] [--focus|--no-focus]"
        .into()
}

fn parse_move_split_direction(value: &str) -> Result<SplitDirection, String> {
    match value {
        "right" => Ok(SplitDirection::Right),
        "down" => Ok(SplitDirection::Down),
        _ => Err(format!(
            "invalid split direction: {value} (expected right or down)"
        )),
    }
}

fn pane_close(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_pane_id) = args.first() else {
        eprintln!("usage: {PANE_CLOSE_USAGE}");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: {PANE_CLOSE_USAGE}");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:pane:close".into(),
        method: Method::PaneClose(PaneTarget {
            pane_id: super::normalize_pane_id(raw_pane_id),
        }),
    })?)
}

fn pane_send_text(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: {PANE_SEND_TEXT_USAGE}");
        return Ok(2);
    }

    let pane_id = super::normalize_pane_id(&args[0]);
    let text = args[1..].join(" ");
    super::send_ok_request(Method::PaneSendText(PaneSendTextParams { pane_id, text }))
}

fn pane_send_keys(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: {PANE_SEND_KEYS_USAGE}");
        return Ok(2);
    }

    let pane_id = super::normalize_pane_id(&args[0]);
    let keys = args[1..].to_vec();
    super::send_ok_request(Method::PaneSendKeys(PaneSendKeysParams { pane_id, keys }))
}

/// Arm a self-compaction of the calling agent's own session (#540).
///
/// The CLI twin of the `flock_self_compact` MCP tool, and it exists because
/// the `flock` skill an agent is given is CLI-shaped: an agent reading that
/// skill has no MCP tools at all, so without this the feature would be
/// unreachable for exactly the audience the skill is written for.
///
/// `pane_id` is optional and omitted by default so the common case is just the
/// continuation prompt — an agent compacting itself should not have to look up
/// its own id first, which is one more step between "context is full" and
/// acting on it.
fn pane_arm_self_compact(args: &[String]) -> std::io::Result<i32> {
    let params = match parse_arm_self_compact(args) {
        Ok(params) => params,
        Err(usage) => {
            eprintln!("{usage}");
            return Ok(2);
        }
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:pane:arm-self-compact".into(),
        method: Method::PaneArmSelfCompact(params),
    })?)
}

/// Pure: the flags and the trailing text of `arm-self-compact` to the params
/// the verb takes, or the usage line to print. Split out from the command so
/// the parsing — which is where the interesting refusals live — is testable
/// without a socket.
fn parse_arm_self_compact(args: &[String]) -> Result<PaneArmSelfCompactParams, String> {
    const USAGE: &str =
        "usage: flk pane arm-self-compact [--pane <pane_id>] [--abort] <handoff prompt>";
    let mut pane = None;
    let mut continuation: Option<String> = None;
    let mut abort = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--pane needs a pane id".to_string());
                };
                pane = Some(super::normalize_pane_id(value));
                index += 2;
            }
            "--abort" => {
                abort = true;
                index += 1;
            }
            text => {
                let mut rest = vec![text.to_string()];
                rest.extend_from_slice(&args[index + 1..]);
                continuation = Some(rest.join(" "));
                break;
            }
        }
    }

    if continuation
        .as_ref()
        .is_some_and(|text| text.trim().is_empty())
    {
        return Err(
            "the handoff prompt must not be empty: it is what you get back on the \
             other side of the compaction"
                .to_string(),
        );
    }
    if !abort && continuation.is_none() {
        return Err(USAGE.to_string());
    }

    Ok(PaneArmSelfCompactParams {
        pane,
        continuation,
        abort,
    })
}

/// How long `pane run` leaves between typing the command and pressing Enter
/// (#362).
///
/// `pane run` used to hand the text and the Enter to the pane in one burst,
/// and documented that as submitting them "atomically". Atomicity is the bug.
/// A program reading a raw-mode stdin sees read boundaries, not keystrokes,
/// and every agent TUI uses those boundaries to tell typing from pasting: text
/// and a carriage return arriving in ONE read look like a pasted block that
/// happens to contain a newline, so the newline is inserted rather than
/// submitting. That is the reported failure — the prompt landed in Claude's
/// input box and sat there, and an explicit `pane send-keys <pane> Enter`
/// afterwards submitted the identical bytes.
///
/// A gap is what makes the two land in separate reads. The value is above the
/// input-inactivity windows TUIs use to close a paste and below anything a
/// caller would notice, and it is deliberately not configurable: a knob here
/// would be a knob for a heuristic.
///
/// It is a heuristic, and the docs say so. A TUI that has not started reading
/// stdin at all misses the text as well as the Enter, and no gap fixes that —
/// `agent start --wait-ready` is the answer to that half.
pub(crate) const PANE_RUN_SUBMIT_GAP: std::time::Duration = std::time::Duration::from_millis(120);

/// One step of a `pane run`, so the ordering contract is a value that can be
/// asserted on rather than a shape buried in a socket conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneRunStep {
    /// Type the command. No keys ride along: that is the whole point.
    Type { pane_id: String, text: String },
    /// Let the pane's reader come back round before the Enter.
    Settle(std::time::Duration),
    /// Press Enter, on its own, as its own write.
    Submit { pane_id: String },
}

fn pane_run_steps(pane_id: &str, text: &str) -> Vec<PaneRunStep> {
    vec![
        PaneRunStep::Type {
            pane_id: pane_id.to_owned(),
            text: text.to_owned(),
        },
        PaneRunStep::Settle(PANE_RUN_SUBMIT_GAP),
        PaneRunStep::Submit {
            pane_id: pane_id.to_owned(),
        },
    ]
}

fn pane_run(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: flk pane run <pane_id> <command>");
        return Ok(2);
    }

    let pane_id = super::normalize_pane_id(&args[0]);
    let text = args[1..].join(" ");
    submit_sequence(&pane_id, &text)
}

/// Type `text` into `pane_id` and press Enter, as the same two separate writes
/// `flk pane run` has always made, and report what the server said (#578 P18).
///
/// The executor beside the existing value: `pane_run_steps` is the ordering
/// contract and stays a value a test can assert on, while this walks it and
/// issues the requests. The delegate deliberately MIRRORS this sequence —
/// same type-then-Enter, same `PANE_RUN_SUBMIT_GAP` — on BOUNDED requests
/// (see `delegate::submit_brief`): sharing `pane_run_steps` would hand the
/// delegate the unbounded `send_ok_request` through this executor, and a
/// server that goes quiet mid-type would then hang `start`/`send` forever
/// (P578 r4-4 W2). The gap value is the single source of truth; the walking
/// code is split by whether a stall must be bounded.
///
/// The exit code is what [`super::send_ok_request`] returns — 0, or 1 after
/// printing the server's refusal to stderr as it does — so `pane run`'s
/// observable behaviour is unchanged, and a caller that only wants to know
/// whether the submit landed reads it as non-zero.
pub(super) fn submit_sequence(pane_id: &str, text: &str) -> std::io::Result<i32> {
    for step in pane_run_steps(pane_id, text) {
        let method = match step {
            PaneRunStep::Type { pane_id, text } => Method::PaneSendInput(PaneSendInputParams {
                pane_id,
                text,
                keys: Vec::new(),
            }),
            PaneRunStep::Settle(gap) => {
                std::thread::sleep(gap);
                continue;
            }
            PaneRunStep::Submit { pane_id } => Method::PaneSendKeys(PaneSendKeysParams {
                pane_id,
                keys: vec!["Enter".into()],
            }),
        };
        let exit_code = super::send_ok_request(method)?;
        if exit_code != 0 {
            return Ok(exit_code);
        }
    }
    Ok(0)
}

const REPORT_AGENT_USAGE: &str = "usage: flk pane report-agent [<pane_id>] --source ID --agent LABEL --state idle|working|blocked|unknown [--pane <pane_id>] [--message TEXT] [--custom-status TEXT] [--seq N] [--agent-session-id ID] [--agent-session-path PATH]";

/// Everything `flk pane report-agent` was told, before the pane is resolved.
///
/// `pane_id` stays `None` when the caller named no pane, so the resolution
/// happens in one place ([`pane_report_agent`]) rather than in the parser.
#[derive(Debug, Clone, PartialEq)]
struct ReportAgentArgs {
    pane_id: Option<String>,
    source: String,
    agent: String,
    state: PaneAgentState,
    message: Option<String>,
    custom_status: Option<String>,
    seq: Option<u64>,
    agent_session_id: Option<String>,
    agent_session_path: Option<String>,
}

/// Parse `flk pane report-agent`'s argv (#454).
///
/// Split out from [`pane_report_agent`] and pure, because the defect this
/// fixes is a PARSING defect and asserting on it must not need a server: the
/// pane id used to be a required positional read as `args[0]`, so a caller
/// that omitted it had its first flag eaten as the pane id — and the first
/// flag is `--source`. A reporting hook wraps its call in
/// `>/dev/null 2>&1 || true` (a hook must never break what it reports on), so
/// that swallow turned every state transition into a discard.
///
/// The pane id is therefore optional and resolves exactly the way
/// `report-recap`/`report-reply` resolve it: `--pane`, else the legacy leading
/// positional, else the calling pane ($FLOCK_PANE_ID, healed server-side by
/// socket-peer process ancestry). A flag can no longer be consumed as the pane
/// id, and a bare word that arrives after the first token is refused by name
/// rather than adopted — so this verb is no laxer than its siblings about a
/// caller's stray word.
fn parse_report_agent_args(args: &[String]) -> Result<ReportAgentArgs, String> {
    let mut pane_id: Option<String> = None;
    let mut source = None;
    let mut agent = None;
    let mut state = None;
    let mut message = None;
    let mut custom_status = None;
    let mut seq = None;
    let mut agent_session_id = None;
    let mut agent_session_path = None;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        // The pane id is positional and comes FIRST, so only the first token
        // may be one. Every value-taking arm below reads `args[index + 1]`,
        // which is why a flag's value never reaches this test — but a bare
        // word arriving AFTER the first token is not a pane id the caller
        // meant: it is an unquoted multi-word value (`--message waiting`).
        // Reading it as the pane would report to a pane nobody named, which
        // is the shape of trap #454 moved rather than closed, and it would
        // make this verb laxer than report-recap/report-reply, which have no
        // positional at all and refuse such a word by name.
        if index == 0 && !arg.starts_with('-') {
            pane_id = Some(super::normalize_pane_id(arg));
            index += 1;
            continue;
        }
        match arg {
            "--pane" => {
                // No flock pane id can begin with `-` (the server accepts only
                // `p_<n>`, `p_<ws>_<n>`, `<ws>:p<n>` and legacy `<ws>-<n>`), so
                // a flag here is a missing value, not an exotic pane id. Naming
                // that is the difference between a diagnosis and a puzzle.
                match args.get(index + 1) {
                    Some(value) if !value.starts_with('-') => {}
                    _ => return Err("missing value for --pane".to_string()),
                }
                if pane_id.is_some() {
                    return Err("the pane id was already given".to_string());
                }
                pane_id = Some(super::normalize_pane_id(&args[index + 1]));
                index += 2;
            }
            "--source" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --source".to_string());
                };
                source = Some(value.clone());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --agent".to_string());
                };
                agent = Some(value.clone());
                index += 2;
            }
            "--state" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --state".to_string());
                };
                state = Some(super::parse_pane_agent_state(value).map_err(|err| err.to_string())?);
                index += 2;
            }
            "--message" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --message".to_string());
                };
                message = Some(value.clone());
                index += 2;
            }
            "--custom-status" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --custom-status".to_string());
                };
                custom_status = Some(value.clone());
                index += 2;
            }
            "--seq" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --seq".to_string());
                };
                seq = Some(super::parse_u64_flag("--seq", value).map_err(|err| err.to_string())?);
                index += 2;
            }
            "--agent-session-id" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --agent-session-id".to_string());
                };
                agent_session_id = Some(value.clone());
                index += 2;
            }
            "--agent-session-path" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --agent-session-path".to_string());
                };
                agent_session_path = Some(value.clone());
                index += 2;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    let Some(source) = source.and_then(|source| {
        let source = source.trim().to_string();
        (!source.is_empty()).then_some(source)
    }) else {
        return Err("missing required --source".to_string());
    };
    let Some(agent) = agent else {
        return Err("missing required --agent".to_string());
    };
    let Some(state) = state else {
        return Err("missing required --state".to_string());
    };

    Ok(ReportAgentArgs {
        pane_id,
        source,
        agent,
        state,
        message,
        custom_status,
        seq,
        agent_session_id,
        agent_session_path,
    })
}

/// `flk pane report-agent`: report an agent lifecycle state onto a pane.
///
/// The pane id is optional (#454) and defaults to the calling pane, the same
/// resolution `report-recap` and `report-reply` use — so the reporting verbs
/// agree on how a pane is supplied. A parse failure exits 2 with the reason on
/// stderr, and when no pane resolves at all the server answers
/// `pane_not_found`, which [`super::send_ok_request`] turns into exit 1.
///
/// That narrows #454's trap; it does not make the failure unswallowable. A
/// reporter that omits the pane, has no `$FLOCK_PANE_ID`, and whose ancestry
/// the server cannot attribute still gets `pane_not_found` → exit 1, which a
/// `|| true` eats — the same as `report-recap` has always behaved. What is
/// fixed is the parser: a flag can no longer be consumed as the pane id.
fn pane_report_agent(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_report_agent_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{REPORT_AGENT_USAGE}");
            return Ok(2);
        }
    };

    super::send_ok_request(Method::PaneReportAgent(PaneReportAgentParams {
        pane_id: parsed.pane_id.unwrap_or_else(calling_pane_id),
        source: parsed.source,
        agent: parsed.agent,
        state: parsed.state,
        message: parsed.message,
        custom_status: parsed.custom_status,
        seq: parsed.seq,
        agent_session_id: parsed.agent_session_id,
        agent_session_path: parsed.agent_session_path,
    }))
}

fn pane_report_metadata(args: &[String]) -> std::io::Result<i32> {
    let mut pane_id = None;
    let mut source = None;
    let mut agent = None;
    let mut applies_to_source = None;
    let mut title = None;
    let mut display_agent = None;
    let mut custom_status = None;
    let mut state_labels = std::collections::HashMap::new();
    let mut clear_title = false;
    let mut clear_display_agent = false;
    let mut clear_custom_status = false;
    let mut clear_state_labels = false;
    let mut seq = None;
    let mut ttl_ms = None;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if index == 0 && !arg.starts_with('-') {
            pane_id = Some(super::normalize_pane_id(arg));
            index += 1;
            continue;
        }
        match arg {
            "--pane" => {
                let Some(value) = args.get(index + 1).filter(|value| !value.starts_with('-'))
                else {
                    eprintln!("missing value for --pane");
                    return Ok(2);
                };
                if pane_id.is_some() {
                    eprintln!("the pane id was already given");
                    return Ok(2);
                }
                pane_id = Some(super::normalize_pane_id(value));
                index += 2;
            }
            "--source" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --source");
                    return Ok(2);
                };
                source = Some(value.clone());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --agent");
                    return Ok(2);
                };
                agent = Some(value.clone());
                index += 2;
            }
            "--applies-to-source" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --applies-to-source");
                    return Ok(2);
                };
                applies_to_source = Some(value.clone());
                index += 2;
            }
            "--title" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --title");
                    return Ok(2);
                };
                title = Some(value.clone());
                index += 2;
            }
            "--clear-title" => {
                clear_title = true;
                index += 1;
            }
            "--display-agent" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --display-agent");
                    return Ok(2);
                };
                display_agent = Some(value.clone());
                index += 2;
            }
            "--clear-display-agent" => {
                clear_display_agent = true;
                index += 1;
            }
            "--custom-status" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --custom-status");
                    return Ok(2);
                };
                custom_status = Some(value.clone());
                index += 2;
            }
            "--clear-custom-status" => {
                clear_custom_status = true;
                index += 1;
            }
            "--state-label" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --state-label");
                    return Ok(2);
                };
                let Some((status, label)) = value.split_once('=') else {
                    eprintln!("expected --state-label STATUS=TEXT");
                    return Ok(2);
                };
                let status = status.trim().to_ascii_lowercase();
                if !matches!(
                    status.as_str(),
                    "idle" | "working" | "blocked" | "done" | "unknown"
                ) {
                    eprintln!("unknown state label: {status}");
                    return Ok(2);
                }
                state_labels.insert(status, label.to_string());
                index += 2;
            }
            "--clear-state-labels" => {
                clear_state_labels = true;
                index += 1;
            }
            "--seq" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --seq");
                    return Ok(2);
                };
                seq = Some(super::parse_u64_flag("--seq", value)?);
                index += 2;
            }
            "--ttl-ms" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --ttl-ms");
                    return Ok(2);
                };
                ttl_ms = Some(super::parse_u64_flag("--ttl-ms", value)?);
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let Some(source) = source.and_then(|source| {
        let source = source.trim().to_string();
        (!source.is_empty()).then_some(source)
    }) else {
        eprintln!("missing required --source");
        return Ok(2);
    };
    if applies_to_source
        .as_deref()
        .is_some_and(|source| source.trim().is_empty())
    {
        eprintln!("missing value for --applies-to-source");
        return Ok(2);
    }
    if title.is_some() && clear_title
        || display_agent.is_some() && clear_display_agent
        || custom_status.is_some() && clear_custom_status
        || !state_labels.is_empty() && clear_state_labels
    {
        eprintln!("cannot set and clear the same metadata field");
        return Ok(2);
    }
    if title.is_none()
        && display_agent.is_none()
        && custom_status.is_none()
        && state_labels.is_empty()
        && !clear_title
        && !clear_display_agent
        && !clear_custom_status
        && !clear_state_labels
    {
        eprintln!("missing metadata field to set or clear");
        return Ok(2);
    }

    super::send_ok_request(Method::PaneReportMetadata(PaneReportMetadataParams {
        pane_id: pane_id.unwrap_or_else(calling_pane_id),
        source,
        agent,
        applies_to_source,
        title,
        display_agent,
        custom_status,
        state_labels,
        clear_title,
        clear_display_agent,
        clear_custom_status,
        clear_state_labels,
        seq,
        ttl_ms,
    }))
}

const SET_FIELD_USAGE: &str =
    "usage: flk pane set-field <key> <value> [--ttl <secs>] [--pane <pane_id>]";
const CLEAR_FIELD_USAGE: &str = "usage: flk pane clear-field <key> [--pane <pane_id>]";
const REPORT_RECAP_USAGE: &str =
    "usage: flk pane report-recap --source ID --agent LABEL --recap TEXT [--seq N] [--pane <pane_id>]";
const REPORT_REPLY_USAGE: &str =
    "usage: flk pane report-reply --source ID --agent LABEL --reply TEXT [--seq N] [--pane <pane_id>]";

/// `flk pane report-recap`: append a recap entry to the calling pane's
/// prompt-history scrollback (#96). The pane defaults to the calling pane,
/// like the other report-* verbs.
fn pane_report_recap(args: &[String]) -> std::io::Result<i32> {
    let mut source = None;
    let mut agent = None;
    let mut recap = None;
    let mut seq = None;
    let mut pane_id = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--source" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --source");
                    return Ok(2);
                };
                source = Some(value.clone());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --agent");
                    return Ok(2);
                };
                agent = Some(value.clone());
                index += 2;
            }
            "--recap" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --recap");
                    return Ok(2);
                };
                recap = Some(value.clone());
                index += 2;
            }
            "--seq" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --seq");
                    return Ok(2);
                };
                match super::parse_u64_flag("--seq", value) {
                    Ok(value) => seq = Some(value),
                    Err(err) => {
                        eprintln!("{err}");
                        return Ok(2);
                    }
                }
                index += 2;
            }
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --pane");
                    return Ok(2);
                };
                pane_id = Some(super::normalize_pane_id(value));
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!("{REPORT_RECAP_USAGE}");
                return Ok(2);
            }
        }
    }

    let Some(source) = source.and_then(|source| {
        let source = source.trim().to_string();
        (!source.is_empty()).then_some(source)
    }) else {
        eprintln!("missing required --source");
        return Ok(2);
    };
    let Some(agent) = agent else {
        eprintln!("missing required --agent");
        return Ok(2);
    };
    let Some(recap) = recap else {
        eprintln!("missing required --recap");
        return Ok(2);
    };
    let pane_id = pane_id.unwrap_or_else(calling_pane_id);

    super::send_ok_request(Method::PaneReportRecap(PaneReportRecapParams {
        pane_id,
        source,
        agent,
        recap,
        seq,
    }))
}

/// `flk pane report-reply`: append an assistant-reply entry to the calling
/// pane's prompt-history scrollback. Wired from the same Stop hook that fires
/// `report-recap` — see `assets/claude/flock-agent-state.sh`.
fn pane_report_reply(args: &[String]) -> std::io::Result<i32> {
    let mut source = None;
    let mut agent = None;
    let mut reply = None;
    let mut seq = None;
    let mut pane_id = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--source" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --source");
                    return Ok(2);
                };
                source = Some(value.clone());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --agent");
                    return Ok(2);
                };
                agent = Some(value.clone());
                index += 2;
            }
            "--reply" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --reply");
                    return Ok(2);
                };
                reply = Some(value.clone());
                index += 2;
            }
            "--seq" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --seq");
                    return Ok(2);
                };
                match super::parse_u64_flag("--seq", value) {
                    Ok(value) => seq = Some(value),
                    Err(err) => {
                        eprintln!("{err}");
                        return Ok(2);
                    }
                }
                index += 2;
            }
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --pane");
                    return Ok(2);
                };
                pane_id = Some(super::normalize_pane_id(value));
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!("{REPORT_REPLY_USAGE}");
                return Ok(2);
            }
        }
    }

    let Some(source) = source.and_then(|source| {
        let source = source.trim().to_string();
        (!source.is_empty()).then_some(source)
    }) else {
        eprintln!("missing required --source");
        return Ok(2);
    };
    let Some(agent) = agent else {
        eprintln!("missing required --agent");
        return Ok(2);
    };
    let Some(reply) = reply else {
        eprintln!("missing required --reply");
        return Ok(2);
    };
    let pane_id = pane_id.unwrap_or_else(calling_pane_id);

    super::send_ok_request(Method::PaneReportReply(PaneReportReplyParams {
        pane_id,
        source,
        agent,
        reply,
        seq,
    }))
}

/// The calling pane's id, like the integration hooks resolve it: the
/// env-baked FLOCK_PANE_ID when present, otherwise an empty claim that the
/// server heals by socket-peer process ancestry.
fn calling_pane_id() -> String {
    std::env::var("FLOCK_PANE_ID").unwrap_or_default()
}

/// Parse the trailing `[--ttl <secs>] [--pane <pane_id>]` options shared by
/// the set-field/clear-field verbs. `allow_ttl` rejects --ttl for
/// clear-field. Returns `(ttl_secs, pane_id)` or an exit code on bad usage.
fn parse_field_options(
    args: &[String],
    usage: &str,
    allow_ttl: bool,
) -> Result<(Option<u64>, String), i32> {
    let mut ttl_secs = None;
    let mut pane_id = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--ttl" if allow_ttl => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --ttl");
                    return Err(2);
                };
                match super::parse_u64_flag("--ttl", value) {
                    Ok(value) => ttl_secs = Some(value),
                    Err(err) => {
                        eprintln!("{err}");
                        return Err(2);
                    }
                }
                index += 2;
            }
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --pane");
                    return Err(2);
                };
                pane_id = Some(super::normalize_pane_id(value));
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                eprintln!("{usage}");
                return Err(2);
            }
        }
    }
    Ok((ttl_secs, pane_id.unwrap_or_else(calling_pane_id)))
}

/// `flk pane set-field <key> <value> [--ttl <secs>]`: promote a
/// session-specific field (container, progress, custom KV) into the calling
/// pane's header. Capped at 6 fields per pane, key <= 16 chars, value <= 48.
fn pane_set_field(args: &[String]) -> std::io::Result<i32> {
    let (Some(key), Some(value)) = (args.first(), args.get(1)) else {
        eprintln!("{SET_FIELD_USAGE}");
        return Ok(2);
    };
    let (ttl_secs, pane_id) = match parse_field_options(&args[2..], SET_FIELD_USAGE, true) {
        Ok(options) => options,
        Err(code) => return Ok(code),
    };

    super::send_ok_request(Method::PaneSetHeaderField(PaneSetHeaderFieldParams {
        pane_id,
        key: key.clone(),
        value: value.clone(),
        ttl_secs,
    }))
}

/// `flk pane clear-field <key>`: remove a promoted header field from the
/// calling pane. Idempotent.
fn pane_clear_field(args: &[String]) -> std::io::Result<i32> {
    let Some(key) = args.first() else {
        eprintln!("{CLEAR_FIELD_USAGE}");
        return Ok(2);
    };
    let (_ttl, pane_id) = match parse_field_options(&args[1..], CLEAR_FIELD_USAGE, false) {
        Ok(options) => options,
        Err(code) => return Ok(code),
    };

    super::send_ok_request(Method::PaneClearHeaderField(PaneClearHeaderFieldParams {
        pane_id,
        key: key.clone(),
    }))
}

fn pane_help_text() -> String {
    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(out, "flk pane commands:");
    let _ = writeln!(out, "  flk pane list [--workspace <workspace_id>]");
    let _ = writeln!(out, "  flk pane get <pane_id>");
    let _ = writeln!(out, "  flk pane rename <pane_id> <label>|--clear");
    let _ = writeln!(out, "  flk pane read <target> [--source visible|recent|recent-unwrapped|detection] [--lines N] [--format text|ansi] [--ansi]");
    let _ = writeln!(
        out,
        "  flk pane split <pane_id> --direction right|down [--cwd PATH] [--focus] [--no-focus]"
    );
    let _ = writeln!(
        out,
        "  flk pane move <pane_id> --tab <tab_id> --split right|down [--target-pane ID] [--ratio FLOAT] [--focus|--no-focus]"
    );
    let _ = writeln!(
        out,
        "  flk pane move <pane_id> --new-tab [--workspace ID] [--label TEXT] [--focus|--no-focus]"
    );
    let _ = writeln!(
        out,
        "  flk pane move <pane_id> --new-workspace [--label TEXT] [--tab-label TEXT] [--focus|--no-focus]"
    );
    let _ = writeln!(out, "  flk pane close <target>");
    let _ = writeln!(out, "  {PANE_SEND_TEXT_USAGE}");
    let _ = writeln!(out, "  flk pane send-keys <target> <key> [key ...]");
    let _ = writeln!(
        out,
        "  read, send-text, send-keys, close: {PANE_TARGET_HELP}"
    );
    let _ = writeln!(
        out,
        "  flk pane arm-self-compact [--pane <pane_id>] [--abort] <handoff prompt>"
    );
    let _ = writeln!(out, "  flk pane report-agent [<pane_id>] --source ID --agent LABEL --state idle|working|blocked|unknown [--pane <pane_id>] [--message TEXT] [--custom-status TEXT] [--seq N] [--agent-session-id ID] [--agent-session-path PATH]");
    let _ = writeln!(out, "  flk pane report-metadata [<pane_id>] [--pane <pane_id>] --source ID [--agent LABEL] [--applies-to-source ID] [--title TEXT|--clear-title] [--display-agent TEXT|--clear-display-agent] [--custom-status TEXT|--clear-custom-status] [--state-label STATUS=TEXT] [--clear-state-labels] [--seq N] [--ttl-ms N]");
    let _ = writeln!(
        out,
        "  flk pane report-recap --source ID --agent LABEL --recap TEXT [--seq N] [--pane <pane_id>]"
    );
    let _ = writeln!(
        out,
        "  flk pane report-reply --source ID --agent LABEL --reply TEXT [--seq N] [--pane <pane_id>]"
    );
    let _ = writeln!(
        out,
        "  flk pane set-field <key> <value> [--ttl <secs>] [--pane <pane_id>]"
    );
    let _ = writeln!(out, "  flk pane clear-field <key> [--pane <pane_id>]");
    let _ = writeln!(out, "  flk pane run <pane_id> <command>");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "set-field promotes a session-specific field (container, progress, custom KV)"
    );
    let _ = writeln!(
        out,
        "into the calling pane's header as a 'key value' chip; --ttl auto-expires it."
    );
    let _ = writeln!(
        out,
        "The pane defaults to the calling pane ($FLOCK_PANE_ID, healed by process"
    );
    let _ = writeln!(
        out,
        "ancestry). Caps: 6 fields per pane, key <= 16 chars, value <= 48 chars."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "report-agent, report-recap and report-reply all take an optional pane id: a leading"
    );
    let _ = writeln!(
        out,
        "positional or --pane. With neither, the report lands on the calling pane."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "report-recap appends a Stop-hook recap to the pane's prompt-history scrollback"
    );
    let _ = writeln!(
        out,
        "(visible in the expanded header panel). report-reply appends the agent's last"
    );
    let _ = writeln!(
        out,
        "assistant message via the same Stop hook; report_prompt is unchanged. All three"
    );
    let _ = writeln!(
        out,
        "feed the same per-pane history ring (cap ~1000 rendered lines, oldest dropped"
    );
    let _ = writeln!(
        out,
        "first), rendered in distinct palette tones so prompt/reply/recap are glanceable."
    );
    out
}

fn print_pane_help() {
    eprint!("{}", pane_help_text());
}

#[cfg(test)]
mod tests {
    use super::{
        pane_help_text, pane_run_steps, parse_report_agent_args, PaneRunStep, ReportAgentArgs,
    };
    use crate::api::schema::PaneAgentState;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    fn working(source: &str) -> ReportAgentArgs {
        ReportAgentArgs {
            pane_id: None,
            source: source.to_string(),
            agent: "zzz".to_string(),
            state: PaneAgentState::Working,
            message: None,
            custom_status: None,
            seq: None,
            agent_session_id: None,
            agent_session_path: None,
        }
    }

    /// #362 (4): `pane run` sent the text and the Enter as one burst, and a
    /// TUI that reads raw stdin cannot tell that burst from a paste — so the
    /// Enter became a newline inside the prompt and the agent never took the
    /// turn. The contract is now three steps, and the middle one is the fix:
    /// the Enter has to be a write of its own, after the pane's reader has
    /// had a chance to come back round.
    ///
    /// The wire-level proof of this lives in `tests/cli_wrapper.rs`, which is
    /// compiled out on macOS. This one runs everywhere, so the ordering is
    /// still pinned on the platform where the other test is not evidence.
    /// The CLI twin of `flock_self_compact`, so an agent reading the `flock`
    /// skill — which is entirely CLI, and hands the agent no MCP tools — can
    /// still compact its own context instead of waiting for a human.
    #[test]
    fn arm_self_compact_defaults_to_my_own_pane_and_keeps_the_whole_prompt() {
        let params = super::parse_arm_self_compact(&argv(&["open", "the", "PR,", "--watch", "CI"]))
            .expect("arms");
        assert_eq!(
            params.pane, None,
            "an agent compacting itself should not have to look up its own id"
        );
        assert_eq!(
            params.continuation.as_deref(),
            Some("open the PR, --watch CI"),
            "like `send-text`, everything from the first non-flag on is the \
             prompt — a flag in there is content, not a flag"
        );
        assert!(!params.abort);
    }

    /// An arming with nothing to resume behind it would shorten the context
    /// and then hand back no thread to hold — strictly worse than not
    /// compacting, so the CLI refuses it before the socket does.
    #[test]
    fn arm_self_compact_refuses_a_missing_or_empty_prompt() {
        for words in [vec![], vec!["", "  "]] {
            let err = super::parse_arm_self_compact(&argv(&words))
                .expect_err("an arming with no prompt is refused");
            assert!(!err.is_empty(), "a refusal must say something");
        }
    }

    #[test]
    fn arm_self_compact_abort_needs_no_prompt() {
        let params = super::parse_arm_self_compact(&argv(&["--abort"])).expect("aborts");
        assert!(params.abort);
        assert_eq!(params.continuation, None);

        let named = super::parse_arm_self_compact(&argv(&["--pane", "w2:p1", "--abort"]))
            .expect("aborts a named pane");
        assert!(named.abort);
        assert_eq!(named.pane.as_deref(), Some("w2:p1"));
    }

    #[test]
    fn pane_run_types_then_settles_then_submits() {
        let steps = pane_run_steps("1:p2", "cargo test");
        assert_eq!(
            steps,
            vec![
                PaneRunStep::Type {
                    pane_id: "1:p2".into(),
                    text: "cargo test".into(),
                },
                PaneRunStep::Settle(super::PANE_RUN_SUBMIT_GAP),
                PaneRunStep::Submit {
                    pane_id: "1:p2".into(),
                },
            ]
        );
    }

    #[test]
    fn the_typed_step_carries_no_enter_of_its_own() {
        // The regression to guard: folding the Enter back into the text step
        // restores the single burst, and every assertion above still passes
        // if the Type step is allowed to carry keys.
        let steps = pane_run_steps("1:p2", "echo hi");
        let PaneRunStep::Type { text, .. } = &steps[0] else {
            panic!("the first step must type the command");
        };
        assert_eq!(text, "echo hi");
        assert!(
            !super::PANE_RUN_SUBMIT_GAP.is_zero(),
            "a zero gap puts the Enter back in the same read as the text"
        );
    }

    #[test]
    fn pane_help_documents_agent_targets_and_pane_id_precedence() {
        let help = super::pane_help_text();
        for verb in ["read", "send-text", "send-keys", "close"] {
            assert!(help.contains(&format!("flk pane {verb} <target>")));
        }
        assert!(help.contains(super::PANE_TARGET_HELP));
        assert!(help.contains("pane id, terminal id, or unique agent name/label"));
        assert!(help.contains("A pane id wins over a same-named agent"));
    }

    #[test]
    fn pane_help_lists_report_recap_and_history_explanation() {
        let help = pane_help_text();
        assert!(
            help.contains("flk pane report-recap --source ID --agent LABEL --recap TEXT"),
            "help should advertise the report-recap subcommand"
        );
        assert!(
            help.contains("flk pane report-reply --source ID --agent LABEL --reply TEXT"),
            "help should advertise the report-reply subcommand"
        );
        assert!(
            help.contains("prompt-history scrollback"),
            "help should explain that report-recap feeds the scrollback"
        );
        // Cap semantics may wrap across lines; check the load-bearing phrase.
        assert!(
            help.contains("~1000 rendered lines"),
            "help should explain the cap semantics"
        );
    }

    /// #454: the pane id used to be a required positional read as `args[0]`,
    /// so a caller that omitted it had its FIRST flag eaten as the pane id —
    /// and the first flag is `--source`. The reporting hook that hit this
    /// wrapped its call in `>/dev/null 2>&1 || true`, so every state
    /// transition became a discard and the orchestrator looked wired up while
    /// observing nothing.
    #[test]
    fn a_leading_source_flag_is_a_flag_and_not_a_pane_id() {
        assert_eq!(
            parse_report_agent_args(&argv(&[
                "--source",
                "ax-dispatch",
                "--agent",
                "zzz",
                "--state",
                "working",
            ])),
            Ok(working("ax-dispatch")),
            "with no pane named, the report targets the calling pane — the flags \
             must survive intact rather than one of them becoming the pane id"
        );
    }

    /// The form `scripts/seed_navigator_demo.sh:90` uses, and the form the
    /// docs show. Breaking it would break the callers that already work.
    /// Note that the integration hook assets do NOT come through here: they
    /// either shell to `flk hook <agent>` (which builds the request in Rust)
    /// or speak the socket directly, so neither parses this argv.
    #[test]
    fn the_legacy_positional_pane_id_still_parses() {
        assert_eq!(
            parse_report_agent_args(&argv(&[
                "p_12",
                "--source",
                "ax-dispatch",
                "--agent",
                "zzz",
                "--state",
                "working",
            ])),
            Ok(ReportAgentArgs {
                pane_id: Some("p_12".into()),
                ..working("ax-dispatch")
            })
        );
    }

    /// `report-recap`/`report-reply` spell the same pane `--pane`; so does
    /// report-agent now, so one reporter can target a pane the same way for
    /// every report verb.
    #[test]
    fn a_pane_flag_names_the_pane_like_the_other_report_verbs() {
        assert_eq!(
            parse_report_agent_args(&argv(&[
                "--pane",
                "p_12",
                "--source",
                "ax-dispatch",
                "--agent",
                "zzz",
                "--state",
                "working",
            ])),
            Ok(ReportAgentArgs {
                pane_id: Some("p_12".into()),
                ..working("ax-dispatch")
            })
        );
    }

    /// A pane id named twice is a caller bug, not a pane to guess at: quietly
    /// preferring one of the two would put the report on a pane the caller did
    /// not mean.
    #[test]
    fn naming_the_pane_twice_is_refused() {
        let err = parse_report_agent_args(&argv(&[
            "p_1", "--pane", "p_2", "--source", "s", "--agent", "a", "--state", "idle",
        ]))
        .expect_err("two panes are ambiguous");
        assert!(
            err.contains("pane"),
            "the refusal must say the pane is the problem: {err}"
        );

        // A second BARE word is not a second pane the parser can diagnose — it
        // reaches the unknown-option arm, which is exactly how `report-recap`
        // and `report-reply` treat a stray word. The verb stays strict, and the
        // message is the sibling one.
        let err = parse_report_agent_args(&argv(&[
            "p_1", "p_2", "--source", "s", "--agent", "a", "--state", "idle",
        ]))
        .expect_err("a second bare word is not a pane");
        assert!(
            err.contains("unknown option"),
            "a stray word is refused by name, as the sibling verbs do: {err}"
        );
    }

    /// #454 follow-up: the fix must not have MOVED the trap rather than closed
    /// it. Accepting the positional only at index 0 is what keeps this true —
    /// with the guard at every position, `--message waiting` set the pane id to
    /// `waiting` and exited 0, where the old parser said `unknown option:
    /// waiting` and exited 2.
    #[test]
    fn a_stray_bare_word_after_a_flag_is_refused_not_eaten_as_the_pane() {
        // A flag's OWN value belongs to that flag wherever it sits: `--message
        // waiting` is a message, never a pane. What must be refused is a bare
        // word no flag claimed — which is exactly what an unquoted multi-word
        // value leaves behind (`--message waiting for the build`).
        assert_eq!(
            parse_report_agent_args(&argv(&[
                "--source",
                "ax",
                "--agent",
                "zzz",
                "--state",
                "working",
                "--message",
                "waiting",
            ])),
            Ok(ReportAgentArgs {
                message: Some("waiting".into()),
                ..working("ax")
            })
        );

        for args in [
            // `--message waiting for the build` with the value unquoted.
            argv(&[
                "--source",
                "ax",
                "--agent",
                "zzz",
                "--state",
                "working",
                "--message",
                "waiting",
                "for",
                "the",
                "build",
            ]),
            // No flag at all claims the trailing word.
            argv(&[
                "--source", "ax", "--agent", "zzz", "--state", "working", "waiting",
            ]),
            // Nor when the pane was named with `--pane`…
            argv(&[
                "--pane", "p_1", "--source", "ax", "--agent", "a", "--state", "idle", "stray",
            ]),
            // …or with the leading positional.
            argv(&[
                "w1:p3", "--source", "ax", "--agent", "a", "--state", "idle", "stray",
            ]),
        ] {
            let err = parse_report_agent_args(&args).expect_err("a stray word must be refused");
            assert!(
                err.contains("unknown option"),
                "a stray word is refused by name, as the sibling verbs do: {err}"
            );
            assert!(
                err.starts_with("unknown option"),
                "and the word it refused leads the message: {err}"
            );
        }
    }

    /// `--pane` with no value, or with a flag where the value belongs. No flock
    /// pane id can begin with `-`, so a flag there is a missing value — and
    /// saying "unexpected argument: x" points at the wrong word entirely.
    #[test]
    fn a_pane_flag_without_a_value_says_so() {
        for args in [
            argv(&["--pane"]),
            argv(&[
                "--pane", "--source", "ax", "--agent", "a", "--state", "idle",
            ]),
        ] {
            let err = parse_report_agent_args(&args).expect_err("--pane needs a pane id");
            assert!(
                err.contains("missing value for --pane"),
                "the refusal must name the option that has no value: {err}"
            );
        }
    }

    /// An unrecognised flag is refused by name, and never read as the pane id —
    /// the one shape that made this failure swallowable.
    #[test]
    fn an_unknown_option_is_refused_by_name() {
        let err = parse_report_agent_args(&argv(&[
            "--sauce",
            "--source",
            "ax-dispatch",
            "--agent",
            "zzz",
            "--state",
            "working",
        ]))
        .expect_err("an unknown option must be refused");
        assert!(err.contains("--sauce"), "the refusal names the flag: {err}");
    }

    #[test]
    fn a_missing_value_or_a_blank_source_is_refused_before_the_socket() {
        for (args, expected) in [
            (argv(&["--source", "ax", "--agent", "zzz"]), "--state"),
            (argv(&["--source"]), "--source"),
            (
                argv(&["--source", "   ", "--agent", "zzz", "--state", "working"]),
                "missing required --source",
            ),
            (
                argv(&["--source", "ax", "--state", "working"]),
                "missing required --agent",
            ),
        ] {
            let err = parse_report_agent_args(&args).expect_err("this must not reach the socket");
            assert!(err.contains(expected), "expected {expected:?}, got {err:?}");
        }
    }

    #[test]
    fn an_invalid_state_is_refused_by_name() {
        let err = parse_report_agent_args(&argv(&[
            "--source",
            "ax",
            "--agent",
            "zzz",
            "--state",
            "pondering",
        ]))
        .expect_err("an unknown state must be refused");
        assert!(
            err.contains("pondering"),
            "the refusal names the value: {err}"
        );
    }

    #[test]
    fn help_advertises_an_optional_pane_for_report_agent() {
        let help = pane_help_text();
        assert!(
            help.contains("flk pane report-agent [<pane_id>] --source ID"),
            "help must show the pane id as optional, or the next reader assumes \
             it is still required: {help}"
        );
        assert!(
            help.contains(
                "report-agent, report-recap and report-reply all take an optional pane id"
            ),
            "help should say the reporting verbs agree on how a pane is supplied"
        );
    }
}
