#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: usage and errors go to stderr for humans and scripts"
)]
use crate::api::schema::{
    EmptyParams, MessageTarget, Method, MsgIntent, MsgListParams, MsgReadParams, MsgReplyParams,
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

/// Every option `flk msg send` understands, named once.
const SEND_OPTIONS: &[&str] = &[
    "--repo",
    "--agent",
    "--from-agent",
    "--correlation-id",
    "--reply-to",
    "--intent",
    "--json",
    "--await",
    "--timeout",
];

const REPLY_OPTIONS: &[&str] = &["--intent", "--json"];

/// Whether an argument is being offered as an option.
///
/// `--` on its own is the terminator, and a lone `-` or a `-1` is body text,
/// so only a `--`-prefixed word longer than the terminator can be refused.
fn looks_like_option(arg: &str) -> bool {
    arg.starts_with("--") && arg.len() > 2
}

/// The one-line refusal an unrecognised flag earns.
///
/// One line on purpose: the relay reports the *last* stderr line of the remote
/// `flk`, so a refusal that wraps loses the half that names the flag.
fn unknown_option(command: &str, flag: &str, known: &[&str]) -> String {
    format!(
        "{command}: unknown option {flag:?} — this build understands {}, and `--` ends flag \
         parsing so a body may begin with dashes",
        known.join(" ")
    )
}

/// Everything `flk msg send` can be told, before the target is resolved.
#[derive(Debug)]
struct SendArgs {
    repo: Option<String>,
    intent: MsgIntent,
    correlation_id: Option<String>,
    in_reply_to: Option<String>,
    agent: Option<String>,
    from_agent: Option<String>,
    /// #576: after sending, wait for the answer (`flk wait reply`).
    await_reply: bool,
    /// How long `--await` waits, in ms.
    timeout_ms: Option<u64>,
    json: bool,
    positional: Vec<String>,
}

/// Parse `flk msg send`'s argv, refusing anything shaped like an option that
/// this build does not know (#380).
///
/// Pure, and split out from [`msg_send`], because the refusal is the whole
/// point: what this returns for an argument it does not understand used to be
/// the message body, and asserting on it must not need a running server — let
/// alone the ssh hop the relay puts in front of one.
fn parse_send_args(args: &[String]) -> Result<SendArgs, String> {
    let mut parsed = SendArgs {
        repo: None,
        intent: MsgIntent::default(),
        correlation_id: None,
        in_reply_to: None,
        agent: None,
        from_agent: None,
        await_reply: false,
        timeout_ms: None,
        json: false,
        positional: Vec::new(),
    };
    let mut index = 0;
    while index < args.len() {
        // Every value-taking arm reads `args[index + 1]`, so name it once.
        let value = || {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("missing value for {}", args[index]))
        };
        match args[index].as_str() {
            "--repo" => {
                parsed.repo = Some(value()?);
                index += 2;
            }
            "--correlation-id" => {
                parsed.correlation_id = Some(value()?);
                index += 2;
            }
            // ADR-0008 addressing: `--agent` targets a fleet-global identity
            // rather than a pane, and `--from-agent` carries the sender's when
            // a peer relays on its behalf (the receiving server has no local
            // ancestry to attest from).
            "--agent" => {
                parsed.agent = Some(value()?);
                index += 2;
            }
            "--from-agent" => {
                parsed.from_agent = Some(value()?);
                index += 2;
            }
            // #280. Optional here, and required on the MCP tool: the CLI
            // caller is an operator or the cross-host relay, and both already
            // know what they meant. The stamp an agent might skip without
            // noticing is the one worth forcing.
            "--intent" => {
                let raw = value()?;
                parsed.intent = MsgIntent::from_wire(&raw).ok_or_else(|| {
                    format!("unknown --intent {raw:?}: expected {}", intent_spellings())
                })?;
                index += 2;
            }
            "--json" => {
                parsed.json = true;
                index += 1;
            }
            "--await" => {
                parsed.await_reply = true;
                index += 1;
            }
            "--timeout" => {
                let raw = value()?;
                parsed.timeout_ms = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("--timeout takes milliseconds, got {raw:?}"))?,
                );
                index += 2;
            }
            "--" => {
                parsed.positional.extend(args[index + 1..].iter().cloned());
                break;
            }
            "--reply-to" => {
                parsed.in_reply_to = Some(value()?);
                index += 2;
            }
            // The fix for #380. An unrecognised flag used to fall through to
            // the positional arm below and become body text, so a peer running
            // a build that predated any flag delivered a message with the flag
            // glued to it — silently, at both ends. Refusing turns that
            // corruption into a failure the relay reports on the SENDING side,
            // the posture `SpawnRefusal` and `PrPollErrorKind` already take: a
            // failure that crosses a host boundary arrives as data, not damage.
            other if looks_like_option(other) => {
                return Err(unknown_option("flk msg send", other, SEND_OPTIONS));
            }
            _ => {
                parsed.positional.push(args[index].clone());
                index += 1;
            }
        }
    }
    // #576. Waiting on an `fyi` would wait for an answer nobody was asked
    // for, and `--timeout` without `--await` would be silently ignored.
    if parsed.await_reply && parsed.intent == MsgIntent::Fyi {
        return Err(
            "--await waits for an answer: send it with --intent needs-reply or blocking".into(),
        );
    }
    if parsed.timeout_ms.is_some() && !parsed.await_reply {
        return Err("--timeout only applies with --await".into());
    }
    Ok(parsed)
}

