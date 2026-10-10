use super::super::{has_confirmation_prompt, has_selection_prompt, AgentState};

/// Claude Code detection. The most complex — it has a structured prompt box UI.
///
/// Screen layout:
/// ```text
///   (agent output / tool results)
///   ───────────────────────── (top border)
///   ❯ _                      (prompt line)
///   ───────────────────────── (bottom border)
/// ```
pub(super) fn detect(content: &str) -> AgentState {
    // A frame we recognise nothing in still has to name a state for the
    // legacy callers; `Idle` stays the answer there. Callers that can act on
    // "no signal" ask [`detect_structural`] instead — see #309.
    detect_structural(content).unwrap_or(AgentState::Idle)
}

/// The state this screen *positively evidences*, or `None` when nothing
/// structural matched at all.
///
/// The distinction matters upstream (#309): returning `Idle` for "I saw a
/// prompt box and no working chrome" and for "I have no idea what this screen
/// is" makes the two indistinguishable, and the arbitration layer then treats
/// a shrug as authority to overrule the agent's own hook report. A live
/// capture had a pane sitting in `Idle` — the settled green checkmark — for
/// ten seconds while its agent was blocked on a permission dialog.
///
/// Every arm below is a positive match on agent-owned chrome. The bare
/// fallthrough is the only "no evidence" case, and it is now `None`.
pub(super) fn detect_structural(content: &str) -> Option<AgentState> {
    let lower = content.to_lowercase();

    // Search prompt is always idle
    if content.contains("⌕ Search…") {
        return Some(AgentState::Idle);
    }

    // ctrl+r toggle — don't change state
    // (we return Idle as a safe default since we don't have previous state here)
    if lower.contains("ctrl+r to toggle") {
        return Some(AgentState::Idle);
    }

    if has_live_blocked_form(content) || has_folder_trust_dialog(content) {
        return Some(AgentState::Blocked);
    }

    if has_working_chrome(content) {
        return Some(AgentState::Working);
    }

    if !has_prompt_box(content) && has_claude_blocked_prompt(content, &lower) {
        return Some(AgentState::Blocked);
    }

    if has_prompt_box(content) {
        return Some(AgentState::Idle);
    }

    None
}

pub(super) fn has_visible_blocker(content: &str) -> bool {
    let lower = content.to_lowercase();
    has_live_blocked_form(content)
        || has_folder_trust_dialog(content)
        || lower.contains("do you want to proceed?")
            && has_claude_yes_no_choice(content)
            && (lower.contains("bash command")
                || lower.contains("bash(")
                || lower.contains("contains expansion")
                || lower.contains("tab to amend")
                || lower.contains("ctrl+e to explain"))
}

pub(super) fn has_working_chrome(content: &str) -> bool {
    let above = content_above_prompt_box(content);
    let above_lower = above.to_lowercase();
    above_lower.contains("esc to interrupt")
        || above_lower.contains("ctrl+c to interrupt")
        || has_running_status_line(above)
        || has_spinner_activity(above)
        || (has_background_shell_footer(content) && !ends_on_done_turn(above))
}

/// Whether a turn ended (Claude's `done <clock>` marker) with background
/// shells or agents still running, as the "N shell still running" suffix on
/// that line or the footer counter below the prompt box (#911). Neither keeps
/// the pane working, so a supervisor reading `idle` can tell this settle from
/// one with nothing left running.
pub(in crate::detect) fn settled_with_background_shells(content: &str) -> bool {
    let above = content_above_prompt_box(content);
    has_prompt_box(content)
        && ends_on_done_turn(above)
        && (last_non_empty_line(above).is_some_and(is_still_running_status_line)
            || has_background_shell_footer(content))
}

pub(super) fn is_transcript_viewer(content: &str) -> bool {
    let bottom_lines = bottom_non_empty_lines(content, 3);
    let Some(last_line) = bottom_lines.last() else {
        return false;
    };
    let bottom_text = normalize_lines(&bottom_lines);

    bottom_text.contains("showing detailed transcript")
        && bottom_text.contains("ctrl+o to toggle")
        && (bottom_text.contains("ctrl+e to show all")
            || bottom_text.contains("ctrl+e to collapse"))
        && transcript_control_tail(last_line)
}

pub(super) fn has_prompt_box(content: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    let Some(top_border_index) = claude_prompt_box_top_border_index(&lines) else {
        return false;
    };

    lines[top_border_index + 1..]
        .iter()
        .take_while(|line| !is_horizontal_rule(line))
        .any(|line| line.trim_start().starts_with('❯'))
}

