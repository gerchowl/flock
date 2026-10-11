use super::super::{has_confirmation_prompt, has_interrupt_pattern, AgentState};

pub(super) fn detect(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    // Strong blocked patterns are structural Codex UI chrome, so they can win
    // even when the prompt region is visible.
    if has_codex_strong_blocked_prompt(&lower) {
        return AgentState::Blocked;
    }

    // Working
    if has_codex_slow_model_notice(content) || has_codex_working_status_at_current_prompt(content) {
        return AgentState::Working;
    }

    // Idle
    if has_codex_current_prompt(content) {
        return AgentState::Idle;
    }

    // Weak blocked patterns are too broad to scan above a visible prompt. They
    // only apply when Codex does not currently show an idle prompt region.
    if has_codex_weak_blocked_prompt(&lower) {
        return AgentState::Blocked;
    }

    // Fallback working signals for narrow captures where the footer scrolled
    // out or the working row is the only Codex chrome visible.
    if has_interrupt_pattern(&lower) || has_codex_working_header(content) {
        return AgentState::Working;
    }

    AgentState::Idle
}

pub(super) fn has_visible_blocker(content: &str) -> bool {
    let lower = content.to_lowercase();
    has_codex_strong_blocked_prompt(&lower)
}

pub(super) fn has_prompt(content: &str) -> bool {
    has_codex_current_prompt(content) || content.lines().any(codex_prompt_line)
}

pub(super) fn has_visible_working(content: &str) -> bool {
    has_codex_slow_model_notice(content)
        || has_codex_live_working_at_current_prompt(content)
        || (!has_codex_current_prompt(content) && has_codex_visible_working_without_prompt(content))
}

/// The running turn just above the live composer follows `text`, echoed as
/// the last prompt (#870). Notices Codex prints in between, like the weekly
/// limit warning, are not prompts and do not break the chain.
pub(in crate::detect) fn live_turn_follows_echo(content: &str, text: &str) -> bool {
    let Some((lines, prompt)) = codex_current_prompt_region(content) else {
        return false;
    };
    let Some(working) = lines[..prompt]
        .iter()
        .rposition(|line| codex_block_marker_line(line))
    else {
        return false;
    };
    // Queued input waits behind another turn, so it is not this prompt's turn.
    if !codex_live_working_line(lines[working]) || codex_queued_input_header_line(lines[working]) {
        return false;
    }
    let Some(echo) = lines[..working]
        .iter()
        .rposition(|line| codex_prompt_line(line))
    else {
        return false;
    };
    super::super::echo_matches(&lines, echo, working, &['›'], text)
}

fn has_codex_slow_model_notice(content: &str) -> bool {
    // Recent-buffer wrapping can split words. Ignore whitespace and the moving
    // selection marker, but require the menu controls and notice at the live tail.
    let compact = |text: &str| {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
    };
    let bottom = compact(&bottom_non_empty_lines(content, 16).join("\n"))
        .replace("›1.", "1.")
        .replace("›2.", "2.")
        .replace("›3.", "3.");
    bottom.contains(&compact("Giving this request a little extra thought"))
        && bottom.contains(&compact(
            "1. Retry with a faster model 2. Dismiss and keep waiting 3. Learn more",
        ))
        && bottom.ends_with(&compact(
            "No action is required. Codex will keep waiting, and this menu will close when the response is ready.",
        ))
}

pub(super) fn is_transcript_viewer(content: &str) -> bool {
    let bottom_lines = bottom_non_empty_lines(content, 3);
    let Some(last_line) = bottom_lines.last() else {
        return false;
    };
    let bottom_text = normalize_lines(&bottom_lines);

    bottom_text.contains("↑/↓ to scroll")
        && bottom_text.contains("pgup/pgdn to page")
        && bottom_text.contains("home/end to jump")
        && bottom_text.contains("q to quit")
        && has_codex_edit_prev_controls(&bottom_text)
        && transcript_control_tail(last_line)
}

fn has_codex_edit_prev_controls(bottom_text: &str) -> bool {
    bottom_text.contains("esc to edit prev") || bottom_text.contains("esc/← to edit prev")
}

fn has_codex_visible_working_without_prompt(content: &str) -> bool {
    let mut recent_lines = content.lines().rev().filter(|line| !line.trim().is_empty());
    let Some(last_line) = recent_lines.next() else {
        return false;
    };

    if codex_live_working_line(last_line) {
        return true;
    }

    codex_status_detail_line(last_line)
        && recent_lines
            .take(4)
            .find(|line| codex_block_marker_line(line))
            .is_some_and(codex_live_working_line)
}

