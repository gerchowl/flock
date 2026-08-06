//! Diagnostic log extraction with an allowlist scrubber (#233).
//!
//! ## Why this is not `crate::logging::tail_session_logs`
//!
//! `tail_session_logs` decodes into [`crate::logging::LogLine`], whose `Wire`
//! struct (`src/logging.rs`) deserializes only `timestamp`/`level`/`target`/
//! `message` — every structured field is silently dropped. That is *safe* but
//! useless for triage: the decisive evidence in gerchowl/flock#232 was
//! `program=git` plus its error, and neither survives that parse. So reports
//! re-parse the raw JSONL and keep a bounded, explicitly-named set of fields.
//!
//! ## Allowlist, never denylist
//!
//! The identifying material lives in field VALUES, not in the message text:
//! `remote_bridge_started` emits `target`/`ssh_opts`/`ssh_config_file`/
//! `proxy_jump`/`remote_command`; `process_exec_completed` emits `args` with up
//! to 512 chars of the real command line (branches, remotes, cwd-derived
//! paths); several emitters log `path = %path.display()` verbatim. A denylist
//! over that surface fails open the moment someone adds an emitter. So
//! [`ALLOWED_FIELDS`] names what may leave the machine and everything else is
//! dropped unread — a new emitter's fields are excluded by default.
//!
//! Two fields are kept *and* scrubbed rather than dropped: `message` (the
//! static event string) and `err` (the failure text). `err` is the single most
//! diagnostic field in a bug report — dropping it would leave records that say
//! only "something failed" — so it is carried through [`Scrubber`] instead.
//! That is a deliberate, tested deviation from "drop anything free-text"; the
//! composed block is always previewed before it can be sent.

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Field names that may be carried out of a log record.
///
/// Every one of these is a low-cardinality enum-ish value or a bare program
/// name — never a path, host, branch, or user-authored string. Adding to this
/// list is a privacy decision, not a formatting one.
const ALLOWED_FIELDS: &[&str] = &[
    "event",
    "subsystem",
    "outcome",
    "status",
    "duration_ms",
    "elapsed_ms",
    "code",
    "protocol",
    "pid",
    "attempt",
    "count",
    "lines",
    "bytes",
    "kind",
    "state",
    "reason",
    // The program NAME only ("git", "ssh", "gh"). Its `args` are NOT allowed —
    // that is where the paths and branch names live.
    "program",
];

/// Allowed, but only after passing through [`Scrubber`].
const SCRUBBED_FIELDS: &[&str] = &["message", "err"];

/// One log record, reduced to what may be published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticRecord {
    pub ts: String,
    pub level: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Allowlisted fields in the order [`ALLOWED_FIELDS`]/[`SCRUBBED_FIELDS`]
    /// declare them, so two runs of the same event render identically.
    pub fields: Vec<(String, String)>,
}

impl DiagnosticRecord {
    fn render(&self) -> String {
        let mut out = format!("{} {:>5}", self.ts, self.level);
        if let Some(source) = &self.source {
            out.push_str(&format!(" [{source}]"));
        }
        for (key, value) in &self.fields {
            out.push_str(&format!(" {key}={value}"));
        }
        out
    }
}

/// Replaces machine- and person-identifying substrings in the values that are
/// allowed through. Constructed from the environment by [`Scrubber::from_env`];
/// the constructor taking explicit values keeps the rules unit-testable.
pub struct Scrubber {
    home: Option<String>,
    user: Option<String>,
}

impl Scrubber {
    pub fn new(home: Option<String>, user: Option<String>) -> Self {
        Self {
            // Trailing separators would leave "~/" as "~//".
            home: home
                .map(|home| home.trim_end_matches('/').to_string())
                .filter(|home| home.len() > 1),
            // Two-character UNIX account names are common in corporate
            // directories; filtering them out silently disabled masking for
            // exactly those users. Only an empty name is useless.
            user: user.filter(|user| !user.is_empty()),
        }
    }