/// The tiers this build accepts, for a refusal to name.
fn intent_spellings() -> String {
    MsgIntent::ALL
        .iter()
        .map(|intent| intent.as_wire())
        .collect::<Vec<_>>()
        .join(", ")
}

fn msg_send(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str = "usage: flk msg send (<target> | --agent ID) <text...> [--repo NAME] \
         [--intent fyi|needs-reply|blocking] [--correlation-id ID] [--reply-to ID] [--from-agent ID] \
         [--await [--timeout MS]] [-- <text starting with dashes>]";
    let parsed = match parse_send_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    let SendArgs {
        repo,
        intent,
        correlation_id,
        in_reply_to,
        agent,
        from_agent,
        await_reply,
        timeout_ms,
        json,
        positional,
    } = parsed;
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
    let response = super::send_request(&Request {
        id: "cli:msg:send".into(),
        method: Method::MsgSend(MsgSendParams {
            from_agent,
            to,
            body,
            correlation_id,
            in_reply_to,
            intent,
        }),
    })?;
    if !await_reply {
        return super::print_response(&response);
    }
    // #576 `--await`: the send's own result goes to stderr, so stdout is the
    // answer alone — what a harness hands back when this ran as a background
    // task.
    eprintln!("{}", serde_json::to_string(&response).unwrap_or_default());
    if response.get("error").is_some() {
        return Ok(1);
    }
    let Some(sent_id) = response["result"]["correlation_id"].as_str() else {
        eprintln!("flk msg send --await: the send returned no correlation id to wait on");
        return Ok(1);
    };
    await_reply_for(sent_id, timeout_ms, json, None)
}

/// `flk wait reply` exit status (#576): an answer arrived.
const EXIT_REPLIED: i32 = 0;
/// No answer is coming: the recipient is muted (its deferral is printed), or
/// the message was dropped unread.
const EXIT_NO_ANSWER: i32 = 3;
/// The receiver refused custody of the message; the reason is printed.
const EXIT_REFUSED: i32 = 4;
/// The wait ran out first. The coreutils `timeout` convention.
const EXIT_TIMEOUT: i32 = 124;

fn exit_for(outcome: &str) -> i32 {
    match outcome {
        "replied" => EXIT_REPLIED,
        "deferred"
        | "expired"
        | "recipient_gone"
        | "outcome_retention_elapsed"
        | "undeliverable" => EXIT_NO_ANSWER,
        "refused" => EXIT_REFUSED,
        "timeout" => EXIT_TIMEOUT,
        _ => 1,
    }
}

