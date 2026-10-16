/// Attention ordering after lifecycle projection. Offline still describes a
/// retained binding and wins over an unknown shell, but never a live turn.
pub(super) fn agent_status_attention_priority(status: crate::api::schema::AgentStatus) -> u8 {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Blocked => 5,
        AgentStatus::Done => 4,
        AgentStatus::Working => 3,
        AgentStatus::Idle | AgentStatus::Hibernated => 2,
        AgentStatus::Offline => 1,
        AgentStatus::Unknown => 0,
    }
}

fn parse_api_key(key: &str) -> Option<crossterm::event::KeyEvent> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let normalized = key.trim();
    match normalized {
        "Enter" | "enter" => Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())),
        "Tab" | "tab" => Some(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty())),
        "Esc" | "esc" => Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
        "Backspace" | "backspace" => Some(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty())),
        "Up" | "up" => Some(KeyEvent::new(KeyCode::Up, KeyModifiers::empty())),
        "Down" | "down" => Some(KeyEvent::new(KeyCode::Down, KeyModifiers::empty())),
        "Left" | "left" => Some(KeyEvent::new(KeyCode::Left, KeyModifiers::empty())),
        "Right" | "right" => Some(KeyEvent::new(KeyCode::Right, KeyModifiers::empty())),
        "C-c" | "c-c" | "ctrl+c" => Some(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        _ if normalized.len() == 1 => normalized
            .chars()
            .next()
            .map(|ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty())),
        _ => None,
    }
}