    /// Build from the process environment, falling back to the password
    /// database.
    ///
    /// A server started by systemd or launchd frequently has neither `HOME` nor
    /// `USER` — flock's own #232 was exactly that class of stripped-environment
    /// bug. With both unset the scrubber silently became a no-op for the
    /// username and for any non-standard home (`/opt/appuser/...`,
    /// `/var/lib/...`), which the `/Users|/home` path rule does not cover.
    pub fn from_env() -> Self {
        let home = std::env::var("HOME").ok().or_else(passwd_home);
        let user = std::env::var("USER")
            .ok()
            .or_else(|| std::env::var("LOGNAME").ok())
            .or_else(passwd_name);
        Self::new(home, user)
    }

    pub fn scrub(&self, value: &str) -> String {
        let mut out = value.to_string();

        // Order matters. Credentials first, so a token embedded in a path is
        // masked before the path rules rewrite around it. The git-remote rule
        // must precede the email rule: `git@github.com:org/private.git` matches
        // as an address, which masks only the prefix and publishes the org and
        // repository in the tail.
        // Swap the public hosts out before the host rule runs, and back after.
        let mut preserved: Vec<(String, &str)> = Vec::new();
        for (index, host) in PUBLIC_HOSTS.iter().enumerate() {
            if out.contains(host) {
                let token = format!("\u{0}flkhost{index}\u{0}");
                out = out.replace(host, &token);
                preserved.push((token, host));
            }
        }

        for (pattern, replacement) in [
            (git_remote_re(), "<git-remote>"),
            (secret_token_re(), "<redacted-token>"),
            (labeled_secret_re(), "$1=<redacted>"),
            (email_re(), "<email>"),
            (ssh_target_re(), "<ssh-target>"),
            (ipv6_re(), "<ip>"),
            (ipv4_re(), "<ip>"),
            (private_host_re(), "<host>"),
        ] {
            out = pattern.replace_all(&out, replacement).to_string();
        }

        for (token, host) in preserved {
            out = out.replace(&token, host);
        }

        // This user's home first (most specific), then anyone else's.
        if let Some(home) = &self.home {
            out = out.replace(home.as_str(), "~");
        }
        out = home_path_re().replace_all(&out, "<path>").to_string();
        if let Some(user) = &self.user {
            out = out.replace(user.as_str(), "<user>");
        }
        out
    }
}

fn secret_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?x)
              gh[pousr]_[A-Za-z0-9]{16,}
            | github_pat_[A-Za-z0-9_]{20,}
            | sk-[A-Za-z0-9_\-]{20,}
            | xox[baprs]-[A-Za-z0-9\-]{10,}
            | AKIA[0-9A-Z]{16}
            | eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}
            ",
        )
        .expect("static secret-token pattern")
    })
}

fn labeled_secret_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // The label may be a SUFFIX of a longer identifier: `refresh_token`,
        // `access_token`, `client_secret`, `github_token`. `\b` does not help
        // there — `_` is a word character, so there is no boundary before
        // `token` in `refresh_token` and the rule never fires.
        Regex::new(
            r"(?i)(?:^|[^A-Za-z0-9])(\w*(?:authorization|bearer|token|password|passwd|secret|credentials?|cookie|session|api[-_]?key))\s*[=:]\s*\S+",
        )
        .expect("static labeled-secret pattern")
    })
}

fn email_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}")
            .expect("static email pattern")
    })
}

/// `user@host` as it appears in ssh targets. Runs after [`email_re`] so a real
/// address is already masked and cannot match here.
fn ssh_target_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b[A-Za-z0-9._\-]+@[A-Za-z0-9.\-]+\b").expect("static ssh-target pattern")
    })
}

/// Non-loopback IPv4. Loopback is kept — it is diagnostic and identifies nobody.
fn ipv4_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\b",
        )
        .expect("static ipv4 pattern")
    })
}

/// `git@host:org/repo.git` and `https://host/org/repo.git`.
///
/// Must run BEFORE the email rule, which matches the `git@host` prefix and
/// leaves the org and repository name — the confidential part — in place.
fn git_remote_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?x)
              \bgit@[A-Za-z0-9._\-]+:[^\s\x22]+
            | \bssh://[^\s\x22]+
            | \bhttps?://[A-Za-z0-9._\-]+/[A-Za-z0-9._\-]+/[A-Za-z0-9._\-]+\.git
            ",
        )
        .expect("static git-remote pattern")
    })
}

