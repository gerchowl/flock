//! Claimed peer names are presentation, never an identity binding (ADR-0026).

use caseless::Caseless;
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_security::{general_security_profile::IdentifierType, GeneralSecurityProfile};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Full unverified badge. A single `?` is its compact form below twelve cells.
pub(crate) const BADGE: &str = "[unverified]";

fn invisible(ch: char) -> bool {
    ch.is_control()
        || get_general_category(ch) == GeneralCategory::Format
        || ch.identifier_type() == Some(IdentifierType::Default_Ignorable)
}

/// Compare names after removing invisible/whitespace scalars, full Unicode
/// casefolding, and the maintained UTS #39 skeleton. Never display this key.
pub(crate) fn skeleton(name: &str) -> String {
    let folded: String = name
        .chars()
        .filter(|ch| !invisible(*ch) && !ch.is_whitespace())
        .default_case_fold()
        .collect();
    unicode_security::skeleton(&folded).collect()
}

fn escaped(name: &str) -> String {
    let chars: Vec<_> = name.chars().collect();
    let mut output = String::new();
    for (index, &ch) in chars.iter().enumerate() {
        let interior_space = ch == ' '
            && index > 0
            && index + 1 < chars.len()
            && [chars[index - 1], chars[index + 1]]
                .iter()
                .all(|other| !other.is_whitespace() && !invisible(*other));
        if invisible(ch) || (ch.is_whitespace() && !interior_space) {
            output.extend(ch.escape_unicode());
        } else if ch == '\\' {
            output.push_str("\\\\");
        } else {
            output.push(ch);
        }
    }
    output
}

/// The only admission to claimed-name presentation. Every result carries the
/// badge; aliases are compared by skeleton, then hidden on collision. Escaping
/// happens even without a collision. Inputs and routing keys remain untouched.
pub(crate) fn claimed<'a>(
    name: &str,
    node: Option<&str>,
    aliases: impl Iterator<Item = &'a str>,
) -> String {
    let key = skeleton(name);
    let collision = aliases.into_iter().any(|alias| skeleton(alias) == key);
    let label = if collision || name.is_empty() {
        escaped(&node.unwrap_or_default().chars().take(8).collect::<String>())
    } else {
        escaped(name)
    };
    if label.is_empty() {
        BADGE.to_string()
    } else {
        format!("{BADGE} {label}")
    }
}

/// Already-rendered text containing an unverified badge. This does not admit
/// raw names; callers must obtain their labels through [`claimed`] first.
pub(crate) fn is_claim(label: &str) -> bool {
    label.contains(BADGE)
}

/// Keep the badge outside truncation. On overflow move it before ornaments or
/// project context; below twelve cells use `?`. Width zero renders no content.
/// The remaining name/context is cut by display cells, never by UTF-8 bytes.
pub(crate) fn fit(label: &str, width: usize) -> Option<String> {
    let (before, after) = label.split_once(BADGE)?;
    if width == 0 {
        return Some(String::new());
    }
    if label.width() <= width {
        return Some(label.to_string());
    }
    let badge = if width < BADGE.width() { "?" } else { BADGE };
    let mut result = badge.to_string();
    let room = width.saturating_sub(badge.width());
    if room <= 1 {
        return Some(result);
    }
    result.push(' ');
    let payload = format!("{before}{}", after.trim_start_matches(' '));
    let budget = room - 1;
    let truncated = payload.width() > budget;
    let mut remaining = budget.saturating_sub(usize::from(truncated));
    for ch in payload.chars() {
        let cells = ch.width().unwrap_or(0);
        if cells > remaining {
            break;
        }
        result.push(ch);
        remaining -= cells;
    }
    if truncated {
        result.push('…');
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security2166_r2_unicode_casefold_and_skeleton_are_comparison_only() {
        assert_eq!(skeleton("Straße"), skeleton("STRASSE"));
        assert_eq!(skeleton("café"), skeleton("cafe\u{301}"));
        assert_eq!(skeleton("node"), skeleton("n\u{43e}de"));
        assert_eq!(
            claimed("Straße", None, std::iter::empty()),
            "[unverified] Straße"
        );
        assert_eq!(claimed("Straße", None, ["STRASSE"].into_iter()), BADGE);
    }

    #[test]
    fn security2166_r2_single_interior_spaces_and_literal_escapes_are_distinct() {
        assert_eq!(
            claimed("two words", None, std::iter::empty()),
            "[unverified] two words"
        );
        assert_ne!(
            claimed("a\u{200b}", None, std::iter::empty()),
            claimed(r"a\u{200b}", None, std::iter::empty())
        );
    }
}
