//! Agent identity text policy (ADR-0014 §4, #317). Reject at ingress, escape at display.
use crate::api::{error_code::ErrorCode, schema::ErrorBody};
use finl_unicode::categories::CharacterCategories;
use std::borrow::Cow;

/// Maximum Unicode scalar values in an agent name, before whitespace trimming.
/// This is a display bound, not a byte limit or the delegate filename policy.
pub const MAX_CHARS: usize = 64;
/// Public refusal text deliberately contains no rejected input.
pub const POLICY: &str =
    "agent names allow at most 64 characters, without controls, line breaks or Unicode format characters (ZWJ is allowed)";

// Unicode Cf is broader than bidi controls. ZWJ supports emoji sequences.
// variation selectors are marks, not Cf, and remain permitted. Reject the
// entire tag block too, including its currently unassigned positions.
fn unsafe_char(c: char) -> bool {
    c.is_control()
        || (c.is_format() && c != '\u{200d}')
        || matches!(c, '\u{2028}' | '\u{2029}' | '\u{e0000}'..='\u{e007f}')
}

/// Remove controls, bidi/format characters and line separators from presentation
/// text (ADR-0014 §4). Unlike identity validation, this never rejects or imposes
/// a name-length cap. ZWJ and variation selectors remain intact. Callers apply
/// their own display budgets after keeping the full sanitized search text.
pub fn presentation(text: &str) -> String {
    text.chars().filter(|c| !unsafe_char(*c)).collect()
}

/// Sanitize a required peer presentation field without rejecting its record.
pub fn deserialize_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    use serde::Deserialize;
    String::deserialize(d).map(|text| presentation(&text))
}

/// Sanitize optional peer presentation text. Missing fields remain optional.
pub fn deserialize_optional_text<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<String>, D::Error> {
    use serde::Deserialize;
    Option::<String>::deserialize(d).map(|text| text.map(|text| presentation(&text)))
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
    let truncated = name.chars().count() > MAX_CHARS;
    for c in name.chars().take(MAX_CHARS - usize::from(truncated)) {
        if unsafe_char(c) {
            safe.extend(c.escape_unicode());
        } else {
            safe.push(c);
        }
    }
    if truncated {
        safe.push('…');
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
    fn security2090_presentation_keeps_emoji_and_full_search_text() {
        let text = format!("{}👩\u{200d}💻\u{fe0f}", "界".repeat(100));
        assert_eq!(presentation(&text), text);
        for (_, text) in crate::test_support::hostile_agent_names() {
            assert!(!presentation(&text).chars().any(unsafe_char));
        }
    }

    #[test]
    fn security_label_legacy_truncation_is_visible() {
        let label = "界".repeat(MAX_CHARS + 1);
        let shown = display(&label);
        assert!(shown.ends_with('…'));
        assert!(shown.chars().count() <= MAX_CHARS);
        assert!(validate("reviewer\u{200b}").is_err());
        assert!(validate("reviewer\u{200d}\u{fe0f}").is_ok());
    }

    #[test]
    fn security_agent_name_display_escapes_rather_than_emitting_control_data() {
        assert_eq!(display("agent\u{1b}[2J"), "agent\\u{1b}[2J");
        assert_eq!(display("agent\u{202e}other"), "agent\\u{202e}other");
    }
}
