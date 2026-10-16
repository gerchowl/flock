//! Serialized, bounded keyboard submissions. Outcomes are separate from mail receipts.
use std::time::{Duration, Instant};

use super::api::responses::{encode_error, encode_error_with_data, encode_success};
use crate::api::schema::{PaneSubmitParams, ResponseResult};
use crate::app::App;
use crate::detect::Agent;
use bytes::Bytes;

pub(crate) const CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(50);
/// The bound on one injection's hold on the operator's input (#850): five
/// seconds of work plus the submit confirmation.
pub(crate) const INJECTION_LEASE: Duration = Duration::from_secs(5 + CONFIRM_WINDOW.as_secs());
/// How long clearing a draft, or reading it back, may take before flock stops
/// and keeps the draft in the pane's stash.
const DRAFT_STEP: Duration = Duration::from_secs(1);
/// Backspaces past the draft's visible length, for whitespace the composer
/// read trims. Extra ones on an empty composer do nothing.
const CLEAR_SLACK: usize = 8;

#[derive(Debug, Clone)]
pub(crate) struct Attempt {
    pub(crate) evidence: crate::api::schema::DeliveryAttempt,
    pub(crate) text: String,
    cursor: Box<str>,
    child_pid: Option<u32>,
    operator_input: Option<Instant>,
    prompt_generation: u64,
    pub(crate) due: Instant,
    deadline: Instant,
    paste_write: Option<crate::pty::write_receipt::WriteReceipt>,
    paste_written: bool,
    pub(crate) sent: bool,
    pub(crate) retried: bool,
    wake: bool,
    session: Option<String>,
    settle: Duration,
    /// The operator's draft, moved aside for this injection (#850).
    pub(crate) stash: Option<DraftStash>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DraftStash {
    pub(crate) draft: String,
    pub(crate) phase: StashPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StashPhase {
    /// Clear keys sent; the injection waits for an empty composer.
    Clearing,
    /// The composer holds the injection; the draft waits for the submit.
    Held,
    /// The submit landed with `outcome`; the draft goes back once the
    /// composer reads empty, then must read back exactly.
    Restoring {
        outcome: Outcome,
        typed: bool,
        until: Instant,
    },
    /// Read back exactly.
    Restored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Accepted,
    ObservedAccepted,
    Unconfirmed(&'static str),
    Abandoned(&'static str),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Composer {
    Empty,
    Owned,
    Other,
    Unknown,
}

/// Match the complete editable region, allowing a terminal wrap at a row boundary.
fn matches_rows(rows: &[&str], text: &str) -> bool {
    let mut rest = text.trim();
    !rows.is_empty()
        && rows.iter().all(|row| {
            if let Some(tail) = rest.strip_prefix(row.trim()) {
                rest = tail.trim_start();
                true
            } else {
                false
            }
        })
        && rest.is_empty()
}

/// `unfaint` is `screen` with its faint cells blanked, row for row. Claude's
/// editor text is read from it, so the suggestion Claude paints faint into an
/// empty box reads as nothing while anything typed still reads (#892). Without
/// styling, pass `screen` for both.
fn composer_rows<'a>(
    agent: Agent,
    screen: &'a str,
    unfaint: &'a str,
) -> Option<(Vec<&'a str>, usize, usize)> {
    composer_rows_with_width(agent, screen, unfaint, None, None)
}

fn composer_rows_with_width<'a>(
    agent: Agent,
    screen: &'a str,
    unfaint: &'a str,
    cols: Option<u16>,
    expected: Option<&str>,
) -> Option<(Vec<&'a str>, usize, usize)> {
    let lines: Vec<_> = screen.lines().collect();
    let (rows, body_start, body_end): (Vec<&str>, usize, usize) = match agent {
        Agent::Claude => {
            crate::detect::claude_composer(screen)?;
            let rule = |line: &str| {
                line.trim()
                    .chars()
                    .filter(|c| *c == '─' || *c == '━')
                    .count()
                    >= 3
            };
            let end = lines.iter().rposition(|line| rule(line))?;
            let start = lines[..end].iter().rposition(|line| rule(line))?;
            let prompt = lines[start + 1..end]
                .iter()
                .position(|line| line.trim_start().starts_with('❯'))?;
            let typed: Vec<_> = unfaint.lines().collect();
            if typed.len() != lines.len() {
                return None;
            }
            let body = &typed[start + 1..end];
            let row = body[prompt].trim_start().strip_prefix('❯')?;
            let mut rows = vec![row.trim_start_matches('❯').trim()];
            rows.extend(body[prompt + 1..].iter().map(|line| line.trim()));
            (rows, start + 1, end)
        }
        Agent::Codex => {
            let (start, mut end) = crate::detect::codex_composer_region_with_width(screen, cols)?;
            let clipped_footer = cols
                .is_some_and(|cols| crate::detect::codex_clipped_shortcut_footer(lines[end], cols));
            let mut rows = vec![lines[start].trim_start().trim_start_matches('›').trim()];
            rows.extend(lines[start + 1..end].iter().map(|line| line.trim()));
            let typed: Vec<_> = unfaint.lines().collect();
            if typed.len() != lines.len() {
                return None;
            }
            // Only the known hint may disappear with faint cells blanked (#1581).
            if let Some((status, width)) = cols.and_then(|cols| {
                if !clipped_footer {
                    return None;
                }
                (start + 1..end)
                    .rfind(|&index| !lines[index].trim().is_empty())
                    .filter(|&index| crate::detect::codex_clipped_model_footer(lines[index], cols))
                    .map(|index| (index, cols))
            }) {
                let candidate = &rows[..status - start];
                let clear = typed[start]
                    .trim_start()
                    .strip_prefix('›')?
                    .trim()
                    .is_empty()
                    && typed[start + 1..status]
                        .iter()
                        .all(|line| line.trim().is_empty())
                    && codex_hint_prefix(candidate);
                let candidate: Vec<_> = candidate
                    .iter()
                    .copied()
                    .filter(|row| !row.is_empty())
                    .collect();
                let owned = expected.is_some_and(|text| {
                    !text.trim().is_empty()
                        && (matches_rows(&candidate, text)
                            || matches_wrapped_tail(
                                &candidate,
                                text,
                                usize::from(width.saturating_sub(2)),
                            )
                            || matches!(candidate.as_slice(), [row] if is_paste_chip(agent, row)))
                });
                // A model-shaped draft is indistinguishable from truncated status text.
                // Only an empty styled hint or the known injection can resolve the boundary.
                if clear || owned {
                    end = status;
                    rows.truncate(status - start);
                } else if expected.is_none() {
                    return None;
                }
            }
            let prompt_clear = typed[start]
                .trim_start()
                .strip_prefix('›')?
                .trim()
                .is_empty();
            let empty_hint = prompt_clear
                && typed[start + 1..end]
                    .iter()
                    .all(|row| row.trim().is_empty())
                && codex_hint_prefix(&rows);
            if empty_hint {
                rows.clear();
            }
            if clipped_footer
                && !empty_hint
                && (expected.is_none()
                    || editor_rows_empty(
                        agent,
                        &rows
                            .iter()
                            .copied()
                            .filter(|row| !row.is_empty())
                            .collect::<Vec<_>>(),
                    ))
            {
                // A draft can itself end in a right-edge shortcut fragment.
                // Clipped controls alone cannot prove an empty editor or a stash boundary.
                return None;
            }
            (rows, start, end)
        }
        Agent::OpenCode => {
            let end = lines
                .iter()
                .rposition(|line| line.trim_start().starts_with('╹'))?;
            if !lines[end + 1..]
                .iter()
                .find(|line| !line.trim().is_empty())
                .is_some_and(|line| line.contains(" commands") && !line.contains("interrupt"))
            {
                return None;
            }
            let start = (0..end)
                .rev()
                .take_while(|&i| lines[i].trim_start().starts_with('┃'))
                .last()?;
            let rows = lines[start..end]
                .iter()
                .map(|line| line.trim_start().trim_start_matches('┃').trim())
                .skip_while(|line| line.is_empty())
                .take_while(|line| !line.is_empty())
                .collect();
            (rows, start, end)
        }
        _ => return None,
    };
    Some((rows, body_start, body_end))
}

// Match the known hint across rows. Arbitrary faint text remains a draft.
fn codex_hint_prefix(rows: &[&str]) -> bool {
    let mut rest = "Ask Codex to do anything";
    let mut seen = false;
    for row in rows
        .iter()
        .map(|row| row.trim())
        .filter(|row| !row.is_empty())
    {
        let Some(tail) = rest.strip_prefix(row) else {
            return false;
        };
        rest = tail.trim_start();
        seen = true;
    }
    seen
}

/// Read only the editor, without the idle and safety gates used to authorize Enter.
pub(crate) fn composer_contents(agent: Agent, screen: &str, unfaint: &str) -> Option<String> {
    composer_rows(agent, screen, unfaint).map(|(rows, _, _)| rows.join("\n"))
}

/// Classify a live pane's editor from one styled read of its detection text.
pub(crate) fn runtime_composer(
    agent: Agent,
    runtime: &crate::terminal::TerminalRuntime,
    text: &str,
) -> Composer {
    let (screen, unfaint) = runtime.detection_text_and_unfaint();
    composer_unfaint_with_width(
        agent,
        &screen,
        &unfaint,
        text,
        Some(runtime.current_size().1),
    )
}

#[cfg(test)]
pub(crate) fn composer(agent: Agent, screen: &str, text: &str) -> Composer {
    composer_unfaint(agent, screen, screen, text)
}

/// `composer` with styled hints read from `unfaint` (see `composer_rows`).
pub(crate) fn composer_unfaint(agent: Agent, screen: &str, unfaint: &str, text: &str) -> Composer {
    composer_unfaint_with_width(agent, screen, unfaint, text, None)
}

fn composer_unfaint_with_width(
    agent: Agent,
    screen: &str,
    unfaint: &str,
    text: &str,
    cols: Option<u16>,
) -> Composer {
    let Some((rows, body_start, body_end)) =
        composer_rows_with_width(agent, screen, unfaint, cols, Some(text))
    else {
        return Composer::Unknown;
    };
    let lines: Vec<_> = screen.lines().collect();
    // Only the live controls around the editor can authorize input. Menu controls
    // make even a retained empty composer unsafe.
    let mut chrome = lines[..body_start].to_vec();
    chrome.push(match agent {
        Agent::Claude => "❯",
        Agent::Codex => "›",
        _ => "┃",
    });
    chrome.extend_from_slice(&lines[body_end..]);
    let detection = crate::detect::detect_agent(Some(agent), &chrome.join("\n"));
    if detection.state != crate::detect::AgentState::Idle || detection.visible_blocker {
        return Composer::Unknown;
    }
    let tail = lines[body_end..].join("\n").to_lowercase();
    if [
        "enter to select",
        "enter to confirm",
        "enter confirm",
        "enter to submit answer",
        "esc dismiss",
        "esc to cancel",
        "↑↓",
        "hooks need review",
        "ctrl+r",
        "search:",
    ]
    .iter()
    .any(|control| tail.contains(control))
    {
        return Composer::Unknown;
    }
    let nonempty: Vec<_> = rows.into_iter().filter(|row| !row.is_empty()).collect();
    if editor_rows_empty(agent, &nonempty) {
        Composer::Empty
    } else if matches_rows(&nonempty, text) && !text.trim().is_empty() {
        Composer::Owned
    } else {
        Composer::Other
    }
}

/// An editor showing nothing, or only its harness's own hint text.
fn editor_rows_empty(agent: Agent, nonempty: &[&str]) -> bool {
    match nonempty {
        [] => true,
        [row] => match agent {
            Agent::Codex => *row == "Ask Codex to do anything",
            Agent::OpenCode => {
                *row == "Ask anything…"
                    || (row.starts_with("Ask anything… \"") && row.ends_with('"'))
            }
            _ => false,
        },
        _ => false,
    }
}

/// The editor's rows with blanks dropped, read without the idle gates: after
/// a submit the agent is working, and its composer still takes input.
fn editor_rows(agent: Agent, runtime: &crate::terminal::TerminalRuntime) -> Option<Vec<String>> {
    editor_rows_for_text(agent, runtime, None)
}

fn editor_rows_for_text(
    agent: Agent,
    runtime: &crate::terminal::TerminalRuntime,
    expected: Option<&str>,
) -> Option<Vec<String>> {
    let (screen, unfaint) = runtime.detection_text_and_unfaint();
    let (rows, _, _) = composer_rows_with_width(
        agent,
        &screen,
        &unfaint,
        Some(runtime.current_size().1),
        expected,
    )?;
    Some(
        rows.into_iter()
            .filter(|row| !row.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

fn editor_is_empty(agent: Agent, runtime: &crate::terminal::TerminalRuntime) -> bool {
    editor_rows(agent, runtime).is_some_and(|rows| {
        editor_rows_empty(agent, &rows.iter().map(String::as_str).collect::<Vec<_>>())
    })
}

fn editor_shows(agent: Agent, runtime: &crate::terminal::TerminalRuntime, text: &str) -> bool {
    editor_rows_for_text(agent, runtime, Some(text)).is_some_and(|rows| {
        matches_rows(&rows.iter().map(String::as_str).collect::<Vec<_>>(), text)
    })
}

/// Claude Code folds a long paste into one `[Pasted text #N]` row (with
/// ` +M lines` for several lines), and Codex into `[Pasted Content]` (with
/// ` <n> chars`), so the editor never shows the sent text itself (#995).
fn is_paste_chip(agent: Agent, row: &str) -> bool {
    let digits = |word: &str| !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit());
    let Some(inner) = row.strip_prefix('[').and_then(|row| row.strip_suffix(']')) else {
        return false;
    };
    match agent {
        Agent::Claude => inner.strip_prefix("Pasted text #").is_some_and(|rest| {
            let (number, lines) = rest.split_once(" +").unwrap_or((rest, ""));
            digits(number) && (lines.is_empty() || lines.strip_suffix(" lines").is_some_and(digits))
        }),
        Agent::Codex => inner.strip_prefix("Pasted Content").is_some_and(|rest| {
            rest.is_empty()
                || rest
                    .strip_prefix(' ')
                    .and_then(|rest| rest.strip_suffix(" chars"))
                    .is_some_and(digits)
        }),
        _ => false,
    }
}

/// The editor holds exactly one paste chip and nothing else.
fn editor_shows_paste_chip(
    agent: Agent,
    runtime: &crate::terminal::TerminalRuntime,
    text: &str,
) -> bool {
    editor_rows_for_text(agent, runtime, Some(text))
        .is_some_and(|rows| matches!(rows.as_slice(), [row] if is_paste_chip(agent, row)))
}

/// A pane too narrow for the message shows only the editor's last rows, wrapped
/// at spaces or mid-word (#1122). Those rows, wraps removed, must end the sent
/// text. Whitespace is dropped on both sides because a wrap at a space keeps it
/// on neither row.
///
/// The editor must also be clipped: the sent text cannot fit in the visible
/// rows at `row_width` columns each, so its start scrolled off. A fully visible
/// editor that holds only the end of the text lost the start, and stays Other.
fn matches_wrapped_tail(rows: &[&str], text: &str, row_width: usize) -> bool {
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let tail = squash(&rows.concat());
    rows.len() > 1
        && !tail.is_empty()
        && text.trim().chars().count() > rows.len() * row_width
        && squash(text).ends_with(&tail)
}

fn editor_shows_wrapped_tail(
    agent: Agent,
    runtime: &crate::terminal::TerminalRuntime,
    text: &str,
) -> bool {
    // Each editor row loses at least the two-column prompt or indent.
    let row_width = usize::from(runtime.current_size().1.saturating_sub(2));
    editor_rows_for_text(agent, runtime, Some(text)).is_some_and(|rows| {
        let rows: Vec<_> = rows.iter().map(String::as_str).collect();
        matches_wrapped_tail(&rows, text, row_width)
    })
}

/// A draft flock can move aside and put back exactly: one editor row, with
/// no paste or image chip whose contents the screen does not show. Several
/// rows are refused because the screen cannot tell a typed newline from the
/// harness's own wrap.
pub(crate) fn stashable_draft(
    agent: Agent,
    runtime: &crate::terminal::TerminalRuntime,
) -> Option<String> {
    match editor_rows(agent, runtime)?.as_slice() {
        [row] if !row.contains("[Pasted") && !row.contains("[Image") => Some(row.clone()),
        _ => None,
    }
}

fn key(runtime: &crate::terminal::TerminalRuntime, code: crossterm::event::KeyCode) -> Vec<u8> {
    runtime.encode_terminal_key(
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::empty()).into(),
    )
}

/// End, then a backspace per character and some slack: the same keys clear
/// a one-row editor in Claude, Codex and OpenCode.
fn clear_keys(runtime: &crate::terminal::TerminalRuntime, draft: &str) -> Vec<u8> {
    let mut bytes = key(runtime, crossterm::event::KeyCode::End);
    let backspace = key(runtime, crossterm::event::KeyCode::Backspace);
    for _ in 0..draft.chars().count() + CLEAR_SLACK {
        bytes.extend_from_slice(&backspace);
    }
    bytes
}

/// The message for a [`App::raw_text_refusal`] code, naming the way out.
pub(crate) fn raw_text_refusal_message(reason: &str) -> &'static str {
    if reason == "injection_pending" {
        "flock is injecting into this agent's composer; send again when it is done, or pass raw to type anyway"
    } else {
        "the agent's composer holds a draft; nothing was typed. Send again once it is empty, or pass raw to type onto it"
    }
}

/// Same execution plus a new working entry also captures turns between polls.
fn progress(before: &str, after: &str) -> Result<bool, &'static str> {
    let before: Vec<_> = before.split(':').collect();
    let after: Vec<_> = after.split(':').collect();
    if before.len() != 5 || after.len() != 5 || before[..2] != after[..2] {
        return Err("execution_changed");
    }
    let old = before[2].parse::<u64>().map_err(|_| "invalid_cursor")?;
    let new = after[2].parse::<u64>().map_err(|_| "invalid_cursor")?;
    Ok(new > old)
}

impl App {
    pub(crate) fn handle_pane_submit(&mut self, id: String, params: PaneSubmitParams) -> String {
        self.handle_guarded_submit(id, params, false)
    }

