//! The one exit-code registry (ADR-0029 §2, #1017).
//!
//! Every exit status a `flk` verb returns is named here, together with what it
//! means and whether retrying is safe. The values are today's: three meanings
//! still share a code with another verb's meaning (`WAIT_BLOCKED`,
//! `MSG_REFUSED`, `DELEGATE_NOT_STARTED`), and moving them to the codes the ADR
//! assigns is #1025, not this table. `flk --help` prints the table from [`ALL`],
//! and the tests below pin it and refuse a bare exit literal outside it.

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
    FAILURE = 1, Maybe, "unclassified failure (transport, internal, refused before acting)";
    CONFIG_CHECK_ADVISORY = 1, Never, "config check: loaded with warnings";
    USAGE = 2, Never, "usage error";
    WORKTREE_NOT_LINKED = 2, Never, "worktree kill / delegate reap: not a linked worktree, or no such workspace";
    CONFIG_CHECK_PARSE_FAILURE = 2, Never, "config check: the config did not load";
    NO_ANSWER = 3, AfterInspecting, "msg / wait reply: no answer is coming (deferred, expired, gone)";
    WAIT_BLOCKED = 3, AfterHuman, "agent wait / wait agent-status: the agent is blocked on a dialog";
    NOT_SHOWN = 3, Later, "notification show: recorded but not shown";
    WORKTREE_GATE_WOULD_FAIL = 3, Later, "worktree kill --dry-run: the merge gate would not pass";
    GONE = 4, Never, "target not found or gone";
    MSG_REFUSED = 4, Never, "msg: the receiver refused the message";
    WORKTREE_NEEDS_FORCE = 4, AfterInspecting, "worktree kill / delegate reap: dirty or has submodules, --force clears it";
    NO_SENTINEL = 5, AfterInspecting, "delegate wait: finished without the expected result";
    AGENT_BLOCKED = 6, AfterHuman, "delegate wait: the agent needs a human";
    STALLED = 7, AfterInspecting, "waits, delegate wait: working, no progress past the stall window";
    UNCONFIRMED = 8, AfterInspecting, "send: effect uncertain, text may be in the composer";
    DELEGATE_NOT_STARTED = 8, AfterInspecting, "delegate start / wait: the delegate did not start";
    ABANDONED = 9, Yes, "send: effect abandoned, composer restored";
    RECONNECT_GAVE_UP = 75, Later, "client gave up reconnecting to its server (EX_TEMPFAIL)";
    PROTOCOL_GAP = 78, AfterHandoff, "client/server protocol gap";
    TIMEOUT = 124, Yes, "timed out";
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
            "3 WAIT_BLOCKED after_human",
            "3 NOT_SHOWN later",
            "3 WORKTREE_GATE_WOULD_FAIL later",
            "4 GONE never",
            "4 MSG_REFUSED never",
            "4 WORKTREE_NEEDS_FORCE after_inspecting",
            "5 NO_SENTINEL after_inspecting",
            "6 AGENT_BLOCKED after_human",
            "7 STALLED after_inspecting",
            "8 UNCONFIRMED after_inspecting",
            "8 DELEGATE_NOT_STARTED after_inspecting",
            "9 ABANDONED yes",
            "75 RECONNECT_GAVE_UP later",
            "78 PROTOCOL_GAP after_handoff",
            "124 TIMEOUT yes",
        ];
        assert_eq!(actual, expected);
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
    /// They all carry registered values. A count may only go down: lower it
    /// when you replace one with a named code, and never raise it. A file
    /// missing from this list may hold none.
    const GRANDFATHERED_BARE_CLI_RETURNS: &[(&str, usize)] = &[
        ("src/cli.rs", 72),
        ("src/cli/agent.rs", 63),
        ("src/cli/checks.rs", 8),
        ("src/cli/delegate.rs", 5),
        ("src/cli/digest.rs", 6),
        ("src/cli/fleet.rs", 7),
        ("src/cli/hook.rs", 7),
        ("src/cli/integration.rs", 22),
        ("src/cli/issue.rs", 16),
        ("src/cli/lineage.rs", 4),
        ("src/cli/mcp.rs", 3),
        ("src/cli/msg.rs", 28),
        ("src/cli/notification.rs", 12),
        ("src/cli/pane.rs", 74),
        ("src/cli/peers.rs", 16),
        ("src/cli/preflight.rs", 1),
        ("src/cli/ready.rs", 1),
        ("src/cli/report.rs", 14),
        ("src/cli/revert.rs", 4),
        ("src/cli/server.rs", 6),
        ("src/cli/status.rs", 7),
        ("src/cli/tab.rs", 16),
        ("src/cli/workspace.rs", 14),
        ("src/cli/worktree.rs", 39),
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
        bare_cli_returns.sort();
        let expected: Vec<(String, usize)> = GRANDFATHERED_BARE_CLI_RETURNS
            .iter()
            .map(|(path, count)| (path.to_string(), *count))
            .collect();
        assert_eq!(
            bare_cli_returns, expected,
            "bare Ok(<n>) exit returns per file in src/cli changed: use a cli::exit \
             constant, and lower the grandfathered count when you remove one"
        );
    }
}