/// IPv6, bracketed or bare. Runs before the IPv4 rule so an embedded
/// dotted-quad tail cannot be masked first and strand the rest.
fn ipv6_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\[?(?:[0-9A-Fa-f]{1,4}:){2,7}[0-9A-Fa-f]{0,4}(?:%[A-Za-z0-9]+)?\]?")
            .expect("static ipv6 pattern")
    })
}

/// Hostnames that identify a network rather than a public service.
///
/// The `user@host` rule only fires when there is a `user@`, and `err` strings
/// overwhelmingly carry a bare FQDN instead ("Could not resolve hostname
/// bastion.internal.acme.corp"). Two shapes are masked: anything under an
/// unambiguously private suffix, and any name of three or more labels that is
/// not on the public allowlist.
///
/// Three labels is the threshold on purpose — it keeps `flock-server.log` and
/// `bug.yml` intact, and requiring an alphabetic final label keeps version
/// strings like `0.6.8-fork.85ce040` from being mistaken for hosts.
fn private_host_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?xi)
              \b[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+)*\.(?:corp|internal|intranet|local|lan|home|priv|private|arpa)\b
            | \b[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+){2,}\.[A-Za-z]{2,}\b
            ",
        )
        .expect("static private-host pattern")
    })
}

/// Public hosts that are diagnostic rather than identifying, and so survive
/// [`private_host_re`]. Restoring them after the fact keeps the pattern itself
/// simple — a negative lookahead is not available in this regex engine.
const PUBLIC_HOSTS: &[&str] = &[
    "api.github.com",
    "objects.githubusercontent.com",
    "raw.githubusercontent.com",
    "static.crates.io",
    "index.crates.io",
];

/// Another account's home directory — the current user's is rewritten to `~`
/// before this runs.
fn home_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:/Users|/home)/[A-Za-z0-9._\-]+(?:/[^\s\x22]*)?")
            .expect("static home-path pattern")
    })
}

/// Parse one JSONL log file into redacted records.
///
/// Non-JSON lines (legacy text records from rotated pre-JSONL generations, or
/// a torn tail) are skipped rather than best-effort parsed: a partially
/// understood line is exactly the case where an unrecognized field could ride
/// along unredacted.
pub fn extract(content: &str, source: &str, scrubber: &Scrubber) -> Vec<DiagnosticRecord> {
    content
        .lines()
        .filter_map(|line| extract_record(line, source, scrubber))
        .collect()
}

fn extract_record(line: &str, source: &str, scrubber: &Scrubber) -> Option<DiagnosticRecord> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;

    let ts = object.get("timestamp")?.as_str()?.to_string();
    let level = object.get("level")?.as_str()?.to_string();

    let mut fields: Vec<(String, String)> = Vec::new();

    // The tracing target, kept only when it is one of ours. `target` is also
    // the FIELD name `remote_bridge_started` uses for its ssh destination, and
    // `flatten_event(true)` puts both at the top level — so an ssh target can
    // land in this key. Requiring the `flk::`/`flock::` namespace rejects that
    // collision instead of publishing a hostname.
    if let Some(target) = object.get("target").and_then(|value| value.as_str()) {
        if target.starts_with("flk::") || target.starts_with("flock::") {
            fields.push(("target".into(), target.to_string()));
        }
    }

    for name in SCRUBBED_FIELDS {
        if let Some(raw) = object.get(*name).and_then(json_scalar) {
            let scrubbed = scrubber.scrub(&raw);
            if !scrubbed.is_empty() {
                fields.push(((*name).to_string(), quote_if_spaced(&scrubbed)));
            }
        }
    }

    for name in ALLOWED_FIELDS {
        if let Some(raw) = object.get(*name).and_then(json_scalar) {
            // Allowlisted values are low-cardinality by construction, but they
            // still pass through the scrubber — being on the list is not a
            // promise that a future emitter keeps the value boring.
            fields.push(((*name).to_string(), quote_if_spaced(&scrubber.scrub(&raw))));
        }
    }

    if fields.is_empty() {
        return None;
    }

    Some(DiagnosticRecord {
        ts,
        level,
        source: Some(source.to_string()),
        fields,
    })
}

