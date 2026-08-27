//! The shared GitHub GraphQL transport (#371).
//!
//! ## Why this exists rather than a second client
//!
//! `src/pr_poll.rs` (#294) already established how flock talks to GitHub:
//! `curl` as a subprocess — flock deliberately carries no HTTP-client
//! dependency, and `src/update.rs` states the same posture — with the token
//! handed over on **stdin** via `--config -`, never in argv. That detail is
//! load-bearing: argv is world-readable through `ps`, and `TracedCommand`
//! writes it to the process-exec log on every run, so a header argument would
//! publish the credential to every local user and persist it on disk.
//!
//! Issue filing needs the same transport with a different document. Copying
//! `fetch_batch`'s body would fork the token cache, the timeout policy, and the
//! error classification — three things that must not drift. So the transport
//! moved here and `pr_poll` calls it, rather than this module growing a second
//! one.
//!
//! ## Why `curl` rather than `gh`
//!
//! On macOS every exec re-validates the binary's code signature through
//! `syspolicyd`, and `gh` is a large ad-hoc-signed Mach-O: **9164 CodeDirectory
//! hashes against `curl`'s 58**, roughly 158x the exec cost. #294 was an
//! incident caused by paying that per worktree per tick. Anything on an
//! interactive path uses this module; `gh` is acceptable only for a one-shot
//! that a human just asked for.

use crate::process::TracedCommand;

/// GitHub's GraphQL endpoint. Overridable for tests and GHES.
pub(crate) const GRAPHQL_URL: &str = "https://api.github.com/graphql";

/// Bound on one request. `curl` enforces these itself, so a hung request is
/// reaped by the transport rather than needing a supervising thread.
const CONNECT_TIMEOUT_SECS: &str = "5";
const MAX_TIME_SECS: &str = "20";

/// Cached GitHub token. `RwLock` rather than `OnceLock` because a server runs
/// for days and a token can be rotated underneath it — see
/// [`forget_cached_token`].
static TOKEN: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Why a GraphQL round failed, as a closed set.
///
/// This classification — never GitHub's own message — is what crosses a host
/// boundary. GitHub's raw `errors[0].message` routinely names private
/// repositories ("Could not resolve to a Repository with the name
/// 'acme/secret-project'") and echoes branch names back, so putting it on the
/// wire discloses to every polling peer what the local token could see. The
/// detailed message stays in the local log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphQlErrorKind {
    NoToken,
    Transport,
    RateLimited,
    Auth,
    /// The token authenticated but lacks the scope the operation needs.
    ///
    /// Split out from [`Self::Auth`] because the operator's remedy is
    /// different and specific: `Auth` means "log in again", this means "your
    /// existing login cannot write". A caller that collapses the two sends
    /// someone to re-authenticate a token that was never the problem.
    Forbidden,
    GraphQl,
}

impl GraphQlErrorKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NoToken => "no_token",
            Self::Transport => "transport",
            Self::RateLimited => "rate_limited",
            Self::Auth => "auth",
            Self::Forbidden => "forbidden",
            Self::GraphQl => "graphql",
        }
    }

    /// Classify from GitHub's message text WITHOUT retaining it.
    pub(crate) fn classify_graphql(message: &str) -> Self {
        let lowered = message.to_ascii_lowercase();
        if lowered.contains("rate limit") || lowered.contains("abuse") {
            Self::RateLimited
        } else if lowered.contains("bad credentials")
            || lowered.contains("unauthorized")
            || lowered.contains("authentication")
        {
            Self::Auth
        } else if lowered.contains("resource not accessible")
            || lowered.contains("must have push access")
            || lowered.contains("does not have permission")
            || lowered.contains("saml")
        {
            Self::Forbidden
        } else {
            Self::GraphQl
        }
    }
}

/// The token, resolved once and then cached.
///
/// The env vars cover CI and anyone already exporting one. The `gh auth token`
/// fallback costs a single large-binary exec for the server's lifetime rather
/// than one per request — which is the entire point of #294.
pub(crate) fn github_token() -> Option<String> {
    if let Ok(guard) = TOKEN.read() {
        if let Some(token) = guard.as_ref() {
            return Some(token.clone());
        }
    }
    let resolved = resolve_token()?;
    if let Ok(mut guard) = TOKEN.write() {
        *guard = Some(resolved.clone());
    }
    Some(resolved)
}

/// Forget the cached token so the next round re-reads it.
///
/// Without this, a token rotated or refreshed underneath a long-running server
/// pins callers at `broken` until restart. Called only on an auth failure, so
/// this is a recovery path, not a per-round re-read.
pub(crate) fn forget_cached_token() {
    if let Ok(mut guard) = TOKEN.write() {
        *guard = None;
    }
}

