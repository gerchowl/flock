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
///
/// The list is not decoration: the cross-host relay IS `flk msg send` run on
/// the peer that owns the recipient (`send_peer_message`), so when a peer
/// refuses a flag this list is that peer telling the sender which build it is
/// running. In a version-skewed fleet that is the entire diagnosis (#380).
const SEND_OPTIONS: &[&str] = &[
    "--repo",
    "--agent",
    "--from-agent",
    "--from-host",
    "--correlation-id",
    "--reply-to",
    "--intent",
    "--json",
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
    from_host: Option<String>,
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
        from_host: None,
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
            "--from-host" => {
                parsed.from_host = Some(value()?);
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
                let Some(intent) = MsgIntent::from_wire(&raw) else {
                    return Err(format!(
                        "unknown --intent {raw:?}: expected fyi or needs-reply"
                    ));
                };
                parsed.intent = intent;
                index += 2;
            }
            "--json" => index += 1,
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
    Ok(parsed)
}

fn msg_send(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str = "usage: flk msg send (<target> | --agent ID) <text...> [--repo NAME] \
         [--intent fyi|needs-reply] [--correlation-id ID] [--reply-to ID] [--from-agent ID] \
         [-- <text starting with dashes>]";
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
        from_host,
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
    super::print_response(&super::send_request(&Request {
        id: "cli:msg:send".into(),
        method: Method::MsgSend(MsgSendParams {
            from_agent,
            from_host,
            to,
            body,
            correlation_id,
            in_reply_to,
            intent,
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
                        "unknown --intent {value:?}: expected fyi or needs-reply"
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
         [--intent fyi|needs-reply] [-- <text starting with dashes>]";
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
        "  flk msg send <target> <text...> [--repo NAME] [--intent fyi|needs-reply] \
         [--correlation-id ID] [--reply-to ID]"
    );
    eprintln!("  flk msg reply <correlation_id> <text...> [--intent fyi|needs-reply]");
    eprintln!(
        "  --intent needs-reply marks a message as owed an answer; it rides the envelope, \
         so the recipient sees it without reading the body (default: fyi)"
    );
    eprintln!("  flk msg list [--pane TARGET]");
    eprintln!("  flk msg read [--pane TARGET]   consume an inbox (agents use the MCP tool)");
    eprintln!("  flk msg status <correlation_id>  what became of a message you sent");
    eprintln!(
        "  flk msg mute <seconds> [--pane TARGET]  stop waking a recipient; 0 clears, \
         mail still arrives"
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
            "agent_sage_1",
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
            "agent_sage_1",
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
    fn every_advertised_option_is_actually_accepted() {
        // The refusal message lists `SEND_OPTIONS`, so a flag that drifts out
        // of the match arms would be advertised and then refused — the same
        // builder/advertisement drift #320 found on the MCP schema, one layer
        // down. Each option is fed with a value; the arms that take none
        // ignore the extra word as body text, which is what makes this cheap.
        for option in SEND_OPTIONS {
            let args = argv(&["--agent", "agent_sage_1", option, "fyi", "body"]);
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
        let parsed = parse_send_args(&argv(&["--agent", "agent_sage_1", "-5", "degrees"]))
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
    fn the_relays_own_command_line_still_parses() {
        // The exact argv `peer_message_command` builds. If refusing unknown
        // flags ever broke this, the fix would have closed the corruption by
        // breaking cross-host messaging outright.
        let parsed = parse_send_args(&argv(&[
            "--agent",
            "agent_sage_1",
            "--from-agent",
            "agent_mba22_2",
            "--from-host",
            "mba22",
            "--correlation-id",
            "c-1",
            "--reply-to",
            "c-0",
            "--intent",
            "needs_reply",
            "--json",
            "--",
            "re-derive both parameters",
        ]))
        .expect("the relay's own command line must survive its own refusal rule");
        assert_eq!(parsed.agent.as_deref(), Some("agent_sage_1"));
        assert_eq!(parsed.from_host.as_deref(), Some("mba22"));
        assert_eq!(parsed.in_reply_to.as_deref(), Some("c-0"));
        assert_eq!(parsed.intent, MsgIntent::NeedsReply);
        assert_eq!(parsed.positional, vec!["re-derive both parameters"]);
    }
}
