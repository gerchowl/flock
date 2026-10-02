//! #455 — `--help` / `-h` on every subcommand, and never as one of its
//! arguments.
//!
//! Help used to be a property of the GROUP verbs and not of the ACTION verbs
//! underneath them, which made probing the CLI expensive in the worst possible
//! way for a tool whose whole job is spawning agents and allocating worktrees:
//!
//! ```text
//! $ flk worktree create --help
//! unknown option: --help
//! $ flk worktree create            # the same question, minus the flag: a real worktree
//! $ flk agent fork --help
//! {"error":{"code":"agent_not_found","message":"agent target --help not found"}}
//! ```
//!
//! So the check lives HERE, once, above the verb parsers rather than inside
//! each of them. Two properties follow from that placement and neither can be
//! had any other way:
//!
//! * a help request cannot allocate — the parser that would allocate is never
//!   reached, so there is nothing to guard and nothing to get wrong per verb;
//! * `--help` is never a target. `flk agent fork --help` cannot read the flag
//!   as an agent name, because the flag is answered before any verb sees an
//!   argument.
//!
//! The cost is one table to keep true. That is the trade this makes on
//! purpose: a usage line that has drifted is a wrong answer, not a wrong
//! action, and the alternative was fifty parsers each owning its own idea of
//! what `--help` means.
//!
//! The entries are the invocation WITHOUT the leading `usage: ` — the
//! dispatcher prints that prefix, so every help answer on the CLI starts with
//! the same word. The `agent` rows are the same constants the agent parsers
//! print, so those cannot drift apart at all.

/// The flags that ask for help. `help` as a bare word is deliberately absent:
/// it is already answered in the verb position by every group's own arm, and
/// unlike `--help` it is ordinary text to the verbs that take literal text
/// (`flk agent send w1 help` is a message that reads "help").
const HELP_FLAGS: [&str; 2] = ["--help", "-h"];