fn has_codex_strong_blocked_prompt(lower_content: &str) -> bool {
    has_codex_hook_review(lower_content)
        || has_codex_folder_trust(lower_content)
        || lower_content.contains("press enter to confirm or esc to cancel")
        || lower_content.contains("enter to submit answer")
        || lower_content.contains("enter to submit all")
        || lower_content.contains("allow command?")
}

fn has_codex_weak_blocked_prompt(lower_content: &str) -> bool {
    lower_content.contains("[y/n]")
        || lower_content.contains("yes (y)")
        || has_confirmation_prompt(lower_content)
}

fn has_codex_live_working_at_current_prompt(content: &str) -> bool {
    codex_last_block_marker_before_current_prompt(content).is_some_and(codex_live_working_line)
}

fn has_codex_working_status_at_current_prompt(content: &str) -> bool {
    codex_last_block_marker_before_current_prompt(content).is_some_and(codex_working_status_line)
}

fn codex_last_block_marker_before_current_prompt(content: &str) -> Option<&str> {
    let (lines, prompt_index) = codex_current_prompt_region(content)?;

    lines[..prompt_index]
        .iter()
        .rev()
        .find(|line| codex_block_marker_line(line))
        .copied()
}

fn has_codex_working_header(content: &str) -> bool {
    content.lines().any(codex_working_status_line)
}

fn codex_live_working_line(line: &str) -> bool {
    if codex_queued_input_header_line(line) {
        return true;
    }

    let trimmed = line.trim_start();
    let lower = trimmed.to_lowercase();
    codex_working_status_line(line)
        && (trimmed.contains("Waiting for background terminal")
            || lower.contains("esc to interrupt")
            || lower.contains("esc…")
            || lower.contains("background terminal running")
            || lower.contains("/ps to view")
            || lower.contains("/stop to close"))
}

fn codex_working_status_line(line: &str) -> bool {
    if codex_queued_input_header_line(line) {
        return true;
    }

    let trimmed = line.trim_start();
    let lower = trimmed.to_lowercase();
    trimmed.starts_with('•')
        && (trimmed.contains("Working (")
            || trimmed.contains("Compacting context (")
            || trimmed.contains("Waiting for background terminal (")
            || lower.contains("reviewing approval request (")
            || (lower.contains("reviewing ") && lower.contains(" approval requests ("))
            || trimmed.contains("Booting MCP server:"))
}

fn has_codex_current_prompt(content: &str) -> bool {
    codex_current_prompt_region(content).is_some()
}

/// Prompt and footer line indices in the live bottom-buffer composer.
pub(in crate::detect) fn composer_region(content: &str) -> Option<(usize, usize)> {
    let lines: Vec<_> = content.lines().collect();
    let prompt = lines
        .iter()
        .rposition(|line| codex_prompt_line(line.trim_start()))?;
    let footer = lines[prompt + 1..]
        .iter()
        .position(|line| composer_footer(line))?;
    let footer = prompt + 1 + footer;
    // Bullets inside a draft are input, but output below the footer means
    // this is no longer the live composer.
    if lines[footer + 1..]
        .iter()
        .any(|line| codex_block_marker_line(line.trim_start()))
    {
        return None;
    }
    Some((prompt, footer))
}

fn composer_footer(line: &str) -> bool {
    if line.contains("? for shortcuts") || line.contains("context left") {
        return true;
    }
    // Codex 0.160.1 can show only its model/effort and directory status line.
    // Require all three fields, below the prompt, rather than banner text.
    let Some((model_effort, directory)) = line.trim().split_once(" · ") else {
        return false;
    };
    let Some((model, effort)) = model_effort.split_once(' ') else {
        return false;
    };
    model.to_ascii_lowercase().starts_with("gpt-")
        && !model.chars().any(char::is_whitespace)
        && matches!(
            effort,
            "default" | "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
        )
        && (directory.starts_with('/') || directory.starts_with("~/"))
}

fn codex_current_prompt_region(content: &str) -> Option<(Vec<&str>, usize)> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt_index = lines.iter().rposition(|line| codex_prompt_line(line))?;

    if lines[prompt_index + 1..]
        .iter()
        .any(|line| codex_block_marker_line(line))
    {
        return None;
    }

    Some((lines, prompt_index))
}

fn codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_marker_line(line: &str) -> bool {
    line.starts_with('•') || line.starts_with('■') || line.starts_with('✗') || line.starts_with('✓')
}

fn codex_status_detail_line(line: &str) -> bool {
    line.trim_start().starts_with('└')
}