fn json_scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// Make a value safe to place inside the rendered block.
///
/// The block is a fenced ```` ```text ```` region nested in `<details>`, and a
/// record is ONE line. A value carrying a fence, a closing `</details>`, or a
/// newline would otherwise break out of both containers and land as live
/// markdown in a public issue — GitHub strips `<script>` but renders
/// `<img src=x onerror=...>` happily, so this is injection, not cosmetics.
fn sanitize_for_block(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            // A record is one line by construction.
            '\n' | '\r' => out.push('\u{23ce}'),
            // Zero-width and bidi controls can hide the rest of a line from a
            // reviewer reading the preview.
            c if c.is_control() => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    // Break any fence or container-closing sequence with a zero-width space so
    // it renders as text and cannot terminate the block.
    out.replace("```", "`\u{200b}`\u{200b}`")
        .replace("~~~", "~\u{200b}~\u{200b}~")
        .replace("</details", "<\u{200b}/details")
        .replace("<summary", "<\u{200b}summary")
}

/// Cap a retained value.
///
/// `process.rs` already truncates logged args at 512 chars, but `err` and
/// `message` have no ceiling, and GitHub rejects issue bodies over 65 535
/// characters — one pathological record should not eat the whole budget.
fn cap(value: &str) -> String {
    const MAX: usize = 1_024;
    if value.chars().count() <= MAX {
        return value.to_string();
    }
    let kept: String = value.chars().take(MAX).collect();
    format!("{kept}...[truncated]")
}

fn quote_if_spaced(value: &str) -> String {
    let value = sanitize_for_block(&cap(value));
    if value.contains(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value
    }
}

/// Render records as the fenced block that goes in a report body.
pub fn render_block(records: &[DiagnosticRecord]) -> String {
    let mut out = String::from("```text\n");
    for record in records {
        out.push_str(&record.render());
        out.push('\n');
    }
    out.push_str("```\n");
    out
}

/// The effective user's home from the password database, for environments that
/// strip `HOME`.
fn passwd_home() -> Option<String> {
    passwd_field(|entry| entry.pw_dir)
}

/// The effective user's name from the password database, for environments that
/// strip `USER`.
fn passwd_name() -> Option<String> {
    passwd_field(|entry| entry.pw_name)
}