/// What is typed into Claude's input box: the `❯` line's text plus any
/// continuation lines up to the bottom border, whitespace-trimmed. `None`
/// when no prompt box is on screen.
pub(in crate::detect) fn prompt_input(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let top_border_index = claude_prompt_box_top_border_index(&lines)?;
    let body: Vec<&str> = lines[top_border_index + 1..]
        .iter()
        .take_while(|line| !is_horizontal_rule(line))
        .copied()
        .collect();
    let start = body
        .iter()
        .position(|line| line.trim_start().starts_with('❯'))?;
    let mut typed = String::new();
    for (offset, line) in body[start..].iter().enumerate() {
        let line = if offset == 0 {
            line.trim_start().trim_start_matches('❯')
        } else {
            line
        };
        typed.push_str(line.trim());
    }
    Some(typed)
}

/// Claude uses the same generic Select and Dialog widgets for both
/// permission flows and ordinary slash/settings menus. Match only the
/// permission and interview prompts that actually need user input.
fn has_claude_blocked_prompt(content: &str, lower_content: &str) -> bool {
    has_confirmation_prompt(lower_content)
        || lower_content.contains("do you want to proceed?")
        || lower_content.contains("would you like to proceed?")
        || lower_content.contains("waiting for permission")
        || lower_content.contains("do you want to allow this connection?")
        || lower_content.contains("tab to amend")
        || lower_content.contains("ctrl+e to explain")
        || lower_content.contains("review your answers")
        || lower_content.contains("skip interview and plan immediately")
        || (has_selection_prompt(content) && has_claude_yes_no_choice(content))
}

/// Claude Code's folder-trust dialogs, as 2.1.285 and 2.1.289 draw them: a box
/// asking whether a folder is one the user trusts, offering "Yes, I trust this
/// folder" against a refusal.
///
/// Each entry is `(question, affirmative option)`, both flattened by
/// [`flat_words`] — the first is what a session's FIRST start in a directory
/// shows, the second what `/cd` shows when it moves one into a folder carrying
/// gated grants.
///
/// Both halves of a pair are required, because neither is safe alone. The
/// question is prose that could appear in a reply or in a diff the agent is
/// reading; the affirmative label is Claude's own and appears on these dialogs
/// only. Together they are a dialog rather than a coincidence — and without
/// them a delegate's readiness gate has nothing to report but a bare `blocked`,
/// because the dialog has no input box for the prompt-box matcher and its
/// confirm footer reads "Enter to confirm" where `has_live_blocked_form` wants
/// "Enter to select". #605 is the pre-trust half; this is the half that makes
/// the failure legible instead of a timeout.
///
/// The cancel label is deliberately not part of the gate: it is "No, exit"
/// normally and "No, continue without these permissions" when the folder also
/// carries gated grants, and the affirmative label above it does not change.
///
/// The match is scoped to the dialog's own BOX, which is what keeps quoted text
/// from reading as a dialog. See [`trust_dialog_box`] for why the frame and not
/// the bottom of the screen.
pub(in crate::detect) fn has_folder_trust_dialog(content: &str) -> bool {
    let Some(box_text) = trust_dialog_box(content) else {
        return false;
    };
    let flat = flat_words(&box_text);
    TRUST_DIALOGS
        .iter()
        .any(|(question, accept)| flat.contains(question) && flat.contains(accept))
}

/// The region between the last box frame on the pane and its bottom border, or
/// `None` when the pane has no closed frame.
///
/// NOT "the bottom N rows", and NOT [`content_after_last_horizontal_rule`].
///
/// - The bottom N rows is a guess about how tall a dialog is. A wrapped
///   question runs to a dozen lines, and a pane narrower than the dialog wraps
///   further; a fixed row count either truncates the question on a narrow pane
///   or reaches up into scrollback on a wide one.
/// - `content_after_last_horizontal_rule` anchors on `──` rules, which is right
///   for the prompt box and wrong here: this dialog is drawn in a rounded frame
///   (`╭ ╮ │ ╰ ╯`) and carries no `──` rule at all, so the last rule on the
///   pane is the prompt box's — the one thing that is NOT there while the
///   dialog is up.
///
/// The frame is the boundary that means "the region Claude is painting right
/// now". Taking the LAST frame on the pane is what excludes scrollback: an
/// agent reading this file, #605, or a transcript of an earlier dialog has
/// printed its quotes above everything since, and a box in that output is not
/// the live region. The trailing check then rejects the one case a frame alone
/// cannot — a dialog that has been answered, which leaves its box on the pane
/// with the prompt box drawn under it.
fn trust_dialog_box(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let top = lines.iter().rposition(|line| line.contains('\u{256d}'))?;
    let bottom = lines[top..]
        .iter()
        .position(|line| line.contains('\u{2570}'))
        .map(|offset| top + offset)?;
    // Nothing under the closing border may be a prompt box: that is the input
    // box of a session that already answered the dialog, so the box above it is
    // history. A live dialog covers the pane and nothing follows it.
    if lines[bottom + 1..]
        .iter()
        .any(|line| line.contains('\u{276f}'))
    {
        return None;
    }
    Some(lines[top..=bottom].join("\n"))
}