    /// `pane.submit`, or `agent.send --submit` when `stash` lets an operator's
    /// draft move aside for the injection (#850). `pane.submit` keeps refusing
    /// a draft: a delegate's brief goes into a pane nobody should be typing in.
    pub(crate) fn handle_guarded_submit(
        &mut self,
        id: String,
        mut params: PaneSubmitParams,
        stash: bool,
    ) -> String {
        let (ws, pane_id) = match self.resolve_pane_target(&params.pane_id) {
            Ok(target) => target,
            Err(error) if error.code == "pane_not_found" => {
                return encode_error(id, "pane_gone", "pane_gone");
            }
            Err(error) => return encode_error(id, &error.code, error.message),
        };
        let Some(pane) = self.public_pane_id(ws, pane_id) else {
            return encode_error(id, "pane_gone", "pane_gone");
        };
        if params.self_submit_confirmed == Some(false) && self.caller_workspace_idx() == Some(ws) {
            return encode_error(
                id,
                "self_submit_unconfirmed",
                "submitting to your own workspace requires self: true",
            );
        }
        params.pane_id = pane;
        match self.begin_guarded_injection(
            &params.pane_id,
            &params.text,
            params.if_session.as_deref(),
            Duration::from_secs(params.min_age_secs),
            Instant::now(),
            false,
            stash,
        ) {
            Ok(attempt) => {
                self.active_submissions.insert(params.pane_id.clone());
                self.pending_agent_submit = Some((id.clone(), params.pane_id, attempt));
                encode_success(id, ResponseResult::Ok {})
            }
            Err("unknown_composer") => {
                if let Some(runtime) = self.lookup_runtime_sender(ws, pane_id) {
                    let (rows, columns) = runtime.current_size();
                    let screen = runtime.detection_text();
                    let mut bottom_rows: Vec<_> = screen.lines().rev().take(8).collect();
                    bottom_rows.reverse();
                    return encode_error_with_data(
                        id,
                        "unknown_composer",
                        format!("unknown_composer in {columns}-column, {rows}-row pane"),
                        serde_json::json!({"columns":columns,"rows":rows,"bottom_rows":bottom_rows}),
                    );
                }
                encode_error(id, "unknown_composer", "unknown_composer")
            }
            Err(reason) => encode_error(id, reason, reason),
        }
    }

