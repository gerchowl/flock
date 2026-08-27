#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::{
    AgentForkParams, AgentReadParams, AgentRenameParams, AgentSendParams, AgentStartParams,
    AgentStatus, AgentTarget, EmptyParams, Method, ReadFormat, ReadSource, Request, Subscription,
};

pub(super) fn run_agent_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_agent_help();
        return Ok(2);
    };

    match subcommand {
        "list" => agent_list(&args[1..]),
        "get" => agent_get(&args[1..]),
        "read" => agent_read(&args[1..]),
        "send" => agent_send(&args[1..]),
        "rename" => agent_rename(&args[1..]),
        "focus" => agent_focus(&args[1..]),
        "wait" => agent_wait(&args[1..]),
        "attach" => agent_attach(&args[1..]),
        "start" => agent_start(&args[1..]),
        "fork" => agent_fork(&args[1..]),
        "hibernate" => agent_hibernate(&args[1..]),
        "resume" => agent_resume(&args[1..]),
        "help" | "--help" | "-h" => {
            print_agent_help();
            Ok(0)
        }
        _ => {
            print_agent_help();
            Ok(2)
        }
    }
}

const AGENT_START_USAGE: &str = "flk agent start <name> [--cwd PATH] [--workspace ID] [--tab ID] [--split right|down] [--focus|--no-focus] [--wait-ready [--ready-timeout MS]] -- <argv...>";

fn agent_start(args: &[String]) -> std::io::Result<i32> {
    let Some(name) = args.first() else {
        eprintln!("usage: {AGENT_START_USAGE}");
        return Ok(2);
    };

    let Some(separator) = args.iter().position(|arg| arg == "--") else {
        eprintln!("usage: {AGENT_START_USAGE}");
        return Ok(2);
    };
    if separator == args.len() - 1 {
        eprintln!("agent start requires argv after --");
        return Ok(2);
    }

    let mut cwd = None;
    let mut workspace_id = None;
    let mut tab_id = None;
    let mut split = None;
    let mut focus = false;
    let mut wait_ready = false;
    let mut ready_timeout_ms = None;

    let mut index = 1;
    while index < separator {
        match args[index].as_str() {
            "--cwd" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --cwd");
                    return Ok(2);
                };
                cwd = Some(value.clone());
                index += 2;
            }
            "--workspace" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --workspace");
                    return Ok(2);
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            "--tab" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --tab");
                    return Ok(2);
                };
                tab_id = Some(super::normalize_tab_id(value));
                index += 2;
            }
            "--split" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --split");
                    return Ok(2);
                };
                split = Some(super::parse_split_direction(value)?);
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
            "--wait-ready" => {
                wait_ready = true;
                index += 1;
            }
            "--ready-timeout" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --ready-timeout");
                    return Ok(2);
                };
                ready_timeout_ms = Some(super::parse_u64_flag("--ready-timeout", value)?);
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    if ready_timeout_ms.is_some() && !wait_ready {
        eprintln!("--ready-timeout only means something with --wait-ready");
        return Ok(2);
    }

    let response = super::send_request(&Request {
        id: "cli:agent:start".into(),
        method: Method::AgentStart(AgentStartParams {
            name: name.clone(),
            cwd,
            workspace_id,
            tab_id,
            split,
            focus,
            argv: args[separator + 1..].to_vec(),
        }),
    })?;
    if !wait_ready || response.get("error").is_some() {
        return super::print_response(&response);
    }

    // A start answers as soon as the child has exec'd and outlived the
    // liveness window (#178), which says nothing about the TUI. Everything
    // below is the second question: is it actually up?
    let Some(pane_id) = response["result"]["agent"]["pane_id"].as_str() else {
        eprintln!("agent start failed: response did not include pane_id");
        return Ok(1);
    };
    super::ready::wait_until_ready(
        name,
        pane_id,
        ready_timeout_ms.unwrap_or(super::ready::DEFAULT_READY_TIMEOUT_MS),
    )
}

/// Fork the target pane's agent conversation into a new linked worktree
/// (#175 F1). `--pivot ""` / `--no-pivot` opt out of the configured seed
/// prompt; omitting the flag uses the `worktrees.branch_pivot_message`
/// template with `<branch>` resolved server-side.
fn agent_fork(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str = "usage: flk agent fork <target> [--branch NAME] [--base REF] [--path PATH] [--label LABEL] [--pivot TEXT|--no-pivot] [--focus|--no-focus]";
    let Some(target) = args.first() else {
        eprintln!("{USAGE}");
        return Ok(2);
    };

    let mut branch = None;
    let mut base = None;
    let mut path = None;
    let mut label = None;
    let mut pivot = None;
    let mut focus = false;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--branch" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --branch");
                    return Ok(2);
                };
                branch = Some(value.clone());
                index += 2;
            }
            "--base" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --base");
                    return Ok(2);
                };
                base = Some(value.clone());
                index += 2;
            }
            "--path" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --path");
                    return Ok(2);
                };
                path = Some(value.clone());
                index += 2;
            }
            "--label" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --label");
                    return Ok(2);
                };
                label = Some(value.clone());
                index += 2;
            }
            "--pivot" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --pivot");
                    return Ok(2);
                };
                pivot = Some(value.clone());
                index += 2;
            }
            "--no-pivot" => {
                pivot = Some(String::new());
                index += 1;
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

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:fork".into(),
        method: Method::AgentFork(AgentForkParams {
            target: target.clone(),
            branch,
            base,
            path,
            label,
            pivot,
            focus,
        }),
    })?)
}