/// Every subcommand that takes a verb, and the one line that explains it.
///
/// Keyed by (group, verb) so a verb the table has never heard of falls
/// through to its own parser and keeps whatever it did before — a missing entry
/// degrades to "unknown option", never to a wrong action.
///
/// `pane` is the group to watch when adding rows here: `src/cli/pane.rs` owns
/// those parsers and its own help text, and this table mirrors them.
const VERBS: &[(&str, &str, &str)] = &[
    ("server", "stop", "flk server stop"),
    (
        "server",
        "live-handoff",
        "flk server live-handoff [--import-exe <path>] [--expected-protocol <n>] [--expected-version <version>]",
    ),
    ("server", "reload-config", "flk server reload-config"),
    ("status", "server", "flk status server [--json]"),
    ("status", "client", "flk status client [--json]"),
    ("config", "edit", "flk config edit"),
    ("config", "reset-keys", "flk config reset-keys"),
    ("config", "check", "flk config check [--path PATH]"),
    ("channel", "set", "flk channel set <stable|preview>"),
    ("channel", "show", "flk channel show"),
    ("workspace", "list", "flk workspace list"),
    (
        "workspace",
        "create",
        "flk workspace create [--cwd PATH] [--label TEXT] [--focus] [--no-focus]",
    ),
    ("workspace", "get", "flk workspace get <workspace_id>"),
    ("workspace", "focus", "flk workspace focus <workspace_id>"),
    (
        "workspace",
        "rename",
        "flk workspace rename <workspace_id> <label>",
    ),
    ("workspace", "close", "flk workspace close <workspace_id>"),
    (
        "worktree",
        "list",
        "flk worktree list [--workspace ID | --cwd PATH] [--scan] [--json]",
    ),
    (
        "worktree",
        "create",
        "flk worktree create [--workspace ID | --cwd PATH] [--branch NAME] [--base REF] [--path PATH] [--label TEXT] [--focus] [--no-focus] [--json]",
    ),
    (
        "worktree",
        "open",
        "flk worktree open [--workspace ID | --cwd PATH] (--path PATH | --branch NAME) [--label TEXT] [--focus] [--no-focus] [--json]",
    ),
    (
        "worktree",
        "remove",
        "flk worktree remove --workspace ID [--force] [--json]",
    ),
    (
        "worktree",
        "kill",
        "flk worktree kill (--workspace ID | --path PATH) [--dry-run] [--force] [--keep-branch] [--keep-procs] [--json]",
    ),
    (
        "worktree",
        "quarantine-list",
        "flk worktree quarantine-list",
    ),
    (
        "worktree",
        "unquarantine",
        "flk worktree unquarantine <quarantined-path> <destination>",
    ),
    ("tab", "list", "flk tab list [--workspace <workspace_id>]"),
    (
        "tab",
        "create",
        "flk tab create [--workspace <workspace_id>] [--cwd PATH] [--label TEXT] [--focus] [--no-focus]",
    ),
    ("tab", "get", "flk tab get <tab_id>"),
    ("tab", "focus", "flk tab focus <tab_id>"),
    ("tab", "rename", "flk tab rename <tab_id> <label>"),
    ("tab", "close", "flk tab close <tab_id>"),
    (
        "notification",
        "show",
        "flk notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request]",
    ),
    (
        "notification",
        "list",
        "flk notification list [--unread] [--limit N]",
    ),
    (
        "notification",
        "read",
        "flk notification read <notification-id> | --all",
    ),
    ("agent", "list", "flk agent list"),
    ("agent", "get", "flk agent get <target>"),
    (
        "agent",
        "read",
        "flk agent read <target> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]",
    ),
    ("agent", "send", "flk agent send <target> <text>"),
    ("agent", "rename", "flk agent rename <target> <name>|--clear"),
    ("agent", "focus", "flk agent focus <target>"),
    ("agent", "wait", super::agent::AGENT_WAIT_USAGE),
    ("agent", "attach", "flk agent attach <target> [--takeover]"),
    ("agent", "start", super::agent::AGENT_START_USAGE),
    ("agent", "fork", super::agent::AGENT_FORK_USAGE),
    ("agent", "hibernate", "flk agent hibernate <target>"),
    ("agent", "resume", "flk agent resume <target>"),
    (
        "msg",
        "send",
        "flk msg send (<target> | --agent ID) <text...> [--repo NAME] [--intent fyi|needs-reply|blocking] [--correlation-id ID] [--reply-to ID] [--from-agent ID] [-- <text starting with dashes>]",
    ),
    (
        "msg",
        "reply",
        "flk msg reply <correlation_id> <text...> [--intent fyi|needs-reply|blocking] [-- <text starting with dashes>]",
    ),
    ("msg", "list", "flk msg list [--pane TARGET]"),
    ("msg", "read", "flk msg read [--pane TARGET]"),
    ("msg", "status", "flk msg status <correlation_id>"),
    (
        "msg",
        "mute",
        "flk msg mute <seconds> [--pane TARGET] [--reason TEXT]   (0 clears)",
    ),
    ("mcp", "serve", "flk mcp serve"),
    ("terminal", "attach", "flk terminal attach <terminal_id> [--takeover]"),
    ("pane", "list", "flk pane list [--workspace <workspace_id>]"),
    ("pane", "get", "flk pane get <pane_id>"),
    ("pane", "rename", "flk pane rename <pane_id> <label>|--clear"),
    (
        "pane",
        "read",
        "flk pane read <pane_id> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]",
    ),
    (
        "pane",
        "split",
        "flk pane split <pane_id> --direction right|down [--cwd PATH] [--focus] [--no-focus]",
    ),
    (
        "pane",
        "move",
        "flk pane move <pane_id> --tab <tab_id> --split right|down [--target-pane ID] [--ratio FLOAT] [--focus|--no-focus]\n       flk pane move <pane_id> --new-tab [--workspace ID] [--label TEXT] [--focus|--no-focus]\n       flk pane move <pane_id> --new-workspace [--label TEXT] [--tab-label TEXT] [--focus|--no-focus]",
    ),
    ("pane", "close", "flk pane close <pane_id>"),
    ("pane", "send-text", "flk pane send-text <pane_id> <text>"),
    ("pane", "send-keys", "flk pane send-keys <pane_id> <key> [key ...]"),
    (
        "pane",
        "report-agent",
        "flk pane report-agent <pane_id> --source ID --agent LABEL --state idle|working|blocked|unknown [--message TEXT] [--custom-status TEXT] [--seq N] [--agent-session-id ID] [--agent-session-path PATH]",
    ),
    (
        "pane",
        "report-metadata",
        "flk pane report-metadata <pane_id> --source ID [--agent LABEL] [--applies-to-source ID] [--title TEXT|--clear-title] [--display-agent TEXT|--clear-display-agent] [--custom-status TEXT|--clear-custom-status] [--state-label STATUS=TEXT] [--clear-state-labels] [--seq N] [--ttl-ms N]",
    ),
    (
        "pane",
        "report-recap",
        "flk pane report-recap --source ID --agent LABEL --recap TEXT [--seq N] [--pane <pane_id>]",
    ),
    (
        "pane",
        "report-reply",
        "flk pane report-reply --source ID --agent LABEL --reply TEXT [--seq N] [--pane <pane_id>]",
    ),
    (
        "pane",
        "set-field",
        "flk pane set-field <key> <value> [--ttl <secs>] [--pane <pane_id>]",
    ),
    (
        "pane",
        "clear-field",
        "flk pane clear-field <key> [--pane <pane_id>]",
    ),
    ("pane", "run", "flk pane run <pane_id> <command>"),
    ("peers", "status", "flk peers status [--json]"),
    ("peers", "summary", "flk peers summary [--json]"),
    (
        "peers",
        "checkout-prepare",
        "flk peers checkout-prepare --workspace <id> [--push] [--json]",
    ),
    ("peers", "logs", "flk peers logs [--all] [--lines N] [--json]"),
    (
        "peers",
        "relay",
        "flk peers relay [--push]   (stdin/stdout API relay, for `ssh <peer> flk peers relay`)",
    ),
    (
        "report",
        "template",
        "flk report template [bug] > bug.md   write the form to edit",
    ),
    ("issue", "repos", "flk issue repos"),
    (
        "issue",
        "drop",
        "flk issue drop --repo owner/name --title <text> [--body-file <path>] [--label <name>] [--type <name>] [--file-it]",
    ),
    (
        "wait",
        "output",
        "flk wait output <pane_id> --match <text> [--source visible|recent|recent-unwrapped] [--lines N] [--timeout MS] [--regex] [--raw]",
    ),
    (
        "wait",
        "agent-status",
        "flk wait agent-status <pane_id> --status <idle|working|blocked|done|unknown> [--timeout MS]",
    ),
    (
        "integration",
        "install",
        "flk integration install <pi|omp|claude|codex|copilot|kimi|opencode|hermes|qodercli>",
    ),
    (
        "integration",
        "uninstall",
        "flk integration uninstall <pi|omp|claude|codex|copilot|kimi|opencode|hermes|qodercli>",
    ),
    ("integration", "status", "flk integration status [--outdated-only]"),
    (
        "integration",
        "manifest",
        "flk integration manifest <pi|omp|claude|codex|copilot|kimi|opencode|hermes|qodercli> [--json]",
    ),
    ("integration", "verify", "flk integration verify"),
    ("checks", "list", "flk checks list"),
    ("checks", "ack", "flk checks ack <name>"),
    ("checks", "run", "flk checks run <name>"),
    ("fleet", "pause", "flk fleet pause [--reason TEXT]"),
    ("fleet", "resume", "flk fleet resume"),
    ("fleet", "status", "flk fleet status"),
    ("session", "list", "flk session list [--json]"),
    ("session", "attach", "flk session attach <name>"),
    ("session", "stop", "flk session stop <name> [--json]"),
    ("session", "delete", "flk session delete <name> [--json]"),
];