    /// A guarded submit that refuses any draft, as `pane.submit` does.
    #[cfg(test)]
    pub(crate) fn begin_guarded_submit(
        &self,
        pane: &str,
        text: &str,
        session: Option<&str>,
        settle: Duration,
        now: Instant,
        wake: bool,
    ) -> Result<Attempt, &'static str> {
        self.begin_guarded_injection(pane, text, session, settle, now, wake, false)
    }

    /// Why automated text may not be typed into this pane now (#1162), for
    /// the verbs that type without submitting through the guard:
    /// `pane.send_text`, `pane.send_input` and `agent.send` without `submit`.
    ///
    /// Only an agent pane is guarded, by its effective known agent: a shell
    /// pane has no composer to merge into. Two cases refuse before a byte is
    /// written:
    /// - `injection_pending`: an injection holds the pane (#850). Text held
    ///   now would replay after the lease, onto any draft the injection typed
    ///   back.
    /// - `input_not_empty`: the editor holds text that is not the harness's
    ///   own hint. Typed text would join it, and the next Enter would submit
    ///   both as one turn. Unlike the guarded submit there is no stash: these
    ///   verbs leave their text in the composer, so a draft has nowhere to go
    ///   back to.
    ///
    /// The editor is read without the idle gates, so a working agent's queued
    /// draft counts too. Known limit: a harness whose editor is not on screen
    /// (a dialog over it, OpenCode's busy footer, #950 §4) shows no draft, and
    /// the text is typed.
    pub(crate) fn raw_text_refusal(
        &self,
        ws: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<&'static str> {
        let agent = self
            .terminal_for_pane(ws, pane_id)?
            .effective_known_agent()?;
        let runtime = self.lookup_runtime_sender(ws, pane_id)?;
        if runtime.injection_lease_until().is_some() {
            return Some("injection_pending");
        }
        let rows = editor_rows(agent, runtime)?;
        let rows: Vec<_> = rows.iter().map(String::as_str).collect();
        (!editor_rows_empty(agent, &rows)).then_some("input_not_empty")
    }

    pub(crate) fn injection_lock_enabled(&self) -> bool {
        self.state.config.ui.injection_lock.enabled
    }

