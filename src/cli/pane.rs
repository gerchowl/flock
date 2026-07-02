#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::{
    Method, PaneClearHeaderFieldParams, PaneListParams, PaneMoveDestination, PaneMoveParams,
    PaneReadParams, PaneRenameParams, PaneReportAgentParams, PaneReportMetadataParams,
    PaneReportRecapParams, PaneReportReplyParams, PaneSendInputParams, PaneSendKeysParams,
    PaneSendTextParams, PaneSetHeaderFieldParams, PaneSplitParams, PaneTarget, ReadFormat,
    ReadSource, Request, SplitDirection,
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
        eprintln!("usage: flock pane get <pane_id>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: flock pane get <pane_id>");
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
        eprintln!("usage: flock pane rename <pane_id> <label>|--clear");
        return Ok(2);
    };
    if args.len() < 2 {
        eprintln!("usage: flock pane rename <pane_id> <label>|--clear");
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

fn pane_read(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_pane_id) = args.first() else {
        eprintln!("usage: flock pane read <pane_id> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]");
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
            "usage: flock pane split <pane_id> --direction right|down [--cwd PATH] [--focus] [--no-focus]"
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
    "usage: flock pane move <pane_id> --tab <tab_id> --split right|down [--target-pane ID] [--ratio FLOAT] [--focus|--no-focus]\n       flock pane move <pane_id> --new-tab [--workspace ID] [--label TEXT] [--focus|--no-focus]\n       flock pane move <pane_id> --new-workspace [--label TEXT] [--tab-label TEXT] [--focus|--no-focus]"
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
        eprintln!("usage: flock pane close <pane_id>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: flock pane close <pane_id>");
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
        eprintln!("usage: flock pane send-text <pane_id> <text>");
        return Ok(2);
    }

    let pane_id = super::normalize_pane_id(&args[0]);
    let text = args[1..].join(" ");
    super::send_ok_request(Method::PaneSendText(PaneSendTextParams { pane_id, text }))
}

fn pane_send_keys(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: flock pane send-keys <pane_id> <key> [key ...]");
        return Ok(2);
    }

    let pane_id = super::normalize_pane_id(&args[0]);
    let keys = args[1..].to_vec();
    super::send_ok_request(Method::PaneSendKeys(PaneSendKeysParams { pane_id, keys }))
}

fn pane_run(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: flock pane run <pane_id> <command>");
        return Ok(2);
    }

    let pane_id = super::normalize_pane_id(&args[0]);
    let text = args[1..].join(" ");
    super::send_ok_request(Method::PaneSendInput(PaneSendInputParams {
        pane_id,
        text,
        keys: vec!["Enter".into()],
    }))
}

fn pane_report_agent(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_pane_id) = args.first() else {
        eprintln!("usage: flock pane report-agent <pane_id> --source ID --agent LABEL --state idle|working|blocked|unknown [--message TEXT] [--custom-status TEXT] [--seq N] [--agent-session-id ID] [--agent-session-path PATH]");
        return Ok(2);
    };

    let pane_id = super::normalize_pane_id(raw_pane_id);
    let mut source = None;
    let mut agent = None;
    let mut state = None;
    let mut message = None;
    let mut custom_status = None;
    let mut seq = None;
    let mut agent_session_id = None;
    let mut agent_session_path = None;

    let mut index = 1;
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
            "--state" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --state");
                    return Ok(2);
                };
                state = Some(super::parse_pane_agent_state(value)?);
                index += 2;
            }
            "--message" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --message");
                    return Ok(2);
                };
                message = Some(value.clone());
                index += 2;
            }
            "--custom-status" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --custom-status");
                    return Ok(2);
                };
                custom_status = Some(value.clone());
                index += 2;
            }
            "--seq" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --seq");
                    return Ok(2);
                };
                seq = Some(super::parse_u64_flag("--seq", value)?);
                index += 2;
            }
            "--agent-session-id" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --agent-session-id");
                    return Ok(2);
                };
                agent_session_id = Some(value.clone());
                index += 2;
            }
            "--agent-session-path" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --agent-session-path");
                    return Ok(2);
                };
                agent_session_path = Some(value.clone());
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
    let Some(agent) = agent else {
        eprintln!("missing required --agent");
        return Ok(2);
    };
    let Some(state) = state else {
        eprintln!("missing required --state");
        return Ok(2);
    };

    super::send_ok_request(Method::PaneReportAgent(PaneReportAgentParams {
        pane_id,
        source,
        agent,
        state,
        message,
        custom_status,
        seq,
        agent_session_id,
        agent_session_path,
    }))
}

