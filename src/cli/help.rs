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
//! what `--help` means. Three tests walk the table against the dispatchers and
//! the parsers to keep that cost honest.
//!
//! What the table deliberately does NOT hold is any command that already prints
//! a better answer: `flk web --help` is forty lines including the loopback and
//! tailscale-funnel posture for exposing a full shell, and `flk lineage --help`
//! is the only place that says what a target resolves from. Those six commands
//! (`SELF_SERVED` in the tests below names them) keep their own text and share
//! the RULE through [`asks_for_help`] — one predicate, no table row, no
//! downgrade. That is the same call made for `flk report bug`, applied to the
//! whole CLI rather than one verb.
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
        "flk workspace create [--cwd PATH] [--label TEXT] [--focus] [--no-focus] [--dry-run]",
    ),
    ("workspace", "get", "flk workspace get <workspace_id>"),
    ("workspace", "focus", "flk workspace focus <workspace_id>"),
    (
        "workspace",
        "rename",
        "flk workspace rename <workspace_id> <label>",
    ),
    ("workspace", "close", "flk workspace close <workspace_id>"),
    ("delegate", "start", super::delegate::START_USAGE),
    ("delegate", "send", super::delegate::SEND_USAGE),
    ("delegate", "wait", super::delegate::WAIT_USAGE),
    ("delegate", "result", super::delegate::RESULT_USAGE),
    ("delegate", "status", super::delegate::STATUS_USAGE),
    ("delegate", "reap", super::delegate::REAP_USAGE),
    (
        "worktree",
        "list",
        "flk worktree list [--workspace ID | --cwd PATH] [--scan] [--json]",
    ),
    (
        "worktree",
        "create",
        "flk worktree create [--workspace ID | --cwd PATH] [--branch NAME] [--base REF] [--path PATH] [--label TEXT] [--focus] [--no-focus] [--json] [--dry-run]",
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
        "flk tab create [--workspace <workspace_id>] [--cwd PATH] [--label TEXT] [--focus] [--no-focus] [--dry-run]\n  --workspace places the tab in that workspace; --cwd only sets the directory.",
    ),
    ("tab", "get", "flk tab get <tab_id>"),
    ("tab", "focus", "flk tab focus <tab_id>"),
    ("tab", "rename", "flk tab rename <tab_id> <label>"),
    ("tab", "close", "flk tab close <tab_id>"),
    (
        "notification",
        "show",
        "flk notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request]\nUses [ui.toast] delivery in the server config. delivery = \"off\" (the default) disables popups and returns reason \"disabled\".\nExit 0: shown; exit 3: not shown (disabled, busy, rate_limited, or no_foreground_client). JSON is printed in either case.\nNotifications are still recorded when not shown. Use flk notification list to read them.",
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
    ("agent", "send", super::agent::AGENT_SEND_USAGE),
    ("agent", "rename", "flk agent rename <target> <name>|--clear"),
    ("agent", "focus", "flk agent focus <target>"),
    ("agent", "wait", super::agent::AGENT_WAIT_USAGE),
    ("agent", "attach", "flk agent attach <target> [--takeover]"),
    ("agent", "start", super::agent::AGENT_START_USAGE),
    ("agent", "fork", super::agent::AGENT_FORK_USAGE),
    ("agent", "hibernate", "flk agent hibernate <target>"),
    ("agent", "resume", "flk agent resume <target>"),
    ("agent", "restart", super::agent::AGENT_RESTART_USAGE),
    ("agent", "history", super::agent::AGENT_HISTORY_USAGE),
    ("agent", "result", super::agent::AGENT_RESULT_USAGE),
    (
        "msg",
        "send",
        "flk msg send (<target> | --agent ID) <text...> [--repo NAME] [--intent fyi|needs-reply|blocking] [--correlation-id ID] [--reply-to ID] [--from-agent ID] [--await [--timeout MS]] [-- <text starting with dashes>]",
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
        super::pane::PANE_READ_USAGE,
    ),
    (
        "pane",
        "split",
        "flk pane split <pane_id> --direction right|down [--cwd PATH] [--focus] [--no-focus] [--dry-run]",
    ),
    (
        "pane",
        "move",
        "flk pane move <pane_id> --tab <tab_id> --split right|down [--target-pane ID] [--ratio FLOAT] [--focus|--no-focus]\n       flk pane move <pane_id> --new-tab [--workspace ID] [--label TEXT] [--focus|--no-focus]\n       flk pane move <pane_id> --new-workspace [--label TEXT] [--tab-label TEXT] [--focus|--no-focus]",
    ),
    ("pane", "close", super::pane::PANE_CLOSE_USAGE),
    ("pane", "send-text", super::pane::PANE_SEND_TEXT_USAGE),
    ("pane", "send-keys", super::pane::PANE_SEND_KEYS_USAGE),
    (
        "pane",
        "arm-self-compact",
        "flk pane arm-self-compact [--pane <pane_id>] [--abort] <handoff prompt>",
    ),
    (
        "pane",
        "report-agent",
        "flk pane report-agent <pane_id> --source ID --agent LABEL --state idle|working|blocked|unknown [--message TEXT] [--custom-status TEXT] [--seq N] [--agent-session-id ID] [--agent-session-path PATH]",
    ),
    (
        "pane",
        "report-metadata",
        "flk pane report-metadata [<pane_id>] [--pane <pane_id>] --source ID [--agent LABEL] [--applies-to-source ID] [--title TEXT|--clear-title] [--display-agent TEXT|--clear-display-agent] [--custom-status TEXT|--clear-custom-status] [--state-label STATUS=TEXT] [--clear-state-labels] [--seq N] [--ttl-ms N]",
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
    ("pane", "run", "flk pane run [--if-input-empty [--if-status idle|done] [--min-age-secs N] [--if-session ID]] <pane_id> <command>"),
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
        super::WAIT_AGENT_STATUS_USAGE,
    ),
    (
        "wait",
        "reply",
        "flk wait reply <correlation_id> [--timeout MS] [--json]",
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
///
/// `pane send-keys` is absent for a different reason: it shares the `args[1..]`
/// shape of its `send-text`/`run` siblings, but what it forwards are key NAMES
/// from a fixed vocabulary (`enter`, `ctrl+c`), and `--help` is not one of
/// them. There is no message anyone could mean by sending it, so answering with
/// usage loses nothing.
const LITERAL_TEXT: &[(&str, &str)] = &[
    ("agent", "send"),
    ("agent", "rename"),
    ("pane", "send-text"),
    ("pane", "run"),
    ("pane", "arm-self-compact"),
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

fn takes_literal_text(group: &str, verb: &str) -> bool {
    LITERAL_TEXT.iter().any(|(g, v)| *g == group && *v == verb)
}

/// Whether these arguments are asking for help.
///
/// The one definition of the rule, exported because six commands answer
/// `--help` from their own handler with their own — richer — text and still
/// have to agree on WHEN they are being asked: `lineage`, `digest`,
/// `preflight`, `revert-run`, `web` and `status`. Sharing the predicate is what
/// stops those six from drifting into a second, looser rule of their own.
///
/// Anything from a bare `--` onwards is the caller's payload rather than this
/// verb's flags, so the scan stops there: `flk agent start api -- claude
/// --help` starts an agent whose argv asks claude for its own help.
pub(super) fn asks_for_help(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| is_help_flag(arg))
}

/// The usage text for `args` when `args` is asking for help, and `None` when it
/// is not — in which case the verb's own parser gets the arguments untouched.
///
/// `args` is the whole argv, `args[0]` being the binary.
///
/// `None` is also the answer for every command that already prints its own
/// better help. `flk web --help` documents forty lines including the loopback
/// and tailscale-funnel posture; `flk lineage --help` is the only place that
/// says what a target resolves from. A table row would replace those with one
/// line, so the rule is shared and the text is not (see [`asks_for_help`]).
pub(super) fn help_usage(args: &[String]) -> Option<&'static str> {
    let group = args.get(1).map(String::as_str)?;
    let tail = args.get(2..)?;

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

    asks_for_help(verb_args).then_some(usage)
}

#[cfg(test)]
mod tests {
    use super::{asks_for_help, help_usage, LITERAL_TEXT, VERBS};

    #[test]
    fn pane_verb_help_documents_targets_and_precedence() {
        for verb in ["read", "send-text", "send-keys", "close"] {
            let usage = usage_for(&["pane", verb, "--help"]).unwrap();
            assert!(usage.contains(&format!("flk pane {verb} <target>")));
            assert!(usage.contains("pane id, terminal id, or unique agent name/label"));
            assert!(usage.contains("A pane id wins over a same-named agent"));
        }
    }

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
        assert!(!asks_for_help(&argv(&["--", "--help"])));
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

    /// Commands that answer `--help` from their own handler, with text richer
    /// than one usage line, and so are deliberately absent from the table.
    ///
    /// This is the `report bug` rule applied to the whole CLI: the table only
    /// covers commands with nothing better to print. A row here would be a
    /// downgrade, so these groups keep their own answer and share the rule
    /// through `asks_for_help`.
    ///
    /// Naming them here rather than only in the module docs is what keeps the
    /// choice reviewable: adding a seventh name is a visible act.
    const SELF_SERVED: &[&str] = &[
        // What a target resolves from, and how reused branch names disambiguate.
        "lineage",
        // `--since` suffixes and where the rendered file lands by default.
        "digest",
        // Why `--require` exists, and the exit-code legend.
        "preflight",
        // What a run-id looks like (`^Agent-Run: <id>$` trailers).
        "revert-run",
        // Every option, the config/env precedence, and the loopback +
        // tailscale-funnel posture for exposing a full shell.
        "web",
        // `hook` has no help of its own to keep — it is listed because it is a
        // verb this table does not cover, and because `flk hook --help` used to
        // exit 2 rather than answer.
        "hook",
    ];

    /// The source of the dispatcher, read at compile time so the group list
    /// below is DERIVED from it rather than transcribed.
    ///
    /// Transcribing the list was the bug this test used to have: adding a
    /// `"foo" =>` arm to `maybe_run` without a table entry left that group
    /// with no per-verb help and the test still green — the exact silent
    /// regression #455 exists to remove, one `mod` line away.
    const DISPATCH_SOURCE: &str = include_str!("../cli.rs");

    /// The match arms of `maybe_run`'s command dispatch.
    fn dispatched_groups() -> Vec<String> {
        let mut groups: Vec<String> = DISPATCH_SOURCE
            .lines()
            .skip_while(|line| !line.contains("let exit_code = match command"))
            .take_while(|line| !line.trim_start().starts_with("_ =>"))
            .filter_map(|line| {
                let name = line.trim().strip_prefix('"')?;
                let name = name.split('"').next()?;
                // `Some("x")` style arms and the `server` arm's inner match are
                // not command names; a command arm is exactly `"name" =>`.
                (line.contains("=>") && name.starts_with(|c: char| c.is_ascii_lowercase()))
                    .then(|| name.to_string())
            })
            .collect();
        groups.sort();
        groups.dedup();
        groups
    }

    /// The source of the module that owns a group's parser.
    fn group_parser_source(group: &str) -> &'static str {
        match group {
            "server" => include_str!("server.rs"),
            "status" => include_str!("status.rs"),
            "workspace" => include_str!("workspace.rs"),
            "worktree" => include_str!("worktree.rs"),
            "tab" => include_str!("tab.rs"),
            "notification" => include_str!("notification.rs"),
            "delegate" => include_str!("delegate.rs"),
            "agent" => include_str!("agent.rs"),
            "msg" => include_str!("msg.rs"),
            "mcp" => include_str!("mcp.rs"),
            "pane" => include_str!("pane.rs"),
            "peers" => include_str!("peers.rs"),
            "report" => include_str!("report.rs"),
            "issue" => include_str!("issue.rs"),
            "integration" => include_str!("integration.rs"),
            "checks" => include_str!("checks.rs"),
            "fleet" => include_str!("fleet.rs"),
            // These six live inline in the dispatcher itself.
            "config" | "channel" | "terminal" | "wait" | "session" | "hook" => {
                include_str!("../cli.rs")
            }
            _ => "",
        }
    }

    /// Every `--flag` literal the group's parser can accept.
    ///
    /// Deliberately an over-approximation on two axes, because narrowing either
    /// is the fragile source-scraping that stops being maintained: it is the
    /// whole module rather than the one verb, and it always includes
    /// `cli.rs` itself, where the shared parsers live (`--takeover` is parsed
    /// by `parse_attach_target` for both `agent attach` and `terminal
    /// attach`, not by either group's module). Over-approximating can only make
    /// the check below weaker, never wrong.
    fn parser_flags(group: &str) -> Vec<String> {
        let module = group_parser_source(group);
        let source: String = [module, DISPATCH_SOURCE].concat();
        let source = source.as_str();
        let mut flags: Vec<String> = source
            .match_indices('"')
            .step_by(2)
            .filter_map(|(index, _)| {
                source[index + 1..]
                    .find('"')
                    .map(|end| source[index + 1..index + 1 + end].to_string())
            })
            .filter(|value| value.starts_with("--") && value.len() > 2)
            .collect();
        flags.sort();
        flags.dedup();
        flags
    }

    /// The `--flag` tokens a usage row claims its verb accepts.
    ///
    /// Scans for the flag itself rather than splitting on whitespace: usage
    /// writes flags inside brackets (`[--scan] [--json]`), and a token-wise
    /// split drops every one of them — which is how this check came to pass
    /// while the row it exists to police was nonsense.
    fn row_flags(usage: &str) -> Vec<String> {
        let bytes = usage.as_bytes();
        let mut flags = Vec::new();
        let mut index = 0;
        while index + 1 < bytes.len() {
            if bytes[index] != b'-' || bytes[index + 1] != b'-' {
                index += 1;
                continue;
            }
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'-')
            {
                index += 1;
            }
            let flag = &usage[start..index];
            // A bare `--` is the terminator, not a flag.
            if flag.len() > 2 {
                flags.push(flag.to_string());
            }
        }
        flags
    }

    /// Every group `cli::maybe_run` dispatches is served — by the table or by
    /// its own richer handler. A new dispatch arm with neither is the silent
    /// regression this PR exists to remove, so the list is derived from the
    /// dispatcher's own source.
    #[test]
    fn every_dispatched_group_is_served() {
        let groups = dispatched_groups();
        assert!(
            groups.len() > 20,
            "the dispatch scan found nothing to check: {groups:?}"
        );
        for group in groups {
            let served =
                VERBS.iter().any(|(g, _, _)| *g == group) || SELF_SERVED.contains(&group.as_str());
            assert!(served, "{group} has no help entry anywhere");
        }
    }

    /// Every verb the table claims exists is a verb its dispatcher routes.
    ///
    /// The other direction of the same walk: a row left behind by a renamed or
    /// deleted verb is a help answer for a command that no longer exists.
    #[test]
    fn every_tabelled_verb_is_still_dispatched() {
        for (group, verb, _) in VERBS {
            let source = group_parser_source(group);
            assert!(
                source.contains(&format!("\"{verb}\" =>"))
                    || source.contains(&format!("Some(\"{verb}\")")),
                "{group} no longer routes a {verb} verb, but the help table does"
            );
        }
    }

    /// ONE DIRECTION ONLY — a row may not document a flag its parser dropped.
    ///
    /// `every_row_documents_the_verb_it_is_keyed_by` catches a row that drifted
    /// into prose. It cannot catch a row that documents a flag the parser no
    /// longer accepts — and 90 of the 93 rows are hand-copies, so that is the
    /// drift a reader would actually notice. This asserts that direction: every
    /// flag a row claims must be a flag that module accepts.
    ///
    /// ## What this test does NOT cover
    ///
    /// The reverse — a parser GAINED a flag the row omits — is **not** checked,
    /// and must not be read as checked. Asserting it means deciding that every
    /// accepted flag is documented, which the CLI does not currently hold: `peers
    /// logs` takes `-n` for `--lines` and documents neither, and the group help
    /// omits both. Asserting it would mean either failing on today's aliases or
    /// inventing an allowlist of undocumented flags, and an allowlist rots in
    /// exactly the way this file exists to avoid.
    ///
    /// So that half is a reviewer's eye, deliberately: adding `--foo` to a
    /// parser means adding it to the row in the same change. This test makes
    /// the other half impossible to get wrong; it does not make drift
    /// impossible, and a reader who takes it as complete coverage would be
    /// wrong.
    ///
    /// (The name says which direction on purpose. The first version of this
    /// check claimed to police the rows and silently did nothing — it split the
    /// usage line on whitespace and so dropped every `[--bracket]`-wrapped flag,
    /// which is nearly all of them. A guard that cannot fail is worse than no
    /// guard: it buys confidence it has not earned.)
    #[test]
    fn a_row_may_not_document_a_flag_the_parser_dropped() {
        for (group, verb, usage) in VERBS {
            let accepted = parser_flags(group);
            for flag in row_flags(usage) {
                assert!(
                    accepted.contains(&flag),
                    "{group} {verb} documents {flag}, which no longer appears \
                     anywhere in its parser"
                );
            }
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
            "pane arm-self-compact",
            "session delete",
            "delegate start",
            "delegate send",
            "delegate reap",
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
            ("delegate", "start"),
            ("delegate", "send"),
            ("delegate", "reap"),
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