    /// Type `text` under an injection lease (#850). With `stash`, a one-row
    /// operator draft is cleared first and typed back after the submit.
    #[allow(clippy::too_many_arguments)] // The guarded submit's own inputs plus the stash opt-in.
    pub(crate) fn begin_guarded_injection(
        &self,
        pane: &str,
        text: &str,
        session: Option<&str>,
        settle: Duration,
        now: Instant,
        wake: bool,
        stash: bool,
    ) -> Result<Attempt, &'static str> {
        if text.trim().is_empty()
            || text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        {
            return Err("unsafe_text");
        }
        let (ws, pane_id) = self.parse_pane_id(pane).ok_or("pane_gone")?;
        let terminal = self.terminal_for_pane(ws, pane_id).ok_or("pane_gone")?;
        if terminal.restart_in_progress {
            return Err("restart_pending");
        }
        if self.active_submissions.contains(pane)
            || (!wake && self.idle_wake.in_flight(pane))
            || terminal.armed_self_compact.as_ref().is_some_and(|armed| {
                matches!(
                    armed.phase,
                    crate::agent_self_compact::SelfCompactPhase::CompactRequested
                        | crate::agent_self_compact::SelfCompactPhase::ContinueTyped
                )
            })
        {
            return Err("injection_pending");
        }
        if session
            .is_some_and(|session| terminal.submission_session_id().as_deref() != Some(session))
        {
            return Err("session_mismatch");
        }
        if let Some(reason) = terminal.guarded_submit_blocker(
            now,
            settle,
            if wake {
                Duration::from_millis(self.state.config.msg.idle_wake_fresh_ms)
            } else {
                Duration::from_secs(5)
            },
        ) {
            return Err(reason);
        }
        let agent = terminal.effective_known_agent().ok_or("unknown_composer")?;
        let runtime = self.lookup_runtime_sender(ws, pane_id).ok_or("pane_gone")?;
        if runtime.last_operator_input_at().is_some_and(|input| {
            now.saturating_duration_since(input) < crate::cli::pane::PANE_RUN_SUBMIT_GAP
        }) {
            return Err("operator_active");
        }
        let lock = self.injection_lock_enabled();
        let draft = match runtime_composer(agent, runtime, text) {
            Composer::Empty => None,
            Composer::Other if stash && lock => {
                Some(stashable_draft(agent, runtime).ok_or("input_not_empty")?)
            }
            Composer::Other | Composer::Owned => return Err("input_not_empty"),
            Composer::Unknown => return Err("unknown_composer"),
        };
        let bytes = super::api_helpers::encode_api_text(runtime, text);
        if text.contains(['\n', '\t']) && !bytes.starts_with(b"\x1b[200~") {
            return Err("unsafe_text");
        }
        let mut evidence = self.new_delivery_attempt(pane, wake)?;
        if draft.is_some() {
            evidence.draft = Some("stashed".into());
        }
        self.record_delivery_attempt(&evidence);
        let abandon = |mut evidence: crate::api::schema::DeliveryAttempt, reason: &str| {
            evidence.state = "abandoned".into();
            evidence.reason = Some(reason.into());
            evidence.finished_at_ms = Some(super::api::messages::now_ms());
            self.record_delivery_attempt(&evidence);
        };
        if lock && !runtime.begin_injection_lease(&evidence.attempt_id, now + INJECTION_LEASE, now)
        {
            abandon(evidence, "injection_pending");
            return Err("injection_pending");
        }
        let first = match &draft {
            Some(draft) => clear_keys(runtime, draft),
            None => bytes,
        };
        let paste_write = match runtime.try_send_flock_authored_tracked(Bytes::from(first)) {
            Ok(receipt) => receipt,
            Err(_) => {
                // Nothing reached the pane, so a draft is still where it was.
                if !runtime.end_injection_lease(&evidence.attempt_id) {
                    evidence.held_input_not_written = true;
                }
                abandon(evidence, "write_failed");
                return Err("write_failed");
            }
        };
        if draft.is_none() {
            evidence.state = "typed".into();
            evidence.typed_at_ms = Some(super::api::messages::now_ms());
            self.record_delivery_attempt(&evidence);
        }
        if let Some(draft) = &draft {
            // From here until the attempt is released, the draft lives only in
            // memory, so the server can find it whoever holds the attempt.
            self.kept_drafts
                .borrow_mut()
                .track_in_flight(&evidence.attempt_id, pane, draft);
        }
        let (due, deadline) = match draft {
            Some(_) => (now + POLL, now + DRAFT_STEP),
            None => (
                now + crate::cli::pane::PANE_RUN_SUBMIT_GAP,
                now + CONFIRM_WINDOW,
            ),
        };
        Ok(Attempt {
            evidence,
            text: text.to_owned(),
            cursor: terminal.turn_cursor().into_boxed_str(),
            child_pid: runtime.child_pid(),
            operator_input: runtime.last_operator_input_at(),
            prompt_generation: terminal.prompt_report_generation,
            due,
            deadline,
            paste_write: if draft.is_none() { paste_write } else { None },
            paste_written: false,
            sent: false,
            retried: false,
            wake,
            session: session.map(str::to_owned),
            settle,
            stash: draft.map(|draft| DraftStash {
                draft,
                phase: StashPhase::Clearing,
            }),
        })
    }

    pub(crate) fn advance_guarded_submit(
        &self,
        pane: &str,
        attempt: &mut Attempt,
        now: Instant,
    ) -> Option<Outcome> {
        let outcome = match attempt.stash.as_ref().map(|stash| stash.phase) {
            Some(StashPhase::Restoring { outcome, .. }) => {
                self.advance_draft_restore(pane, attempt, outcome, now)
            }
            _ => self
                .advance_guarded_submit_inner(pane, attempt, now)
                .and_then(|outcome| self.begin_draft_restore(pane, attempt, outcome, now)),
        };
        let outcome = outcome.map(|outcome| self.conclude_guarded_attempt(pane, attempt, &outcome));
        if outcome.is_none()
            && attempt.sent
            && (attempt.evidence.state != "submit_sent"
                || attempt.evidence.retried != attempt.retried)
        {
            attempt.evidence.state = "submit_sent".into();
            attempt
                .evidence
                .submit_sent_at_ms
                .get_or_insert_with(super::api::messages::now_ms);
            attempt.evidence.retried = attempt.retried;
            self.record_delivery_attempt(&attempt.evidence);
        }
        outcome
    }

    fn advance_guarded_submit_inner(
        &self,
        pane: &str,
        attempt: &mut Attempt,
        now: Instant,
    ) -> Option<Outcome> {
        let Some((ws, pane_id)) = self.parse_pane_id(pane) else {
            return Some(Outcome::Abandoned("pane_gone"));
        };
        let Some(terminal) = self.terminal_for_pane(ws, pane_id) else {
            return Some(Outcome::Abandoned("pane_gone"));
        };
        let Some(runtime) = self.lookup_runtime_sender(ws, pane_id) else {
            return Some(Outcome::Abandoned("pane_gone"));
        };
        if runtime.child_pid() != attempt.child_pid || terminal.restart_in_progress {
            return Some(Outcome::Abandoned("execution_changed"));
        }
        if attempt
            .session
            .as_deref()
            .is_some_and(|session| terminal.submission_session_id().as_deref() != Some(session))
        {
            return Some(Outcome::Abandoned("session_mismatch"));
        }
        if runtime.last_operator_input_at() != attempt.operator_input {
            return Some(Outcome::Abandoned("operator_active"));
        }
        let started = match progress(&attempt.cursor, &terminal.turn_cursor()) {
            Ok(started) => started,
            Err(reason) => return Some(Outcome::Abandoned(reason)),
        };
        let reported_text = crate::control_bytes::strip(&attempt.text);
        let reported = terminal.prompt_report_generation > attempt.prompt_generation
            && terminal.last_prompt.as_deref() == Some(reported_text.trim());
        if !attempt.sent && (started || reported) {
            return Some(Outcome::Abandoned("turn_started_before_enter"));
        }
        if attempt
            .stash
            .as_ref()
            .is_some_and(|stash| stash.phase == StashPhase::Clearing)
        {
            return self.advance_draft_clear(terminal, runtime, attempt, now);
        }
        if attempt.sent && reported {
            return Some(Outcome::Accepted);
        }
        if attempt.sent && started {
            return Some(Outcome::ObservedAccepted);
        }
        // The screen can prove the turn before the cursor moves: the sent text
        // echoed with the running turn under it, even past a provider notice
        // that leaves no owned composer to retry into (#870).
        if attempt.sent
            && terminal.effective_known_agent().is_some_and(|agent| {
                crate::detect::live_turn_follows_submitted(
                    agent,
                    &runtime.detection_text(),
                    &reported_text,
                )
            })
        {
            return Some(Outcome::ObservedAccepted);
        }
        match attempt.wait_for_paste_write(now) {
            Ok(true) => return None,
            Err(outcome) => return Some(outcome),
            Ok(false) => {}
        }
        if now < attempt.due {
            return None;
        }
        if attempt.sent && attempt.retried {
            // Both Enters went out and no turn started. If the composer still
            // holds the sent text, it swallowed the key and the caller must
            // clear it before retrying (ADR-0029 §1).
            // A folded paste chip stands in for the text it folded (#995), and
            // a wrapped tail for the text above it (#1122).
            let typed = terminal.effective_known_agent().is_some_and(|agent| {
                editor_shows(agent, runtime, &reported_text)
                    || editor_shows_paste_chip(agent, runtime, &attempt.text)
                    || editor_shows_wrapped_tail(agent, runtime, &reported_text)
            });
            return Some(Outcome::Unconfirmed(if typed {
                "typed_not_submitted"
            } else {
                "confirm_timeout"
            }));
        }
        if let Some(reason) = terminal.guarded_submit_blocker(
            now,
            attempt.settle,
            if attempt.wake {
                Duration::from_millis(self.state.config.msg.idle_wake_fresh_ms)
            } else {
                Duration::from_secs(5)
            },
        ) {
            return Some(Outcome::Abandoned(reason));
        }
        let Some(agent) = terminal.effective_known_agent() else {
            return Some(Outcome::Abandoned("unknown_composer"));
        };
        let editor = runtime_composer(agent, runtime, &attempt.text);
        // The first Enter may precede the child's paste repaint. An unchanged
        // empty editor remains safe, but only exact owned text permits a retry.
        // The composer read empty before the paste and no operator input came
        // since, so a lone paste chip in the idle editor is the text flock
        // typed, folded by the harness (#995). The same holds for the wrapped
        // tail a narrow pane leaves visible of a long message (#1122).
        let owned = editor == Composer::Owned
            || (editor == Composer::Other
                && (editor_shows_paste_chip(agent, runtime, &attempt.text)
                    || editor_shows_wrapped_tail(agent, runtime, &reported_text)));
        if !owned && (attempt.sent || editor != Composer::Empty) {
            // A partial paste repaint may omit the footer or still show only
            // part of our text. Wait for ownership before the first Enter.
            if !attempt.sent && now < attempt.deadline {
                attempt.due = (now + POLL).min(attempt.deadline);
                return None;
            }
            return Some(Outcome::Unconfirmed("owned_composer_not_visible"));
        }
        let enter = runtime.encode_terminal_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::empty(),
            )
            .into(),
        );
        if runtime.try_send_flock_authored(Bytes::from(enter)).is_err() {
            return Some(Outcome::Abandoned("write_failed"));
        }
        attempt.retried = attempt.sent;
        attempt.sent = true;
        attempt.due = now + CONFIRM_WINDOW;
        None
    }

    /// Type the injection once the cleared composer reads empty.
    fn advance_draft_clear(
        &self,
        terminal: &crate::terminal::TerminalState,
        runtime: &crate::terminal::TerminalRuntime,
        attempt: &mut Attempt,
        now: Instant,
    ) -> Option<Outcome> {
        if now < attempt.due {
            return None;
        }
        let Some(agent) = terminal.effective_known_agent() else {
            return Some(Outcome::Abandoned("unknown_composer"));
        };
        if !runtime.holds_injection_lease(&attempt.evidence.attempt_id, now) {
            return Some(Outcome::Abandoned("injection_lease_expired"));
        }
        if runtime_composer(agent, runtime, &attempt.text) != Composer::Empty {
            if now < attempt.deadline {
                attempt.due = (now + POLL).min(attempt.deadline);
                return None;
            }
            return Some(Outcome::Abandoned("draft_not_cleared"));
        }
        let bytes = super::api_helpers::encode_api_text(runtime, &attempt.text);
        attempt.paste_write = match runtime.try_send_flock_authored_tracked(Bytes::from(bytes)) {
            Ok(receipt) => receipt,
            Err(_) => return Some(Outcome::Abandoned("write_failed")),
        };
        if let Some(stash) = attempt.stash.as_mut() {
            stash.phase = StashPhase::Held;
        }
        attempt.evidence.state = "typed".into();
        attempt.evidence.typed_at_ms = Some(super::api::messages::now_ms());
        self.record_delivery_attempt(&attempt.evidence);
        attempt.due = now + crate::cli::pane::PANE_RUN_SUBMIT_GAP;
        attempt.deadline = now + CONFIRM_WINDOW;
        None
    }

    /// A submit that landed puts a held draft back before it reports.
    fn begin_draft_restore(
        &self,
        pane: &str,
        attempt: &mut Attempt,
        outcome: Outcome,
        now: Instant,
    ) -> Option<Outcome> {
        let Some(stash) = attempt.stash.as_mut() else {
            return Some(outcome);
        };
        if stash.phase != StashPhase::Held
            || !matches!(outcome, Outcome::Accepted | Outcome::ObservedAccepted)
        {
            return Some(outcome);
        }
        stash.phase = StashPhase::Restoring {
            outcome,
            typed: false,
            until: now + DRAFT_STEP,
        };
        self.advance_draft_restore(pane, attempt, outcome, now)
    }

    /// Type the draft into the emptied composer and read it back, all under
    /// the lease. Whatever cannot be confirmed in time is left for the
    /// pane's stash when the attempt concludes.
    fn advance_draft_restore(
        &self,
        pane: &str,
        attempt: &mut Attempt,
        outcome: Outcome,
        now: Instant,
    ) -> Option<Outcome> {
        let token = &attempt.evidence.attempt_id;
        let Some(stash) = attempt.stash.as_mut() else {
            return Some(outcome);
        };
        let StashPhase::Restoring { typed, until, .. } = stash.phase else {
            return Some(outcome);
        };
        let Some((ws, pane_id)) = self.parse_pane_id(pane) else {
            return Some(outcome);
        };
        let (Some(terminal), Some(runtime)) = (
            self.terminal_for_pane(ws, pane_id),
            self.lookup_runtime_sender(ws, pane_id),
        ) else {
            return Some(outcome);
        };
        let Some(agent) = terminal.effective_known_agent() else {
            return Some(outcome);
        };
        if !runtime.holds_injection_lease(token, now) {
            return Some(outcome);
        }
        if typed && editor_shows(agent, runtime, &stash.draft) {
            stash.phase = StashPhase::Restored;
            return Some(outcome);
        }
        if !typed && editor_is_empty(agent, runtime) {
            let bytes = super::api_helpers::encode_api_text(runtime, &stash.draft);
            if runtime.try_send_flock_authored(Bytes::from(bytes)).is_err() {
                return Some(outcome);
            }
            stash.phase = StashPhase::Restoring {
                outcome,
                typed: true,
                until: now + DRAFT_STEP,
            };
        } else if now >= until {
            return Some(outcome);
        }
        attempt.due = now + POLL;
        None
    }

    /// Every attempt ends here, whoever drops it: the pane is released, then
    /// the evidence closes.
    pub(crate) fn conclude_guarded_attempt(
        &self,
        pane: &str,
        attempt: &mut Attempt,
        outcome: &Outcome,
    ) -> Outcome {
        let disposition = self.release_guarded_attempt(pane, attempt);
        let outcome = attempt.outcome_after_release(*outcome, disposition);
        self.finish_delivery_attempt(attempt, &outcome);
        outcome
    }

    /// A draft that did not come back exactly goes to the pane's stash, then
    /// the lease ends and its held keystrokes replay. Safe to repeat.
    pub(crate) fn release_guarded_attempt(
        &self,
        pane: &str,
        attempt: &mut Attempt,
    ) -> Option<crate::pty::write_receipt::WriteDisposition> {
        // Cancel before replaying held operator input on every release path.
        let disposition = attempt.paste_write.as_ref().map(|receipt| receipt.cancel());
        let target = self.parse_pane_id(pane).and_then(|(ws, pane_id)| {
            Some((
                self.terminal_for_pane(ws, pane_id)?,
                self.lookup_runtime_sender(ws, pane_id)?,
            ))
        });
        if let Some(stash) = attempt.stash.take() {
            // Settled already (a handoff or shutdown kept every draft in
            // flight): the stash has it, so keeping it again would duplicate.
            let owned = self
                .kept_drafts
                .borrow_mut()
                .claim_in_flight(&attempt.evidence.attempt_id);
            let untouched = || {
                target.is_some_and(|(terminal, runtime)| {
                    terminal
                        .effective_known_agent()
                        .is_some_and(|agent| editor_shows(agent, runtime, &stash.draft))
                })
            };
            let verdict = match stash.phase {
                _ if !owned => "kept",
                StashPhase::Restored => "restored",
                StashPhase::Clearing if untouched() => "untouched",
                _ => {
                    self.keep_draft(pane, stash.draft.clone());
                    "kept"
                }
            };
            attempt.evidence.draft = Some(verdict.into());
        }
        if let Some((_, runtime)) = target {
            // #1011 review: a pane that refuses the replay loses the operator's
            // held keystrokes, and the attempt's outcome says so.
            if !runtime.end_injection_lease(&attempt.evidence.attempt_id) {
                attempt.evidence.held_input_not_written = true;
            }
        }
        disposition
    }

    fn pane_runtimes(&self) -> Vec<&crate::terminal::TerminalRuntime> {
        let runtimes: Vec<_> = self.terminal_runtimes.values().collect();
        // Test fixtures attach runtimes to the workspace instead.
        #[cfg(test)]
        let runtimes = {
            let mut runtimes = runtimes;
            for ws in &self.state.workspaces {
                runtimes.extend(ws.test_runtimes.values());
                runtimes.extend(ws.tabs.iter().flat_map(|tab| tab.runtimes.values()));
            }
            runtimes
        };
        runtimes
    }

    /// End every lease past its bound and replay what it held (#850). The
    /// bound holds even when the injection's owner never comes back.
    pub(crate) fn tick_injection_leases(&mut self, now: Instant) -> bool {
        let mut ended = 0;
        for runtime in self.pane_runtimes() {
            ended += usize::from(runtime.expire_injection_lease(now));
            // A refusal is logged per pane where it happens. No attempt is
            // left to carry it here: the lease past its bound has no owner.
            let _ = runtime.drain_held_input();
        }
        self.surface_kept_drafts();
        ended > 0
    }

    /// The next lease bound, or a prompt retry while held input waits on a
    /// full channel.
    pub(crate) fn next_injection_lease_deadline(&self) -> Option<Instant> {
        let runtimes = self.pane_runtimes();
        let retry = runtimes
            .iter()
            .any(|runtime| runtime.has_held_backlog())
            .then(|| Instant::now() + POLL);
        runtimes
            .iter()
            .filter_map(|runtime| runtime.injection_lease_until())
            .chain(retry)
            .min()
    }

    /// Before the server stops or hands off: every draft an injection holds
    /// goes to the stash (and is announced), then every held keystroke is
    /// written. The attempts themselves die with this server, so nothing
    /// else would ever put those drafts back.
    pub(crate) fn settle_injections_for_exit(&mut self) {
        let kept = self.kept_drafts.borrow_mut().keep_all_in_flight();
        for pane in &kept {
            crate::logging::pane_draft_kept(pane);
        }
        self.surface_kept_drafts();
        for runtime in self.pane_runtimes() {
            runtime.end_all_injection_leases();
            let _ = runtime.drain_held_input();
        }
    }

    pub(crate) fn schedule_guarded_request(
        &self,
        request_id: String,
        pane_id: String,
        attempt: Attempt,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(POLL.min(attempt.due.saturating_duration_since(Instant::now())))
                .await;
            let _ = tx
                .send(crate::events::AppEvent::AgentSubmit {
                    request_id,
                    pane_id,
                    attempt,
                    respond_to,
                })
                .await;
        });
    }

    pub(crate) fn advance_guarded_request(
        &mut self,
        id: String,
        pane: String,
        mut attempt: Attempt,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        if let Some(outcome) = self.advance_guarded_submit(&pane, &mut attempt, Instant::now()) {
            self.active_submissions.remove(&pane);
            let (outcome, reason) = match outcome {
                Outcome::Accepted => ("accepted", None),
                Outcome::ObservedAccepted => ("observed_accepted", None),
                Outcome::Unconfirmed(reason) => ("unconfirmed", Some(reason.to_owned())),
                Outcome::Abandoned(reason) => ("abandoned", Some(reason.to_owned())),
            };
            let _ = respond_to.send(encode_success(
                id,
                ResponseResult::GuardedSubmit {
                    attempt: Some(attempt.evidence),
                    outcome: outcome.to_owned(),
                    reason,
                    retried: attempt.retried,
                },
            ));
            self.surface_kept_drafts();
        } else {
            self.schedule_guarded_request(id, pane, attempt, respond_to);
        }
    }
}