pub(super) fn encode_api_text(runtime: &crate::terminal::TerminalRuntime, text: &str) -> Vec<u8> {
    let bracketed = runtime
        .input_state()
        .map(|state| state.bracketed_paste)
        .unwrap_or(false);
    if bracketed {
        // Remove delimiters until stripping nested ones cannot expose another.
        let mut text = text.to_string();
        while text.contains("\x1b[200~") || text.contains("\x1b[201~") {
            text = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
        }
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

pub(super) fn encode_api_keys(
    runtime: &crate::terminal::TerminalRuntime,
    keys: &[String],
) -> Result<Vec<Vec<u8>>, String> {
    let mut encoded_keys = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(key_event) = parse_api_key(key) else {
            return Err(key.clone());
        };
        encoded_keys.push(runtime.encode_terminal_key(key_event.into()));
    }
    Ok(encoded_keys)
}

pub(super) fn detect_state_from_api(
    state: crate::api::schema::PaneAgentState,
) -> crate::detect::AgentState {
    match state {
        crate::api::schema::PaneAgentState::Idle => crate::detect::AgentState::Idle,
        crate::api::schema::PaneAgentState::Working => crate::detect::AgentState::Working,
        crate::api::schema::PaneAgentState::Blocked => crate::detect::AgentState::Blocked,
        crate::api::schema::PaneAgentState::Unknown => crate::detect::AgentState::Unknown,
    }
}

pub(super) fn pane_agent_status(
    state: crate::detect::AgentState,
    seen: bool,
) -> crate::api::schema::AgentStatus {
    crate::terminal::lifecycle::detector_status(state, seen)
}

/// The ADR-0028 `state` object (#1019) from today's detectors. `background`
/// is a task count and the source that counted it (#1022), `None` when
/// nothing could. `screen_attention` is what the screen says a `blocked`
/// agent waits on (#1023): without it, `blocked` is a prompt of unknown kind,
/// as a hook reports it. A visible prompt is not a turn fact, so it reads
/// `turn: unknown`. `waiting` is the agent's oldest registered wait (#1031).
pub(super) fn agent_state(
    status: crate::api::schema::AgentStatus,
    authority: crate::terminal::StateAuthority,
    agent: Option<crate::detect::Agent>,
    held_ms: Option<u64>,
    background: Option<(u32, crate::api::agent_state::StateSource)>,
    screen_attention: Option<crate::api::agent_state::Attention>,
    waiting: crate::api::agent_state::Waiting,
) -> crate::api::agent_state::AgentState {
    use crate::api::agent_state::{
        AgentState, Attention, Background, HumanReason, Lifecycle, StateSource, StateSources, Turn,
        Waiting,
    };
    use crate::api::schema::AgentStatus;
    let (turn, lifecycle) = match status {
        AgentStatus::Working => (Turn::Working, Lifecycle::Live),
        AgentStatus::Idle | AgentStatus::Done => (Turn::Idle, Lifecycle::Live),
        AgentStatus::Blocked | AgentStatus::Unknown => (Turn::Unknown, Lifecycle::Live),
        AgentStatus::Hibernated => (Turn::Unknown, Lifecycle::Hibernated),
        AgentStatus::Offline => (Turn::Unknown, Lifecycle::Offline),
    };
    let turn_source = match authority {
        crate::terminal::StateAuthority::Hook if agent == Some(crate::detect::Agent::OpenCode) => {
            StateSource::Plugin
        }
        crate::terminal::StateAuthority::Hook => StateSource::Hook,
        _ => StateSource::Screen,
    };
    let (attention, attention_source) = match (status, screen_attention) {
        (AgentStatus::Blocked, Some(attention)) => (attention, Some(StateSource::Screen)),
        (AgentStatus::Blocked, None) => (
            Attention::NeedsHuman {
                reason: HumanReason::Unknown,
                question: None,
            },
            Some(turn_source),
        ),
        (AgentStatus::Working | AgentStatus::Idle | AgentStatus::Done, _) => {
            (Attention::None, Some(turn_source))
        }
        _ => (Attention::Unknown, None),
    };
    let source = StateSources {
        turn: (turn != Turn::Unknown).then_some(turn_source),
        background: background.map(|(_, source)| source),
        attention: attention_source,
        waiting: (waiting != Waiting::Unknown).then_some(StateSource::WaitRegistry),
    };
    AgentState::derive(
        turn,
        Background::from_count(background.map(|(count, _)| count)),
        attention,
        lifecycle,
        waiting,
        held_ms,
        source,
    )
}

/// What the screen says a `blocked` agent waits on (#1023): a provider limit
/// outranks the prompt that may still be drawn over it. A `question` prompt
/// carries the question it draws, when its form could be read (#1038).
pub(super) fn screen_attention(
    provider_limit: Option<&crate::detect::provider_limit::ProviderLimit>,
    needs_human: Option<crate::api::agent_state::HumanReason>,
    question: Option<&crate::api::question::Question>,
) -> Option<crate::api::agent_state::Attention> {
    use crate::api::agent_state::{Attention, BlockedReason, HumanReason};
    match provider_limit {
        Some(wait) => Some(Attention::Blocked {
            reason: BlockedReason::ProviderLimit,
            retry_after_ms: wait.retry_after_ms,
        }),
        None => needs_human.map(|reason| Attention::NeedsHuman {
            reason,
            question: question
                .filter(|_| reason == HumanReason::Question)
                .map(|question| Box::new(question.clone())),
        }),
    }
}

/// Same shape as [`pane_agent_status`] but honours the terminal's
/// `hibernated_resume_plan` — a hibernated pane always reports Hibernated
/// regardless of the underlying detected state (which will drift to Unknown
/// once the child exits). A deferred native restore without a runtime is
/// offline, not a live idle turn (ADR-0028 §1); its resume plan stays intact.
pub(super) fn pane_agent_status_from_terminal(
    terminal: &crate::terminal::TerminalState,
    seen: bool,
    runtime_present: bool,
) -> crate::api::schema::AgentStatus {
    pane_agent_status_for_state(terminal, terminal.state, seen, runtime_present)
}

/// Project a detector transition through the terminal's current lifecycle.
/// Lifecycle outranks both sides of a turn transition (ADR-0028 §1).
pub(super) fn pane_agent_status_for_state(
    terminal: &crate::terminal::TerminalState,
    state: crate::detect::AgentState,
    seen: bool,
    runtime_present: bool,
) -> crate::api::schema::AgentStatus {
    terminal.projected_agent_status(state, seen, runtime_present)
}

/// Cap reported prompts to a sane size and strip control characters that
/// could corrupt the float rendering. Newlines and tabs survive.
///
/// Stripping is the right policy HERE: this text is appended to a turn that
/// already exists, so a mangled sentence beats a refused delivery. The spawn
/// path shares the filter and refuses instead, because there the text is the
/// whole turn (`crate::spawn::prompt`, ADR-0014 §4).
pub(crate) fn sanitize_reported_prompt(prompt: &str) -> String {
    const MAX_PROMPT_LEN: usize = 16 * 1024;
    let mut cleaned = crate::control_bytes::strip(prompt);
    if cleaned.len() > MAX_PROMPT_LEN {
        let mut cut = MAX_PROMPT_LEN;
        while !cleaned.is_char_boundary(cut) {
            cut -= 1;
        }
        cleaned.truncate(cut);
        cleaned.push('\u{2026}');
    }
    cleaned.trim().to_string()
}

pub(super) fn normalize_reported_agent_label(agent: &str) -> Option<String> {
    let trimmed = agent.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(agent) = crate::detect::parse_agent_label(trimmed) {
        return Some(crate::detect::agent_label(agent).to_string());
    }
    Some(trimmed.to_string())
}

pub(super) fn normalize_custom_status(status: Option<String>) -> Option<String> {
    let trimmed = status?.trim().to_string();
    let mut normalized = String::new();
    for ch in trimmed.chars().filter(|ch| !ch.is_control()).take(32) {
        normalized.push(ch);
    }
    (!normalized.trim().is_empty()).then(|| normalized.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::encode_api_text;
    use crate::api::agent_state::{Phase, StateSource, Turn, Waiting, SETTLE_MS};
    use crate::detect::Agent;
    use crate::terminal::{StateAuthority, TerminalRuntime};

    /// Fixture screens through today's detector and `agent_status` into
    /// `state` (#1019), with the turn held 3 s (settling) or `SETTLE_MS`.
    #[test]
    fn harness_screens_map_to_the_documented_phase() {
        let rule = "\u{2500}".repeat(40);
        let box_ = format!("\n\n{rule}\n\u{276f} \n{rule}\n");
        let claude_working =
            format!("> Fix it\n\n\u{2736} Pondering\u{2026} (3s \u{b7} esc to interrupt){box_}");
        let claude_idle = format!("> Fix it\n\n\u{273b} Brewed for 3s{box_}");
        let claude_trust = concat!(
            "\u{256d}\u{2500} Accessing workspace: \u{2500}\u{256e}\n",
            "\u{2502} Quick safety check: Is this a project you created or one you trust?\n",
            "\u{2502} Claude Code'll be able to read, edit, and execute files here.\n",
            "\u{276f} No, exit\n\u{2502}   Yes, I trust this folder\n",
            "\u{2502} Enter to confirm \u{b7} Esc to cancel\n\u{2570}\u{2500}\u{256f}\n",
        );
        let codex_working =
            include_str!("../../tests/fixtures/codex-submit-721/inline-working.txt");
        let codex_idle = include_str!("../../tests/fixtures/codex-submit-721/inline-idle.txt");
        let codex_trust = include_str!("../../tests/fixtures/codex/folder-trust.txt");
        let one_shell = include_str!("../../tests/fixtures/claude-background-1022/one-shell.txt");
        let two_shells = include_str!("../../tests/fixtures/claude-background-1022/two-shells.txt");
        let later_turn =
            include_str!("../../tests/fixtures/claude-background-1022/later-turn-two-shells.txt");
        let none = include_str!("../../tests/fixtures/claude-background-1022/none.txt");
        // Claude's footer fills `background`, so a settled Claude is
        // `finished` or `finished_with_background` (#1022). Codex's screen
        // cannot say: its background comes from the process tree, so a
        // screen alone leaves it `unknown`. A visible prompt is attention,
        // not a turn fact (#1023).
        #[rustfmt::skip]
        let cases = [
            (Agent::Claude, claude_working.as_str(), 3_000, Turn::Working, Phase::Working),
            (Agent::Claude, &claude_idle, 3_000, Turn::Idle, Phase::Settling),
            (Agent::Claude, &claude_idle, SETTLE_MS, Turn::Idle, Phase::Finished),
            (Agent::Claude, one_shell, SETTLE_MS, Turn::Idle, Phase::FinishedWithBackground),
            (Agent::Claude, two_shells, SETTLE_MS, Turn::Idle, Phase::FinishedWithBackground),
            (Agent::Claude, later_turn, SETTLE_MS, Turn::Idle, Phase::FinishedWithBackground),
            (Agent::Claude, none, SETTLE_MS, Turn::Idle, Phase::Finished),
            (Agent::Claude, claude_trust, 3_000, Turn::Unknown, Phase::NeedsHuman),
            (Agent::Codex, codex_working, 3_000, Turn::Working, Phase::Working),
            (Agent::Codex, codex_idle, 3_000, Turn::Idle, Phase::Settling),
            (Agent::Codex, codex_idle, SETTLE_MS, Turn::Idle, Phase::Unknown),
            (Agent::Codex, codex_trust, 3_000, Turn::Unknown, Phase::NeedsHuman),
        ];
        for (agent, screen, held, turn, phase) in cases {
            let state = screen_state(agent, screen, held);
            assert_eq!(
                (state.turn, state.phase),
                (turn, phase),
                "{agent:?} {screen}"
            );
        }
    }

    /// A screen through the detector, `agent_status` and the terminal's
    /// recorded facts into `state`, the way `agent.get` assembles it.
    fn screen_state(agent: Agent, screen: &str, held: u64) -> crate::api::agent_state::AgentState {
        let detection = crate::detect::detect_agent(Some(agent), screen);
        let status = super::pane_agent_status(detection.state, true);
        let background = detection
            .background_tasks
            .map(|count| (count, StateSource::Screen));
        let attention = super::screen_attention(
            detection
                .provider_limit
                .as_ref()
                .filter(|_| status == crate::api::schema::AgentStatus::Blocked),
            detection.needs_human,
            None,
        );
        super::agent_state(
            status,
            StateAuthority::Screen,
            Some(agent),
            Some(held),
            background,
            attention,
            Waiting::None,
        )
    }

    /// Screens captured from throwaway Claude Code 2.1.296 and Codex 0.160.1
    /// sessions in an isolated flk, each against a local fake API endpoint
    /// (tests/fixtures/attention-1023/README.md), map to their `attention`
    /// while `agent_status` still reads `blocked` (#1023).
    #[test]
    fn captured_dialogs_and_quota_screens_map_to_their_attention() {
        use crate::api::agent_state::{Attention, BlockedReason, HumanReason as R};
        use crate::api::schema::AgentStatus;
        macro_rules! fixture {
            ($name:literal) => {
                include_str!(concat!(
                    "../../tests/fixtures/attention-1023/",
                    $name,
                    ".txt"
                ))
            };
        }
        const LIMIT: Attention = Attention::Blocked {
            reason: BlockedReason::ProviderLimit,
            retry_after_ms: None,
        };
        let human = |reason| Attention::NeedsHuman {
            reason,
            question: None,
        };
        #[rustfmt::skip]
        let cases = [
            (Agent::Claude, fixture!("claude-trust"), human(R::Trust), Phase::NeedsHuman),
            (Agent::Claude, fixture!("claude-login"), human(R::Login), Phase::NeedsHuman),
            (Agent::Claude, fixture!("claude-api-key"), human(R::Login), Phase::NeedsHuman),
            (Agent::Claude, fixture!("claude-permission"), human(R::Permission), Phase::NeedsHuman),
            (Agent::Claude, fixture!("claude-question"), human(R::Question), Phase::NeedsHuman),
            (Agent::Claude, fixture!("claude-plan-exit"), human(R::Dialog), Phase::NeedsHuman),
            (Agent::Claude, fixture!("claude-limit-footer"), LIMIT, Phase::LimitReached),
            (Agent::Claude, fixture!("claude-limit-options"), LIMIT, Phase::LimitReached),
            (Agent::Claude, fixture!("claude-limit-cancelled"), LIMIT, Phase::LimitReached),
            (Agent::Claude, fixture!("claude-weekly-options"), LIMIT, Phase::LimitReached),
            (Agent::Claude, fixture!("claude-weekly-stopped"), LIMIT, Phase::LimitReached),
            (Agent::Codex, include_str!("../../tests/fixtures/codex/folder-trust.txt"), human(R::Trust), Phase::NeedsHuman),
            (Agent::Codex, fixture!("codex-login"), human(R::Login), Phase::NeedsHuman),
            (Agent::Codex, fixture!("codex-permission"), human(R::Permission), Phase::NeedsHuman),
            (Agent::Codex, fixture!("codex-question"), human(R::Question), Phase::NeedsHuman),
            (Agent::Codex, fixture!("codex-limit"), LIMIT, Phase::LimitReached),
        ];
        for (agent, screen, attention, phase) in cases {
            let detection = crate::detect::detect_agent(Some(agent), screen);
            assert_eq!(
                super::pane_agent_status(detection.state, true),
                AgentStatus::Blocked,
                "{agent:?} {screen}"
            );
            let state = screen_state(agent, screen, 3_000);
            assert_eq!(
                (state.attention, state.phase, state.source.attention),
                (attention, phase, Some(StateSource::Screen)),
                "{agent:?} {screen}"
            );
        }
        // The limit is turn-scoped: a later turn that went through, or a
        // reply quoting the notice, is not one.
        let claude_later = fixture!("claude-weekly-stopped").replace(
            "\u{273b} Brewed for 0s \u{b7} done 2:26 AM",
            "\u{273b} Brewed for 0s \u{b7} done 2:26 AM\n\n\u{276f} next\n\n\
             \u{23fa} You've hit your weekly limit \u{b7} resets soon, it said\n\n\
             \u{273b} Baked for 1s \u{b7} done 2:27 AM",
        );
        let codex_later = fixture!("codex-limit").replace(
            "\u{203a} Ask Codex to do anything",
            "\u{203a} next\n\n\u{2022} ok\n\n\u{203a} Ask Codex to do anything",
        );
        for (agent, screen) in [(Agent::Claude, claude_later), (Agent::Codex, codex_later)] {
            let state = screen_state(agent, &screen, 3_000);
            assert_eq!(state.attention, Attention::None, "{agent:?} {screen}");
        }
    }

    /// A hook can report `blocked` without saying why: that is a prompt of
    /// unknown kind, named by the hook. Old consumers keep reading `blocked`.
    #[test]
    fn a_blocked_status_without_a_screen_reason_is_needs_human_unknown() {
        use crate::api::agent_state::{Attention, HumanReason};
        use crate::api::schema::AgentStatus as S;
        let state = super::agent_state(
            S::Blocked,
            StateAuthority::Hook,
            Some(Agent::Kimi),
            Some(1),
            None,
            None,
            Waiting::None,
        );
        assert_eq!(
            (state.attention, state.phase, state.source.attention),
            (
                Attention::NeedsHuman {
                    reason: HumanReason::Unknown,
                    question: None,
                },
                Phase::NeedsHuman,
                Some(StateSource::Hook)
            )
        );
        let idle = super::agent_state(
            S::Idle,
            StateAuthority::Screen,
            None,
            Some(1),
            None,
            None,
            Waiting::None,
        );
        assert_eq!(idle.attention, Attention::None);
        let gone = super::agent_state(
            S::Offline,
            StateAuthority::Screen,
            None,
            Some(1),
            None,
            None,
            Waiting::None,
        );
        assert_eq!(
            (gone.attention, gone.source.attention),
            (Attention::Unknown, None)
        );
    }

    #[test]
    fn lifecycle_outranks_turn_and_each_axis_names_its_source() {
        use crate::api::schema::AgentStatus as S;
        use StateAuthority as By;
        let state = |status, by, agent| {
            super::agent_state(status, by, agent, Some(1), None, None, Waiting::None)
        };
        let hibernated = state(S::Hibernated, By::Screen, None);
        assert_eq!(
            (hibernated.phase, hibernated.source.turn),
            (Phase::Hibernated, None)
        );
        assert_eq!(
            hibernated.held_ms, None,
            "the screen's age is not the lifecycle's"
        );
        assert_eq!(state(S::Offline, By::Hook, None).phase, Phase::Offline);
        let turn_source = |status, by, agent| state(status, by, agent).source.turn;
        let kimi = turn_source(S::Done, By::Hook, Some(Agent::Kimi));
        assert_eq!(kimi, Some(StateSource::Hook));
        let opencode = turn_source(S::Working, By::Hook, Some(Agent::OpenCode));
        assert_eq!(opencode, Some(StateSource::Plugin));
        let screen = state(S::Working, By::VisibleWorking, None);
        assert_eq!(screen.source.turn, Some(StateSource::Screen));
        assert_eq!(
            (screen.source.background, screen.source.attention),
            (None, Some(StateSource::Screen))
        );
    }

    #[tokio::test]
    async fn api_text_strips_embedded_paste_delimiters() {
        let runtime = TerminalRuntime::test_with_screen_bytes(80, 24, b"\x1b[?2004h");
        for text in [
            "Ship\x1b[200~ it!\x1b[201~\nworld!",
            "Ship\x1b[2\x1b[200~01~ it!\nworld!",
            "Ship it!\nworld!",
        ] {
            assert_eq!(
                encode_api_text(&runtime, text),
                b"\x1b[200~Ship it!\nworld!\x1b[201~"
            );
        }
    }

    #[tokio::test]
    async fn api_text_without_bracketed_paste_preserves_bytes() {
        let runtime = TerminalRuntime::test_with_screen_bytes(80, 24, b"\x1b[?2004l");
        let text = "literal!\n\r\x03\x1b[200~\x1b[201~";
        assert_eq!(encode_api_text(&runtime, text), text.as_bytes());
    }
}
