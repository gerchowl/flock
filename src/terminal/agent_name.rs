//! Agent identity text policy (ADR-0014 §4, #317). Reject at ingress, escape at display.
use crate::api::{error_code::ErrorCode, schema::ErrorBody};
use std::borrow::Cow;

/// Maximum Unicode scalar values in an agent name, before whitespace trimming.
/// This is a display bound, not a byte limit or the delegate filename policy.
pub const MAX_CHARS: usize = 64;
/// Public refusal text deliberately contains no rejected input.
pub const POLICY: &str =
    "agent names allow at most 64 characters, without controls, line breaks or bidi formatting";

fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Validate raw text before trimming, so trailing newlines cannot disappear.
/// Empty text remains valid for the existing clear-name operation. Creation
/// paths separately require a nonempty trimmed name.
pub fn validate(name: &str) -> Result<&str, ErrorCode> {
    if name.chars().count() > MAX_CHARS || name.chars().any(unsafe_char) {
        return Err(ErrorCode::InvalidAgentName);
    }
    Ok(name.trim())
}

/// Typed refusal, without reflecting control data to a CLI or peer.
pub fn refusal() -> ErrorBody {
    ErrorBody {
        code: ErrorCode::InvalidAgentName.as_str().into(),
        message: POLICY.into(),
        data: None,
    }
}

/// Drop invalid older-peer or saved names while keeping the surrounding record.
/// Only the typed code and a fixed source label enter the warning.
pub fn received(name: Option<String>, source: &'static str) -> Option<String> {
    name.filter(|name| {
        if validate(name).is_err() {
            crate::logging::agent_name_dropped(source);
            false
        } else {
            true
        }
    })
}

/// JSON receive adapter. Missing names stay optional; invalid names are discarded.
pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    use serde::Deserialize;
    Ok(received(Option::<String>::deserialize(d)?, "peer_json"))
}

/// Escape a bounded legacy/config label at the final render boundary. Valid
/// labels borrow their input. Control/bidi characters become visible Unicode
/// escapes, never terminal commands; overlong legacy labels are truncated.
pub fn display(name: &str) -> Cow<'_, str> {
    if validate(name).is_ok() {
        return Cow::Borrowed(name);
    }
    let mut safe = String::new();
    for c in name.chars().take(MAX_CHARS) {
        if unsafe_char(c) {
            safe.extend(c.escape_unicode());
        } else {
            safe.push(c);
        }
    }
    Cow::Owned(safe)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn security_agent_name_accepts_printable_unicode_at_the_character_bound() {
        let name = "界".repeat(MAX_CHARS);
        assert_eq!(validate(&name), Ok(name.as_str()));
        assert_eq!(validate("  reviewer 👩‍💻  "), Ok("reviewer 👩‍💻"));
        assert_eq!(validate("   "), Ok(""));
        assert_eq!(display(&name), Cow::Borrowed(name.as_str()));
    }
    #[test]
    fn security_agent_name_display_escapes_rather_than_emitting_control_data() {
        assert_eq!(display("agent\u{1b}[2J"), "agent\\u{1b}[2J");
        assert_eq!(display("agent\u{202e}other"), "agent\\u{202e}other");
    }
}
