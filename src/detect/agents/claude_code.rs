use super::super::{has_confirmation_prompt, has_selection_prompt, AgentState};
use crate::api::agent_state::HumanReason;

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

    if has_live_blocked_form(content)
        || has_folder_trust_dialog(content)
        || has_login_dialog(content)
    {
        return Some(AgentState::Blocked);
    }

    if has_working_chrome(content) {
        return Some(AgentState::Working);
    }

    if has_provider_limit(content) {
        return Some(AgentState::Blocked);
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
        || has_login_dialog(content)
        || has_provider_limit(content)
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

/// The spinner above the prompt box follows `text`, echoed as the last prompt
/// in the transcript (#870).
pub(in crate::detect) fn live_turn_follows_echo(content: &str, text: &str) -> bool {
    if !has_prompt_box(content) || !has_working_chrome(content) {
        return false;
    }
    let lines: Vec<&str> = content_above_prompt_box(content).lines().collect();
    let spinner = spinner_rows(&lines)
        .last()
        .map(|spinner| spinner.rows.start);
    let interrupt = lines
        .iter()
        .rposition(|line| line.to_lowercase().contains("esc to interrupt"));
    let Some(working) = spinner.max(interrupt) else {
        return false;
    };
    let Some(echo) = lines[..working]
        .iter()
        .rposition(|line| line.starts_with("> ") || line.starts_with("❯ "))
    else {
        return false;
    };
    super::super::echo_matches(&lines, echo, working, &['>', '❯'], text)
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
        && (last_non_empty_line(above).is_some_and(|line| still_running_task_count(line) > 0)
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
///
/// 2.1.296 draws the first-start dialog under a `──` rule instead of in a
/// frame, ending on its own confirm footer (#1023). A prompt box drawn after
/// it would own the last rule, so the region after the last rule is the live
/// dialog there.
pub(in crate::detect) fn has_folder_trust_dialog(content: &str) -> bool {
    if has_folder_trust_with_permissions_dialog(content) {
        return true;
    }
    let after_rule = Some(flat_words(content_after_last_horizontal_rule(content)))
        .filter(|flat| flat.ends_with("enter to confirm esc to cancel"));
    trust_dialog_box(content)
        .map(|box_text| flat_words(&box_text))
        .into_iter()
        .chain(after_rule)
        .any(|flat| {
            TRUST_DIALOGS
                .iter()
                .any(|(question, accept)| flat.contains(question) && flat.contains(accept))
        })
}

/// The live folder-trust variant that also pre-approves configured tools (#1116).
/// The numeric notice and known trust controls must share a live dialog region.
/// A frame or final confirm footer bounds it, independently of the cancel label.
pub(in crate::detect) fn has_folder_trust_with_permissions_dialog(content: &str) -> bool {
    let framed = trust_dialog_box(content);
    let region = framed
        .as_deref()
        .unwrap_or_else(|| content_after_last_horizontal_rule(content));
    let flat = flat_words(region);
    let notice = flat
        .split_once("this folder pre approves ")
        .and_then(|(_, rest)| rest.split_once(" tool permissions"))
        .is_some_and(|(count, _)| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()));
    let trust_controls = TRUST_DIALOGS
        .iter()
        .any(|(question, accept)| flat.contains(question) && flat.contains(accept))
        || (flat.contains("accessing workspace") && flat.contains(TRUST_DIALOGS[0].1));
    notice
        && trust_controls
        && (framed.is_some() || flat.ends_with("enter to confirm esc to cancel"))
}

/// Claude's login method picker, or its question whether to use an API key
/// found in the environment, each as the live tail (2.1.296, #1023).
fn has_login_dialog(content: &str) -> bool {
    let tail = flat_words(&bottom_non_empty_lines(content, 12).join("\n"));
    (tail.contains("select login method") && tail.contains("claude account with subscription"))
        || (tail.contains("detected a custom api key")
            && tail.contains("do you want to use this api key"))
}

/// Claude's own usage-limit chrome (2.1.296, #1023): the warning in the
/// footer under the prompt box while it waits to continue, the
/// `/rate-limit-options` menu, or a last turn that ended on the limit
/// notice, which is all a stopped wait leaves on screen. Turn-scoped, so a
/// later turn that went through clears it.
pub(in crate::detect) fn has_provider_limit(content: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    let footer = claude_prompt_box_top_border_index(&lines).and_then(|top| {
        let bottom = lines[top + 1..]
            .iter()
            .position(|line| is_horizontal_rule(line))?;
        Some(&lines[top + 2 + bottom..])
    });
    if footer.is_some_and(|footer| {
        footer.iter().any(|line| {
            line.trim_start()
                .starts_with("\u{26a0} Usage limit reached")
        })
    }) {
        return true;
    }
    let tail = flat_words(&bottom_non_empty_lines(content, 8).join("\n"));
    if tail.contains("what do you want to do")
        && tail.contains("limit to reset")
        && tail.ends_with("enter to confirm esc to cancel")
    {
        return true;
    }
    let above: Vec<&str> = content_above_prompt_box(content).lines().collect();
    let turn = above
        .iter()
        .rposition(|line| line.starts_with("\u{276f} "))
        .map_or(&above[..0], |echo| &above[echo + 1..]);
    turn.iter().any(|line| {
        line.trim_start()
            .strip_prefix('\u{23bf}')
            .is_some_and(|notice| {
                notice.trim_start().starts_with("You've hit your ")
                    && notice.contains(" \u{b7} resets ")
            })
    })
}

/// What a blocked Claude screen asks of a human (#1023). Checked in order of
/// specificity: the dialogs that own the whole pane, then the interview, then
/// a tool permission. A prompt none of them names is a `dialog`.
pub(in crate::detect) fn needs_human(content: &str) -> HumanReason {
    let tail = bottom_non_empty_lines(content, 16)
        .join("\n")
        .to_lowercase();
    if has_folder_trust_dialog(content) {
        HumanReason::Trust
    } else if has_login_dialog(content) {
        HumanReason::Login
    } else if [
        "type something.",
        "chat about this",
        "review your answers",
        "skip interview and plan immediately",
    ]
    .iter()
    .any(|marker| tail.contains(marker))
    {
        HumanReason::Question
    } else if [
        "do you want to proceed?",
        "do you want to make this edit",
        "do you want to create",
        "waiting for permission",
        "do you want to allow this connection?",
        "tab to amend",
        "ctrl+e to explain",
    ]
    .iter()
    .any(|marker| tail.contains(marker))
    {
        HumanReason::Permission
    } else {
        HumanReason::Dialog
    }
}

/// The `AskUserQuestion` form a `question` screen draws (ADR-0028 §8a,
/// #1038), read from its invariant controls, all required: `Enter to select`
/// as the last line, a numbered `Chat about this` under the rule that closes
/// the form, and the rule that opens it above the numbered options. A form's
/// review page and every other dialog lack that frame and read `None`. The
/// cursor (`❯`) and the checkbox marks are not part of the question, so a
/// redraw reads the same.
pub(in crate::detect) fn question(content: &str) -> Option<crate::api::question::QuestionForm> {
    use crate::api::question::{QuestionForm, QuestionOption};
    let lines: Vec<&str> = content.lines().map(str::trim_end).collect();
    let last = lines.iter().rposition(|line| !line.trim().is_empty())?;
    if !lines[last].starts_with("Enter to select") {
        return None;
    }
    let close = lines[..last]
        .iter()
        .rposition(|line| is_horizontal_rule(line))?;
    lines[close + 1..last]
        .iter()
        .filter_map(|line| question_option_line(line))
        .find(|(_, _, label)| *label == "Chat about this")?;
    let open = lines[..close]
        .iter()
        .rposition(|line| is_horizontal_rule(line))?;
    let form = &lines[open + 1..close];
    let first = form
        .iter()
        .position(|line| question_option_line(line).is_some())?;
    let tabs = form
        .first()
        .filter(|line| line.contains(['\u{2610}', '\u{2612}']))
        .map(|line| {
            line.split("  ")
                .filter_map(|tab| tab.trim().strip_prefix(['\u{2610}', '\u{2612}']))
                .map(|tab| tab.trim().to_string())
                .collect::<Vec<_>>()
        });
    let prompt = form[usize::from(tabs.is_some())..first]
        .iter()
        .map(|line| line.trim().trim_start_matches('\u{2502}').trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if prompt.is_empty() {
        return None;
    }
    let mut question = QuestionForm {
        header: tabs
            .filter(|tabs| tabs.len() == 1)
            .and_then(|mut tabs| tabs.pop()),
        prompt,
        options: Vec::new(),
        multi_select: false,
        allow_other: false,
    };
    // The column a description is indented to, while one may follow.
    let mut description_col = None;
    for line in &form[first..] {
        if let Some((col, id, label)) = question_option_line(line) {
            let (col, label) = match label
                .strip_prefix("[ ] ")
                .or_else(|| label.strip_prefix("[\u{2714}] "))
            {
                Some(label) => {
                    question.multi_select = true;
                    (col + 4, label)
                }
                None => (col, label),
            };
            description_col = None;
            if matches!(label, "Type something." | "Type something") {
                question.allow_other = true;
            } else {
                description_col = Some(col);
                question.options.push(QuestionOption {
                    id: id.to_string(),
                    label: label.to_string(),
                    description: None,
                });
            }
            continue;
        }
        let indent = line.chars().take_while(|c| *c == ' ').count();
        let text = line.trim();
        match (description_col, question.options.last_mut()) {
            (Some(col), Some(option)) if !text.is_empty() && indent >= col => {
                let description = option.description.get_or_insert_with(String::new);
                if !description.is_empty() {
                    description.push(' ');
                }
                description.push_str(text);
            }
            _ if text.is_empty() => {}
            _ => description_col = None,
        }
    }
    (!question.options.is_empty()).then_some(question)
}

/// A numbered row of the question form, `❯ 2. Blue` or `  2. Blue`: the
/// column its label starts at, its number and its label.
fn question_option_line(line: &str) -> Option<(usize, &str, &str)> {
    let rest = line
        .strip_prefix("\u{276f} ")
        .or_else(|| line.strip_prefix("  "))?;
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let label = rest[digits..].strip_prefix(". ")?;
    (digits > 0).then(|| (2 + digits + 2, &rest[..digits], label.trim()))
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
/// turn rather than working chrome (#911). The line must start with the
/// completion glyph and the clock is required (`4:32 PM`, `8:05pm`, `17:59`),
/// so prose that merely says "done" does not count.
fn has_turn_done_marker(line: &str) -> bool {
    let line = line.trim();
    // The recap glyph `※` is in the spinner set but starts prose, not a
    // completion line.
    if !line.starts_with(|c: char| c != '\u{203b}' && SPINNER_CHARS.contains(c)) {
        return false;
    }
    line.split('\u{b7}').skip(1).any(|segment| {
        segment
            .trim()
            .strip_prefix("done ")
            .and_then(|clock| clock.split_whitespace().next())
            .is_some_and(|clock| {
                let lower = clock.to_ascii_lowercase();
                let clock = lower
                    .strip_suffix("am")
                    .or_else(|| lower.strip_suffix("pm"))
                    .unwrap_or(&lower);
                clock.starts_with(|c: char| c.is_ascii_digit())
                    && clock.contains(':')
                    && clock.chars().all(|c| c.is_ascii_digit() || c == ':')
            })
    })
}

/// Background tasks a completion line says are still running, counted the way
/// Claude lists them: `1 shell still running`, `2 shells, 1 monitor still
/// running`, `2 local agents still running`. Only the segment that ends in
/// `still running` counts. Used for the done-marked line alone, where the
/// count is a report about a finished turn rather than evidence of work.
fn still_running_task_count(line: &str) -> u32 {
    let lower = line.to_ascii_lowercase();
    let Some(segment) = lower
        .split('\u{b7}')
        .map(str::trim)
        .find(|segment| segment.ends_with("still running"))
    else {
        return 0;
    };
    named_task_count(segment)
}

/// Sum of the `N shell(s)` / `N monitor(s)` / `N (local) agent(s)` counts in
/// lowercased text.
fn named_task_count(lower_text: &str) -> u32 {
    let words: Vec<&str> = lower_text
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|word| !word.is_empty() && *word != "local")
        .collect();
    words
        .windows(2)
        .filter_map(|pair| {
            let count = pair[0].parse::<u32>().ok()?;
            matches!(
                pair[1],
                "shell" | "shells" | "monitor" | "monitors" | "agent" | "agents"
            )
            .then_some(count)
        })
        .sum()
}

/// Background tasks Claude says are running (#1022): the counter in the
/// footer below the prompt box (`⏵⏵ bypass permissions on · 2 shells`), or
/// the `still running` suffix of a done-marked line that ends the transcript.
/// Both name the same tasks, so the larger reading wins. A narrow pane wraps
/// the done line, and the footer drops its `↓ to manage` hint, so neither is
/// required. `None` without a prompt box: an unreadable footer is not an
/// empty one.
pub(in crate::detect) fn background_task_count(content: &str) -> Option<u32> {
    let lines: Vec<&str> = content.lines().collect();
    let top_border = claude_prompt_box_top_border_index(&lines)?;
    if !has_prompt_box(content) {
        return None;
    }
    let footer = lines[top_border + 1..]
        .iter()
        .skip_while(|line| !is_horizontal_rule(line))
        .skip(1)
        .map(|line| named_task_count(&line.to_ascii_lowercase()))
        .max()
        .unwrap_or(0);
    let above = content_above_prompt_box(content);
    let done_line = last_non_empty_line(above)
        .filter(|line| has_turn_done_marker(line))
        .map_or(0, still_running_task_count);
    Some(footer.max(done_line))
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

/// The glyphs Claude cycles through on its spinner line. The finished turn's
/// completion line (`✻ Brewed for 3s`) keeps whichever one it stopped on.
const SPINNER_CHARS: &str = "·✱✲✳✴✵✶✷✸✹✺✻✼✽✾✿❀❁❂❃❇❈❉❊❋✢✣✤✥✦✧✨⊛⊕⊙◉◎◍⁂⁕※⍟☼★☆";

/// Claude Code spinner characters + activity label.
/// The verb changes frequently ("Processing…", "Pouncing…", etc.), so rely
/// on the spinner glyph + trailing ellipsis rather than specific wording.
/// Include Claude's narrow-pane middle-dot frame too, and a label wrapped
/// over several rows (see [`spinner_at`]).
pub(in crate::detect) fn has_spinner_activity(content: &str) -> bool {
    spinner_activity_text(content).is_some()
}

/// The free-text activity label from Claude's spinner line, e.g.
/// "✶ Implementing the parser… (esc to interrupt)" -> "Implementing the parser".
/// A wrapped label comes back joined into one line.
pub(in crate::detect) fn spinner_activity_text(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    (0..lines.len())
        .find_map(|start| spinner_at(&lines, start))
        .map(|spinner| spinner.label)
}

/// One spinner as drawn on screen: the rows it covers and its label.
pub(in crate::detect) struct SpinnerRows {
    pub(in crate::detect) rows: std::ops::Range<usize>,
    pub(in crate::detect) label: String,
}

/// Every spinner on these rows, top to bottom. A wrapped spinner is one
/// entry covering all of its rows.
pub(in crate::detect) fn spinner_rows(lines: &[&str]) -> Vec<SpinnerRows> {
    let mut found = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        match spinner_at(lines, start) {
            Some(spinner) => {
                start = spinner.rows.end;
                found.push(spinner);
            }
            None => start += 1,
        }
    }
    found
}

/// The spinner whose glyph sits on row `start`, if there is one.
///
/// Claude draws `<glyph> <label>…`, then its stats (` (12s · ↓ 3k tokens)`)
/// when they fit. A label too long for the pane is wrapped, not truncated,
/// and the stats are dropped. Captured from a real Claude in an isolated flk
/// (#1264), the two wrapped layouts alternate with the pane's width:
///
/// ```text
/// ✢ Investigating why every working pane reads idle      label beside the glyph
///   after a client detaches from the server…
///
/// ·                                                       label below the glyph
/// Investigating why every working pane reads idle after a client detaches
/// from the server…
/// ```
///
/// A wrapped label counts only as that whole shape: each continuation row
/// starts exactly at the label's column, and the only `…` is the one that ends
/// the last row. Reading the glyph row alone took a wrapped spinner for no
/// spinner at all, so every working Claude pane whose activity label outgrew
/// its width read idle, as all of them did when a client shrank the server to
/// 80x24 and detached (#1264). The recap glyph `※` starts prose, never a
/// wrapped label.
fn spinner_at(lines: &[&str], start: usize) -> Option<SpinnerRows> {
    let line = lines[start];
    let indent = leading_whitespace(line);
    let mut chars = line.trim().chars();
    let glyph = chars.next().filter(|c| SPINNER_CHARS.contains(*c))?;
    let rest = chars.as_str();
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    if let Some((label, _)) = rest.split_once('\u{2026}') {
        return spinner_label(label.trim().to_string(), start..start + 1);
    }
    if glyph == '\u{203b}' {
        return None;
    }
    let (column, mut label) = match rest.trim() {
        "" => (indent, String::new()),
        beside => (indent + 2, beside.to_string()),
    };
    for (row, line) in lines.iter().enumerate().skip(start + 1) {
        let text = line.trim();
        if text.is_empty() || text.starts_with('⎿') || leading_whitespace(line) != column {
            return None;
        }
        if !label.is_empty() {
            label.push(' ');
        }
        let Some((last, tail)) = text.split_once('\u{2026}') else {
            label.push_str(text);
            continue;
        };
        if !tail.is_empty() && !tail.starts_with(" (") {
            return None;
        }
        label.push_str(last.trim_end());
        return spinner_label(label, start..row + 1);
    }
    None
}

fn spinner_label(label: String, rows: std::ops::Range<usize>) -> Option<SpinnerRows> {
    label
        .chars()
        .any(char::is_alphanumeric)
        .then_some(SpinnerRows { rows, label })
}

fn leading_whitespace(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
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

    #[test]
    fn permission_trust_fixture_is_a_strong_human_trust_block() {
        let screen = include_str!(
            "../../../tests/fixtures/attention-1023/claude-trust-with-permissions.txt"
        );
        for screen in [
            screen.to_string(),
            screen
                .replace("17 tool", "301 tool")
                .replace("devkit-3c4e4016", "another-checkout"),
        ] {
            assert_eq!(detect_structural(&screen), Some(AgentState::Blocked));
            assert!(has_visible_blocker(&screen));
            assert_eq!(needs_human(&screen), HumanReason::Trust);
        }
    }

    /// Captured from a throwaway Claude in an isolated flk (#1022): the
    /// footer counts the `run_in_background` shells after the turn, across a
    /// later turn that started none, and drops the counter once they exit. A
    /// 52-column pane wraps the done line, so only the footer can say.
    #[test]
    fn captured_footers_count_background_tasks() {
        macro_rules! fixture {
            ($name:literal) => {
                include_str!(concat!(
                    "../../../tests/fixtures/claude-background-1022/",
                    $name,
                    ".txt"
                ))
            };
        }
        for (screen, count) in [
            (fixture!("one-shell"), 1),
            (fixture!("two-shells"), 2),
            (fixture!("later-turn-two-shells"), 2),
            (fixture!("narrow-wrapped"), 1),
            (fixture!("none"), 0),
        ] {
            assert_eq!(background_task_count(screen), Some(count), "{screen}");
        }
        assert!(!settled_with_background_shells(fixture!("narrow-wrapped")));
        let trust = "Quick safety check\n\u{276f} No, exit\n  Yes, I trust this folder\n";
        assert_eq!(background_task_count(trust), None, "no footer to read");
        let monitor =
            prompt_box_below("x") + "  \u{23f5}\u{23f5} auto mode on \u{b7} 2 shells, 1 monitor\n";
        assert_eq!(background_task_count(&monitor), Some(3));
    }

    #[test]
    fn a_spinner_follows_the_echoed_prompt_past_a_notice() {
        let rule = "─".repeat(40);
        let screen = format!(
            "> Read /fixture/briefs/task.md and\n  execute it exactly.\n\n  ⎿  Heads up, an update is available.\n\n✶ Pondering… (3s · esc to interrupt)\n\n{rule}\n❯ \n{rule}\n"
        );
        let text = "Read /fixture/briefs/task.md and execute it exactly.";
        assert!(live_turn_follows_echo(&screen, text));
        assert!(live_turn_follows_echo(&screen.replace("> ", "❯ "), text));
        assert!(!live_turn_follows_echo(
            &screen,
            "Read /fixture/briefs/other.md"
        ));
        // A finished turn under the echo is not a live one.
        let done = screen.replace("✶ Pondering… (3s · esc to interrupt)", "✻ Brewed for 3s");
        assert!(!live_turn_follows_echo(&done, text));
    }

    /// Captured from a real Claude in an isolated flk (#1264): the same
    /// working turn in a 52-column pane (label beside the glyph) and a
    /// 72-column one (label below it). Neither shows `esc to interrupt`, so the
    /// wrapped spinner is the only working chrome on screen.
    #[test]
    fn captured_wrapped_spinner_labels_are_working() {
        for screen in [
            include_str!(
                "../../../tests/fixtures/claude-wrapped-spinner-1264/label-beside-glyph.txt"
            ),
            include_str!(
                "../../../tests/fixtures/claude-wrapped-spinner-1264/label-below-glyph.txt"
            ),
        ] {
            assert!(has_prompt_box(screen), "{screen}");
            assert!(has_working_chrome(screen), "{screen}");
            assert_eq!(detect(screen), AgentState::Working, "{screen}");
            assert_eq!(
                live_activity_text(screen).as_deref(),
                Some("Investigating why every working pane reads idle after a client detaches from the server"),
            );
        }
    }

    #[test]
    fn a_wrapped_spinner_is_one_row_range() {
        let lines = [
            "out",
            "",
            "✢ Reading the",
            "  brief…",
            "  ⎿  Tip: x…",
            "·",
            "Next",
            "step… (4s)",
        ];
        let spinners = spinner_rows(&lines);
        let found: Vec<_> = spinners
            .iter()
            .map(|s| (s.rows.clone(), s.label.as_str()))
            .collect();
        assert_eq!(found, [(2..4, "Reading the brief"), (5..8, "Next step")]);
    }

    #[test]
    fn a_wrapped_label_must_keep_its_shape() {
        for (screen, why) in [
            (
                "✢ Reading the\n    brief…",
                "continuation off the label column",
            ),
            (
                "✢ Reading the\nbrief…",
                "beside-glyph label continued at the glyph column",
            ),
            ("·\n  Reading the brief…", "below-glyph label indented"),
            ("✢ Reading the\n\n  brief…", "a blank row ends the label"),
            (
                "✢ Reading the\n  ⎿  Tip: try this…",
                "the tip row is not the label",
            ),
            (
                "✢ Reading the\n  brief… and more prose",
                "prose after the ellipsis",
            ),
            ("※ recap: the parser\n  works… Next: ship it", "recap prose"),
            (
                "※ recap: the parser\n  works…",
                "the recap glyph never wraps a label",
            ),
            (
                "✻ Crunched for 4s · done 1:26 AM · 1 shell still\n  running",
                "a wrapped done line",
            ),
            ("✢ Reading the\n  brief", "no ellipsis at all"),
        ] {
            assert!(!has_spinner_activity(screen), "{why}: {screen}");
        }
    }

    #[test]
    fn a_wrapped_spinner_follows_the_echoed_prompt() {
        let rule = "─".repeat(40);
        let screen = format!(
            "❯ Read /fixture/briefs/task.md and\n  execute it exactly.\n\n✶ Reading the brief and\n  planning the change…\n\n{rule}\n❯ \n{rule}\n"
        );
        assert!(live_turn_follows_echo(
            &screen,
            "Read /fixture/briefs/task.md and execute it exactly."
        ));
    }

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
        assert!(has_turn_done_marker(
            "\u{2736} Brewed for 3s \u{b7} done 8:05pm"
        ));
        assert!(has_turn_done_marker(
            "  \u{273b} Brewed for 3s \u{b7} done 8:05 pm"
        ));
        for line in [
            "\u{273b} Crunched for 7s \u{b7} 1 shell still running",
            "\u{273b} Crunched for 7s \u{b7} done \u{b7} 1 shell still running",
            "\u{273b} Crunched for 7s \u{b7} done soon \u{b7} 1 shell still running",
            "done 4:32 PM \u{b7} 1 shell still running",
            // Prose, not the completion glyph, even with a clock in it.
            "Deploy \u{b7} done 4:32 PM \u{b7} 1 shell still running",
            "\u{25cf} Shipped \u{b7} done 4:32 PM \u{b7} 1 shell still running",
            "\u{203b} recap: merged \u{b7} done 4:32 PM \u{b7} 1 shell still running",
            "\u{273b} Brewed for 3s \u{b7} done 8:05xm",
        ] {
            assert!(!has_turn_done_marker(line), "{line}");
        }
        // Without the marker the line is still working chrome, as before.
        let content =
            prompt_box_below("\u{273b} Crunched for 7s \u{b7} done \u{b7} 1 shell still running");
        assert!(has_working_chrome(&content));
        assert!(!settled_with_background_shells(&content));
    }

    /// The live footer shape from `background_shell_footer_matchers_miss_the_real_footer`
    /// under a done-marked completion line: the comma and the monitor no longer
    /// hide the count once the turn is known to be over.
    #[test]
    fn done_marked_turn_with_shells_and_a_monitor_is_settled() {
        let content = done_turn_with_leftover_shell(
            "  \u{23f5}\u{23f5} bypass permissions on \u{b7} 2 shells, 1 monitor \u{b7} \u{2190} for agents",
        )
        .replace(
            "Crunched for 7m 41s \u{b7} done 4:32 PM \u{b7} 1 shell still running",
            "Worked for 27s \u{b7} done 8:05 PM \u{b7} 2 shells, 1 monitor still running",
        );
        assert!(content.contains("2 shells, 1 monitor still running"));
        assert!(!has_working_chrome(&content));
        assert_eq!(detect_structural(&content), Some(AgentState::Idle));
        assert!(settled_with_background_shells(&content));
    }

    #[test]
    fn still_running_task_count_tolerates_claude_list_punctuation() {
        for (line, count) in [
            ("\u{273b} Brewed for 3s \u{b7} done 4:32 PM \u{b7} 1 shell still running", 1),
            ("\u{273b} Worked for 27s \u{b7} done 8:05 PM \u{b7} 2 shells, 1 monitor still running", 3),
            ("\u{273b} Worked for 4s \u{b7} done 9:01 AM \u{b7} 2 local agents still running", 2),
            ("\u{273b} Worked for 4s \u{b7} done 9:01 AM \u{b7} 1 monitor still running", 1),
            ("\u{273b} Baked for 16m 38s \u{b7} done 5:54 PM", 0),
            ("\u{273b} Baked for 3s \u{b7} 2 files still running late", 0),
        ] {
            assert_eq!(still_running_task_count(line), count, "{line}");
        }
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
        let screen =
            include_str!("../../../tests/fixtures/attention-1023/claude-trust-gated-grants.txt");
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
