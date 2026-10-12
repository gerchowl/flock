//! The one exit-code registry (ADR-0029 §2, #1017).
//!
//! Every exit status a `flk` verb returns is named here, together with what it
//! means and whether retrying is safe. `flk --help` prints the table from
//! [`ALL`], and the tests below pin it and refuse a bare exit literal outside
//! it. Four cases moved to the codes ADR-0029 §2 assigns them (#1025), and
//! [`MOVED`] names the code each one had before.

/// Whether a caller that got this exit status may simply run the call again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// Success: there is nothing to retry.
    NotApplicable,
    /// The call itself is wrong, or the target is gone: fix or re-resolve.
    Never,
    /// Unclassified: it may work again, it may not.
    Maybe,
    /// Safe to run again as is.
    Yes,
    /// Nothing changed; run again once the reported condition clears.
    Later,
    /// The effect may have happened: read the state before retrying.
    AfterInspecting,
    /// A human has to act first.
    AfterHuman,
    /// The client and server disagree on the protocol: hand off first.
    AfterHandoff,
}

impl Retry {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Retry::NotApplicable => "n/a",
            Retry::Never => "never",
            Retry::Maybe => "maybe",
            Retry::Yes => "yes",
            Retry::Later => "later",
            Retry::AfterInspecting => "after_inspecting",
            Retry::AfterHuman => "after_human",
            Retry::AfterHandoff => "after_handoff",
        }
    }
}

/// One registered exit status.
pub(crate) struct Exit {
    #[cfg_attr(not(test), allow(dead_code))] // Read by the snapshot test.
    pub(crate) name: &'static str,
    pub(crate) code: i32,
    pub(crate) retry: Retry,
    pub(crate) meaning: &'static str,
}

macro_rules! exits {
    ($($name:ident = $code:literal, $retry:ident, $meaning:literal;)*) => {
        $(pub(crate) const $name: i32 = $code;)*
        /// Every registered exit status, in code order.
        pub(crate) const ALL: &[Exit] = &[
            $(Exit { name: stringify!($name), code: $code, retry: Retry::$retry, meaning: $meaning },)*
        ];
    };
}

exits! {
    SUCCESS = 0, NotApplicable, "success, effect observed";
    FAILURE = 1, Maybe, "unclassified failure (transport, internal)";
    CONFIG_CHECK_ADVISORY = 1, Never, "config check: loaded with warnings";
    USAGE = 2, Never, "usage error";
    WORKTREE_NOT_LINKED = 2, Never, "worktree kill / delegate reap: not a linked worktree, or no such workspace";
    CONFIG_CHECK_PARSE_FAILURE = 2, Never, "config check: the config did not load";
    NO_ANSWER = 3, AfterInspecting, "msg / wait reply: no answer is coming (deferred, expired, gone)";
    REPLY_BLOCKED = 3, AfterInspecting, "delegate wait: the agent answered BLOCKED";
    NOT_SHOWN = 3, Later, "notification show: recorded but not shown";
    WORKTREE_GATE_WOULD_FAIL = 3, Later, "worktree kill --dry-run: the merge gate would not pass";
    GONE = 4, Never, "target not found or gone";
    WORKTREE_NEEDS_FORCE = 4, AfterInspecting, "worktree kill / delegate reap: dirty or has submodules, --force clears it";
    NO_SENTINEL = 5, AfterInspecting, "delegate wait: finished without the expected result";
    AGENT_BLOCKED = 6, AfterHuman, "delegate wait: the agent needs a human";
    WAIT_BLOCKED = 6, AfterHuman, "agent wait / wait agent-status: the agent is blocked on a dialog";
    STALLED = 7, AfterInspecting, "waits, delegate wait: working, no progress past the stall window";
    UNCONFIRMED = 8, AfterInspecting, "send: effect uncertain, text may be in the composer";
    ABANDONED = 9, Yes, "send: effect abandoned, composer restored";
    REFUSED = 10, Later, "refused before acting, nothing changed";
    DELEGATE_NOT_STARTED = 11, AfterInspecting, "delegate start / wait: the delegate did not start";
    MSG_REFUSED = 13, Never, "msg: the receiver refused the message";
    OPERATOR_ONLY = 13, Never, "operator verb: refused for an agent or relayed caller (ADR-0014 boundary)";
    RECONNECT_GAVE_UP = 75, Later, "client gave up reconnecting to its server (EX_TEMPFAIL)";
    PROTOCOL_GAP = 78, AfterHandoff, "client/server protocol gap";
    TIMEOUT = 124, Yes, "timed out";
}

/// The cases that moved code in #1025, as `(new, old)`. For one release a
/// moved case names its old code on stderr, through [`moved`].
pub(crate) const MOVED: &[(i32, i32)] = &[
    (REFUSED, 1),
    (WAIT_BLOCKED, 3),
    (DELEGATE_NOT_STARTED, 8),
    (MSG_REFUSED, 4),
];