/// `flk agent hibernate <target>` — park the agent pane (#175 C3).
/// Mirrors the socket verb; refusals surface with the wire's code+message.
fn agent_hibernate(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: flk agent hibernate <target>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: flk agent hibernate <target>");
        return Ok(2);
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:hibernate".into(),
        method: Method::AgentHibernate(AgentTarget {
            target: target.clone(),
        }),
    })?)
}

/// `flk agent resume <target>` — spawn the hibernated pane's argv back
/// into the same terminal (#175 C3).
fn agent_resume(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: flk agent resume <target>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: flk agent resume <target>");
        return Ok(2);
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:resume".into(),
        method: Method::AgentResume(AgentTarget {
            target: target.clone(),
        }),
    })?)
}

fn agent_list(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: flk agent list");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:list".into(),
        method: Method::AgentList(EmptyParams::default()),
    })?)
}

fn agent_get(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: flk agent get <target>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: flk agent get <target>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:get".into(),
        method: Method::AgentGet(AgentTarget {
            target: target.clone(),
        }),
    })?)
}

fn agent_focus(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: flk agent focus <target>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: flk agent focus <target>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:focus".into(),
        method: Method::AgentFocus(AgentTarget {
            target: target.clone(),
        }),
    })?)
}

fn agent_attach(args: &[String]) -> std::io::Result<i32> {
    let (target, takeover) =
        match super::parse_attach_target(args, "usage: flk agent attach <target> [--takeover]") {
            Ok(parsed) => parsed,
            Err(code) => return Ok(code),
        };

    let response = resolve_agent_target(&target, "cli:agent:attach:resolve")?;
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap());
        return Ok(1);
    }
    let Some(terminal_id) = response["result"]["agent"]["terminal_id"].as_str() else {
        eprintln!("agent attach failed: response did not include terminal_id");
        return Ok(1);
    };
    crate::client::run_terminal_attach(terminal_id.to_owned(), takeover)?;
    Ok(0)
}

const AGENT_WAIT_USAGE: &str =
    "flk agent wait <target> --status <idle|working|blocked|unknown> | --ready [--timeout MS]";

fn agent_wait(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: {AGENT_WAIT_USAGE}");
        return Ok(2);
    };

    let mut timeout_ms = None;
    let mut desired_status = None;
    let mut ready = false;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--ready" => {
                ready = true;
                index += 1;
            }
            "--status" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --status");
                    return Ok(2);
                };
                desired_status = Some(parse_agent_wait_status(value)?);
                index += 2;
            }
            "--timeout" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --timeout");
                    return Ok(2);
                };
                timeout_ms = Some(super::parse_u64_flag("--timeout", value)?);
                index += 2;
            }
            "help" | "--help" | "-h" => {
                eprintln!("usage: {AGENT_WAIT_USAGE}");
                return Ok(0);
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    // `--ready` and `--status` are different questions, and a caller who asks
    // both has not decided which one they mean: `--status idle` on a pane that
    // came up `blocked` waits out the whole timeout, which is the failure
    // `--ready` exists to end.
    if ready && desired_status.is_some() {
        eprintln!("--ready and --status ask different questions; pass one");
        return Ok(2);
    }
    if ready {
        let response = resolve_agent_target(target, "cli:agent:wait:resolve")?;
        if response.get("error").is_some() {
            eprintln!("{}", serde_json::to_string(&response).unwrap());
            return Ok(1);
        }
        let Some(pane_id) = response["result"]["agent"]["pane_id"].as_str() else {
            eprintln!("agent wait failed: response did not include pane_id");
            return Ok(1);
        };
        return super::ready::wait_until_ready(
            target,
            pane_id,
            timeout_ms.unwrap_or(super::ready::DEFAULT_READY_TIMEOUT_MS),
        );
    }

    let Some(agent_status) = desired_status else {
        eprintln!("missing required --status or --ready");
        return Ok(2);
    };

    let response = resolve_agent_target(target, "cli:agent:wait:resolve")?;
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap());
        return Ok(1);
    }
    if response["result"]["agent"]["agent_status"]
        .as_str()
        .is_some_and(|current| agent_wait_status_satisfied(agent_status, current))
    {
        println!("{}", serde_json::to_string(&response).unwrap());
        return Ok(0);
    }

    let Some(pane_id) = response["result"]["agent"]["pane_id"].as_str() else {
        eprintln!("agent wait failed: response did not include pane_id");
        return Ok(1);
    };

    let subscriptions = if agent_status == AgentStatus::Idle {
        vec![
            Subscription::PaneAgentStatusChanged {
                pane_id: pane_id.to_owned(),
                agent_status: Some(AgentStatus::Idle),
            },
            Subscription::PaneAgentStatusChanged {
                pane_id: pane_id.to_owned(),
                agent_status: Some(AgentStatus::Done),
            },
        ]
    } else {
        vec![Subscription::PaneAgentStatusChanged {
            pane_id: pane_id.to_owned(),
            agent_status: Some(agent_status),
        }]
    };

    super::wait_for_agent_change(
        Request {
            id: "cli:agent:wait".into(),
            method: Method::EventsSubscribe(crate::api::schema::EventsSubscribeParams {
                subscriptions,
            }),
        },
        timeout_ms,
        "timed out waiting for agent status change",
    )
}