fn new_evidence(
    attempt_id: String,
    pane: &str,
    wake: bool,
    correlation_ids: Vec<String>,
) -> crate::api::schema::DeliveryAttempt {
    crate::api::schema::DeliveryAttempt {
        attempt_id,
        pane: pane.into(),
        correlation_ids,
        wake,
        state: "queued".into(),
        reason: None,
        queued_at_ms: super::api::messages::now_ms(),
        typed_at_ms: None,
        submit_sent_at_ms: None,
        finished_at_ms: None,
        retried: false,
        draft: None,
        held_input_not_written: false,
    }
}

impl App {
    fn new_delivery_attempt(
        &self,
        pane: &str,
        wake: bool,
    ) -> Result<crate::api::schema::DeliveryAttempt, &'static str> {
        let queued_ids = self.mailboxes.queued_correlation_ids();
        let id = self
            .delivery_attempt_registry
            .borrow_mut()
            .reserve_id(|id| queued_ids.contains(id))?;
        let ids = if wake {
            self.mailboxes
                .queued_infos(Some(pane))
                .into_iter()
                .map(|m| m.correlation_id)
                .collect()
        } else {
            Vec::new()
        };
        let evidence = new_evidence(id, pane, wake, ids);
        if self.node_id.is_some() {
            match crate::mesh::hello::with_store(|store| {
                store.record_attempt(&evidence).map_err(|e| e.to_string())
            }) {
                Ok(_) => {}
                // Only a full durable registry refuses admission. Any other
                // store failure leaves local typing to the in-memory registry.
                Err(reason) if reason.starts_with("mail_store_full") => {
                    return Err("delivery_attempt_capacity");
                }
                Err(_) => {
                    crate::logging::mesh_custody_failed("record_attempt", "mail_store_unavailable")
                }
            }
        }
        Ok(evidence)
    }

    pub(crate) fn record_delivery_attempt(&self, attempt: &crate::api::schema::DeliveryAttempt) {
        if self.node_id.is_some()
            && crate::mesh::hello::with_store(|store| {
                store.record_attempt(attempt).map_err(|e| e.to_string())
            })
            .is_err()
        {
            crate::logging::mesh_custody_failed("record_attempt", "mail_store_unavailable");
        }
        self.delivery_attempt_registry
            .borrow_mut()
            .record(attempt.clone());
        self.emit_event(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::DeliveryAttemptUpdated,
            data: crate::api::schema::EventData::DeliveryAttemptUpdated {
                attempt: attempt.clone(),
            },
        });
    }

    pub(crate) fn finish_delivery_attempt(&self, attempt: &mut Attempt, outcome: &Outcome) {
        #[cfg(test)]
        tests::FINISH_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if attempt.evidence.finished_at_ms.is_some() {
            return;
        }
        let (state, reason) = match outcome {
            Outcome::Accepted => ("accepted", None),
            Outcome::ObservedAccepted => ("observed_accepted", None),
            Outcome::Unconfirmed(reason) => ("unconfirmed", Some(*reason)),
            Outcome::Abandoned(reason) => ("abandoned", Some(*reason)),
        };
        attempt.evidence.state = state.into();
        attempt.evidence.reason = reason.map(str::to_owned);
        attempt.evidence.retried = attempt.retried;
        attempt.evidence.finished_at_ms = Some(super::api::messages::now_ms());
        self.record_delivery_attempt(&attempt.evidence);
    }

    /// The registry is write-through to the custody store and restored from
    /// it at boot, so a query never re-reads the durable table.
    pub(crate) fn delivery_attempts(&self) -> Vec<crate::api::schema::DeliveryAttempt> {
        self.delivery_attempt_registry.borrow().snapshot()
    }
}

