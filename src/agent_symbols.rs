//! Agent HARNESS symbol registry (#542): maps the closed
//! [`crate::detect::Agent`] enum to a flat single-cell glyph.
//!
//! This is the display counterpart of [`crate::server_icons`], and the two are
//! deliberately disjoint in vocabulary: a server icon answers *which machine* a
//! row is on, an agent symbol answers *which harness is running there*. Both
//! appear on the same row, so a shared glyph would render twice and read as one
//! fact stated twice.
//!
//! Keying on the enum rather than on a name is the whole point. The sidebar's
//! identity input used to be a bare `String`, so a harness could only be
//! recognised by matching text — and text is exactly what
//! [`crate::detect::short_agent_label`] cannot repair when the caller of
//! `flock_agent_start` invents a label. Keyed here, a renamed agent still
//! resolves its harness's symbol, because the harness is known structurally.
//!
//! Every glyph is outside the Private Use Area and is covered by ordinary font
//! coverage, so a viewer without a Nerd Font renders a shape rather than tofu.
//! `every_harness_symbol_is_one_cell_distinct_and_disjoint` is the gate that
//! keeps it that way.

use crate::detect::Agent;

/// The symbol for a harness. Always `Some` for every variant — a harness we can
/// name, we can draw. Callers that have no harness at all (an unidentified
/// pane, a remote row whose summary carried only free text) get `None` from the
/// caller's `Option<Agent>`, never a placeholder from here.
pub fn symbol(agent: Agent) -> &'static str {
    match agent {
        Agent::Pi => "π",     // U+03C0 GREEK SMALL LETTER PI — the constant it is named for
        Agent::Claude => "✻", // U+273B TEARDROP-SPOKED ASTERISK
        Agent::Codex => "⌬",  // U+232C BENZENE RING
        Agent::Gemini => "✦", // U+2726 BLACK FOUR POINTED STAR — its natural sign is U+264A, which is
        // East Asian Wide and so TWO cells: it would have silently
        // shifted every column right of it. See the width test.
        Agent::Cursor => "➤", // U+27A4 BLACK RIGHTWARDS ARROWHEAD — a pointer
        Agent::Antigravity => "▲", // U+25B2 BLACK UP-POINTING TRIANGLE — lift
        Agent::Cline => "◫",  // U+25EB WHITE SQUARE WITH VERTICAL BISECTING LINE
        Agent::OpenCode => "⧉", // U+29C9 TWO JOINED SQUARES
        Agent::GithubCopilot => "⑂", // U+2442 OCR FORK
        Agent::Kimi => "☾",   // U+263E LAST QUARTER MOON
        Agent::Kiro => "◈",   // U+25C8 WHITE DIAMOND CONTAINING BLACK SMALL DIAMOND
        Agent::Droid => "❖",  // U+2756 BLACK DIAMOND MINUS WHITE X
        Agent::Amp => "↯",    // U+21AF DOWNWARDS ZIGZAG ARROW — U+26A1 HIGH VOLTAGE is
        // East Asian Wide; this is the narrow electrical sign
        Agent::Grok => "✱", // U+2731 HEAVY ASTERISK — a heavy asterisk reads "sharp / got it"
        // where a light one reads "idea", and it is one codepoint away
        // from Gemini's star in a table a human is scanning
        Agent::Hermes => "✈",   // U+2708 AIRPLANE — the messenger
        Agent::Kilo => "⚖",     // U+2696 BLACK SCALES — the weight
        Agent::Qodercli => "⌗", // U+2317 VIEW DATA SQUARE
    }
}