/// `flk wait reply <correlation_id> [--timeout MS] [--json]` (#576).
pub(super) fn wait_reply(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str = "usage: flk wait reply <correlation_id> [--timeout MS] [--json]";
    let mut correlation_id = None;
    let mut timeout_ms = None;
    let mut reference = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--reference" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing --reference JSON");
                    return Ok(2);
                };
                reference = match serde_json::from_str(value) {
                    Ok(reference) => Some(reference),
                    Err(error) => {
                        eprintln!("invalid status reference: {error}");
                        return Ok(2);
                    }
                };
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
            "--json" => {
                json = true;
                index += 1;
            }
            other if looks_like_option(other) => {
                eprintln!(
                    "{}",
                    unknown_option(
                        "flk wait reply",
                        other,
                        &["--timeout", "--json", "--reference"]
                    )
                );
                return Ok(2);
            }
            other if correlation_id.is_none() => {
                correlation_id = Some(other.to_string());
                index += 1;
            }
            _ => {
                eprintln!("{USAGE}");
                return Ok(2);
            }
        }
    }
    let Some(correlation_id) = correlation_id else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    await_reply_for(&correlation_id, timeout_ms, json, reference)
}

/// Hold `msg.wait_reply` and report how it ended: the answer's body on
/// stdout (its metadata on stderr), or the whole result with `--json`.
fn await_reply_for(
    correlation_id: &str,
    timeout_ms: Option<u64>,
    json: bool,
    reference: Option<crate::mesh::store::StatusReference>,
) -> std::io::Result<i32> {
    let response = super::send_request(&Request {
        id: "cli:wait:reply".into(),
        method: Method::MsgWaitReply(crate::api::schema::MsgWaitReplyParams {
            reference,
            correlation_id: correlation_id.to_string(),
            timeout_ms,
        }),
    })?;
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap_or_default());
        return Ok(1);
    }
    let result = &response["result"];
    let outcome = result["outcome"].as_str().unwrap_or_default();
    if json {
        println!("{}", serde_json::to_string(&response).unwrap_or_default());
        return Ok(exit_for(outcome));
    }
    let reply = &result["reply"];
    match outcome {
        "replied" | "deferred" => {
            let from = reply["from_agent"]
                .as_str()
                .or_else(|| reply["from_pane"].as_str())
                .unwrap_or("an unattested sender");
            eprintln!(
                "{outcome}: {} from {from}{}",
                reply["correlation_id"].as_str().unwrap_or_default(),
                if reply["held"].as_bool() == Some(true) {
                    " (held for this waiter)"
                } else {
                    ""
                }
            );
            println!("{}", reply["body"].as_str().unwrap_or_default());
        }
        "recipient_gone" | "outcome_retention_elapsed" => eprintln!("{correlation_id}: {outcome}"),
        "refused" => eprintln!(
            "{correlation_id} was refused by the receiver: {}",
            result["detail"].as_str().unwrap_or("no reason given")
        ),
        "expired" => eprintln!("{correlation_id} was dropped unread; no answer is coming"),
        "undeliverable" => eprintln!(
            "{correlation_id} could not be delivered by a forwarding hub: {}",
            result["detail"].as_str().unwrap_or("no reason given")
        ),
        "timeout" => eprintln!(
            "no answer to {correlation_id} yet (last state: {})",
            result["state"].as_str().unwrap_or("unknown")
        ),
        other => eprintln!("unexpected outcome {other:?}"),
    }
    Ok(exit_for(outcome))
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

/// Parse `flk msg reply`'s argv into `(intent, positional)`.
///
/// Same refusal rule as [`parse_send_args`], and for the same reason: `reply`
/// grew `--intent` in the same PR `send` did (#280), so it carries the same
/// skew hazard. Refusing on one verb and swallowing on the other would leave a
/// caller unable to tell which behaviour it is talking to.
fn parse_reply_args(args: &[String]) -> Result<(MsgIntent, Vec<String>), String> {
    let mut intent = MsgIntent::default();
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--intent" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --intent".to_string());
                };
                let Some(parsed) = MsgIntent::from_wire(value) else {
                    return Err(format!(
                        "unknown --intent {value:?}: expected {}",
                        intent_spellings()
                    ));
                };
                intent = parsed;
                index += 2;
            }
            "--json" => index += 1,
            "--" => {
                positional.extend(args[index + 1..].iter().cloned());
                break;
            }
            other if looks_like_option(other) => {
                return Err(unknown_option("flk msg reply", other, REPLY_OPTIONS));
            }
            _ => {
                positional.push(args[index].clone());
                index += 1;
            }
        }
    }
    Ok((intent, positional))
}