/// The stderr notice for a moved case's code, if `code` is one.
pub(crate) fn move_notice(code: i32) -> Option<String> {
    let (new, old) = MOVED.iter().find(|(new, _)| *new == code)?;
    Some(format!(
        "note: exit code is now {new} (was {old}); see CHANGELOG"
    ))
}

/// Return a moved case's code, printing its notice on stderr. Only a moved
/// case may come through here: code 6 also means `AGENT_BLOCKED`, which did not
/// move and prints nothing.
pub(crate) fn moved(code: i32) -> i32 {
    if let Some(notice) = move_notice(code) {
        eprintln!("{notice}");
    }
    code
}

/// The `Exit codes:` section of `flk --help`.
pub(crate) fn help_section() -> String {
    let mut out = String::from("Exit codes:\n");
    for exit in ALL {
        out.push_str(&format!(
            "  {:>3}  {} (retry: {})\n",
            exit.code,
            exit.meaning,
            exit.retry.as_str()
        ));
    }
    out.push_str("Moved this release (the old code is named on stderr):");
    for (new, old) in MOVED {
        out.push_str(&format!(" {old} -> {new}"));
    }
    out.push('\n');
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::ALL;
    use std::path::Path;

    /// The registry is a contract: a code may be added here, never renamed,
    /// renumbered or dropped without this snapshot changing in the same diff.
    #[test]
    fn every_registered_exit_code_and_its_retry_class() {
        let actual: Vec<String> = ALL
            .iter()
            .map(|e| format!("{} {} {}", e.code, e.name, e.retry.as_str()))
            .collect();
        let expected = [
            "0 SUCCESS n/a",
            "1 FAILURE maybe",
            "1 CONFIG_CHECK_ADVISORY never",
            "2 USAGE never",
            "2 WORKTREE_NOT_LINKED never",
            "2 CONFIG_CHECK_PARSE_FAILURE never",
            "3 NO_ANSWER after_inspecting",
            "3 REPLY_BLOCKED after_inspecting",
            "3 NOT_SHOWN later",
            "3 WORKTREE_GATE_WOULD_FAIL later",
            "4 GONE never",
            "4 WORKTREE_NEEDS_FORCE after_inspecting",
            "5 NO_SENTINEL after_inspecting",
            "6 AGENT_BLOCKED after_human",
            "6 WAIT_BLOCKED after_human",
            "7 STALLED after_inspecting",
            "8 UNCONFIRMED after_inspecting",
            "9 ABANDONED yes",
            "10 REFUSED later",
            "11 DELEGATE_NOT_STARTED after_inspecting",
            "13 MSG_REFUSED never",
            "13 OPERATOR_ONLY never",
            "75 RECONNECT_GAVE_UP later",
            "78 PROTOCOL_GAP after_handoff",
            "124 TIMEOUT yes",
        ];
        assert_eq!(actual, expected);
    }

    /// Each moved case names its old code, and only a moved code has a notice.
    #[test]
    fn every_moved_code_names_its_old_one() {
        let notices: Vec<_> = [10, 6, 11, 13]
            .into_iter()
            .map(|code| super::move_notice(code).unwrap())
            .collect();
        assert_eq!(
            notices,
            [
                "note: exit code is now 10 (was 1); see CHANGELOG",
                "note: exit code is now 6 (was 3); see CHANGELOG",
                "note: exit code is now 11 (was 8); see CHANGELOG",
                "note: exit code is now 13 (was 4); see CHANGELOG",
            ]
        );
        for code in [0, 1, 2, 3, 4, 5, 7, 8, 9, 124] {
            assert_eq!(super::move_notice(code), None, "{code}");
        }
        assert!(super::help_section().contains(" 1 -> 10 3 -> 6 8 -> 11 4 -> 13\n"));
    }

    #[test]
    fn help_lists_every_registered_code() {
        let help = super::help_section();
        for exit in ALL {
            assert!(help.contains(exit.meaning), "{} missing", exit.name);
        }
    }

    /// Non-test Rust source under `src/`, with any trailing `#[cfg(test)] mod`
    /// cut off.
    pub(crate) fn production_sources() -> Vec<(String, String)> {
        fn walk(dir: &Path, test_mod: &regex::Regex, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if path.is_dir() {
                    walk(&path, test_mod, out);
                } else if name.ends_with(".rs")
                    && !name.ends_with("tests.rs")
                    && name != "test_support.rs"
                {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let end = test_mod.find(&text).map_or(text.len(), |m| m.start());
                    out.push((path.display().to_string(), text[..end].to_string()));
                }
            }
        }
        let test_mod =
            regex::Regex::new(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")
                .unwrap();
        let mut out = Vec::new();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        walk(&src, &test_mod, &mut out);
        out
    }

    /// Bare `Ok(<n>)` returns in the CLI that predate the registry, per file.
    /// They all carry registered values. A count above its pin fails, and so
    /// does any bare return in a file missing from this list. A count below its
    /// pin only prints a reminder to lower it, so a branch that removes some
    /// does not break the others in flight.
    const GRANDFATHERED_BARE_CLI_RETURNS: &[(&str, usize)] = &[
        ("src/cli.rs", 68),
        ("src/cli/agent.rs", 57),
        ("src/cli/checks.rs", 8),
        ("src/cli/delegate.rs", 5),
        ("src/cli/digest.rs", 6),
        ("src/cli/fleet.rs", 7),
        ("src/cli/hook.rs", 7),
        ("src/cli/integration.rs", 22),
        ("src/cli/issue.rs", 16),
        ("src/cli/lineage.rs", 4),
        ("src/cli/mcp.rs", 3),
        ("src/cli/msg.rs", 26),
        ("src/cli/notification.rs", 12),
        ("src/cli/pane.rs", 73),
        ("src/cli/peers.rs", 16),
        ("src/cli/preflight.rs", 1),
        ("src/cli/ready.rs", 1),
        ("src/cli/report.rs", 14),
        ("src/cli/revert.rs", 4),
        ("src/cli/status.rs", 7),
        ("src/cli/tab.rs", 16),
        ("src/cli/workspace.rs", 14),
        ("src/cli/worktree.rs", 38),
    ];

    #[test]
    fn no_bare_or_unregistered_exit_literal_in_src() {
        let exit_call = regex::Regex::new(r"process::exit\(\s*(\d+)\s*\)").unwrap();
        let cli_return = regex::Regex::new(r"\bOk\((\d+)\)").unwrap();
        // A literal as a match arm's value, or as both branches of an `if`.
        // In the CLI those are exit codes, and none is grandfathered.
        let cli_arm = regex::Regex::new(r"=>\s*(\d+)\s*,").unwrap();
        let cli_if_else = regex::Regex::new(r"\{\s*(\d+)\s*\}\s*else\s*\{\s*(\d+)\s*\}").unwrap();
        let root = format!("{}/", env!("CARGO_MANIFEST_DIR"));
        let mut offences = Vec::new();
        let mut bare_cli_returns = Vec::new();
        for (path, text) in production_sources() {
            let path = path.strip_prefix(&root).unwrap_or(&path).to_string();
            for m in exit_call.captures_iter(&text) {
                offences.push(format!("{path}: process::exit({})", &m[1]));
            }
            if !(path.starts_with("src/cli/") || path == "src/cli.rs") {
                continue;
            }
            for m in cli_arm.captures_iter(&text) {
                offences.push(format!("{path}: match arm => {}", &m[1]));
            }
            for m in cli_if_else.captures_iter(&text) {
                offences.push(format!("{path}: {{ {} }} else {{ {} }}", &m[1], &m[2]));
            }
            let mut count = 0;
            for m in cli_return.captures_iter(&text) {
                count += 1;
                let code: i32 = m[1].parse().unwrap();
                if !ALL.iter().any(|e| e.code == code) {
                    offences.push(format!("{path}: Ok({code}) is not a registered exit code"));
                }
            }
            if count > 0 {
                bare_cli_returns.push((path, count));
            }
        }
        assert!(
            offences.is_empty(),
            "use a cli::exit constant: {offences:#?}"
        );
        // Only a rise fails: a sibling change that removes a bare return must
        // not break every other branch. A drop prints a reminder to lower the pin.
        let mut grown = Vec::new();
        for (path, count) in &bare_cli_returns {
            let pinned = GRANDFATHERED_BARE_CLI_RETURNS
                .iter()
                .find(|(pinned_path, _)| pinned_path == path)
                .map_or(0, |(_, pinned)| *pinned);
            if *count > pinned {
                grown.push(format!("{path}: {count} bare Ok(<n>), pinned {pinned}"));
            } else if *count < pinned {
                eprintln!(
                    "reminder: {path} is down to {count} bare Ok(<n>), lower its pin from {pinned}"
                );
            }
        }
        for (path, pinned) in GRANDFATHERED_BARE_CLI_RETURNS {
            if !bare_cli_returns.iter().any(|(p, _)| p == path) {
                eprintln!("reminder: {path} has no bare Ok(<n>) left, drop its pin of {pinned}");
            }
        }
        assert!(
            grown.is_empty(),
            "bare Ok(<n>) exit returns grew in src/cli: use a cli::exit constant \
             instead: {grown:#?}"
        );
    }
}