/// Groups that take a target in the verb position instead of a verb, so the
/// first token after the group is an argument rather than a subcommand.
const POSITIONAL: &[(&str, &str)] = &[
    ("lineage", "flk lineage <target> [--json]"),
    (
        "digest",
        "flk digest [--since <duration>] [--path FILE] [--json]",
    ),
    (
        "preflight",
        "flk preflight [--require TARGET]...  (exit 0 ready, 1 advisory, 2 blocking)",
    ),
    ("revert-run", "flk revert-run <run-id> [--dry-run] [--json]"),
    (
        "hook",
        "flk hook <agent> <session|prompt|stop|working|idle|blocked|release>",
    ),
    (
        "web",
        "flk web [--bind <addr>]  (requires the `web` feature)",
    ),
];

/// Verbs whose trailing argument is literal text the caller means to deliver,
/// not a flag.
///
/// For these, `--help` is only a help request BEFORE the text begins. After
/// it, the token is content: `flk agent send w1 --help` and `flk pane
/// send-text 1:p1 --help` are two panes asking each other how to use flock, and
/// intercepting them would break the one path where the text is the point.
/// (`msg send` and `msg reply` are deliberately absent — their parsers already
/// refuse an unrecognised flag rather than taking it as body text, so nothing
/// is lost by answering `--help` there.)
const LITERAL_TEXT: &[(&str, &str)] = &[
    ("agent", "send"),
    ("agent", "rename"),
    ("pane", "send-text"),
    ("pane", "run"),
    ("pane", "rename"),
    ("tab", "rename"),
    ("workspace", "rename"),
];

fn is_help_flag(arg: &str) -> bool {
    HELP_FLAGS.contains(&arg)
}