fn resolve_agent_target(target: &str, request_id: &str) -> std::io::Result<serde_json::Value> {
    super::send_request(&Request {
        id: request_id.into(),
        method: Method::AgentGet(AgentTarget {
            target: target.to_owned(),
        }),
    })
}

fn agent_rename(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: flk agent rename <target> <name>|--clear");
        return Ok(2);
    };
    if args.len() < 2 {
        eprintln!("usage: flk agent rename <target> <name>|--clear");
        return Ok(2);
    }
    let name = if args.len() == 2 && args[1] == "--clear" {
        None
    } else {
        Some(args[1..].join(" "))
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:rename".into(),
        method: Method::AgentRename(AgentRenameParams {
            target: target.clone(),
            name,
        }),
    })?)
}

fn agent_send(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: flk agent send <target> <text>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:send".into(),
        method: Method::AgentSend(AgentSendParams {
            target: args[0].clone(),
            text: args[1..].join(" "),
        }),
    })?)
}

fn agent_read(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: flk agent read <target> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]");
        return Ok(2);
    };

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
                strip_ansi = !matches!(format, ReadFormat::Ansi);
                index += 2;
            }
            "--ansi" => {
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

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:read".into(),
        method: Method::AgentRead(AgentReadParams {
            target: target.clone(),
            source,
            lines,
            format,
            strip_ansi,
        }),
    })?)
}

fn agent_wait_status_satisfied(desired: AgentStatus, current: &str) -> bool {
    match desired {
        AgentStatus::Idle => matches!(current, "idle" | "done"),
        AgentStatus::Working => current == "working",
        AgentStatus::Blocked => current == "blocked",
        AgentStatus::Unknown => current == "unknown",
        AgentStatus::Done => false,
        // #175 C3: `hibernated` is a valid wait target so operators can
        // block until a pane finishes going away.
        AgentStatus::Hibernated => current == "hibernated",
    }
}

fn parse_agent_wait_status(value: &str) -> std::io::Result<AgentStatus> {
    match value {
        "idle" => Ok(AgentStatus::Idle),
        "working" => Ok(AgentStatus::Working),
        "blocked" => Ok(AgentStatus::Blocked),
        "unknown" => Ok(AgentStatus::Unknown),
        "done" => Err(std::io::Error::other(
            "done is a UI attention state; use idle for CLI agent completion waits",
        )),
        _ => Err(std::io::Error::other(format!(
            "invalid agent status: {value} (expected idle, working, blocked, or unknown)"
        ))),
    }
}

fn print_agent_help() {
    eprintln!("flk agent commands:");
    eprintln!("  flk agent list");
    eprintln!("  flk agent get <target>");
    eprintln!("  flk agent read <target> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]");
    eprintln!("  flk agent send <target> <text>");
    eprintln!("  flk agent rename <target> <name>|--clear");
    eprintln!("  flk agent focus <target>");
    eprintln!("  {AGENT_WAIT_USAGE}");
    eprintln!("  flk agent attach <target> [--takeover]");
    eprintln!("  {AGENT_START_USAGE}");
    eprintln!("  flk agent fork <target> [--branch NAME] [--base REF] [--path PATH] [--label LABEL] [--pivot TEXT|--no-pivot] [--focus|--no-focus]");
    eprintln!("  flk agent hibernate <target>");
    eprintln!("  flk agent resume <target>");
    eprintln!("  agent start without --cwd starts in the targeted workspace's checkout; with no target, in the server's cwd");
    eprintln!("  targets accept terminal ids, unique agent names, detected/reported agent labels, and legacy pane ids");
    eprintln!(
        "  agent send writes literal text; use pane run when you want command text plus Enter"
    );
    eprintln!("  --ready / --wait-ready block until the pane reports a status other than unknown:");
    eprintln!("    a TUI that has not painted yet is unknown, so ready is the first moment idle,");
    eprintln!("    working or blocked is a real answer rather than flock not being able to tell");
}
