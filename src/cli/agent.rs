#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use super::wait_status_vocab;
use crate::api::schema::{
    AgentForkParams, AgentReadParams, AgentRenameParams, AgentSendParams, AgentStartParams,
    AgentTarget, EmptyParams, Method, ReadFormat, ReadSource, Request, SplitDirection,
    Subscription,
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
        "result" => agent_result(&args[1..]),
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

/// The usage lines live here rather than inside the parsers that print them,
/// because `cli::help` answers `flk agent <verb> --help` from the same
/// constants (#455) — one answer per verb, not two that can disagree.
pub(super) const AGENT_RESULT_USAGE: &str =
    "flk agent result <target> [--max-chars N] [--offset N]";
pub(super) const AGENT_START_USAGE: &str = "flk agent start <name> [--cwd PATH] [--workspace ID] [--tab ID] [--active|--here] [--split right|down] [--focus|--no-focus] [--wait-ready [--ready-timeout MS]] -- <argv...>";

pub(super) const AGENT_FORK_USAGE: &str = "flk agent fork <target> [--branch NAME] [--base REF] [--path PATH] [--label LABEL] [--pivot TEXT|--no-pivot] [--focus|--no-focus]";

pub(super) const AGENT_WAIT_USAGE: &str = concat!(
    "flk agent wait <target> --status <",
    wait_status_vocab!(),
    "> [--after CURSOR] [--settle MS] [--timeout MS] | --ready [--timeout MS]\n",
    "  idle     ready for input: an agent that is idle, or done — done means it went quiet while unseen\n",
    "  done     exactly done: idle, plus a finished pane you have not looked at\n",
    "  settled  observed quiescence: after --after, the server saw this agent enter working and then\n",
    "           saw its status hold idle/done with NO state transition at all for --settle\n",
    "  --after CURSOR  a turn_cursor from `flk agent get`, captured BEFORE you prompted.\n",
    "           Without it any quiet counts: an agent already idle settles after one settle window.\n",
    "  --settle MS  how long that quiet must hold. Default 5000; --settle 0 settles on the sample\n",
    "           after the first qualifying one.\n",
    "  --timeout MS  give up after this long. Absent means no deadline: only a server that stays\n",
    "           unreachable for 30 s ends the wait (exit 1), so this never waits on a dead server.\n",
    "  prints one JSON line on stdout:\n",
    "           {\"status\":\"settled\"|\"blocked\"|\"gone\",\"pane_id\":…,\"agent_status\":…,\n",
    "            \"turn_cursor\":…,\"held_ms\":N}\n",
    "           a gone line adds \"reason\":\"closed\"|\"hibernated\"|\"restarted\".\n",
    "  exit 0 settled · 3 blocked · 4 gone · 124 timeout · 2 usage error or refused cursor\n",
    "           · 1 every other error, including a server unreachable for 30 s. A timeout is 1 for\n",
    "           the non-settled statuses above.\n",
    "  settled is what flock OBSERVED, not proof that a turn produced a result: for the turn's\n",
    "  output use `flk agent result` (once #575 lands). Native agents are read from the screen every\n",
    "  300-500 ms, so a working phase shorter than one sample is never observed — a cursor taken\n",
    "  before one waits for the next turn instead. A turn cursor never satisfies a wait on another\n",
    "  terminal or another execution.",
);

/// Everything `agent start` accepts between the name and the `--` terminator.
///
/// A type rather than a pile of `Option`s out-param'd into the request, and a
/// pure function over `args`, so the flags can be pinned by tests that do not
/// need a server: this parser decides what a caller said, and the placement
/// rules on the other end of the socket are worth nothing if the flag that opts
/// into them never reaches the request.
#[derive(Debug, Default, PartialEq, Eq)]
struct AgentStartFlags {
    cwd: Option<String>,
    workspace_id: Option<String>,
    tab_id: Option<String>,
    /// #398: ask for the workspace you are LOOKING AT — a recollection the
    /// server reads from `state.active`.
    active: bool,
    /// #398: ask for the workspace your own PANE is in — a locality the server
    /// reads from the caller's process ancestry. Two different questions, so
    /// two different flags; `parse_agent_start_flags` keeps them apart rather
    /// than aliasing one for the other.
    here: bool,
    split: Option<SplitDirection>,
    focus: bool,
    wait_ready: bool,
    ready_timeout_ms: Option<u64>,
}

/// Parse the flags before `--`. `separator` is the index of the terminator, so
/// a value that happens to be `--` cannot be swallowed as one.
///
/// The error is the line to print: every refusal here is a usage error, so each
/// one carries the whole remedy in the words rather than sending the caller to
/// `--help` for a flag that exists.
fn parse_agent_start_flags(args: &[String], separator: usize) -> Result<AgentStartFlags, String> {
    let mut flags = AgentStartFlags::default();

    let mut index = 1;
    while index < separator {
        // Every value-taking flag reads the next word, and only if it is still
        // on this side of `--`.
        let value_for = |flag: &str| -> Result<String, String> {
            args.get(index + 1)
                .filter(|_| index + 1 < separator)
                .cloned()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match args[index].as_str() {
            "--cwd" => {
                flags.cwd = Some(value_for("--cwd")?);
                index += 2;
            }
            "--workspace" => {
                flags.workspace_id =
                    Some(super::normalize_workspace_id(&value_for("--workspace")?));
                index += 2;
            }
            "--tab" => {
                flags.tab_id = Some(super::normalize_tab_id(&value_for("--tab")?));
                index += 2;
            }
            // #398. Not an alias pair: `--active` is "the workspace you are
            // looking at", which the server remembers, and `--here` is "the
            // workspace your own pane is in", which the server attests from
            // the caller's process ancestry. The second was originally spelled
            // as an alias of the first and promised a locality the code did
            // not implement; for a caller in a pane of a background space those
            // are different workspaces, which is the #398 defect with the
            // exemption bolted on.
            "--active" => {
                flags.active = true;
                index += 1;
            }
            "--here" => {
                flags.here = true;
                index += 1;
            }
            "--split" => {
                flags.split = Some(
                    super::parse_split_direction(&value_for("--split")?)
                        .map_err(|err| err.to_string())?,
                );
                index += 2;
            }
            "--focus" => {
                flags.focus = true;
                index += 1;
            }
            "--no-focus" => {
                flags.focus = false;
                index += 1;
            }
            "--wait-ready" => {
                flags.wait_ready = true;
                index += 1;
            }
            "--ready-timeout" => {
                let value = value_for("--ready-timeout")?;
                flags.ready_timeout_ms = Some(
                    super::parse_u64_flag("--ready-timeout", &value)
                        .map_err(|err| err.to_string())?,
                );
                index += 2;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    if flags.ready_timeout_ms.is_some() && !flags.wait_ready {
        return Err("--ready-timeout only means something with --wait-ready".into());
    }
    Ok(flags)
}

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

    let flags = match parse_agent_start_flags(args, separator) {
        Ok(flags) => flags,
        Err(reason) => {
            eprintln!("{reason}");
            return Ok(2);
        }
    };
    let AgentStartFlags {
        cwd,
        workspace_id,
        tab_id,
        active,
        here,
        split,
        focus,
        wait_ready,
        ready_timeout_ms,
    } = flags;

    let response = super::send_request(&Request {
        id: "cli:agent:start".into(),
        method: Method::AgentStart(AgentStartParams {
            name: name.clone(),
            cwd,
            workspace_id,
            tab_id,
            split,
            active,
            here,
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
    let Some(target) = args.first() else {
        eprintln!("usage: {AGENT_FORK_USAGE}");
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

fn agent_wait(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: {AGENT_WAIT_USAGE}");
        return Ok(2);
    };

    let mut raw_timeout: Option<String> = None;
    let mut desired_status = None;
    let mut ready = false;
    let mut settle_flags = super::settled::SettleFlags::default();

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
                desired_status = Some(match super::settled::WaitTarget::parse(value) {
                    Ok(target) => target,
                    Err(reason) => {
                        eprintln!("{reason}");
                        return Ok(2);
                    }
                });
                index += 2;
            }
            "--timeout" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --timeout");
                    return Ok(2);
                };
                // Kept raw until the target is known: with `--status settled` a
                // bad value is a usage error (exit 2, before anything is
                // waited on), and the other statuses keep their historical
                // behaviour of reporting it as an io error.
                raw_timeout = Some(value.clone());
                index += 2;
            }
            "--after" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --after");
                    return Ok(2);
                };
                settle_flags.after = Some(value.clone());
                index += 2;
            }
            "--settle" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --settle");
                    return Ok(2);
                };
                match value.parse::<u64>() {
                    Ok(settle_ms) => settle_flags.settle_ms = Some(settle_ms),
                    // A usage error, not a request error: this is a typo, and it
                    // has to say so before anything is waited on.
                    Err(_) => {
                        eprintln!("invalid value for --settle: {value} (expected milliseconds)");
                        return Ok(2);
                    }
                }
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
    // `--after` and `--settle` are questions only a settle can answer. A `--ready`
    // wait with either is the same refusal as a plain-status wait with either:
    // the flags name a turn that this mode never looks at.
    if let Err(reason) = super::settled::check_settle_flags(desired_status, &settle_flags) {
        eprintln!("{reason}");
        return Ok(2);
    }
    // Resolved here rather than in the parse loop because what a bad `--timeout`
    // means depends on the target: with `--status settled` it is a usage error
    // (exit 2, before anything is waited on), and every other status keeps its
    // historical behaviour of reporting it as an io error.
    let settled = desired_status == Some(super::settled::WaitTarget::Settled);
    let timeout_ms = match (raw_timeout, settled) {
        (None, _) => None,
        (Some(raw), false) => Some(super::parse_u64_flag("--timeout", &raw)?),
        (Some(raw), true) => Some(match raw.parse::<u64>() {
            Ok(ms) => ms,
            Err(_) => {
                eprintln!("invalid value for --timeout: {raw} (expected milliseconds)");
                return Ok(2);
            }
        }),
    };
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

    let Some(target_status) = desired_status else {
        eprintln!("missing required --status or --ready");
        return Ok(2);
    };

    if target_status == super::settled::WaitTarget::Settled {
        return super::settled::run_settled_wait(
            "agent wait",
            super::settled::InitialTarget::Agent(target),
            settle_flags.after.as_deref(),
            settle_flags
                .settle_ms
                .unwrap_or(super::settled::DEFAULT_SETTLE_MS),
            timeout_ms,
        );
    }

    let response = resolve_agent_target(target, "cli:agent:wait:resolve")?;
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap());
        return Ok(1);
    }
    let Some(pane_id) = response["result"]["agent"]["pane_id"].as_str() else {
        eprintln!("agent wait failed: response did not include pane_id");
        return Ok(1);
    };
    if response["result"]["agent"]["agent_status"]
        .as_str()
        .is_some_and(|current| target_status.satisfied_by(current))
    {
        println!("{}", serde_json::to_string(&response).unwrap());
        return Ok(0);
    }

    // `idle` is "ready for input", so it watches `done` as well: an unattended
    // agent goes quiet as `done`, which is an effective idle (#553).
    let subscriptions = target_status
        .watched_statuses()
        .into_iter()
        .map(|agent_status| Subscription::PaneAgentStatusChanged {
            pane_id: pane_id.to_owned(),
            agent_status: Some(agent_status),
        })
        .collect();

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

/// `flk agent result` (#575): the reply an agent's newest turn ended on, and
/// the `DONE:` / `BLOCKED:` / `VERDICT:` line it ends with — Claude and
/// opencode alike. `--offset` pages a long report by characters.
fn agent_result(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first().filter(|arg| !arg.starts_with("--")) else {
        eprintln!("usage: {AGENT_RESULT_USAGE}");
        return Ok(2);
    };
    let mut max_chars = None;
    let mut offset = None;
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--max-chars" | "--offset" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for {flag}");
                    return Ok(2);
                };
                let parsed = Some(super::parse_u32_flag(flag, value)?);
                if flag == "--offset" {
                    offset = parsed;
                } else {
                    max_chars = parsed;
                }
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:result".into(),
        method: Method::AgentResult(crate::api::schema::AgentResultParams {
            target: target.clone(),
            max_chars,
            offset,
        }),
    })?)
}