fn codex_queued_input_header_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('•') {
        return false;
    }

    let lower = trimmed.to_lowercase();
    lower.starts_with("• queued follow-up inputs")
        || lower.starts_with("• messages to be submitted after next tool call")
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
    lower.contains("q to quit")
        || lower.contains("esc to edit")
        || lower.contains("esc/← to edit")
        || lower.contains("edit message")
}

pub(in crate::detect) fn is_progress_chrome(line: &str) -> bool {
    codex_working_status_line(line)
}

pub(in crate::detect) fn has_codex_hook_review(content: &str) -> bool {
    let bottom = bottom_non_empty_lines(content, 10);
    bottom
        .last()
        .is_some_and(|line| line.trim() == "enter confirm · esc skip")
        && bottom.iter().any(|line| line.trim() == "hooks need review")
        && bottom.iter().any(|line| line.trim() == "› 1. review hooks")
}

/// Codex's first-start "Folder access" dialog (#828), which has no composer.
///
/// All three controls must sit in the live tail: the question that opens the
/// explanation, the selected trust choice, and the footer as the last line.
/// A transcript quoting the dialog is followed by a prompt, so it fails the
/// footer gate.
pub(in crate::detect) fn has_codex_folder_trust(content: &str) -> bool {
    let bottom = bottom_non_empty_lines(content, 16);
    bottom
        .last()
        .is_some_and(|line| line.trim() == "enter continue · esc quit")
        && bottom
            .iter()
            .any(|line| line.trim_start().starts_with("trust this folder?"))
        && bottom
            .iter()
            .any(|line| line.trim() == "› 1. trust and continue")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOLDER_TRUST: &str = include_str!("../../../tests/fixtures/codex/folder-trust.txt");

    const WEEKLY_NOTICE: &str =
        include_str!("../../../tests/fixtures/codex/working-after-weekly-notice.txt");
    const BRIEF_870: &str = "Read /fixture/codex-870/briefs/63.md and execute it exactly.";

    #[test]
    fn a_running_turn_past_the_weekly_notice_follows_its_echoed_prompt() {
        assert!(live_turn_follows_echo(WEEKLY_NOTICE, BRIEF_870));
        // Wrapping is not part of the prompt.
        assert!(live_turn_follows_echo(
            include_str!("../../../tests/fixtures/codex-submit-721/inline-working.txt"),
            "Reply exactly INLINE_CAPTURE_721. Do not use tools."
        ));
    }

    #[test]
    fn a_running_turn_confirms_only_the_prompt_echoed_above_it() {
        for text in [
            "Read /fixture/codex-870/briefs/64.md and execute it exactly.",
            // A prefix or an extension of the echo is a different prompt.
            "Read /fixture/codex-870/briefs/63.md",
            "Read /fixture/codex-870/briefs/63.md and execute it exactly. Then stop.",
            "",
        ] {
            assert!(!live_turn_follows_echo(WEEKLY_NOTICE, text), "{text:?}");
        }
        // An older echo with a finished turn under it, or with a Working row
        // in scrollback above it, is not a live turn of this prompt.
        let finished = WEEKLY_NOTICE.replace("• Working (6s • esc to interrupt)", "• Done.");
        assert!(!live_turn_follows_echo(&finished, BRIEF_870));
        let scrollback = format!(
            "{}\n› {BRIEF_870}\n\n› Ask Codex to do anything\n\n  GPT-6.1-Sol default · /fixture/codex-870/…\n",
            "• Working (6s • esc to interrupt)"
        );
        assert!(!live_turn_follows_echo(&scrollback, BRIEF_870));
        // A later prompt between the echo and the turn owns that turn.
        let later = WEEKLY_NOTICE.replace("⚠ Heads up", "› something else\n⚠ Heads up");
        assert!(!live_turn_follows_echo(&later, BRIEF_870));
        // Input queued behind another turn has not started.
        let queued = WEEKLY_NOTICE.replace(
            "• Working (6s • esc to interrupt)",
            "• Queued follow-up inputs",
        );
        assert!(!live_turn_follows_echo(&queued, BRIEF_870));
    }

    #[test]
    fn folder_trust_dialog_is_blocked_only_at_the_live_tail() {
        assert_eq!(detect(FOLDER_TRUST), AgentState::Blocked);
        assert!(has_visible_blocker(FOLDER_TRUST));
        // The dialog at the repository root itself carries no `Note:` paragraph.
        let at_root: String = FOLDER_TRUST
            .lines()
            .filter(|line| !line.contains("Note:") && !line.ends_with("/repo"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(detect(&at_root), AgentState::Blocked);
        // Accepted: the dialog is history above a live prompt.
        let accepted = format!("{FOLDER_TRUST}\n› Ask Codex to do anything\n  ? for shortcuts\n");
        assert_eq!(detect(&accepted), AgentState::Idle);
        assert!(!has_visible_blocker(&accepted));
        // Each control alone is not the dialog.
        for missing in [
            "Trust this folder?",
            "› 1. Trust and continue",
            "enter continue · esc quit",
        ] {
            let partial = FOLDER_TRUST.replace(missing, "");
            assert!(
                !has_codex_folder_trust(&partial.to_lowercase()),
                "{missing}"
            );
        }
        // The update modal shares the `enter continue` footer but not the dialog.
        let update = include_str!("../../../tests/fixtures/codex/startup-update-dialog.txt");
        assert!(!has_codex_folder_trust(&update.to_lowercase()));
    }

    #[test]
    fn detect_codex_compaction_remains_working_under_its_visible_prompt() {
        // #627's owner-provided screen, replayed through an isolated recent
        // pane read. The bullet and compaction header are working chrome.
        let screen = "• Compacting context (3s • esc to interrupt) · 1 bac\nkground terminal running · /ps to view · /stop to cl\nose\n  └ Making room to continue.\n› Ask Codex to do anything\n";
        assert_eq!(detect(screen), AgentState::Working);
        assert!(has_visible_working(screen));
        assert_eq!(
            detect("Compacting context is described in the transcript\n› Ask Codex to do anything"),
            AgentState::Idle
        );
    }

    const REVIEW: &str = "  Hooks need review\n  1 hook is new or changed.\n  Hooks can run outside the sandbox after you trust them.\n\n\n› 1. Review hooks\n  2. Trust all and continue\n  3. Continue without trusting (hooks won't run)\n\n  enter confirm · esc skip";

    #[test]
    fn hook_review_is_blocked_only_at_the_live_tail() {
        assert_eq!(detect(REVIEW), AgentState::Blocked);
        assert!(has_visible_blocker(REVIEW));
        let idle = format!("{REVIEW}\n\n› ");
        assert_eq!(detect(&idle), AgentState::Idle);
        assert!(!has_visible_blocker(&idle));
        let working = format!("{REVIEW}\n• Working (2s • esc to interrupt)\n› ");
        assert_eq!(detect(&working), AgentState::Working);
        assert!(!has_visible_blocker(&working));
        assert_eq!(detect("Hooks need review\n› "), AgentState::Idle);
        assert_eq!(detect("enter confirm · esc skip"), AgentState::Idle);
    }
}

#[cfg(test)]
#[path = "codex/submit_721_tests.rs"]
mod submit_721_tests;

#[cfg(test)]
mod slow_model_tests {
    use super::*;
    use crate::detect::{detect_agent, Agent};

    const MENU: &str = include_str!("../../../tests/fixtures/codex/slow-model-menu.txt");

    #[test]
    fn codex_slow_model_menu_is_visible_working_for_every_selection() {
        for choice in [
            "1. Retry with a faster model",
            "2. Dismiss and keep waiting",
            "3. Learn more",
        ] {
            let screen = MENU
                .replace("› ", "  ")
                .replace(&format!("  {choice}"), &format!("› {choice}"));
            let detection = detect_agent(Some(Agent::Codex), &screen);
            assert_eq!(detection.state, AgentState::Working);
            assert!(detection.visible_working);
            assert!(!detection.visible_idle);
            assert!(!detection.visible_blocker);
            assert!(!detection.skip_state_update);
            assert_eq!(composer_region(&screen), None);
        }
    }

    #[test]
    fn codex_slow_model_notice_in_history_is_not_live_working() {
        for tail in ["› ", "› Ask Codex to do anything\n? for shortcuts"] {
            let screen = format!("{MENU}\n{tail}");
            assert_eq!(detect(&screen), AgentState::Idle);
            assert!(!has_visible_working(&screen));
        }
        let screen = format!("{MENU}\nAllow command?\n[y/n]");
        assert_eq!(detect(&screen), AgentState::Blocked);
        assert!(!has_visible_working(&screen));
    }

    #[test]
    fn codex_slow_model_menu_requires_all_controls() {
        for line in [
            "Giving this request a little extra thought",
            "1. Retry with a faster model",
            "2. Dismiss and keep waiting",
            "3. Learn more",
            "No action is required.",
            "this menu will close when the response is ready.",
        ] {
            let screen = MENU.replace(line, "");
            assert_eq!(detect(&screen), AgentState::Idle);
            assert!(!has_visible_working(&screen));
        }
    }
}