fn msg_reply(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str = "usage: flk msg reply <correlation_id> <text...> \
         [--intent fyi|needs-reply|blocking] [-- <text starting with dashes>]";
    let (intent, positional) = match parse_reply_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    if positional.len() < 2 {
        eprintln!("{USAGE}");
        return Ok(2);
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:msg:reply".into(),
        method: Method::MsgReply(MsgReplyParams {
            correlation_id: positional[0].clone(),
            body: positional[1..].join(" "),
            reply_correlation_id: None,
            intent,
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
    let reference = match args.get(1).map(String::as_str) {
        None => None,
        Some("--reference") if args.len() == 3 => match serde_json::from_str(&args[2]) {
            Ok(reference) => Some(reference),
            Err(error) => {
                eprintln!("invalid status reference: {error}");
                return Ok(2);
            }
        },
        _ => {
            eprintln!("usage: flk msg status <correlation_id> [--reference JSON]");
            return Ok(2);
        }
    };
    let response = super::send_request(&Request {
        id: "cli:msg:status".into(),
        method: Method::MsgStatus(crate::api::schema::MsgStatusParams {
            reference,
            correlation_id: correlation_id.clone(),
        }),
    })?;
    let code = super::print_response(&response)?;
    if code != 0 {
        return Ok(code);
    }
    Ok(status_exit(response["result"]["state"].as_str()))
}

/// `flk msg status` exit code: states no answer can follow exit like a
/// no-answer wait, everything still live or settled-good exits 0.
fn status_exit(state: Option<&str>) -> i32 {
    match state {
        Some("refused") => EXIT_REFUSED,
        Some(
            "expired"
            | "recipient_gone"
            | "outcome_retention_elapsed"
            | "collect_failed"
            | "undeliverable",
        ) => EXIT_NO_ANSWER,
        _ => 0,
    }
}

/// `flk msg mute` — the receiver-side half of the wake rule (#316).
///
/// Suppresses the WAKE for a bounded window, never the delivery: mail keeps
/// arriving and `flk msg list` keeps showing it. `0` clears. The operator
/// path exists alongside the agent's `flock_msg_mute` so a mute an agent set
/// on itself can always be lifted from outside it.
///
/// A mute answers (ADR-0018 §3): every sender whose `needs_reply` message
/// waits or arrives while it holds is told, once, with `--reason` if given and
/// the time the mute lifts.
fn msg_mute(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str =
        "usage: flk msg mute <seconds> [--pane TARGET] [--reason TEXT]   (0 clears)";
    let mut pane = None;
    let mut seconds = None;
    let mut reason = None;
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
            "--reason" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --reason");
                    return Ok(2);
                };
                reason = Some(value.clone());
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
        method: Method::MsgMute(crate::api::schema::MsgMuteParams {
            pane,
            seconds,
            reason,
        }),
    })?)
}