fn print_agent_help() {
    eprintln!("flk agent commands:");
    eprintln!("  flk agent list");
    eprintln!("  flk agent get <target>");
    eprintln!("  flk agent read <target> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]");
    eprintln!("  {AGENT_RESULT_USAGE}");
    eprintln!(
        "    the reply the agent's newest turn ended on (claude or opencode), with the status of"
    );
    eprintln!(
        "    a final DONE: / BLOCKED: / VERDICT: line; `finished` says whether that turn is over"
    );
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
    eprintln!(
        "  --cwd also picks the SPACE: a cwd naming an already-open checkout joins that space"
    );
    eprintln!("    as a new tab, and --split then splits it rather than whatever is focused;");
    eprintln!(
        "    a cwd matching nothing gets a space of its own, and --workspace/--tab still win"
    );
    eprintln!(
        "  --active names the workspace you are LOOKING AT as the placement; --here names the"
    );
    eprintln!(
        "    workspace your own PANE is in, read from your process ancestry rather than from"
    );
    eprintln!("    what was last focused — so a script run from a background space gets its own");
    eprintln!("    space, not the one somebody is looking at;");
    eprintln!(
        "  a start with no placement at all is refused unless the caller is running inside a"
    );
    eprintln!("    pane of this session, because \"the workspace you are looking at\" is only an");
    eprintln!(
        "    answer to a caller that has one — so ssh dispatch and resume scripts are told to"
    );
    eprintln!("    pass --workspace/--tab/--cwd, or --active, instead of being placed at random;");
    eprintln!("  targets accept terminal ids, unique agent names, detected/reported agent labels, and legacy pane ids");
    eprintln!(
        "  agent send writes literal text; use pane run when you want command text plus Enter"
    );
    eprintln!("  --ready / --wait-ready block until the pane reports a status other than unknown:");
    eprintln!("    a TUI that has not painted yet is unknown, so ready is the first moment idle,");
    eprintln!("    working or blocked is a real answer rather than flock not being able to tell");
    eprintln!(
        "  both wait verbs take the same seven statuses, and `idle` means ready for input in both"
    );
    eprintln!(
        "    (`done` too: an unattended agent goes quiet as `done`, which is an effective idle)."
    );
    eprintln!("    `done` on its own means exactly that effective state, not a UI-only marker:");
    eprintln!(
        "    --status settled waits for the agent to go quiet; `flk agent result` (once #575 lands) is"
    );
    eprintln!("    the turn's output, which settled does not promise.");
}