/// Whether a caller-supplied override is usable as an agent symbol directly.
///
/// The same shape as [`crate::server_icons`]'s raw-glyph hatch, for the same
/// reason: the agent field sits in a fixed one-cell slot, and a value that is
/// two cells wide (an emoji), a multi-glyph run, or a control/escape payload
/// either reflows every column to its right or injects into the row. This is the
/// only way a value reaches the field without passing through [`symbol`] —
/// `agent_aliases` names free text, and `symbol` mode shows a non-harness alias
/// verbatim — so it needed the same gate rather than a hopeful doc comment.
///
/// Note what this is NOT: a way to smuggle in a multi-cell brand mark. A mark
/// that does not fit one cell does not fit the sidebar, whatever it is.
pub fn is_renderable_override(value: &str) -> bool {
    use unicode_width::UnicodeWidthStr;
    let trimmed = value.trim();
    !trimmed.is_empty()
        && trimmed.width() == 1
        && trimmed.chars().count() <= 4
        && !trimmed.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// Every glyph the registry can return, straight off the enum.
    ///
    /// Derived from [`Agent::ALL`] rather than hand-listed. This list used to
    /// be spelled out here, and `the_tested_agent_list_covers_every_harness`
    /// claimed to catch a variant missing from it — but it iterated the list
    /// rather than the enum, so it could not. Add a variant, give `symbol()`
    /// and `agent_label()` arms (both compile errors), leave it out of a
    /// hand-written list, and every gate below would still pass over the other
    /// sixteen. Deriving from the enum's own `ALL` removes the possibility.
    #[test]
    fn every_harness_symbol_is_one_cell_distinct_and_disjoint() {
        use std::collections::HashMap;
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for agent in Agent::ALL {
            let glyph = symbol(agent);
            assert_eq!(
                glyph.width(),
                1,
                "{glyph:?} for {agent:?} is {} cells wide, which would break the \
                 fixed slot the sidebar budgets for it",
                glyph.width()
            );
            assert!(
                glyph.chars().count() <= 4,
                "{glyph:?} for {agent:?} is a multi-glyph run"
            );
            assert!(
                !glyph.chars().any(char::is_control),
                "{glyph:?} for {agent:?} carries a control character"
            );
            if let Some(other) = seen.insert(glyph, crate::detect::agent_label(agent)) {
                panic!(
                    "{glyph:?} is claimed by both {other} and {}",
                    crate::detect::agent_label(agent)
                );
            }
        }
    }

    /// The agents-panel row already spends its leading cell on STATE
    /// (`◉`, the braille spinner, `●`, `✓`, `○`). An agent symbol drawn from
    /// the same set would make the two fields indistinguishable, which is the
    /// one thing a second glyph column must not do.
    #[test]
    fn no_agent_symbol_collides_with_a_state_glyph() {
        const STATE_GLYPHS: &[&str] = &["◉", "●", "✓", "○"];
        for glyph in STATE_GLYPHS {
            assert!(
                !Agent::ALL.iter().any(|agent| symbol(*agent) == *glyph),
                "{glyph:?} is already the agents panel's STATE glyph"
            );
        }
        for frame in crate::ui::SPINNERS {
            assert!(
                !Agent::ALL.iter().any(|agent| symbol(*agent) == *frame),
                "{frame:?} is a spinner frame"
            );
        }
    }

    /// `server_icons` is a different vocabulary on purpose: the agent field and
    /// the server field sit side by side on one row. A glyph in both registries
    /// renders twice and reads as one fact stated twice.
    #[test]
    fn agent_symbols_are_disjoint_from_the_server_icon_registry() {
        for agent in Agent::ALL {
            let label = crate::detect::agent_label(agent);
            let glyph = symbol(agent);
            for name in crate::server_icons::known_names() {
                if let Some(server_glyph) = crate::server_icons::glyph(name) {
                    assert_ne!(
                        glyph, server_glyph,
                        "{label:?} shares {glyph:?} with server icon {name:?}"
                    );
                }
            }
        }
    }

    /// Mirrors `server_icons::resolve_rejects_unsafe_or_oversized_raw_values`
    /// for the same reasons, and because the two registries share a slot on the
    /// same row: whatever rule keeps a server icon in one cell has to keep an
    /// agent override in one cell, or a row can be broken from either side.
    #[test]
    fn an_override_must_be_one_cell_and_clean() {
        assert!(
            is_renderable_override("\u{f092b}"),
            "a PUA glyph is one cell"
        );
        assert!(is_renderable_override("\u{2726}"), "so is a sign");
        assert!(
            is_renderable_override("  \u{2726}  "),
            "trimming is allowed"
        );

        for bad in [
            "\u{1f916}",        // emoji: two cells
            "\u{264a}",         // East Asian Wide: two cells
            "\u{26a1}",         // also two cells — the trap #550's own table hit
            "ab",               // more than one cell
            "\u{2726}\u{2726}", // two glyphs
            "",                 // empty
            "   ",              // whitespace only
            "\u{1b}",           // ESC: would inject into the row
            "\u{200b}",         // zero-width: measures 0
        ] {
            assert!(
                !is_renderable_override(bad),
                "{bad:?} must not reach the agent field"
            );
        }
    }

    /// Every harness's LABEL round-trips through the parser back to itself.
    ///
    /// This is the load-bearing property for the two paths that recover a
    /// harness from text — a remote row whose peer summary carried a label,
    /// and an alias naming a harness. `Agent::ALL` is the source, so this
    /// cannot pass while skipping a variant.
    #[test]
    fn every_harness_label_round_trips_through_the_parser() {
        for agent in Agent::ALL {
            let label = crate::detect::agent_label(agent);
            assert_eq!(
                crate::detect::parse_agent_label(label),
                Some(agent),
                "{label:?} does not parse back to its own variant"
            );
        }
    }
}
