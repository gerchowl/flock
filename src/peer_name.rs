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

/// Two comparison keys after removing invisible/whitespace scalars (ADR-0026).
/// K1 is skeleton(fold(x)); K2 starts at fold(skeleton(x)) and repeats that
/// composition to its fixed point, with at most eight further steps. If that
/// backstop is exhausted, the last value is used. Folding first preserves case
/// equivalence, while skeleton first preserves capital I/lowercase l confusables.
/// Keys are never displayed, persisted, or used to bind routing identities.
pub(crate) fn comparison_keys(name: &str) -> (String, String) {
    let stripped: String = name
        .chars()
        .filter(|ch| !invisible(*ch) && !ch.is_whitespace())
        .collect();
    let folded: String = stripped.chars().default_case_fold().collect();
    let fold_first = unicode_security::skeleton(&folded).collect();
    let skeleton_first = unicode_security::skeleton(&stripped)
        .default_case_fold()
        .collect();
    let skeleton_first = fixed_point(skeleton_first, |value| {
        unicode_security::skeleton(value)
            .default_case_fold()
            .collect()
    });
    (fold_first, skeleton_first)
}

fn fixed_point(mut value: String, mut step: impl FnMut(&str) -> String) -> String {
    for _ in 0..8 {
        let next = step(&value);
        if next == value {
            return value;
        }
        value = next;
    }
    value
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
    let (fold_first, skeleton_first) = comparison_keys(name);
    let collision = aliases.into_iter().any(|alias| {
        let (alias_fold_first, alias_skeleton_first) = comparison_keys(alias);
        fold_first == alias_fold_first || skeleton_first == alias_skeleton_first
    });
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
    fn security2166_r3_capital_i_in_a_lowercase_alias_collides() {
        assert_eq!(
            claimed("kiIn", Some("abcdefgh"), ["kiln"].into_iter()),
            "[unverified] abcdefgh"
        );
    }

    #[test]
    fn security2166_r3_capital_i_and_lowercase_l_collide() {
        assert_eq!(
            claimed("I", Some("abcdefgh"), ["l"].into_iter()),
            "[unverified] abcdefgh"
        );
    }

    #[test]
    fn security2166_r3_capital_i_and_lowercase_i_remain_case_equivalent() {
        assert_eq!(
            claimed("I", Some("abcdefgh"), ["i"].into_iter()),
            "[unverified] abcdefgh"
        );
    }

    #[test]
    fn security2166_r3_distinct_names_keep_their_escaped_claim() {
        assert_eq!(
            claimed(
                "node-distinct.example",
                Some("abcdefgh"),
                ["node-trusted.example"].into_iter()
            ),
            "[unverified] node-distinct.example"
        );
    }

    #[test]
    fn security2166_r3_skeleton_first_requires_a_fixed_point() {
        assert_eq!(
            unicode_security::skeleton("M")
                .default_case_fold()
                .collect::<String>(),
            "m"
        );
        assert_eq!(comparison_keys("M").1, "rn");
        assert_eq!(comparison_keys("M").1, comparison_keys("rn").1);
    }

    #[test]
    fn security2166_r4_fixed_point_caps_nonconverging_data() {
        let mut calls = 0;
        let last = fixed_point(String::new(), |value| {
            calls += 1;
            assert!(calls <= 8, "normalization exceeded its eight-step backstop");
            format!("{value}x")
        });
        assert_eq!(calls, 8);
        assert_eq!(last, "xxxxxxxx");
    }

    #[test]
    fn security2166_r4_fast_convergence_cases() {
        for text in [
            "M",
            "I",
            "i",
            "l",
            "1",
            "kiIn",
            "kiln",
            "n\u{43e}de",
            "M\u{301}I",
            "\u{345}\u{310}M",
            "Straße",
            "\u{212a}",
            "\u{1d400}",
            "\u{feff}",
        ] {
            let (_, key) = comparison_keys(text);
            assert_eq!(
                unicode_security::skeleton(&key)
                    .default_case_fold()
                    .collect::<String>(),
                key,
                "{text:?}"
            );
        }
    }

    #[test]
    #[ignore = "exhaustive Unicode-data audit; run explicitly when updating normalization dependencies"]
    fn security2166_r3_unicode_data_converges_with_a_bounded_reference() {
        let step = |s: &str| {
            unicode_security::skeleton(s)
                .default_case_fold()
                .collect::<String>()
        };
        for ch in (0..=0x10ffff).filter_map(char::from_u32) {
            let first = step(&ch.to_string());
            let second = step(&first);
            let third = step(&second);
            // Never call the production loop here: changed data must fail, not hang.
            assert_eq!(
                step(&third),
                third,
                "Unicode data did not converge for {ch:?}"
            );
        }
        for text in ["M\u{301}I", "\u{345}\u{310}M", "Straße", "kiIn"] {
            let (_, key) = comparison_keys(text);
            assert_eq!(step(&key), key);
        }
    }

    #[test]
    fn security2166_r3_digit_one_and_lowercase_l_remain_confusable() {
        assert_eq!(
            claimed("1", Some("abcdefgh"), ["l"].into_iter()),
            "[unverified] abcdefgh"
        );
    }

    #[test]
    fn security2166_r2_unicode_casefold_and_skeleton_are_comparison_only() {
        assert_eq!(comparison_keys("Straße"), comparison_keys("STRASSE"));
        assert_eq!(comparison_keys("café"), comparison_keys("cafe\u{301}"));
        assert_eq!(comparison_keys("node"), comparison_keys("n\u{43e}de"));
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