/// The trust dialogs as (question, affirmative option), flattened.
const TRUST_DIALOGS: [(&str, &str); 2] = [
    (
        "quick safety check is this a project you created or one you trust",
        "yes i trust this folder",
    ),
    (
        "is this a directory you created or one you trust",
        "yes trust it and apply them",
    ),
];

/// The screen as lowercase words separated by single spaces.
///
/// A phrase matcher that reads raw lines cannot be trusted on a dialog: Claude
/// word-wraps its body inside the box, so on an 80-column pane "one you trust?"
/// lands on the line after "...or", and the box's own `│` and `❯` characters sit
/// between the words that belong together. Flattening removes both problems and
/// costs nothing else, because every needle here is prose rather than a glyph.
fn flat_words(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    for word in content.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().flat_map(char::to_lowercase));
    }
    out
}

fn has_live_blocked_form(content: &str) -> bool {
    let region = content_after_last_horizontal_rule(content);
    region.lines().any(|line| {
        let lower = line.to_lowercase();
        lower.contains("enter to select")
            && lower.contains("esc to cancel")
            && (lower.contains("tab/arrow keys to navigate")
                || lower.contains("arrow keys to navigate")
                || lower.contains("arrows to navigate")
                || lower.contains("↑/↓ to navigate")
                || lower.contains("↑↓ to navigate"))
    })
}

fn last_non_empty_line(content: &str) -> Option<&str> {
    content.lines().rev().find(|line| !line.trim().is_empty())
}

fn has_running_status_line(content_above_prompt: &str) -> bool {
    let Some(line) = last_non_empty_line(content_above_prompt) else {
        return false;
    };

    is_background_agent_wait_line(line)
        || (is_still_running_status_line(line) && !has_turn_done_marker(line))
}

/// Does the newest line above the prompt box end a turn with Claude's
/// `done <clock>` marker?
fn ends_on_done_turn(content_above_prompt: &str) -> bool {
    last_non_empty_line(content_above_prompt).is_some_and(has_turn_done_marker)
}

/// Claude's turn-completion line, `✻ Brewed for 34m 19s · done 10:59 AM · 1
/// shell still running`, carries a `done <clock>` segment only once the turn
/// is over. The shells it names outlive the turn, so the line is a finished
/// turn rather than working chrome (#911). The clock is required, so prose
/// that merely says "done" does not count.
fn has_turn_done_marker(line: &str) -> bool {
    line.split('\u{b7}').skip(1).any(|segment| {
        segment
            .trim()
            .strip_prefix("done ")
            .and_then(|clock| clock.split_whitespace().next())
            .is_some_and(|clock| {
                clock.starts_with(|c: char| c.is_ascii_digit())
                    && clock.contains(':')
                    && clock.chars().all(|c| c.is_ascii_digit() || c == ':')
            })
    })
}

fn is_background_agent_wait_line(line: &str) -> bool {
    let mut text = line.trim();
    if !text.starts_with("Waiting for ") && !text.starts_with("waiting for ") {
        let mut chars = text.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if first.is_alphanumeric() {
            return false;
        }
        text = chars.as_str().trim_start();
    }

    let lower = text.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("waiting for ") else {
        return false;
    };
    let Some((count, rest)) = rest.split_once(' ') else {
        return false;
    };
    if count.parse::<u32>().ok().is_none_or(|count| count == 0) {
        return false;
    }

    rest == "background agent to finish" || rest == "background agents to finish"
}

fn is_still_running_status_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();

    for (index, word) in words.iter().enumerate() {
        let Ok(count) = word.parse::<u32>() else {
            continue;
        };
        if count == 0 {
            continue;
        }

        if matches!(
            words.get(index + 1..index + 4),
            Some(["shell" | "shells", "still", "running"])
        ) {
            return true;
        }

        if matches!(
            words.get(index + 1..index + 5),
            Some(["local", "agent" | "agents", "still", "running"])
        ) {
            return true;
        }
    }

    false
}

/// Claude's bottom status bar carries a background-task counter — e.g.
/// `⏵⏵ auto mode on · 1 shell · ← for agents · ↓ to manage` — that PERSISTS
/// below the prompt box after the transient "N shell still running" activity
/// line has scrolled off. `has_running_status_line` only inspects the line
/// ABOVE the prompt box, so that footer-only state read as idle (#47). Detect a
/// footer shell/agent counter (> 0) here, gated on the `↓ to manage` affordance
/// Claude only renders when background tasks exist — so a stray "1 shell"
/// elsewhere (e.g. the agent-picker hint) can't trip it.
fn has_background_shell_footer(content: &str) -> bool {
    bottom_non_empty_lines(content, 5).iter().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("to manage") && footer_background_task_count(&lower) > 0
    })
}