fn pane_report_metadata(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_pane_id) = args.first() else {
        eprintln!("usage: flock pane report-metadata <pane_id> --source ID [--agent LABEL] [--applies-to-source ID] [--title TEXT|--clear-title] [--display-agent TEXT|--clear-display-agent] [--custom-status TEXT|--clear-custom-status] [--state-label STATUS=TEXT] [--clear-state-labels] [--seq N] [--ttl-ms N]");
        return Ok(2);
    };

    let pane_id = super::normalize_pane_id(raw_pane_id);
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

    let mut index = 1;
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
        pane_id,
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
    "usage: flock pane set-field <key> <value> [--ttl <secs>] [--pane <pane_id>]";
const CLEAR_FIELD_USAGE: &str = "usage: flock pane clear-field <key> [--pane <pane_id>]";
const REPORT_RECAP_USAGE: &str =
    "usage: flock pane report-recap --source ID --agent LABEL --recap TEXT [--seq N] [--pane <pane_id>]";
const REPORT_REPLY_USAGE: &str =
    "usage: flock pane report-reply --source ID --agent LABEL --reply TEXT [--seq N] [--pane <pane_id>]";

/// `flock pane report-recap`: append a recap entry to the calling pane's
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

/// `flock pane report-reply`: append an assistant-reply entry to the calling
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

/// `flock pane set-field <key> <value> [--ttl <secs>]`: promote a
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

/// `flock pane clear-field <key>`: remove a promoted header field from the
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
    let _ = writeln!(out, "flock pane commands:");
    let _ = writeln!(out, "  flock pane list [--workspace <workspace_id>]");
    let _ = writeln!(out, "  flock pane get <pane_id>");
    let _ = writeln!(out, "  flock pane rename <pane_id> <label>|--clear");
    let _ = writeln!(out, "  flock pane read <pane_id> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]");
    let _ = writeln!(
        out,
        "  flock pane split <pane_id> --direction right|down [--cwd PATH] [--focus] [--no-focus]"
    );
    let _ = writeln!(
        out,
        "  flock pane move <pane_id> --tab <tab_id> --split right|down [--target-pane ID] [--ratio FLOAT] [--focus|--no-focus]"
    );
    let _ = writeln!(
        out,
        "  flock pane move <pane_id> --new-tab [--workspace ID] [--label TEXT] [--focus|--no-focus]"
    );
    let _ = writeln!(
        out,
        "  flock pane move <pane_id> --new-workspace [--label TEXT] [--tab-label TEXT] [--focus|--no-focus]"
    );
    let _ = writeln!(out, "  flock pane close <pane_id>");
    let _ = writeln!(out, "  flock pane send-text <pane_id> <text>");
    let _ = writeln!(out, "  flock pane send-keys <pane_id> <key> [key ...]");
    let _ = writeln!(out, "  flock pane report-agent <pane_id> --source ID --agent LABEL --state idle|working|blocked|unknown [--message TEXT] [--custom-status TEXT] [--seq N] [--agent-session-id ID] [--agent-session-path PATH]");
    let _ = writeln!(out, "  flock pane report-metadata <pane_id> --source ID [--agent LABEL] [--applies-to-source ID] [--title TEXT|--clear-title] [--display-agent TEXT|--clear-display-agent] [--custom-status TEXT|--clear-custom-status] [--state-label STATUS=TEXT] [--clear-state-labels] [--seq N] [--ttl-ms N]");
    let _ = writeln!(
        out,
        "  flock pane report-recap --source ID --agent LABEL --recap TEXT [--seq N] [--pane <pane_id>]"
    );
    let _ = writeln!(
        out,
        "  flock pane report-reply --source ID --agent LABEL --reply TEXT [--seq N] [--pane <pane_id>]"
    );
    let _ = writeln!(
        out,
        "  flock pane set-field <key> <value> [--ttl <secs>] [--pane <pane_id>]"
    );
    let _ = writeln!(out, "  flock pane clear-field <key> [--pane <pane_id>]");
    let _ = writeln!(out, "  flock pane run <pane_id> <command>");
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
    use super::pane_help_text;

    #[test]
    fn pane_help_lists_report_recap_and_history_explanation() {
        let help = pane_help_text();
        assert!(
            help.contains("flock pane report-recap --source ID --agent LABEL --recap TEXT"),
            "help should advertise the report-recap subcommand"
        );
        assert!(
            help.contains("flock pane report-reply --source ID --agent LABEL --reply TEXT"),
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
}