fn verb_usage(group: &str, verb: &str) -> Option<&'static str> {
    VERBS
        .iter()
        .find(|(g, v, _)| *g == group && *v == verb)
        .map(|(_, _, usage)| *usage)
}

fn positional_usage(group: &str) -> Option<&'static str> {
    POSITIONAL
        .iter()
        .find(|(g, _)| *g == group)
        .map(|(_, usage)| *usage)
}

fn takes_literal_text(group: &str, verb: &str) -> bool {
    LITERAL_TEXT.iter().any(|(g, v)| *g == group && *v == verb)
}

/// The usage text for `args` when `args` is asking for help, and `None` when it
/// is not — in which case the verb's own parser gets the arguments untouched.
///
/// `args` is the whole argv, `args[0]` being the binary. Anything from a bare
/// `--` onwards is the caller's payload, not this verb's flags, so the scan
/// stops there: `flk agent start api -- claude --help` starts an agent whose
/// argv asks claude for its own help.
pub(super) fn help_usage(args: &[String]) -> Option<&'static str> {
    let group = args.get(1).map(String::as_str)?;
    let tail = args.get(2..)?;

    if let Some(usage) = positional_usage(group) {
        return asks_help_in(tail).then_some(usage);
    }

    let verb = tail.first()?.as_str();
    let usage = verb_usage(group, verb)?;
    let verb_args = &tail[1..];

    if takes_literal_text(group, verb) {
        // Only the leading argument can still be a flag; past it, it is text.
        return verb_args
            .first()
            .filter(|arg| is_help_flag(arg))
            .map(|_| usage);
    }

    asks_help_in(verb_args).then_some(usage)
}

fn asks_help_in(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| is_help_flag(arg))
}

#[cfg(test)]
mod tests {
    use super::{asks_help_in, help_usage, LITERAL_TEXT, POSITIONAL, VERBS};

    fn argv(words: &[&str]) -> Vec<String> {
        std::iter::once("flk")
            .chain(words.iter().copied())
            .map(String::from)
            .collect()
    }