/// Count of background shells/agents named in a footer line: `N shell(s)` or
/// `N agent(s)` immediately after a number. The nav hint `← for agents` has no
/// leading number, so it never contributes.
fn footer_background_task_count(lower_line: &str) -> u32 {
    let words: Vec<&str> = lower_line.split_whitespace().collect();
    let mut total = 0;
    for (index, word) in words.iter().enumerate() {
        let Ok(count) = word.parse::<u32>() else {
            continue;
        };
        let next = words
            .get(index + 1)
            .map(|w| w.trim_matches(|c: char| !c.is_alphabetic()));
        if matches!(next, Some("shell" | "shells" | "agent" | "agents")) {
            total += count;
        }
    }
    total
}

fn has_claude_yes_no_choice(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line
            .trim()
            .trim_start_matches('❯')
            .trim_start()
            .to_lowercase();
        trimmed == "yes"
            || trimmed == "no"
            || trimmed.starts_with("1. yes")
            || trimmed.starts_with("2. no")
            || trimmed.starts_with("yes, and ")
            || trimmed.starts_with("no, and tell claude")
    })
}

/// Claude Code spinner characters + activity label.
/// The verb changes frequently ("Processing…", "Pouncing…", etc.), so rely
/// on the spinner glyph + trailing ellipsis rather than specific wording.
/// Include Claude's narrow-pane middle-dot frame too.
pub(in crate::detect) fn has_spinner_activity(content: &str) -> bool {
    spinner_activity_text(content).is_some()
}

