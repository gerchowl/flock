//! Destination resolution and prefilled-URL construction (#233).

use super::template::{Form, FormField};

/// Hard ceiling on the whole URL.
///
/// Measured against github.com rather than guessed: 4 079 chars answered 302
/// normally, 7 079 returned 500, 8 079 reset the connection, and 8 279+
/// returned 414. Browsers tolerate far more (Chrome/Firefox ~32 KB) — GitHub's
/// server cap bites first. 7 500 leaves room under the first failing size.
pub const MAX_URL_LEN: usize = 7_500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoError {
    /// No repository is compiled into this binary and none was passed.
    Unknown,
    /// A `--repo` value that is not `owner/name`.
    Malformed(String),
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(
                f,
                "this build has no issue destination compiled in; pass --repo owner/name"
            ),
            Self::Malformed(value) => write!(f, "expected owner/name, got {value:?}"),
        }
    }
}

/// The destination compiled into this binary.
///
/// Read from `CARGO_PKG_REPOSITORY` — i.e. `Cargo.toml`'s `repository` field —
/// which is present on **every** build path (Nix, crates.io, `cargo install
/// --git`, release CI) and needs no build script.
///
/// A `build.rs` running `git remote get-url origin` was considered and
/// rejected: `nix/package.nix`'s fileset excludes `.git` entirely, the
/// crates.io package has no `.git`, and `cargo install --git <fork-url>` would
/// bake whichever fork the user happened to type — silently filing reports to
/// the wrong tracker, which is the worst outcome available here.
pub fn baked_repo() -> Option<String> {
    normalize(option_env!("CARGO_PKG_REPOSITORY")?)
}

/// Resolve the destination.
///
/// **The environment is deliberately not consulted.** An env var would let a
/// `.envrc`, direnv config, or shell profile silently redirect every report —
/// and its diagnostics — to a repository the reporter never chose. An override
/// has to be typed on the command line, where it is visible in shell history
/// and in the invocation itself.
pub fn resolve(cli_repo: Option<&str>) -> Result<String, RepoError> {
    match cli_repo {
        Some(value) => normalize(value).ok_or_else(|| RepoError::Malformed(value.to_string())),
        None => baked_repo().ok_or(RepoError::Unknown),
    }
}

