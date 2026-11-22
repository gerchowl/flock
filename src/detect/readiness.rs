use std::borrow::Cow;

use super::Agent;

fn braille(c: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&c)
}

// Codex hides its empty-composer hint when input is present (#2990).
// Preserve prompt text unless the entire hint proves the editor empty.
pub(crate) fn readiness_text(agent: Agent, content: &str) -> Cow<'_, str> {
    if agent != Agent::Codex || !content.chars().any(braille) {
        return Cow::Borrowed(content);
    }
    let prompt = content
        .lines()
        .enumerate()
        .filter(|(_, row)| row.starts_with('›'))
        .last();
    let Some((prompt, row)) = prompt else {
        return Cow::Borrowed(content);
    };
    let clean: String = row.chars().filter(|c| !braille(*c)).collect();
    if clean
        .strip_prefix('›')
        .is_none_or(|text| text.trim() != "Ask Codex to do anything")
    {
        return Cow::Borrowed(content);
    }
    let mut result = String::with_capacity(content.len());
    for (index, row) in content.split_inclusive('\n').enumerate() {
        let line = row.trim_end_matches('\n');
        if index == prompt {
            result.push_str("› Ask Codex to do anything");
            if row.ends_with('\n') {
                result.push('\n');
            }
            continue;
        } else if index > prompt && line.chars().all(|c| c.is_whitespace() || braille(c)) {
            // Keep even a final unterminated row for row-aligned styled reads.
            result.push(' ');
            if row.ends_with('\n') {
                result.push('\n');
            }
            continue;
        }
        // Transcript echoes must remain exact, including Braille-only rows.
        result.push_str(row);
    }
    // The normalized marker must belong to the live editor above its footer.
    if super::agents::codex::composer_region(&result).is_none_or(|(start, _)| start != prompt) {
        return Cow::Borrowed(content);
    }
    Cow::Owned(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braille_2990_range_requires_complete_hint_and_preserves_styled_row_alignment() {
        for glyph in '\u{2800}'..='\u{28ff}' {
            let screen = format!(
                "›{glyph}Ask Codex to do anything{glyph}\n{glyph}\n  ? for shortcuts\n{glyph}"
            );
            let normalized = readiness_text(Agent::Codex, &screen);
            assert!(normalized.starts_with("› Ask Codex to do anything\n"));
            assert_eq!(normalized.lines().count(), screen.lines().count());
            let draft = format!("› {glyph}\n  ? for shortcuts");
            assert_eq!(readiness_text(Agent::Codex, &draft), draft);
        }
    }

    #[test]
    fn braille_2990_does_not_normalize_other_harnesses_or_non_braille_symbols() {
        let screen = "›⠁Ask Codex to do anything\n⣿\n? for shortcuts";
        assert_eq!(readiness_text(Agent::Claude, screen), screen);
        for screen in [
            "  ›⠁Ask Codex to do anything\n  ? for shortcuts",
            "›⠁Ask Codex to do anything\n",
            "›⠁Ask Codex to do anything\n  ? for shortcuts\n• Later output",
        ] {
            assert_eq!(readiness_text(Agent::Codex, screen), screen);
        }
        for symbol in ['\u{27ff}', '\u{2900}', '•', '◦', '✓'] {
            let screen = format!("› {symbol}Ask Codex to do anything\n? for shortcuts");
            assert_eq!(readiness_text(Agent::Codex, &screen), screen);
        }
    }
}
