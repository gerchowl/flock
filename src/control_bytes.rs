//! One answer to "does this API-supplied text carry terminal control
//! sequences", shared by every caller that has to care (#348, ADR-0014 §4).
//!
//! Two handlers ask the question and answer it differently. `msg.send` STRIPS,
//! because a message body is appended to a turn that already exists and a
//! mangled sentence is better than a refused delivery. `agent.spawn` REFUSES,
//! because the prompt IS the child's whole opening turn: silently editing it
//! changes the task the caller asked for, and the caller never learns that
//! what ran was not what it wrote.
//!
//! The policies differ; the filter must not. Two hand-written scanners drift,
//! and the one that drifts is the one nobody is looking at — so the predicate
//! here is defined AS the stripper ([`carries_any`] asks whether [`strip`]
//! would change the text), which makes divergence unrepresentable rather than
//! merely discouraged.
//!
//! Newlines and tabs survive. They are the only two control characters a
//! human writing a task description or a message actually types, and a filter
//! that ate them would refuse every multi-line issue body.

/// Remove ANSI CSI/OSC escape sequences and every other control character,
/// keeping `\n` and `\t`.
///
/// Whole SEQUENCES rather than the escape byte alone: dropping `\u{1b}` and
/// keeping `[31m` turns an invisible colour change into visible garbage, and
/// leaves an OSC 8 hyperlink's target sitting in the text as prose.
pub fn strip(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: consume through the final byte (@..~).
                    for follow in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&follow) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: consume through BEL or ESC\.
                    while let Some(follow) = chars.next() {
                        if follow == '\u{7}' {
                            break;
                        }
                        if follow == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if !c.is_control() || c == '\n' || c == '\t' {
            cleaned.push(c);
        }
    }
    cleaned
}

/// Whether [`strip`] would remove anything — i.e. whether this text carries a
/// control sequence at all.
///
/// Deliberately implemented by running the stripper rather than by a second
/// scan of its own. It costs an allocation on a bounded string, and it buys
/// the guarantee that "refused" and "stripped" can never disagree about what
/// a control sequence is.
pub fn carries_any(text: &str) -> bool {
    strip(text) != text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_text_survives_untouched() {
        for benign in [
            "review #42 against the ADRs",
            "line one\nline two\n\tindented",
            "unicode: café — ✅ 日本語",
            "",
        ] {
            assert_eq!(strip(benign), benign);
            assert!(!carries_any(benign), "{benign:?} carries no control bytes");
        }
    }

    #[test]
    fn a_whole_csi_sequence_goes_not_just_the_escape() {
        assert_eq!(strip("\u{1b}[31mred\u{1b}[0m"), "red");
        assert!(carries_any("\u{1b}[31mred"));
    }

    /// OSC carries a payload — a window title, a hyperlink target. Dropping
    /// the introducer alone would leave that payload sitting in the text as
    /// if the author had written it.
    #[test]
    fn an_osc_sequence_takes_its_payload_with_it() {
        assert_eq!(strip("\u{1b}]0;pwned\u{7}after"), "after");
        assert_eq!(strip("\u{1b}]8;;http://evil\u{1b}\\link"), "link");
    }

    /// A bare carriage return is the overwrite primitive: text after it
    /// redraws over text before it, so a prompt can display one thing and say
    /// another. It is not an escape sequence, and a filter that only looked
    /// for `\u{1b}` would miss it.
    #[test]
    fn lone_control_characters_are_caught_too() {
        for hostile in ["a\rb", "a\u{7}b", "a\u{0}b", "a\u{8}b", "a\u{9b}b"] {
            assert!(carries_any(hostile), "{hostile:?} must be caught");
        }
    }

    /// The predicate is the stripper. This is the property that makes one
    /// filter with two policies safe, so it is asserted rather than assumed.
    #[test]
    fn the_predicate_agrees_with_the_stripper_by_construction() {
        for sample in ["plain", "\u{1b}[2Jclear", "tab\there", "bell\u{7}"] {
            assert_eq!(carries_any(sample), strip(sample) != sample);
        }
    }
}