fn print_msg_help() {
    eprintln!("flk msg commands:");
    eprintln!(
        "  flk msg send <target> <text...> [--repo NAME] [--intent fyi|needs-reply|blocking] \
         [--correlation-id ID] [--reply-to ID]"
    );
    eprintln!("  flk msg reply <correlation_id> <text...> [--intent fyi|needs-reply|blocking]");
    eprintln!(
        "  --intent rides the envelope: fyi never wakes the recipient (default); needs-reply \
         nudges it at its next turn boundary; blocking also surfaces it to the operator, \
         under a tighter rate limit"
    );
    eprintln!("  flk msg list [--pane TARGET]");
    eprintln!("  flk msg read [--pane TARGET]   consume an inbox (agents use the MCP tool)");
    eprintln!(
        "  flk msg status <correlation_id>  what became of a message you sent, and its reply"
    );
    eprintln!(
        "  --await [--timeout MS] on a needs-reply/blocking send waits for the answer, like \
         `flk wait reply`: exit 0 replied, 3 deferred or expired, 124 timed out"
    );
    eprintln!(
        "  flk msg mute <seconds> [--pane TARGET] [--reason TEXT]  stop waking a recipient; \
         0 clears, mail still arrives, and each needs-reply sender is told once when it lifts"
    );
    eprintln!(
        "  --  ends flag parsing: everything after it is body text, so a message may begin \
         with dashes"
    );
    eprintln!(
        "  an unrecognised --flag is refused, never appended to the body: the relay is this same \
         command run on the peer, and a peer too old to know a flag must say so rather than \
         deliver it as text"
    );
    eprintln!("  targets: pane id, terminal id, unique agent name; or repo:pane / --repo NAME");
    eprintln!("  agents read their own inbox (flock_msg_read); flock never types into a session");
}

#[cfg(test)]
mod tests {
    use super::{
        looks_like_option, parse_reply_args, parse_send_args, REPLY_OPTIONS, SEND_OPTIONS,
    };
    use crate::api::schema::MsgIntent;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_sent_as_body_text() {
        // #380. The relay is `flk msg send` run over ssh on the peer that owns
        // the recipient, and a fleet is routinely version-skewed — so before
        // this, every flag ever added was, on the day it shipped, a way to
        // deliver `--intent needs_reply` as the first four words of somebody's
        // message. Silently, with a success at the sending end.
        let err = parse_send_args(&argv(&[
            "--agent",
            "agent_atlas_1",
            "--intent-typo",
            "needs_reply",
            "the message",
        ]))
        .expect_err("an unrecognised flag must not become body text");
        assert!(
            err.contains("--intent-typo"),
            "the refusal must name the flag it refused: {err}"
        );
        // The refusal crosses the ssh hop as the remote's LAST stderr line, so
        // it has to be one line and it has to carry the diagnosis with it.
        assert_eq!(err.lines().count(), 1, "{err}");
        assert!(
            err.contains("--reply-to"),
            "a peer that refuses also says which flags its build does know: {err}"
        );
    }

    #[test]
    fn a_dash_dash_terminator_lets_a_body_begin_with_dashes() {
        // The escape hatch the refusal depends on, and the one the relay
        // already uses for every body it sends.
        let parsed = parse_send_args(&argv(&[
            "--agent",
            "agent_atlas_1",
            "--",
            "--intent",
            "is what I typed",
        ]))
        .expect("`--` ends flag parsing");
        assert_eq!(parsed.positional, vec!["--intent", "is what I typed"]);
        assert_eq!(
            parsed.intent,
            MsgIntent::Fyi,
            "a flag after `--` is text, not a flag"
        );
    }

    #[test]
    fn await_needs_an_answer_to_wait_for_and_timeout_needs_await() {
        // #576. Waiting on an `fyi` would wait for an answer nobody was asked
        // for; a `--timeout` without `--await` would be silently ignored.
        let refused = |args: &[&str]| parse_send_args(&argv(args)).expect_err("refused");
        assert!(refused(&["w1:p1", "hi", "--await"]).contains("--intent needs-reply"));
        assert!(refused(&["w1:p1", "hi", "--timeout", "5"]).contains("only applies with --await"));
        assert!(refused(&[
            "w1:p1",
            "hi",
            "--intent",
            "needs-reply",
            "--await",
            "--timeout",
            "soon"
        ])
        .contains("milliseconds"));
        let parsed = parse_send_args(&argv(&[
            "w1:p1",
            "hi",
            "--intent",
            "blocking",
            "--await",
            "--timeout",
            "250",
            "--json",
        ]))
        .expect("a blocking send can await");
        assert!(parsed.await_reply && parsed.json);
        assert_eq!(parsed.timeout_ms, Some(250));
        assert_eq!(parsed.positional, vec!["w1:p1", "hi"]);
    }