/// The free-text activity label from Claude's spinner line, e.g.
/// "✶ Implementing the parser… (esc to interrupt)" -> "Implementing the parser".
pub(in crate::detect) fn spinner_activity_text(content: &str) -> Option<String> {
    const SPINNER_CHARS: &str = "·✱✲✳✴✵✶✷✸✹✺✻✼✽✾✿❀❁❂❃❇❈❉❊❋✢✣✤✥✦✧✨⊛⊕⊙◉◎◍⁂⁕※⍟☼★☆";
    for line in content.lines() {
        let trimmed = line.trim();
        let mut chars = trimmed.chars();
        if let Some(first) = chars.next() {
            if SPINNER_CHARS.contains(first) {
                let rest: String = chars.collect();
                if rest.starts_with(' ') && rest.contains('\u{2026}') {
                    let text = rest
                        .split('\u{2026}')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if text.chars().any(|c| c.is_alphanumeric()) {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

/// Public seam for detect::detect_agent: activity text relative to the live
/// prompt area (ignores transcript scrollback above the prompt box).
pub(in crate::detect) fn live_activity_text(content: &str) -> Option<String> {
    spinner_activity_text(content_above_prompt_box(content))
}

/// Extract content above Claude's prompt box.
/// The prompt box is two ─── border lines with ❯ between them.
pub(in crate::detect) fn content_above_prompt_box(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();

    if let Some(i) = claude_prompt_box_top_border_index(&lines) {
        let byte_offset: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
        return &content[..byte_offset.min(content.len())];
    }

    // No prompt box found, return all content
    content
}

fn content_after_last_horizontal_rule(content: &str) -> &str {
    let mut last_rule_end = 0usize;
    let mut offset = 0usize;
    for line in content.lines() {
        let next_offset = offset + line.len() + 1;
        if is_horizontal_rule(line) {
            last_rule_end = next_offset.min(content.len());
        }
        offset = next_offset;
    }

    &content[last_rule_end..]
}

fn claude_prompt_box_top_border_index(lines: &[&str]) -> Option<usize> {
    let mut border_count = 0;

    for i in (0..lines.len()).rev() {
        if is_horizontal_rule(lines[i]) {
            border_count += 1;
            if border_count == 2 {
                return Some(i);
            }
        }
    }

    None
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let rule_chars = trimmed.chars().take_while(|&c| c == '─').count();
    if rule_chars == 0 {
        return false;
    }

    let rule_bytes = trimmed
        .char_indices()
        .nth(rule_chars)
        .map(|(index, _)| index)
        .unwrap_or(trimmed.len());
    let suffix = trimmed[rule_bytes..].trim_start();

    suffix.is_empty() || rule_chars >= 3
}

fn bottom_non_empty_lines(content: &str, max_lines: usize) -> Vec<&str> {
    let mut lines: Vec<&str> = content
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(max_lines)
        .collect();
    lines.reverse();
    lines
}

fn normalize_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .flat_map(|line| line.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn transcript_control_tail(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("ctrl+e")
        || lower.contains("show all")
        || lower.contains("collapse")
        || lower.contains("verbose")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_box_below(content_above_prompt: &str) -> String {
        format!(
            "{content_above_prompt}\n────────────────────────────────\n❯ \n────────────────────────────────\n"
        )
    }

    #[test]
    fn shell_still_running_status_line_is_working() {
        let content = prompt_box_below(
            "● Started. I'll tell you when it finishes.\n\n✻ Crunched for 7s · 1 shell still running",
        );

        assert_eq!(detect(&content), AgentState::Working);
        assert!(has_working_chrome(&content));
    }

    /// The #911 screen, shaped after a live capture: a turn that ended on its
    /// `DONE:` line, Claude's completion line with its `done <clock>` marker and
    /// the shell it left running, and the footer counting that shell.
    fn done_turn_with_leftover_shell(footer: &str) -> String {
        format!(
            "● DONE: https://github.com/example/repo/pull/1\n\n\
             \u{273b} Crunched for 7m 41s \u{b7} done 4:32 PM \u{b7} 1 shell still running\n\
             \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
             \u{276f} \n\
             \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
             {footer}\n"
        )
    }

    #[test]
    fn done_marked_turn_with_leftover_shell_is_settled_not_working() {
        for footer in [
            "  \u{23f5}\u{23f5} bypass permissions on \u{b7} 1 shell \u{b7} \u{2190} for agents",
            "  \u{23f5}\u{23f5} auto mode on \u{b7} 1 shell \u{b7} \u{2190} for agents \u{b7} \u{2193} to manage",
        ] {
            let content = done_turn_with_leftover_shell(footer);
            assert!(!has_working_chrome(&content), "{footer}");
            assert_eq!(detect_structural(&content), Some(AgentState::Idle));
            assert!(settled_with_background_shells(&content), "{footer}");
        }
    }

    #[test]
    fn done_marked_turn_with_only_the_footer_counter_is_settled() {
        let content = format!(
            "{}\u{23f5}\u{23f5} auto mode on \u{b7} 1 shell \u{b7} \u{2193} to manage\n",
            prompt_box_below("● DONE: shipped\n\n\u{273b} Brewed for 34m 19s \u{b7} done 17:59")
        );
        assert!(!has_working_chrome(&content));
        assert_eq!(detect_structural(&content), Some(AgentState::Idle));
        assert!(settled_with_background_shells(&content));
    }

    #[test]
    fn done_marked_turn_with_nothing_left_running_is_plainly_idle() {
        let content =
            prompt_box_below("● DONE: shipped\n\n\u{273b} Baked for 16m 38s \u{b7} done 5:54 PM");
        assert_eq!(detect_structural(&content), Some(AgentState::Idle));
        assert!(!settled_with_background_shells(&content));
    }

    #[test]
    fn turn_done_marker_needs_a_clock_segment() {
        assert!(has_turn_done_marker(
            "\u{273b} Brewed for 34m 19s \u{b7} done 10:59 AM \u{b7} 1 shell still running"
        ));
        assert!(has_turn_done_marker(
            "\u{273b} Brewed for 3s \u{b7} done 17:59"
        ));
        for line in [
            "\u{273b} Crunched for 7s \u{b7} 1 shell still running",
            "\u{273b} Crunched for 7s \u{b7} done \u{b7} 1 shell still running",
            "\u{273b} Crunched for 7s \u{b7} done soon \u{b7} 1 shell still running",
            "done 4:32 PM \u{b7} 1 shell still running",
        ] {
            assert!(!has_turn_done_marker(line), "{line}");
        }
        // Without the marker the line is still working chrome, as before.
        let content =
            prompt_box_below("\u{273b} Crunched for 7s \u{b7} done \u{b7} 1 shell still running");
        assert!(has_working_chrome(&content));
        assert!(!settled_with_background_shells(&content));
    }

    #[test]
    fn local_agent_still_running_status_line_is_working() {
        let content = prompt_box_below(
            "● Hey. What do you want to work on?\n\n✻ Worked for 4s · 2 local agents still running",
        );

        assert_eq!(detect(&content), AgentState::Working);
        assert!(has_working_chrome(&content));
    }

    #[test]
    fn lower_agent_picker_shell_count_is_not_working_chrome() {
        let content = prompt_box_below("  ~/P/flock ⎇ master ▱▱▱▱▱ 0%\n  1 shell · ← for agents");

        assert_eq!(detect(&content), AgentState::Idle);
        assert!(!has_working_chrome(&content));
    }

    /// A prompt box with `footer` as the bottom status bar BELOW it — the
    /// real Claude layout (output, prompt box, then the footer/manage bar).
    fn with_footer(footer: &str) -> String {
        format!(
            "● Started the task.\n────────────────────────────────\n❯ \n────────────────────────────────\n{footer}\n"
        )
    }

    #[test]
    fn footer_background_shell_counter_is_working() {
        // The transient "still running" line is gone; only the persistent
        // footer counter remains, BELOW the prompt box (#47).
        let content = with_footer("⏵⏵ auto mode on · 1 shell · ← for agents · ↓ to manage");
        assert_eq!(detect(&content), AgentState::Working);
        assert!(has_working_chrome(&content));
    }

    #[test]
    fn footer_background_agents_counter_is_working() {
        let content = with_footer("⏵⏵ auto mode on · 2 agents · ↓ to manage");
        assert_eq!(detect(&content), AgentState::Working);
    }

    #[test]
    fn footer_without_background_tasks_is_idle() {
        let content = with_footer("⏵⏵ auto-accept edits on (shift+tab to cycle)");
        assert_eq!(detect(&content), AgentState::Idle);
        assert!(!has_working_chrome(&content));
    }

    #[test]
    fn footer_manage_hint_with_only_nav_agents_is_idle() {
        // "← for agents" is a nav hint (no leading count); with no real
        // shell/agent count the manage line must not read as working.
        let content = with_footer("⏵⏵ auto mode on · ← for agents · ↓ to manage");
        assert_eq!(detect(&content), AgentState::Idle);
        assert!(!has_working_chrome(&content));
    }

    #[test]
    fn stale_shell_running_line_above_newer_output_is_not_working_chrome() {
        let content = prompt_box_below(
            "● Started. I'll tell you when it finishes.\n\n✻ Crunched for 7s · 1 shell still running\n\n● hi",
        );

        assert_eq!(detect(&content), AgentState::Idle);
        assert!(!has_working_chrome(&content));
    }

    // ------------------------------------------------------------------
    // #309: "no structural evidence" must not masquerade as Idle.
    // ------------------------------------------------------------------

    /// The whole point of `detect_structural`: a frame we recognise nothing in
    /// returns `None`, while a frame with a real prompt box returns `Idle`.
    /// Before #309 both produced `AgentState::Idle` and were indistinguishable.
    #[test]
    fn unrecognised_frame_yields_no_signal_but_a_prompt_box_yields_idle() {
        let idle = concat!(
            "\u{23fa} #307 merged. Both of mine are in.\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "\u{276f} keep going\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "  atlas \u{b7} Opus 5 \u{b7} ~/Projects/flock main\n",
        );
        assert_eq!(detect_structural(idle), Some(AgentState::Idle));

        // Mid-scroll transcript with no prompt box and no working chrome.
        let nothing = "\u{23fa} some output that is neither a prompt box nor a spinner\n";
        assert_eq!(
            detect_structural(nothing),
            None,
            "no chrome matched, so there is no evidence of any state"
        );
        // The legacy entry point still has to name something.
        assert_eq!(detect(nothing), AgentState::Idle);
    }

    /// A torn frame — the prompt box's bottom border not yet flushed — used to
    /// publish `Idle`. `claude_prompt_box_top_border_index` counts rules from
    /// the bottom, so losing one border re-anchors it onto a rule in the
    /// streamed output above and the box stops parsing. Now it reports no
    /// signal, and the arbitration layer holds the last state instead.
    #[test]
    fn torn_prompt_box_frame_reports_no_signal_instead_of_idle() {
        let torn = concat!(
            "\u{23fa} #307 merged. Both of mine are in.\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "\u{276f} keep going\n",
            "  atlas \u{b7} Opus 5 \u{b7} ~/Projects/flock main\n",
        );
        assert!(
            !has_prompt_box(torn),
            "one missing border and the box is gone"
        );
        assert_eq!(detect_structural(torn), None);
    }

    /// Same for a Working pane: the spinner sits ABOVE the box, so a
    /// re-anchored box clips it away and the working chrome disappears with it.
    #[test]
    fn torn_working_frame_reports_no_signal_instead_of_idle() {
        let working = concat!(
            "\u{23fa} Editing src/detect/mod.rs\n",
            "\u{273b} Implementing\u{2026} (esc to interrupt)\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "\u{276f}\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
        );
        assert_eq!(detect_structural(working), Some(AgentState::Working));

        let torn = concat!(
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "\u{23fa} Editing src/detect/mod.rs\n",
            "\u{273b} Implementing\u{2026} (esc to interrupt)\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "\u{276f}\n",
        );
        assert!(!has_working_chrome(torn), "the spinner is clipped away");
        assert_eq!(
            detect_structural(torn),
            None,
            "a torn working frame must not publish Idle"
        );
    }

    /// A REAL captured footer from a live pane that had 2 background shells and
    /// 1 monitor outstanding. Both background-work matchers miss it:
    /// `is_still_running_status_line` word-matches `N ["shell"|"shells",
    /// "still","running"]`, but Claude writes "2 shells, 1 monitor still
    /// running" — the comma is glued to "shells," and "monitor" sits between
    /// the count and "still running". `has_background_shell_footer` needs
    /// "to manage", which this footer does not carry.
    ///
    /// Documented as-is: the pane has a real prompt box, so this is a genuine
    /// `Idle` reading, not a no-signal one. Fixing the counters is #309 P2.
    #[test]
    fn background_shell_footer_matchers_miss_the_real_footer() {
        let screen = concat!(
            "\u{23fa} #296 updated onto main and waiting on checks.\n",
            "\u{273b} Cooked for 5s \u{b7} 2 shells, 1 monitor still running\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "\u{276f} keep going\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "  atlas \u{b7} Opus 5 (1M context) \u{b7} ~/Projects/flock main\n",
            "  \u{23f5}\u{23f5} bypass permissions on \u{b7} 2 shells, 1 monitor \u{b7} \u{2190} for agents\n",
        );
        assert!(
            !has_background_shell_footer(screen),
            "the footer counter needs \"to manage\", which is absent"
        );
        assert!(!has_working_chrome(screen), "so no working chrome is found");
        assert_eq!(detect_structural(screen), Some(AgentState::Idle));
    }

    /// The first-run folder-trust dialog, reconstructed from the component
    /// Claude Code 2.1.285 and 2.1.289 both render: a bordered box titled
    /// "Accessing workspace:", the safety-check question, the cwd in bold, and
    /// a confirm widget offering the refusal FIRST and focused (the dialog's
    /// `cancelFirst` is true, so "No, exit" is what the arrow sits on) with its
    /// own two chord hints rather than the select prompt's navigation line.
    ///
    /// This is what a `delegate start --worktree --harness claude` finds on
    /// every fresh worktree (#605), and it is why the pattern exists: the box
    /// has no ── borders for `has_prompt_box`, its footer says "Enter to
    /// confirm" where `has_live_blocked_form` requires "Enter to select", and
    /// its options carry no indexes, so every existing matcher walked past it
    /// and reported no state at all.
    #[test]
    fn the_folder_trust_dialog_is_blocked() {
        let screen = concat!(
            "\u{256d}\u{2500} Accessing workspace: \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}\n",
            "\u{2502} /Users/dev/flock/.worktrees/builder-8ef0136f\n",
            "\u{2502}\n",
            "\u{2502} Quick safety check: Is this a project you created or one you trust?\n",
            "\u{2502} Claude Code'll be able to read, edit, and execute files here.\n",
            "\u{2502}\n",
            "\u{276f} No, exit\n",
            "\u{2502}   Yes, I trust this folder\n",
            "\u{2502}\n",
            "\u{2502} Enter to confirm \u{b7} Esc to cancel\n",
            "\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}\n",
        );
        assert!(
            has_folder_trust_dialog(screen),
            "the dialog's own question and its affirmative option are both on screen"
        );
        assert_eq!(detect(screen), AgentState::Blocked);
        assert_eq!(detect_structural(screen), Some(AgentState::Blocked));
        // A strong blocker, so it may overrule a hook report: Claude does not
        // load its hooks until trust is accepted, so nothing hook-reported can
        // speak for this pane.
        assert!(has_visible_blocker(screen));
        assert!(
            !has_prompt_box(screen),
            "and it has no prompt box to type into"
        );
    }

    /// A dialog on an 80-column pane. Claude word-wraps the question inside the
    /// box, so "one you trust?" is on the line after "...created or" and the
    /// frame above it is its own horizontal rule — a raw-line matcher reads this
    /// as a torn frame and reports nothing, which is how the whole screen went
    /// unnoticed until #612 needed it.
    #[test]
    fn the_trust_dialog_survives_the_wrap_a_narrow_pane_gives_it() {
        let wrapped = concat!(
            "\u{256d}\u{2500} Accessing workspace: \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}\n",
            "\u{2502} /repo/.worktrees/builder\n",
            "\u{2502}\n",
            "\u{2502} Quick safety check: Is this a project you created or\n",
            " one you trust?\n",
            "\u{2502} Claude Code'll be able to read, edit, and execute fi\n",
            "les here.\n",
            "\u{2502}\n",
            "\u{276f} No, exit\n",
            "\u{2502}   Yes, I trust this folder\n",
            "\u{2502}\n",
            "\u{2502} Enter to confirm \u{b7} Esc to cancel\n",
            "\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}\n",
        );
        assert!(has_folder_trust_dialog(wrapped));
        assert_eq!(detect_structural(wrapped), Some(AgentState::Blocked));
    }

    /// `/cd` asks the same question with the same two answers, so it is the
    /// same gate rather than a second one.
    #[test]
    fn the_relocate_trust_prompt_is_the_same_dialog() {
        let screen = concat!(
            "\u{256d}\u{2500} Now in a new directory: \u{2500}\u{2500}\u{2500}\u{2500}\u{256e}\n",
            "\u{2502} This session hasn't worked here before. Is this a directory\n",
            " you created or one you trust?\n",
            "\u{2502}\n",
            "\u{276f} Yes, trust it and apply them\n",
            "\u{2502}   No, keep them off\n",
            "\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}\n",
        );
        assert!(has_folder_trust_dialog(screen));
        assert_eq!(detect_structural(screen), Some(AgentState::Blocked));
    }

    /// The blocker an independent review found: the trust text is prose, so an
    /// agent reading this very file, or #605, or a transcript of this dialog,
    /// puts the question and the affirmative option on screen together — and a
    /// matcher that reads the whole pane cannot tell that from the dialog being
    /// up. The pane then reads `blocked` over a live `working`, which is worse
    /// than not seeing the dialog at all: `settled` and `delegate wait` stop
    /// waiting for a turn that is still running.
    #[test]
    fn the_trust_text_in_scrollback_over_an_idle_prompt_is_not_blocked() {
        let quoted = concat!(
            "\u{23fa} I read src/detect/agents/claude_code.rs and found the matcher.\n",
            "\u{23fa}   Quick safety check: Is this a project you created or one you trust?\n",
            "\u{23fa}   Yes, I trust this folder / No, exit\n",
            "These are the strings it matches, quoted from the detector.\n",
        );
        let content = prompt_box_below(quoted);
        let (question, accept) = TRUST_DIALOGS[0];
        assert!(
            flat_words(&content).contains(question) && flat_words(&content).contains(accept),
            "the fixture really does carry a whole dialog's worth of text"
        );
        assert!(
            !has_folder_trust_dialog(&content),
            "an unboxed quote is not a dialog"
        );
        assert_eq!(
            detect_structural(&content),
            Some(AgentState::Idle),
            "a live prompt box below the quoted text is idle, not blocked"
        );
        assert!(!has_visible_blocker(&content));
    }

    /// The same scrollback with the spinner still running, which is the case
    /// that broke `settled`.
    #[test]
    fn the_trust_text_in_scrollback_over_working_chrome_is_working() {
        let quoted = concat!(
            "\u{23fa} Quick safety check: Is this a project you created or one you trust?\n",
            "\u{23fa} Yes, I trust this folder\n",
            "\u{273b} Implementing\u{2026} (esc to interrupt)\n",
        );
        assert_eq!(
            detect_structural(quoted),
            Some(AgentState::Working),
            "a running turn reads working even with the dialog quoted above it"
        );
        assert!(!has_visible_blocker(quoted));
    }

    /// The gate is two controls, so each half on its own must not match: the
    /// question is prose an agent can quote, and the option is ordinary UI.
    #[test]
    fn the_trust_pattern_needs_both_halves() {
        let question_only = concat!(
            "Quick safety check: Is this a project you created or one you trust?\n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "\u{276f} \n",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
        );
        assert!(!has_folder_trust_dialog(question_only));

        let option_only = prompt_box_below("\u{276f} Yes, I trust this folder\n");
        assert!(!has_folder_trust_dialog(&option_only));
        assert_eq!(
            detect_structural(&option_only),
            Some(AgentState::Idle),
            "and ordinary prompt chrome is still idle"
        );
    }

    /// The alternative cancel label is deliberate: a folder that also carries
    /// gated grants draws "No, continue without these permissions" instead of
    /// "No, exit", and the dialog is still the dialog.
    #[test]
    fn the_trust_dialog_with_gated_grants_is_still_blocked() {
        let screen = concat!(
            "\u{256d}\u{2500} Accessing workspace: \u{2500}\u{2500}\u{2500}\u{256e}\n",
            "\u{2502} Quick safety check: Is this a project you created or one you trust?\n",
            "\u{2502}\n",
            "\u{2502} This folder pre-approves 3 tool permissions in\n",
            " .claude/settings.json:\n",
            "\u{2502}   Bash(npm run test)\n",
            "\u{2502}\n",
            "\u{276f} Yes, I trust this folder\n",
            "\u{2502}   No, continue without these permissions\n",
            "\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}\n",
        );
        assert!(has_folder_trust_dialog(screen));
        assert_eq!(detect_structural(screen), Some(AgentState::Blocked));
    }

    /// How narrow the still-running matcher is: one comma defeats it.
    #[test]
    fn still_running_matcher_is_positional() {
        assert!(is_still_running_status_line(
            "\u{273b} Cooked for 5s \u{b7} 2 shells still running"
        ));
        assert!(
            !is_still_running_status_line("\u{273b} Cooked for 5s \u{b7} 2 shells, still running"),
            "one comma defeats it"
        );
        // Interleaved survives only because the tail stays positionally intact.
        assert!(is_still_running_status_line(
            "\u{273b} Cooked for 5s \u{b7} 1 monitor, 2 shells still running"
        ));
    }
}