fn passwd_field(select: fn(&libc::passwd) -> *mut libc::c_char) -> Option<String> {
    // SAFETY: getpwuid returns a pointer into a static buffer owned by libc, or
    // null. The value is copied into an owned String before returning, so the
    // borrow does not outlive the call. Single-threaded use at report time.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() {
            return None;
        }
        let field = select(&*entry);
        if field.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(field)
            .to_str()
            .ok()
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrubber() -> Scrubber {
        Scrubber::new(Some("/Users/attacker".into()), Some("attacker".into()))
    }

    fn extract_one(line: &str) -> DiagnosticRecord {
        extract_record(line, "flock-server.log", &scrubber()).expect("record")
    }

    fn rendered(line: &str) -> String {
        extract_one(line).render()
    }

    #[test]
    fn keeps_the_fields_that_made_232_diagnosable() {
        let out = rendered(
            r#"{"timestamp":"2026-08-06T13:05:12.7Z","level":"ERROR","message":"process exec failed","event":"process.exec","subsystem":"worktree","program":"git","args":"-C /home/someone/Projects/cx remote get-url origin","err":"No such file or directory (os error 2)","target":"flk::logging"}"#,
        );
        assert!(out.contains("program=git"), "{out}");
        assert!(out.contains("event=process.exec"), "{out}");
        assert!(out.contains("subsystem=worktree"), "{out}");
        assert!(out.contains("No such file or directory"), "{out}");
    }

    #[test]
    fn drops_args_even_though_the_record_carries_them() {
        let out = rendered(
            r#"{"timestamp":"t","level":"ERROR","message":"process exec failed","program":"git","args":"-C /home/someone/Projects/secret-client remote get-url origin","target":"flk::logging"}"#,
        );
        assert!(!out.contains("args"), "{out}");
        assert!(!out.contains("secret-client"), "{out}");
    }

    #[test]
    fn unknown_fields_are_excluded_by_default() {
        // A future emitter's field must not ride along just because nobody
        // added it to a denylist.
        let out = rendered(
            r#"{"timestamp":"t","level":"INFO","message":"m","brand_new_field":"/Users/attacker/secret","target":"flk::logging"}"#,
        );
        assert!(!out.contains("brand_new_field"), "{out}");
        assert!(!out.contains("secret"), "{out}");
    }

    #[test]
    fn ssh_target_in_the_target_key_is_rejected() {
        // remote_bridge_started's `target` field collides with the tracing
        // target under flatten_event(true).
        let out = rendered(
            r#"{"timestamp":"t","level":"INFO","message":"remote bridge started","target":"deploy@internal.acme.corp"}"#,
        );
        assert!(!out.contains("acme.corp"), "{out}");
        assert!(!out.contains("deploy@"), "{out}");
    }

    #[test]
    fn scrubs_home_user_secrets_hosts_and_addresses() {
        let scrubber = scrubber();
        assert_eq!(scrubber.scrub("/Users/attacker/secret"), "~/secret");
        assert_eq!(scrubber.scrub("/home/someone/x"), "<path>");
        assert_eq!(
            scrubber.scrub("ghp_abcdefghijklmnopqrstuvwxyz01"),
            "<redacted-token>"
        );
        assert_eq!(scrubber.scrub("token=hunter2"), "token=<redacted>");
        // A `user@host` may be masked as either an address or an ssh target
        // depending on which rule matches first; what matters is that neither
        // the account nor the hostname survives.
        let masked = scrubber.scrub("ops@int.acme.corp");
        assert!(!masked.contains("ops"), "{masked}");
        assert!(!masked.contains("acme"), "{masked}");
        assert_eq!(scrubber.scrub("someone@example.com"), "<email>");
        assert_eq!(scrubber.scrub("10.1.2.3"), "<ip>");
        assert_eq!(scrubber.scrub("127.0.0.1"), "<ip>");
    }

    /// The canary the security review asked for: seed a record with material of
    /// every leak class and assert none of it survives.
    #[test]
    fn canary_record_leaks_nothing() {
        let line = r#"{"timestamp":"t","level":"ERROR","message":"remote install failed for attacker at /Users/attacker/secret with ghp_abcdefghijklmnopqrstuvwxyz01","event":"remote.install","subsystem":"remote","target":"flk::logging","dest":"/Users/attacker/.local/bin/flk","ssh_opts":"-o ProxyJump=bastion.int.acme.corp","remote_command":"flk server --token ghp_abcdefghijklmnopqrstuvwxyz01","path":"/Users/attacker/Projects/private-client","err":"auth failed for ops@int.acme.corp at 10.1.2.3"}"#;
        let out = rendered(line);
        for forbidden in [
            "attacker",
            "/Users/",
            "ghp_",
            "acme.corp",
            "bastion",
            "private-client",
            "10.1.2.3",
            "ProxyJump",
        ] {
            assert!(!out.contains(forbidden), "leaked {forbidden:?} in: {out}");
        }
        // Still diagnostic.
        assert!(out.contains("event=remote.install"), "{out}");
    }

    // ---- Red-team regressions -------------------------------------------
    //
    // Each of these was a demonstrated leak against the first implementation.

    #[test]
    fn attack_value_cannot_escape_the_markdown_fence() {
        let out = rendered(
            r#"{"timestamp":"t","level":"ERROR","message":"boom","target":"flk::logging","err":"parse failed near ```\n</details>\n\n# INJECTED\n<img src=x onerror=alert(1)>"}"#,
        );
        assert!(!out.contains("```"), "fence escaped: {out}");
        assert!(!out.contains("</details"), "container escaped: {out}");
        assert_eq!(
            out.lines().count(),
            1,
            "record broke onto several lines: {out}"
        );
    }

    #[test]
    fn attack_bare_internal_hostname_is_masked() {
        let out = rendered(
            r#"{"timestamp":"t","level":"WARN","message":"ssh bridge failed","target":"flk::logging","err":"Could not resolve hostname bastion.internal.acme.corp: nodename nor servname provided"}"#,
        );
        assert!(!out.contains("acme"), "{out}");
        assert!(!out.contains("bastion"), "{out}");
    }

    #[test]
    fn attack_ipv6_is_masked() {
        let out = rendered(
            r#"{"timestamp":"t","level":"WARN","message":"connect failed","target":"flk::logging","err":"connect to [2001:db8:85a3::8a2e:370:7334]:22 refused"}"#,
        );
        assert!(!out.contains("2001:db8"), "{out}");
    }

    #[test]
    fn attack_underscored_secret_labels_are_masked() {
        let scrubber = scrubber();
        for input in [
            "auth failed: refresh_token=1//0abcDEFghiJKL_opaque",
            "client_secret: s3cr3t-value",
            "github_token=abcdefghijklmnop",
            "Cookie: session=deadbeefcafe",
        ] {
            let masked = scrubber.scrub(input);
            assert!(masked.contains("<redacted>"), "not masked: {masked}");
        }
    }

    #[test]
    fn attack_scrubber_still_masks_when_home_and_user_are_unset() {
        // A server started by systemd/launchd has neither set — flock#232's
        // exact class of stripped-environment bug.
        let bare = Scrubber::new(None, None);
        let masked = bare.scrub("spawned for ops@int.acme.corp from /home/someone/x");
        assert!(!masked.contains("acme"), "{masked}");
        assert!(!masked.contains("someone"), "{masked}");
    }

    #[test]
    fn attack_git_remote_does_not_leak_org_or_repo() {
        let masked = scrubber().scrub("cloned git@github.com:acmecorp/prod-secrets-vault.git");
        assert!(!masked.contains("acmecorp"), "{masked}");
        assert!(!masked.contains("prod-secrets-vault"), "{masked}");
    }

    #[test]
    fn two_character_usernames_are_still_masked() {
        let masked = Scrubber::new(Some("/home/al".into()), Some("al".into()))
            .scrub("process spawned by al");
        assert!(!masked.contains(" al"), "{masked}");
    }

    #[test]
    fn oversized_values_are_capped() {
        let line = format!(
            r#"{{"timestamp":"t","level":"ERROR","message":"boom","target":"flk::logging","err":"{}"}}"#,
            "x".repeat(50_000)
        );
        let out = rendered(&line);
        assert!(out.len() < 2_000, "value not capped: {} chars", out.len());
        assert!(out.contains("[truncated]"), "{out}");
    }

    #[test]
    fn diagnostic_text_survives_the_new_rules() {
        // Over-masking would make the block useless. These must NOT be touched.
        let scrubber = scrubber();
        for keep in [
            "No such file or directory (os error 2)",
            "process exec completed",
            "flock-server.log",
            "flk::logging",
            "0.6.8-fork.85ce040",
            "protocol 24 vs 25",
        ] {
            assert_eq!(scrubber.scrub(keep), keep, "wrongly masked {keep:?}");
        }
    }

    #[test]
    fn non_json_lines_are_skipped_not_guessed() {
        assert!(extract_record("2026-01-01T00:00:00Z INFO flk: hello", "f", &scrubber()).is_none());
        assert!(extract_record("{\"partial\":", "f", &scrubber()).is_none());
    }

    #[test]
    fn extract_reads_a_whole_file() {
        let content = format!(
            "{}\n{}\n",
            r#"{"timestamp":"t1","level":"INFO","message":"a","target":"flk::logging"}"#,
            r#"{"timestamp":"t2","level":"WARN","message":"b","target":"flk::logging"}"#,
        );
        let records = extract(&content, "flock.log", &scrubber());
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].level, "WARN");
    }
}