/// Reduce a repository URL or `owner/name` pair to `owner/name`.
fn normalize(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches('/');
    let value = value.strip_suffix(".git").unwrap_or(value);

    let tail = value
        .rsplit_once("github.com")
        .map(|(_, tail)| tail.trim_start_matches(['/', ':']))
        .unwrap_or(value);

    let mut parts = tail.split('/').filter(|part| !part.is_empty());
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !valid(owner) || !valid(name) {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefilledUrl {
    pub url: String,
    /// Some prose was shortened to stay under [`MAX_URL_LEN`]. The caller must
    /// say so out loud — a silently truncated report reads as complete.
    pub truncated: bool,
}

/// Build the prefilled issue-form URL.
///
/// `values` are `(field id, value)` pairs. Diagnostics are **never** passed
/// here: GitHub records full request URLs server-side, so anything in the query
/// string is disclosed at the moment the browser opens — before the reporter
/// has decided to submit. Log tails go to the clipboard for a deliberate paste
/// instead.
pub fn build(repo: &str, form: &Form, values: &[(&str, String)]) -> PrefilledUrl {
    // `blank_issues_enabled: false` means a bare /issues/new lands on the
    // chooser, not a form — `template=` is not optional.
    let base = format!(
        "https://github.com/{repo}/issues/new?template={}",
        encode(form.template_file)
    );

    let mut budget = MAX_URL_LEN.saturating_sub(base.len());
    let mut query = String::new();
    let mut truncated = false;

    for (id, value) in values {
        if value.trim().is_empty() {
            continue;
        }
        let prefix = format!("&{}=", encode(id));
        let mut encoded = encode(value);
        let cost = prefix.len() + encoded.len();
        if cost > budget {
            const NOTE: &str = "\n\n[truncated by flk report — full text not sent]";
            let room = budget.saturating_sub(prefix.len() + encode(NOTE).len());
            if room == 0 {
                truncated = true;
                continue;
            }
            encoded = truncate_encoded(&encoded, room);
            encoded.push_str(&encode(NOTE));
            truncated = true;
        }
        budget = budget.saturating_sub(prefix.len() + encoded.len());
        query.push_str(&prefix);
        query.push_str(&encoded);
    }

    PrefilledUrl {
        url: format!("{base}{query}"),
        truncated,
    }
}

/// Cut an already-encoded string without splitting a `%XX` triplet.
fn truncate_encoded(encoded: &str, room: usize) -> String {
    let mut cut = encoded.len().min(room);
    while cut > 0 && !encoded.is_char_boundary(cut) {
        cut -= 1;
    }
    // Back off out of a percent escape.
    let bytes = encoded.as_bytes();
    while cut > 0 {
        let tail = &bytes[cut.saturating_sub(2)..cut];
        if tail.contains(&b'%') || (cut >= 1 && bytes[cut - 1] == b'%') {
            cut -= 1;
        } else {
            break;
        }
    }
    encoded[..cut].to_string()
}

/// Percent-encode for `application/x-www-form-urlencoded` values.
///
/// Space becomes `%20` rather than `+` so the value round-trips regardless of
/// how the receiver decodes, and `+` itself is escaped so it is never read back
/// as a space.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Gate for anything handed to the platform URL opener.
///
/// `crate::platform::open_url` passes its argument straight to `open`/
/// `xdg-open` with no `--` separator, so a value beginning with `-` would be
/// read as an option (`open` honours `-a`, `-b`, `-g`). Refusing anything that
/// is not an `https://github.com/` URL closes that off at this call site
/// regardless of what the opener does.
pub fn is_safe_to_open(url: &str) -> bool {
    url.starts_with("https://github.com/") && !url.contains(['\n', '\r', '\0', ' '])
}

/// Which fields carry human prose, paired with the values supplied.
pub fn prose_pairs<'a>(
    fields: impl Iterator<Item = &'a FormField>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(&'a str, String)> {
    fields
        .filter_map(|field| lookup(field.id).map(|value| (field.id, value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::template::{form, ReportKind};

    #[test]
    fn normalizes_every_shape_of_repo_reference() {
        for input in [
            "https://github.com/gerchowl/flock",
            "https://github.com/gerchowl/flock.git",
            "https://github.com/gerchowl/flock/",
            "git@github.com:gerchowl/flock.git",
            "gerchowl/flock",
        ] {
            assert_eq!(
                normalize(input).as_deref(),
                Some("gerchowl/flock"),
                "{input}"
            );
        }
    }

    #[test]
    fn rejects_malformed_repo_values() {
        for input in ["flock", "a/b/c", "", "owner/na me", "owner/na;me"] {
            assert_eq!(normalize(input), None, "{input}");
        }
    }

    #[test]
    fn this_build_knows_its_destination() {
        // Cargo.toml declares `repository`, so every build path resolves.
        assert_eq!(baked_repo().as_deref(), Some("gerchowl/flock"));
    }

    #[test]
    fn environment_cannot_redirect_a_report() {
        // The exfil path the security review flagged: a .envrc exporting a
        // destination would otherwise reroute reports and their diagnostics.
        std::env::set_var("FLOCK_ISSUE_REPO", "attacker/mirror");
        let resolved = resolve(None).expect("baked destination");
        std::env::remove_var("FLOCK_ISSUE_REPO");
        assert_eq!(resolved, "gerchowl/flock");
    }

    #[test]
    fn cli_override_is_honoured() {
        assert_eq!(resolve(Some("someone/fork")).unwrap(), "someone/fork");
        assert!(matches!(
            resolve(Some("nonsense")),
            Err(RepoError::Malformed(_))
        ));
    }

    #[test]
    fn builds_a_prefilled_form_url() {
        let built = build(
            "gerchowl/flock",
            form(ReportKind::Bug),
            &[
                ("current-behavior", "panes freeze".into()),
                ("reproduction", "run `flk` then split".into()),
            ],
        );
        assert!(built
            .url
            .starts_with("https://github.com/gerchowl/flock/issues/new?template=bug.yml"));
        assert!(built.url.contains("&current-behavior=panes%20freeze"));
        assert!(built.url.contains("%60flk%60"), "{}", built.url);
        assert!(!built.truncated);
    }

    #[test]
    fn skips_empty_values() {
        let built = build(
            "o/r",
            form(ReportKind::Bug),
            &[("impact", "   ".into()), ("reproduction", "x".into())],
        );
        assert!(!built.url.contains("impact"));
        assert!(built.url.contains("reproduction=x"));
    }

    #[test]
    fn stays_under_the_measured_github_cap() {
        let built = build(
            "gerchowl/flock",
            form(ReportKind::Bug),
            &[("current-behavior", "x".repeat(40_000))],
        );
        assert!(built.truncated);
        assert!(
            built.url.len() <= MAX_URL_LEN,
            "url was {} chars",
            built.url.len()
        );
        assert!(built.url.contains("truncated"));
    }

    #[test]
    fn truncation_never_splits_a_percent_escape() {
        for len in 1..64 {
            let built = build("o/r", form(ReportKind::Bug), &[("impact", "é".repeat(len))]);
            // Every '%' must be followed by two hex digits.
            let bytes = built.url.as_bytes();
            for (index, byte) in bytes.iter().enumerate() {
                if *byte == b'%' {
                    assert!(index + 2 < bytes.len(), "dangling escape in {}", built.url);
                }
            }
        }
    }

    #[test]
    fn open_gate_rejects_option_shaped_and_foreign_urls() {
        assert!(is_safe_to_open(
            "https://github.com/gerchowl/flock/issues/new?template=bug.yml"
        ));
        for bad in [
            "-a/Applications/Calculator.app",
            "--version",
            "file:///etc/passwd",
            "https://evil.example/github.com/",
            "http://github.com/o/r",
            "https://github.com/o/r\nmore",
        ] {
            assert!(!is_safe_to_open(bad), "{bad}");
        }
    }
}
