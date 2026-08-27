#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: usage and errors go to stderr for humans and scripts"
)]
use crate::api::schema::{
    EmptyParams, MessageTarget, Method, MsgListParams, MsgReadParams, MsgReplyParams,
    MsgSendParams, Request,
};

/// `flk msg` (#175 M1): queue a message for another pane's agent. The
/// recipient reads it from its own inbox (ADR-0008) — flock does not type
/// into a session, so there is no settled-turn-boundary window to wait for;
/// the stop hook wakes an idle recipient. Addressing per ADR-0006: the wire is
/// structured; the CLI accepts `--repo NAME` explicitly, or a `<repo>:<pane>`
/// positional shorthand that only splits when the left side matches a known
/// repo name (pane ids and agent labels also contain `:`).
pub(super) fn run_msg_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_msg_help();
        return Ok(2);
    };
    match subcommand {
        "send" => msg_send(&args[1..]),
        "reply" => msg_reply(&args[1..]),
        "list" => msg_list(&args[1..]),
        "read" => msg_read(&args[1..]),
        "status" => msg_status(&args[1..]),
        "mute" => msg_mute(&args[1..]),
        "help" | "--help" | "-h" => {
            print_msg_help();
            Ok(0)
        }
        _ => {
            print_msg_help();
            Ok(2)
        }
    }
}

fn msg_send(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str = "usage: flk msg send (<target> | --agent ID) <text...> [--repo NAME] \
         [--correlation-id ID] [--reply-to ID] [--from-agent ID]";
    let mut repo = None;
    let mut correlation_id = None;
    let mut in_reply_to = None;
    let mut agent = None;
    let mut from_agent = None;
    let mut from_host = None;
    let mut positional: Vec<String> = Vec::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--repo" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --repo");
                    return Ok(2);
                };
                repo = Some(value.clone());
                index += 2;
            }
            "--correlation-id" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --correlation-id");
                    return Ok(2);
                };
                correlation_id = Some(value.clone());
                index += 2;
            }
            // ADR-0008 addressing: `--agent` targets a fleet-global identity
            // rather than a pane, and `--from-agent` carries the sender's when
            // a peer relays on its behalf (the receiving server has no local
            // ancestry to attest from).
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --agent");
                    return Ok(2);
                };
                agent = Some(value.clone());
                index += 2;
            }
            "--from-host" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --from-host");
                    return Ok(2);
                };
                from_host = Some(value.clone());
                index += 2;
            }
            "--from-agent" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --from-agent");
                    return Ok(2);
                };
                from_agent = Some(value.clone());
                index += 2;
            }
            "--json" => index += 1,
            "--" => {
                positional.extend(args[index + 1..].iter().cloned());
                break;
            }
            "--reply-to" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --reply-to");
                    return Ok(2);
                };
                in_reply_to = Some(value.clone());
                index += 2;
            }
            _ => {
                positional.push(args[index].clone());
                index += 1;
            }
        }
    }
    // With --agent the identity IS the target, so only the body is positional.
    let (to, body) = if let Some(agent) = agent {
        if positional.is_empty() {
            eprintln!("{USAGE}");
            return Ok(2);
        }
        (MessageTarget::Agent { agent }, positional.join(" "))
    } else {
        if positional.len() < 2 {
            eprintln!("{USAGE}");
            return Ok(2);
        }
        let target = positional[0].clone();
        let body = positional[1..].join(" ");
        let to = match repo {
            Some(repo) => MessageTarget::RepoPane { repo, pane: target },
            None => resolve_shorthand(target)?,
        };
        (to, body)
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:msg:send".into(),
        method: Method::MsgSend(MsgSendParams {
            from_agent,
            from_host,
            to,
            body,
            correlation_id,
            in_reply_to,
        }),
    })?)
}