impl Attempt {
    /// ADR-0029 §1 permits abandonment only when retrying cannot duplicate text.
    pub(super) fn outcome_after_release(
        &self,
        outcome: Outcome,
        disposition: Option<crate::pty::write_receipt::WriteDisposition>,
    ) -> Outcome {
        use crate::pty::write_receipt::WriteDisposition;
        // After Enter, the existing turn/execution verdict owns the outcome.
        // This classification covers paste interruption before submission.
        if self.sent || !matches!(outcome, Outcome::Abandoned(_)) {
            return outcome;
        }
        match disposition {
            Some(WriteDisposition::Partial) => Outcome::Unconfirmed("paste_write_incomplete"),
            Some(WriteDisposition::Completed) => Outcome::Unconfirmed("paste_written"),
            None if self.paste_written => Outcome::Unconfirmed("paste_written"),
            _ => outcome,
        }
    }

    /// ADR-0028's type-then-Enter gap starts after the final PTY write, not enqueue.
    fn wait_for_paste_write(&mut self, now: Instant) -> Result<bool, Outcome> {
        let Some(receipt) = &self.paste_write else {
            return Ok(false);
        };
        match receipt.try_completed() {
            Ok(at) => {
                self.paste_write = None;
                self.paste_written = true;
                self.due = at + crate::cli::pane::PANE_RUN_SUBMIT_GAP;
                Ok(now < self.due)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) if now < self.deadline => {
                self.due = (now + POLL).min(self.deadline);
                Ok(true)
            }
            Err(_) => Err(self
                .outcome_after_release(Outcome::Abandoned("write_failed"), Some(receipt.cancel()))),
        }
    }
}