#[cfg(test)]
mod tests {
    use super::{parse_agent_start_flags, AGENT_START_USAGE};

    /// `-- active` is the terminator in these tests, so the parser sees exactly
    /// what it would see on the command line before the child's argv.
    fn flags(words: &[&str]) -> Result<super::AgentStartFlags, String> {
        let mut args = vec!["worker".to_string()];
        args.extend(words.iter().map(|word| word.to_string()));
        args.push("--".into());
        args.push("claude".into());
        let separator = args.iter().position(|arg| arg == "--").expect("terminator");
        parse_agent_start_flags(&args, separator)
    }

    /// #398: the flags have to actually reach the request, and they have to
    /// arrive as TWO fields. The server-side gate is only escapable by a caller
    /// that asked for a placement, so a parser that dropped either would leave a
    /// headless dispatcher with a refusal and no way out of it — and a parser
    /// that mapped `--here` onto `active` would answer "the space my pane is
    /// in" with "the space somebody last looked at", which is the bug the two
    /// questions exist to keep apart.
    #[test]
    fn active_and_here_are_separate_requests_not_one_flag_twice() {
        let parsed = flags(&["--active"]).expect("a flag this build documents");
        assert!(
            parsed.active,
            "--active asks for the workspace being looked at"
        );
        assert!(!parsed.here, "and nothing else");

        let parsed = flags(&["--here"]).expect("a flag this build documents");
        assert!(parsed.here, "--here asks for the caller's own space");
        assert!(
            !parsed.active,
            "--here must not be answered by the remembered workspace"
        );

        let parsed = flags(&["--active", "--here"]).expect("parses");
        assert!(
            parsed.active && parsed.here,
            "both may be sent; the server refuses the combination, since only it knows whether \
             they name different spaces"
        );
    }