/// ADR-0006 shorthand: split `<repo>:<pane>` only when the left side names
/// a known repo (from the live workspace list); otherwise the whole string
/// is a bare pane target.
fn resolve_shorthand(target: String) -> std::io::Result<MessageTarget> {
    let Some((left, right)) = target.split_once(':') else {
        return Ok(MessageTarget::Pane { pane: target });
    };
    let known_repos = super::send_request(&Request {
        id: "cli:msg:repos".into(),
        method: Method::WorkspaceList(EmptyParams {}),
    })
    .ok()
    .and_then(|response| {
        response
            .get("result")
            .and_then(|result| result.get("workspaces"))
            .and_then(|workspaces| workspaces.as_array())
            .map(|workspaces| {
                workspaces
                    .iter()
                    .filter_map(|workspace| {
                        workspace
                            .get("worktree")
                            .and_then(|worktree| worktree.get("repo_name"))
                            .and_then(|name| name.as_str())
                            .map(str::to_string)
                    })
                    .collect::<Vec<_>>()
            })
    })
    .unwrap_or_default();
    if known_repos.iter().any(|repo| repo == left) {
        Ok(MessageTarget::RepoPane {
            repo: left.to_string(),
            pane: right.to_string(),
        })
    } else {
        Ok(MessageTarget::Pane { pane: target })
    }
}

fn msg_reply(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: flk msg reply <correlation_id> <text...>");
        return Ok(2);
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:msg:reply".into(),
        method: Method::MsgReply(MsgReplyParams {
            correlation_id: args[0].clone(),
            body: args[1..].join(" "),
            reply_correlation_id: None,
        }),
    })?)
}

fn msg_list(args: &[String]) -> std::io::Result<i32> {
    let mut pane = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --pane");
                    return Ok(2);
                };
                pane = Some(value.clone());
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:msg:list".into(),
        method: Method::MsgList(MsgListParams { pane }),
    })?)
}

/// `flk msg read` — consume an inbox (ADR-0008). The agent-facing path is the
/// `flock_msg_read` MCP tool; this is the same verb for operators and scripts,
/// so there is one delivery semantic rather than a CLI-only variant.
fn msg_read(args: &[String]) -> std::io::Result<i32> {
    let mut pane = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --pane");
                    return Ok(2);
                };
                pane = Some(value.clone());
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:msg:read".into(),
        method: Method::MsgRead(MsgReadParams { pane }),
    })?)
}

/// `flk msg status` — the sender's view.
///
/// `msg list` answers "what is waiting for me"; nothing answered "what
/// happened to what I sent". A cross-host send was the worst case: it left no
/// local record at all, so the trail went cold at the correlation id.
fn msg_status(args: &[String]) -> std::io::Result<i32> {
    let Some(correlation_id) = args.first().filter(|arg| !arg.starts_with("--")) else {
        eprintln!("usage: flk msg status <correlation_id>");
        return Ok(2);
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:msg:status".into(),
        method: Method::MsgStatus(crate::api::schema::MsgStatusParams {
            correlation_id: correlation_id.clone(),
        }),
    })?)
}

/// `flk msg mute` — the receiver-side half of the wake rule (#316).
///
/// Suppresses the WAKE for a bounded window, never the delivery: mail keeps
/// arriving and `flk msg list` keeps showing it. `0` clears. The operator
/// path exists alongside the agent's `flock_msg_mute` so a mute an agent set
/// on itself can always be lifted from outside it.
fn msg_mute(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str = "usage: flk msg mute <seconds> [--pane TARGET]   (0 clears)";
    let mut pane = None;
    let mut seconds = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --pane");
                    return Ok(2);
                };
                pane = Some(value.clone());
                index += 2;
            }
            other => {
                let Ok(parsed) = other.parse::<u64>() else {
                    eprintln!("{USAGE}");
                    return Ok(2);
                };
                seconds = Some(parsed);
                index += 1;
            }
        }
    }
    let Some(seconds) = seconds else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:msg:mute".into(),
        method: Method::MsgMute(crate::api::schema::MsgMuteParams { pane, seconds }),
    })?)
}

fn print_msg_help() {
    eprintln!("flk msg commands:");
    eprintln!(
        "  flk msg send <target> <text...> [--repo NAME] [--correlation-id ID] [--reply-to ID]"
    );
    eprintln!("  flk msg reply <correlation_id> <text...>");
    eprintln!("  flk msg list [--pane TARGET]");
    eprintln!("  flk msg read [--pane TARGET]   consume an inbox (agents use the MCP tool)");
    eprintln!("  flk msg status <correlation_id>  what became of a message you sent");
    eprintln!(
        "  flk msg mute <seconds> [--pane TARGET]  stop waking a recipient; 0 clears, \
         mail still arrives"
    );
    eprintln!("  targets: pane id, terminal id, unique agent name; or repo:pane / --repo NAME");
    eprintln!("  agents read their own inbox (flock_msg_read); flock never types into a session");
}