#[cfg(test)]
impl Attempt {
    pub(crate) fn test_new() -> Self {
        Self {
            evidence: new_evidence("test-attempt".into(), "test-pane", false, Vec::new()),
            text: String::new(),
            cursor: "1:0:0:0:i".into(),
            child_pid: None,
            operator_input: None,
            prompt_generation: 0,
            due: Instant::now(),
            deadline: Instant::now() + CONFIRM_WINDOW,
            paste_write: None,
            paste_written: false,
            sent: false,
            retried: false,
            wake: false,
            session: None,
            settle: Duration::ZERO,
            stash: None,
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    // The idempotence guard hides duplicate calls from the event stream. Count
    // invocations so App-level lifecycle tests can enforce a single finalizer.
    pub(crate) static FINISH_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// Replace the writer boundary after a real App submission, retaining the
    /// receipt while the producer stalls with zero, some, or all bytes written.
    pub(crate) fn stalled_paste(
        attempt: &mut Attempt,
        written: usize,
    ) -> crate::pty::write_receipt::WriteCompletion {
        let (completion, receipt) = crate::pty::write_receipt::WriteReceipt::channel();
        attempt.paste_write = Some(receipt);
        if written > 0 {
            assert_eq!(
                completion.write(5, || Ok(written)).unwrap().unwrap(),
                written
            );
        }
        if written == 5 {
            completion.send(Instant::now()).unwrap();
        }
        completion
    }

    #[test]
    fn guarded_submit_enter_follows_completed_paste_by_at_least_100ms() {
        let queued = Instant::now();
        let written = queued + Duration::from_millis(400);
        let (completion, receipt) = crate::pty::write_receipt::WriteReceipt::channel();
        let mut attempt = Attempt::test_new();
        attempt.due = queued + crate::cli::pane::PANE_RUN_SUBMIT_GAP;
        attempt.deadline = queued + CONFIRM_WINDOW;
        attempt.paste_write = Some(receipt);
        // The old enqueue deadline has expired while the actor is still busy.
        assert_eq!(attempt.wait_for_paste_write(written), Ok(true));
        completion.send(written).unwrap();
        let early = written + Duration::from_millis(99);
        assert_eq!(attempt.wait_for_paste_write(early), Ok(true));
        assert_eq!(attempt.due, written + crate::cli::pane::PANE_RUN_SUBMIT_GAP);
        assert!(attempt.due.duration_since(written) >= Duration::from_millis(100));
        assert!(early < attempt.due);
        assert_eq!(attempt.wait_for_paste_write(attempt.due), Ok(false));
    }

    #[test]
    fn pending_paste_write_has_a_bounded_failure_and_rejects_disconnection() {
        let now = Instant::now();
        for disconnected in [false, true] {
            let (completion, receipt) = crate::pty::write_receipt::WriteReceipt::channel();
            let mut attempt = Attempt::test_new();
            attempt.paste_write = Some(receipt);
            attempt.deadline = now + CONFIRM_WINDOW;
            let mut completion = Some(completion);
            let at = if disconnected {
                drop(completion.take());
                now
            } else {
                // The producer stays alive and silent through the failure ceiling.
                assert_eq!(attempt.wait_for_paste_write(now), Ok(true));
                attempt.deadline
            };
            assert_eq!(
                attempt.wait_for_paste_write(at),
                Err(Outcome::Abandoned("write_failed"))
            );
            if let Some(completion) = completion {
                assert!(completion
                    .write(5, || panic!("abandoned paste reached resumed writer"))
                    .is_none());
            }
        }
    }

    #[test]
    fn partial_paste_at_deadline_is_unconfirmed_even_if_writer_disconnects() {
        for disconnected in [false, true] {
            let (completion, receipt) = crate::pty::write_receipt::WriteReceipt::channel();
            let mut attempt = Attempt::test_new();
            attempt.paste_write = Some(receipt);
            assert_eq!(completion.write(5, || Ok(2)).unwrap().unwrap(), 2);
            let mut completion = Some(completion);
            if disconnected {
                drop(completion.take());
            }
            assert_eq!(
                attempt.wait_for_paste_write(attempt.deadline),
                Err(Outcome::Unconfirmed("paste_write_incomplete"))
            );
            assert_eq!(
                attempt.outcome_after_release(
                    Outcome::Abandoned("operator_active"),
                    attempt.paste_write.as_ref().map(|receipt| receipt.cancel())
                ),
                Outcome::Unconfirmed("paste_write_incomplete")
            );
        }
    }

    #[tokio::test]
    async fn codex_dim_hint_1581_replays_captured_26_column_screen() {
        let bytes = include_bytes!("../../tests/fixtures/codex-hint-1581/26-cols.ansi");
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(26, 31, bytes);
        let (screen, unfaint) = runtime.detection_text_and_unfaint();
        assert!(screen.contains("Ask Codex to do anythin"));
        assert!(!unfaint.contains("Ask Codex"));
        assert_eq!(
            runtime_composer(Agent::Codex, &runtime, "brief"),
            Composer::Empty
        );
        assert!(editor_is_empty(Agent::Codex, &runtime));
        assert_eq!(
            composer_contents(Agent::Codex, &screen, &unfaint).as_deref(),
            Some("")
        );
    }

    #[tokio::test]
    async fn codex_dim_hint_1581_recognizes_known_faint_hint_prefixes() {
        for cols in [18, 22, 26] {
            let hint: String = "Ask Codex to do anything".chars().take(cols - 3).collect();
            let frame = |body: &str| format!("OpenAI Codex\r\n› {body}\r\n  ? for shortcuts");
            let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(
                cols as u16,
                31,
                frame(&format!("\x1b[2m{hint}\x1b[0m")).as_bytes(),
            );
            assert_eq!(
                runtime_composer(Agent::Codex, &runtime, "brief"),
                Composer::Empty,
                "{cols}"
            );
        }
    }

    #[tokio::test]
    async fn codex_dim_hint_1581_preserves_plain_mixed_and_unrelated_faint_drafts() {
        for cols in [18, 22, 26] {
            let hint: String = "Ask Codex to do anything".chars().take(cols - 3).collect();
            let frame = |body: &str| format!("OpenAI Codex\r\n› {body}\r\n  ? for shortcuts");
            for draft in [
                hint.clone(),
                "real draft".into(),
                "\x1b[2mreal draft\x1b[0m".into(),
                format!("\x1b[2m{hint}\x1b[0m x"),
            ] {
                let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(
                    cols as u16,
                    31,
                    frame(&draft).as_bytes(),
                );
                assert_eq!(
                    runtime_composer(Agent::Codex, &runtime, "brief"),
                    Composer::Other,
                    "{cols}: {draft:?}"
                );
                assert!(!editor_is_empty(Agent::Codex, &runtime));
            }
        }
    }

    #[tokio::test]
    async fn codex_dim_hint_1581_does_not_authorize_missing_or_dialog_controls() {
        let shown = "› Ask Codex\n? for shortcuts";
        assert_eq!(
            composer_unfaint(Agent::Codex, shown, "›", "brief"),
            Composer::Unknown
        );
        for screen in [
            "› \x1b[2mAsk Codex\x1b[0m",
            "› \x1b[2mAsk Codex\x1b[0m\r\nenter to confirm",
            "Hooks need review\r\n› \x1b[2mAsk Codex\x1b[0m\r\n? for shortcuts\r\nenter confirm · esc skip",
        ] {
            let runtime =
                crate::terminal::TerminalRuntime::test_with_screen_bytes(26, 31, screen.as_bytes());
            assert_eq!(
                runtime_composer(Agent::Codex, &runtime, "brief"),
                Composer::Unknown
            );
        }
    }

    #[test]
    fn codex_startup_passive_banners_leave_composer_ready() {
        let screen = include_str!("../../tests/fixtures/codex/startup-passive-banners.txt");
        let captured = include_str!("../../tests/fixtures/codex-submit-721/inline-idle.txt");
        let prompt = "› Ask Codex to do anything";
        // Preserve the real prompt, blank row and model/effort/directory footer.
        assert_eq!(
            screen.split_once(prompt).unwrap().1,
            captured.split_once(prompt).unwrap().1
        );
        let (_, footer) = crate::detect::codex_composer_region(screen).expect("live composer");
        assert_eq!(screen.lines().nth(footer), captured.lines().last());
        let detection = crate::detect::detect_agent(Some(Agent::Codex), screen);
        assert_eq!(detection.state, crate::detect::AgentState::Idle);
        assert!(detection.visible_idle);
        assert!(!detection.visible_blocker);
        assert!(detection.provider_limit.is_none());
        assert_eq!(composer(Agent::Codex, screen, "brief"), Composer::Empty);
        assert_eq!(
            composer(
                Agent::Codex,
                &screen.replace("Ask Codex to do anything", "brief"),
                "brief"
            ),
            Composer::Owned
        );
    }

    #[test]
    fn codex_startup_update_dialog_is_not_a_composer() {
        let screen = include_str!("../../tests/fixtures/codex/startup-update-dialog.txt");
        let captured = include_str!("../../tests/fixtures/codex-submit-721/alt-idle.txt");
        let (history, _) = captured.split_once("› Ask Codex to do anything").unwrap();
        assert!(screen.starts_with(history));
        // Like the captured /model picker, an active menu replaces the editor
        // and its footer rather than retaining an editable prompt underneath.
        let picker = include_str!("../../tests/fixtures/codex-submit-721/alt-model.txt");
        assert!(crate::detect::codex_composer_region(picker).is_none());
        assert!(crate::detect::codex_composer_region(screen).is_none());
        assert_eq!(composer(Agent::Codex, screen, "brief"), Composer::Unknown);
    }

    #[test]
    fn guarded_composer_requires_complete_exact_editor() {
        let screen = "──────\n❯ hello world\n──────\n? for shortcuts";
        assert_eq!(
            composer(Agent::Claude, screen, "hello world"),
            Composer::Owned
        );
        assert_eq!(
            composer(
                Agent::Claude,
                &screen.replace("hello world", "enter to confirm"),
                "enter to confirm"
            ),
            Composer::Owned
        );
        assert_eq!(composer(Agent::Claude, screen, "hello"), Composer::Other);
        assert_eq!(
            composer(
                Agent::Claude,
                &screen.replace("hello world", "\u{a0}"),
                "hello"
            ),
            Composer::Empty
        );
        assert_eq!(
            composer(
                Agent::Claude,
                &screen.replace("hello world", "hello\n  world"),
                "hello world"
            ),
            Composer::Owned
        );
        assert_eq!(
            composer(
                Agent::Claude,
                &format!("{screen}\nEnter to select · Esc to cancel"),
                "hello world"
            ),
            Composer::Unknown
        );
        assert_eq!(
            composer(Agent::Claude, "❯ hello world", "hello world"),
            Composer::Unknown
        );
        // Claude paints its suggestion faint, so the unfaint read is blank
        // where it sits, whatever its shape (#892).
        for suggestion in [
            "Try \"refactor check-ssot.sh\"",
            "watch CI on #891 and merge when green",
        ] {
            let shown = screen.replace("hello world", suggestion);
            let unfaint = screen.replace("hello world", "");
            assert_eq!(
                composer_unfaint(Agent::Claude, &shown, &unfaint, "x"),
                Composer::Empty,
                "{suggestion:?}"
            );
            // The same words typed are not faint, so they are a draft.
            assert_eq!(
                composer_unfaint(Agent::Claude, &shown, &shown, "x"),
                Composer::Other,
                "{suggestion:?}"
            );
        }
        // Rows that do not line up with the screen prove nothing about it.
        assert_eq!(
            composer_unfaint(Agent::Claude, screen, "❯", "hello world"),
            Composer::Unknown
        );
        assert_eq!(
            composer(
                Agent::Claude,
                &screen.replace("hello world", "[Pasted text #1]"),
                "hello world"
            ),
            Composer::Other
        );
    }
    #[test]
    fn guarded_cursor_confirms_short_turn_and_rejects_restart() {
        assert_eq!(progress("1:0:2:4:i", "1:0:3:6:i"), Ok(true));
        assert_eq!(progress("1:0:2:4:i", "1:0:2:5:i"), Ok(false));
        assert_eq!(progress("1:0:2:4:i", "1:1:3:6:i"), Err("execution_changed"));
    }
}

#[cfg(test)]
#[path = "guarded_submit/codex_721_tests.rs"]
mod codex_721_tests;

#[cfg(test)]
#[path = "guarded_submit/paste_chip_995_tests.rs"]
mod paste_chip_995_tests;

#[cfg(test)]
#[path = "guarded_submit/wrapped_tail_1122_tests.rs"]
mod wrapped_tail_1122_tests;

#[cfg(test)]
#[path = "guarded_submit/narrow_1315_tests.rs"]
mod narrow_1315_tests;