    /// Absent means "not asked for", which the server reads as a refusal rather
    /// than a guess. A parser defaulting it the other way would reopen the bug.
    #[test]
    fn a_start_without_the_flag_does_not_ask_for_the_active_workspace() {
        let parsed = flags(&["--cwd", "/tmp"]).expect("parses");
        assert!(!parsed.active && !parsed.here);
        let parsed = flags(&["--split", "right"]).expect("parses");
        assert!(!parsed.active && !parsed.here);
    }

    /// `--no-focus` is the CLI's DEFAULT, which is why #398 could not use it as
    /// the headless signal. Pins the two as independent: a caller may name the
    /// active workspace and still not want to be taken to it.
    #[test]
    fn focus_and_the_active_workspace_are_independent_flags() {
        let parsed = flags(&["--active", "--no-focus"]).expect("parses");
        assert!(parsed.active && !parsed.here);
        assert!(!parsed.focus);

        let parsed = flags(&["--here", "--focus"]).expect("parses");
        assert!(parsed.here && !parsed.active);
        assert!(parsed.focus);
    }

    /// The `--` terminator ends the flags, so a value that looks like one is
    /// data. `--cwd --active` is a cwd named `--active`, not a placement.
    #[test]
    fn a_value_is_never_read_as_a_flag() {
        let parsed = flags(&["--cwd", "--active", "--split", "right"]).expect("parses");
        assert_eq!(parsed.cwd.as_deref(), Some("--active"));
        assert!(
            !parsed.active && !parsed.here,
            "the flag loop must stop at the value it just consumed"
        );
        assert_eq!(
            parsed.split,
            Some(crate::api::schema::SplitDirection::Right)
        );
    }

    #[test]
    fn a_flag_missing_its_value_is_refused_by_name() {
        for (words, flag) in [
            (vec!["--cwd"], "--cwd"),
            (vec!["--split"], "--split"),
            (vec!["--workspace"], "--workspace"),
        ] {
            let err = flags(&words).expect_err("a value-taking flag needs a value");
            assert_eq!(err, format!("missing value for {flag}"));
        }
    }

    /// The help row and the parser have to agree in both directions, or a
    /// caller is told about a flag that does nothing.
    #[test]
    fn the_usage_line_documents_the_flag_the_parser_accepts() {
        assert!(AGENT_START_USAGE.contains("[--active|--here]"));
        assert!(AGENT_START_USAGE.contains("[--split"));
    }
}