    #[test]
    fn wait_reply_exit_codes_follow_the_outcome() {
        assert_eq!(super::exit_for("replied"), 0);
        assert_eq!(super::exit_for("deferred"), 3);
        assert_eq!(super::exit_for("expired"), 3);
        assert_eq!(super::exit_for("timeout"), 124);
        assert_eq!(super::exit_for("anything else"), 1);
        assert_eq!(super::exit_for("recipient_gone"), 3);
        assert_eq!(super::exit_for("outcome_retention_elapsed"), 3);
        assert_eq!(super::exit_for("undeliverable"), 3);
        assert_eq!(super::exit_for("refused"), 4);
    }

    #[test]
    fn msg_status_exits_3_only_for_states_no_answer_can_follow() {
        for state in [
            "queued",
            "custody",
            "delivered",
            "read",
            "held",
            "collected",
            "relayed",
        ] {
            assert_eq!(super::status_exit(Some(state)), 0, "{state}");
        }
        for state in [
            "expired",
            "outcome_retention_elapsed",
            "recipient_gone",
            "collect_failed",
            "undeliverable",
        ] {
            assert_eq!(super::status_exit(Some(state)), 3, "{state}");
        }
        assert_eq!(super::status_exit(Some("refused")), 4);
    }

    #[test]
    fn every_advertised_option_is_actually_accepted() {
        // The refusal message lists `SEND_OPTIONS`, so a flag that drifts out
        // of the match arms would be advertised and then refused — the same
        // builder/advertisement drift #320 found on the MCP schema, one layer
        // down. Each option is fed with a value; the arms that take none
        // ignore the extra word as body text, which is what makes this cheap.
        for option in SEND_OPTIONS {
            // #576's two need a context to be valid in: `--await` waits for an
            // answer, so not on an `fyi`; `--timeout` is `--await`'s, in ms.
            let (value, context): (&str, &[&str]) = match *option {
                "--await" => ("fyi", &["--intent", "needs-reply"]),
                "--timeout" => ("1000", &["--intent", "needs-reply", "--await"]),
                _ => ("fyi", &[]),
            };
            let mut args = argv(&["--agent", "agent_atlas_1", option, value]);
            args.extend(argv(context));
            args.push("body".into());
            assert!(
                parse_send_args(&args).is_ok(),
                "{option} is advertised but refused"
            );
        }
        for option in REPLY_OPTIONS {
            let args = argv(&[option, "fyi", "c-1", "body"]);
            assert!(
                parse_reply_args(&args).is_ok(),
                "{option} is advertised but refused"
            );
        }
    }

    #[test]
    fn a_body_may_still_start_with_a_single_dash() {
        // Only a `--`-prefixed word is refused. A lone `-`, a `-5` or a diff
        // line is body text, exactly as before — narrowing the escape hatch
        // any further would break bodies that work today.
        let parsed = parse_send_args(&argv(&["--agent", "agent_atlas_1", "-5", "degrees"]))
            .expect("a single dash is body text");
        assert_eq!(parsed.positional, vec!["-5", "degrees"]);
        assert!(!looks_like_option("-"));
        assert!(!looks_like_option("--"));
        assert!(looks_like_option("--anything"));
    }

    #[test]
    fn reply_refuses_unknown_flags_and_honours_the_terminator() {
        // `reply` grew `--intent` alongside `send` (#280) and had no `--` at
        // all, so a reply whose text began with dashes lost its correlation id
        // to the body.
        let err = parse_reply_args(&argv(&["--needs-reply", "c-1", "answered"]))
            .expect_err("an unrecognised flag must not become the correlation id");
        assert!(err.contains("--needs-reply"), "{err}");

        let (intent, positional) =
            parse_reply_args(&argv(&["c-1", "--", "--not-a-flag"])).expect("`--` ends parsing");
        assert_eq!(positional, vec!["c-1", "--not-a-flag"]);
        assert_eq!(intent, MsgIntent::Fyi);
    }

    #[test]
    fn removed_from_host_flag_is_refused() {
        let error = parse_send_args(&argv(&["--from-host", "nodea", "p1", "hello"]))
            .expect_err("removed flag");
        assert!(error.contains("unknown option \"--from-host\""), "{error}");
    }
}