    fn usage_for(words: &[&str]) -> Option<&'static str> {
        help_usage(&argv(words))
    }

    /// The reported trap, verbatim: `flk agent fork --help` used to take the
    /// flag as an agent target and answer `agent_not_found`.
    #[test]
    fn agent_fork_help_is_not_an_agent_target() {
        for flag in ["--help", "-h"] {
            let usage = usage_for(&["agent", "fork", flag]).expect("help, not a target");
            assert!(usage.starts_with("flk agent fork <target>"), "{usage}");
        }
    }

    /// The other half of the same report: probing `worktree create` allocated a
    /// git worktree and a branch. Usage must be reachable without a branch.
    #[test]
    fn worktree_create_help_is_reachable_and_allocates_nothing() {
        for flag in ["--help", "-h"] {
            let usage = usage_for(&["worktree", "create", flag]).expect("usage");
            assert!(usage.starts_with("flk worktree create"), "{usage}");
        }
    }

    /// "Anywhere in the argument list": a flag asked for after the value it
    /// would modify still asks for help, rather than being parsed as a bad
    /// value or an unknown option.
    #[test]
    fn help_is_recognised_anywhere_before_the_terminator() {
        assert!(usage_for(&["worktree", "create", "--branch", "w", "--help"]).is_some());
        assert!(usage_for(&["agent", "fork", "w1", "--label", "--help", "x"]).is_some());
        assert!(usage_for(&["worktree", "kill", "--path", "/w", "-h"]).is_some());
        assert!(usage_for(&["config", "check", "--path", "/w/config.toml", "--help"]).is_some());
    }

    /// A `--` terminator means the rest is the caller's payload. `agent start`
    /// and `msg send` both rely on it, so `--help` after it belongs to the
    /// child process or the message body, not to flock.
    #[test]
    fn help_after_the_terminator_belongs_to_the_payload() {
        assert!(usage_for(&["agent", "start", "api", "--", "claude", "--help"]).is_none());
        assert!(usage_for(&["msg", "send", "w1", "--", "--help"]).is_none());
        assert!(!asks_help_in(&argv(&["--", "--help"])));
    }

    /// The verbs that write literal text somewhere still have to be able to
    /// write `--help` there. Asking for help and meaning it are different
    /// questions, and only the position can tell them apart.
    #[test]
    fn literal_text_verbs_still_deliver_the_help_flag_as_text() {
        for (group, verb) in LITERAL_TEXT {
            assert!(
                usage_for(&[group, verb, "target", "--help"]).is_none(),
                "{group} {verb} must still deliver the literal text"
            );
            assert!(
                usage_for(&[group, verb, "--help"]).is_some(),
                "{group} {verb} must still answer a bare --help"
            );
        }
    }

    /// A typo must still be a typo. An unmapped verb is left to its own parser,
    /// so `flk worktree creat` keeps saying so instead of being answered with a
    /// help page for a command that does not exist.
    #[test]
    fn an_unknown_verb_is_not_answered_with_another_verbs_help() {
        assert!(usage_for(&["worktree", "creat", "--help"]).is_none());
        assert!(usage_for(&["not-a-group", "create", "--help"]).is_none());
    }

    /// Group-level help (`flk worktree --help`) is answered by the group
    /// itself, which prints its full command list. This table must not claim
    /// it, or one line would replace the list.
    #[test]
    fn group_help_is_left_to_the_group() {
        for group in ["worktree", "agent", "pane", "session", "wait"] {
            assert!(
                usage_for(&[group, "--help"]).is_none(),
                "{group} prints its own command list"
            );
        }
    }

    /// Every row names the verb it is keyed by. A row that had drifted into
    /// prose would answer `flk worktree create --help` with a sentence about
    /// worktrees.
    #[test]
    fn every_row_documents_the_verb_it_is_keyed_by() {
        for (group, verb, usage) in VERBS {
            assert!(
                usage.starts_with(&format!("flk {group} {verb}")),
                "{group} {verb} is documented as: {usage}"
            );
        }
        for (group, usage) in POSITIONAL {
            assert!(
                usage.starts_with(&format!("flk {group}")),
                "{group} is documented as: {usage}"
            );
        }
    }

    /// Two rows for one (group, verb) means one of them is dead weight that
    /// also silently decides which line answers.
    #[test]
    fn no_verb_is_listed_twice() {
        let mut seen: Vec<(&str, &str)> = VERBS.iter().map(|(g, v, _)| (*g, *v)).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count);
    }

    /// Every group `cli::maybe_run` dispatches is either in the verb table or
    /// in the positional table — otherwise one group quietly has no per-verb
    /// help while the rest of the CLI does.
    #[test]
    fn every_dispatched_group_is_documented() {
        for group in [
            "server",
            "status",
            "hook",
            "config",
            "channel",
            "workspace",
            "worktree",
            "tab",
            "notification",
            "agent",
            "lineage",
            "msg",
            "mcp",
            "terminal",
            "pane",
            "peers",
            "report",
            "issue",
            "wait",
            "integration",
            "preflight",
            "checks",
            "digest",
            "fleet",
            "revert-run",
            "session",
            "web",
        ] {
            let documented = POSITIONAL.iter().any(|(g, _)| *g == group)
                || VERBS.iter().any(|(g, _, _)| *g == group);
            assert!(documented, "{group} has no help entry at all");
        }
    }

    /// The allocation verbs are the ones this issue is about. Each answers a
    /// help request; a regression here is a worktree or an agent process
    /// created by someone asking how to use flock.
    #[test]
    fn every_allocating_verb_answers_help() {
        for invocation in [
            "worktree create",
            "worktree open",
            "worktree kill",
            "agent fork",
            "agent start",
            "agent resume",
            "agent hibernate",
            "workspace create",
            "tab create",
            "pane split",
            "pane run",
            "pane send-text",
            "pane send-keys",
            "session delete",
        ] {
            let (group, verb) = invocation.split_once(' ').expect("group verb");
            assert!(
                usage_for(&[group, verb, "--help"]).is_some(),
                "{invocation} must answer --help"
            );
        }
    }

    /// The end of the road: the verbs the table covers, asked for help, reach
    /// the dispatcher and are answered there — before any parser, and so
    /// before anything that would allocate.
    ///
    /// Running this through `maybe_run` rather than `help_usage` is the point.
    /// A unit test of the table proves the table; this proves the wiring, and
    /// it does it without a server: `worktree create` reaching its parser would
    /// try to open the session socket, which does not exist here, and the call
    /// would come back as an error rather than `Handled(0)`.
    #[test]
    fn the_dispatcher_answers_help_before_any_parser_runs() {
        use crate::cli::{maybe_run, CommandOutcome};

        for invocation in [
            ("worktree", "create"),
            ("worktree", "kill"),
            ("agent", "fork"),
            ("agent", "start"),
            ("pane", "split"),
            ("workspace", "create"),
        ] {
            let words = [invocation.0, invocation.1, "--help"];
            assert_eq!(
                maybe_run(&argv(&words)).expect("help path never touches the socket"),
                CommandOutcome::Handled(0),
                "flk {} must exit 0 with usage",
                words.join(" ")
            );
        }
    }
}