fn resolve_token() -> Option<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    let out = TracedCommand::new("gh", "github_graphql")
        .args(["auth", "token"])
        .periodic()
        .output_traced()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!token.is_empty()).then_some(token)
}

/// The first top-level `errors[].message`, if the response carries one.
pub(crate) fn top_level_error(body: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let errors = value.get("errors")?.as_array()?;
    let first = errors.first()?;
    Some(
        first
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("graphql error")
            .to_string(),
    )
}

/// The exact argv the transport runs.
///
/// Extracted so the token-on-stdin invariant can be asserted against what is
/// actually executed rather than against a copy of it in a test. A mirrored
/// argv passes forever after the real one grows a header argument.
fn curl_args(body: &str) -> Vec<&str> {
    vec![
        "-sS",
        "--connect-timeout",
        CONNECT_TIMEOUT_SECS,
        "--max-time",
        MAX_TIME_SECS,
        "--config",
        "-",
        "-H",
        "Content-Type: application/json",
        "-X",
        "POST",
        "-d",
        body,
        GRAPHQL_URL,
    ]
}

/// POST one GraphQL document and return the raw response body.
///
/// `source` labels the caller in the process-exec log so one poller's traffic
/// stays distinguishable from another's. `variables` is passed as a JSON object
/// rather than interpolated into the document: values that reach GitHub as
/// variables cannot terminate a GraphQL string and inject document text, which
/// matters here because issue titles and bodies are arbitrary operator prose.
pub(crate) fn execute(
    document: &str,
    variables: serde_json::Value,
    source: &'static str,
) -> Result<String, GraphQlErrorKind> {
    let token = github_token().ok_or(GraphQlErrorKind::NoToken)?;
    let body = serde_json::json!({ "query": document, "variables": variables }).to_string();
    // The token is handed over on STDIN via `--config -`, never in argv. See
    // the module header for why that is not a style preference.
    let config = format!("header = \"Authorization: bearer {token}\"\n");
    let output = TracedCommand::new("curl", source)
        .periodic()
        .args(curl_args(&body))
        .output_traced_with_stdin(config.as_bytes())
        .map_err(|_| GraphQlErrorKind::Transport)?;
    if !output.status.success() {
        return Err(GraphQlErrorKind::Transport);
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if let Some(message) = top_level_error(&text) {
        // Classified here so the message itself is dropped rather than stored.
        return Err(GraphQlErrorKind::classify_graphql(&message));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_separates_the_four_remedies() {
        assert_eq!(
            GraphQlErrorKind::classify_graphql("Bad credentials"),
            GraphQlErrorKind::Auth
        );
        assert_eq!(
            GraphQlErrorKind::classify_graphql("API rate limit exceeded for user"),
            GraphQlErrorKind::RateLimited
        );
        // A scope failure must NOT read as "log in again" — the token is fine,
        // it simply cannot write. Collapsing these sends the operator to
        // re-authenticate something that was never broken.
        assert_eq!(
            GraphQlErrorKind::classify_graphql("Resource not accessible by personal access token"),
            GraphQlErrorKind::Forbidden
        );
        assert_eq!(
            GraphQlErrorKind::classify_graphql("Something else entirely"),
            GraphQlErrorKind::GraphQl
        );
    }

    #[test]
    fn classification_does_not_retain_the_message() {
        // The whole point of the enum: a message naming a private repo is
        // classified and dropped, never stored where it can reach a peer.
        let leaky = "Could not resolve to a Repository with the name 'acme/secret'";
        let kind = GraphQlErrorKind::classify_graphql(leaky);
        assert_eq!(kind, GraphQlErrorKind::GraphQl);
        assert!(!kind.as_str().contains("acme"));
        assert!(!kind.as_str().contains("secret"));
    }

    /// The credential must not reach argv: argv is world-readable via `ps` and
    /// `TracedCommand` writes it to the process-exec log on every run.
    #[test]
    fn the_token_never_appears_in_the_command_arguments() {
        let joined = curl_args("{\"query\":\"x\"}").join(" ");
        assert!(
            !joined.to_ascii_lowercase().contains("authorization"),
            "no Authorization header may be passed as an argument: {joined}"
        );
        assert!(
            joined.contains("--config -"),
            "token must arrive on stdin: {joined}"
        );
    }

    #[test]
    fn top_level_error_reads_the_first_message() {
        let body = r#"{"errors":[{"message":"Bad credentials"},{"message":"other"}]}"#;
        assert_eq!(top_level_error(body).as_deref(), Some("Bad credentials"));
    }

    #[test]
    fn a_clean_response_has_no_top_level_error() {
        assert_eq!(top_level_error(r#"{"data":{"viewer":{}}}"#), None);
        // Not JSON at all (a proxy error page, say) is also not a GraphQL error.
        assert_eq!(top_level_error("<html>502</html>"), None);
    }
}
