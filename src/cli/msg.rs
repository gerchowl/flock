#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: usage and errors go to stderr for humans and scripts"
)]
use crate::api::schema::{
    EmptyParams, MessageTarget, Method, MsgListParams, MsgReplyParams, MsgSendParams, Request,
};

/// `flk msg` (#175 M1): queue a message for another pane's agent, delivered
/// at that pane's next settled turn boundary. Addressing per ADR-0006: the
/// wire is structured; the CLI accepts `--repo NAME` explicitly, or a
/// `<repo>:<pane>` positional shorthand that only splits when the left side
/// matches a known repo name (pane ids and agent labels also contain `:`).
pub(super) fn run_msg_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_msg_help();
        return Ok(2);
    };
    match subcommand {
        "send" => msg_send(&args[1..]),
        "reply" => msg_reply(&args[1..]),
        "list" => msg_list(&args[1..]),
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
    const USAGE: &str =
        "usage: flk msg send <target> <text...> [--repo NAME] [--correlation-id ID] [--reply-to ID]";
    let mut repo = None;
    let mut correlation_id = None;
    let mut in_reply_to = None;
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
    super::print_response(&super::send_request(&Request {
        id: "cli:msg:send".into(),
        method: Method::MsgSend(MsgSendParams {
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

fn print_msg_help() {
    eprintln!("flk msg commands:");
    eprintln!(
        "  flk msg send <target> <text...> [--repo NAME] [--correlation-id ID] [--reply-to ID]"
    );
    eprintln!("  flk msg reply <correlation_id> <text...>");
    eprintln!("  flk msg list [--pane TARGET]");
    eprintln!("  targets: pane id, terminal id, unique agent name; or repo:pane / --repo NAME");
    eprintln!("  delivery happens at the recipient's next settled turn boundary, never mid-turn");
}
