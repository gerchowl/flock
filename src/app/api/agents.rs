use bytes::Bytes;

use crate::api::schema::{
    AgentHistoryParams, AgentHistoryResult, AgentRenameParams, AgentSendParams, AgentStartParams,
    AgentTarget, ErrorBody, HistoryTurnInfo, PaneReadResult, ReadFormat, ReadSource,
    ResponseResult, AGENT_HISTORY_DEFAULT_TURNS, AGENT_HISTORY_MAX_TURNS,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

impl App {
    pub(super) fn handle_agent_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentList {
                agents: self.collect_agent_infos(),
                // #320: the same call answers "who is here" and "who can I
                // reach". A separate fleet verb would have been a second
                // listing to keep in step with this one.
                fleet: self.collect_fleet_agents(),
            },
        )
    }

    pub(super) fn handle_agent_get(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.agent_info_for_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_focus(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.focus_agent_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, id: String, params: AgentRenameParams) -> String {
        let agent = match self.rename_agent_target(&params.target, params.name) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_rename_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_start(&mut self, id: String, params: AgentStartParams) -> String {
        let (agent, argv) = match self.start_agent(params) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentStarted { agent, argv })
    }

    pub(super) fn handle_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
    ) -> String {
        let resolved = match self.resolve_terminal_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &params.target);
        };
        let requested_lines = params.lines.unwrap_or(80).min(1000) as usize;
        let text = match params.format {
            ReadFormat::Text => match params.source {
                ReadSource::Visible => pane.visible_text(),
                ReadSource::Recent => pane.recent_text(requested_lines),
                ReadSource::RecentUnwrapped => pane.recent_unwrapped_text(requested_lines),
            },
            ReadFormat::Ansi => match params.source {
                ReadSource::Visible => pane.visible_ansi(),
                ReadSource::Recent => pane.recent_ansi(requested_lines),
                ReadSource::RecentUnwrapped => pane.recent_unwrapped_ansi(requested_lines),
            },
        };

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    tab_id: self
                        .public_tab_id(resolved.ws_idx, resolved.tab_idx)
                        .unwrap(),
                    source: params.source,
                    format: params.format,
                    text,
                    revision: 0,
                    truncated: false,
                },
            },
        )
    }

    /// `agent.history` (#276): one page of an agent's conversation, read from
    /// its session transcript.
    ///
    /// Two properties are the whole reason this is not a `source` on
    /// `agent.read`:
    ///
    /// * It touches no pane state. Nothing here marks a pane seen, and
    ///   `agent.history` is absent from `request_changes_ui`, so polling it
    ///   cannot reorder the operator's attention. That is what makes it safe
    ///   to call on a loop.
    /// * It renders at the detail the CALLER asked for, through
    ///   `turns_at_level`, rather than serving the ring the panel hydrated.
    ///   Serving the ring would couple this answer to whichever level a human
    ///   last selected.
    ///
    /// Cost is bounded by the byte window, not by the transcript: a poll that
    /// returns `next_cursor` parses only what was appended since.
    pub(super) fn handle_agent_history(
        &mut self,
        id: String,
        params: AgentHistoryParams,
    ) -> String {
        let resolved = match self.resolve_terminal_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        // Resolved from the workspace rather than through `lookup_runtime`:
        // a hibernated agent has no live runtime and still has a transcript,
        // and its history is exactly what an operator wants to read before
        // deciding whether to wake it.
        let workspace_id = self.public_workspace_id(resolved.ws_idx);
        let terminal = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|ws| ws.pane_state(resolved.pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id));
        let session_info = terminal.and_then(super::super::creation::terminal_agent_session_info);
        let session_id = terminal.and_then(crate::terminal::TerminalState::claude_session_id);

        let Some(session_id) = session_id else {
            // A pane with a session flock cannot read is a different answer
            // from a pane with no session at all: only Claude has a
            // transcript parser today, and telling a codex operator to check
            // their hooks would send them after the wrong thing.
            if let Some(info) = session_info {
                return encode_error(
                    id,
                    "unsupported_for_agent",
                    format!(
                        "{} ({}) has no transcript flock can read: only claude writes a \
                         session transcript flock parses",
                        info.agent, info.source
                    ),
                );
            }
            return encode_error(
                id,
                "no_agent_session",
                format!(
                    "agent target {} has no known session — check `flk integration status`",
                    params.target
                ),
            );
        };

        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let path =
            home.and_then(|home| crate::agent_resume::claude_transcript_path(&home, &session_id));
        let Some(path) = path else {
            return encode_error(
                id,
                "transcript_not_found",
                format!(
                    "no transcript on disk for session {session_id} — transcript saving may \
                     be disabled, or the agent has not written its first turn yet"
                ),
            );
        };

        let limit = params
            .limit
            .unwrap_or(AGENT_HISTORY_DEFAULT_TURNS)
            .clamp(1, AGENT_HISTORY_MAX_TURNS) as usize;
        let page =
            match crate::agent_transcript::read_history(&path, params.detail, params.cursor, limit)
            {
                Ok(page) => page,
                Err(err) => {
                    // The error never carries transcript CONTENT — these files
                    // hold everything the agent read and everything it was told.
                    crate::logging::transcript_unreadable(&session_id, &format!("{err:?}"));
                    return encode_error(
                        id,
                        "transcript_unreadable",
                        format!("transcript for session {session_id} could not be read"),
                    );
                }
            };

        // Read before `turns` is moved out of the page below.
        let more = page.more();
        encode_success(
            id,
            ResponseResult::AgentHistory {
                history: AgentHistoryResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    session_id,
                    detail: params.detail,
                    turns: page
                        .turns
                        .into_iter()
                        .map(|turn| HistoryTurnInfo {
                            role: turn.role,
                            text: turn.text,
                            at_ms: turn.at.and_then(unix_ms),
                        })
                        .collect(),
                    cursor: page.cursor,
                    next_cursor: page.next_cursor,
                    more,
                    truncated: page.truncated,
                },
            },
        )
    }

    /// `agent.hibernate` (#175 C3): park a pane. Refuses with a typed code
    /// when the agent has no resumable session (data loss guard) or the pane
    /// is already hibernated. Emits `AgentHibernated` on success.
    pub(super) fn handle_agent_hibernate(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_terminal_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        match self.hibernate_pane(resolved.ws_idx, resolved.pane_id) {
            Ok(_) => {
                let agent = self
                    .agent_info(resolved.ws_idx, resolved.pane_id)
                    .expect("hibernated pane's agent_info");
                encode_success(id, ResponseResult::AgentInfo { agent })
            }
            Err(err) => encode_error_body(
                id,
                ErrorBody {
                    code: err.code().to_string(),
                    message: err.message(),
                },
            ),
        }
    }

    /// `agent.resume` (#175 C3): spawn the hibernated pane's stashed argv
    /// back into the same terminal.
    pub(super) fn handle_agent_resume(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_terminal_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        match self.resume_hibernated_pane(resolved.ws_idx, resolved.pane_id) {
            Ok(_) => {
                let agent = self
                    .agent_info(resolved.ws_idx, resolved.pane_id)
                    .expect("resumed pane's agent_info");
                encode_success(id, ResponseResult::AgentInfo { agent })
            }
            Err(err) => encode_error_body(
                id,
                ErrorBody {
                    code: err.code().to_string(),
                    message: err.message(),
                },
            ),
        }
    }

    pub(super) fn handle_agent_send(&mut self, id: String, params: AgentSendParams) -> String {
        let resolved = match self.resolve_terminal_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if let Err(err) = runtime.try_send_bytes(Bytes::from(params.text)) {
            return encode_error(id, "agent_send_failed", err.to_string());
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

fn agent_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

/// Wall-clock ms for a transcript stamp. Anything before the epoch is a
/// writer that stamped nonsense, and is reported as no stamp at all.
fn unix_ms(at: std::time::SystemTime) -> Option<u64> {
    at.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{
        AgentHistoryParams, ErrorResponse, Method, Request, ResponseResult, SuccessResponse,
    };
    use crate::app::App;

    fn test_app() -> App {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app
    }

    fn focused_terminal_id(app: &App) -> crate::terminal::TerminalId {
        let ws = &app.state.workspaces[0];
        let pane_id = ws.focused_pane_id().expect("focused pane");
        ws.pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone()
    }

    /// Give the focused pane a live agent session the way a hook report does,
    /// so `claude_session_id` resolves it.
    fn stamp_session(app: &mut App, source: &str, agent: &str, session_id: &str) -> String {
        let terminal_id = focused_terminal_id(app);
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal");
        terminal.set_agent_session_ref(
            source.into(),
            agent.into(),
            crate::agent_resume::AgentSessionRef::id(session_id),
            Some(1),
        );
        terminal_id.to_string()
    }

    /// A HOME with one Claude transcript in it. nextest runs a process per
    /// test, so the env mutation stays inside this test.
    fn claude_home_with(name: &str, session_id: &str, body: &str) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("flock-history-home-{}-{name}", std::process::id()));
        let project = home.join(".claude/projects/-repo");
        std::fs::create_dir_all(&project).expect("fixture project dir");
        std::fs::write(project.join(format!("{session_id}.jsonl")), body).expect("transcript");
        std::env::set_var("HOME", &home);
        home
    }

    fn user_line(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": text}
            })
        )
    }

    fn history(app: &mut App, params: AgentHistoryParams) -> String {
        app.handle_api_request_after_internal_events_drained(Request {
            id: "req".into(),
            method: Method::AgentHistory(params),
        })
    }

    fn params(target: &str) -> AgentHistoryParams {
        AgentHistoryParams {
            target: target.into(),
            detail: crate::agent_transcript::TranscriptDetail::Reply,
            cursor: None,
            limit: None,
        }
    }

    #[test]
    fn agent_history_returns_turns_and_a_cursor_that_resumes() {
        let body = user_line("first") + &user_line("second");
        let home = claude_home_with("turns", "sess-history", &body);
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:claude", "claude", "sess-history");

        let response = history(&mut app, params(&target));

        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentHistory { history } = parsed.result else {
            panic!("expected an agent_history result");
        };
        assert_eq!(history.session_id, "sess-history");
        assert_eq!(
            history
                .turns
                .iter()
                .map(|turn| turn.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert!(!history.more, "the whole transcript was returned");
        assert!(!history.truncated);
        assert!(history.next_cursor > 0);
        let _ = std::fs::remove_dir_all(home);
    }

    /// The property the verb exists for. `flock_agent_read` moves the
    /// operator's attention ordering; a history read must not, or it cannot
    /// be polled.
    #[test]
    fn agent_history_does_not_mark_the_pane_seen() {
        let home = claude_home_with("seen", "sess-seen", &user_line("hello"));
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:claude", "claude", "sess-seen");
        let pane_id = app.state.workspaces[0]
            .focused_pane_id()
            .expect("focused pane");
        app.state.workspaces[0]
            .panes
            .get_mut(&pane_id)
            .expect("pane state")
            .seen = false;

        let response = history(&mut app, params(&target));
        assert!(
            serde_json::from_str::<SuccessResponse>(&response).is_ok(),
            "{response}"
        );

        assert!(
            !app.state.workspaces[0]
                .pane_state(pane_id)
                .expect("pane state")
                .seen,
            "reading history is not an attention event; marking the pane seen would make \
             polling it destroy the operator's queue"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// The other half of the design: the answer is rendered at the detail the
    /// CALLER asked for, so a human cycling the panel cannot change what an
    /// agent sees.
    #[test]
    fn agent_history_ignores_the_panels_detail_level() {
        let line = format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [
                    {"type": "text", "text": "on it"},
                    {"type": "tool_use", "name": "Edit"},
                ]}
            })
        );
        let home = claude_home_with("detail", "sess-detail", &line);
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:claude", "claude", "sess-detail");
        // The operator is looking at the most verbose level.
        app.state.prompt_history_detail = crate::agent_transcript::TranscriptDetail::Full;

        let response = history(&mut app, params(&target));

        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentHistory { history } = parsed.result else {
            panic!("expected an agent_history result");
        };
        assert_eq!(
            history.detail,
            crate::agent_transcript::TranscriptDetail::Reply
        );
        assert_eq!(
            history.turns[0].text, "on it",
            "the reply level drops tool calls, whatever the panel is showing"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn agent_history_clamps_limit_to_the_documented_ceiling() {
        let body: String = (0..40).map(|i| user_line(&format!("turn {i}"))).collect();
        let home = claude_home_with("limit", "sess-limit", &body);
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:claude", "claude", "sess-limit");

        let response = history(
            &mut app,
            AgentHistoryParams {
                limit: Some(u32::MAX),
                ..params(&target)
            },
        );

        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentHistory { history } = parsed.result else {
            panic!("expected an agent_history result");
        };
        assert_eq!(history.turns.len(), 40, "everything, but under the cap");
        assert!(
            history.turns.len() <= crate::api::schema::AGENT_HISTORY_MAX_TURNS as usize,
            "a caller must not be able to ask for an unbounded response"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn agent_history_defaults_to_the_latest_turns() {
        let body: String = (0..40).map(|i| user_line(&format!("turn {i}"))).collect();
        let home = claude_home_with("default", "sess-default", &body);
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:claude", "claude", "sess-default");

        let response = history(&mut app, params(&target));

        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentHistory { history } = parsed.result else {
            panic!("expected an agent_history result");
        };
        assert_eq!(
            history.turns.len(),
            crate::api::schema::AGENT_HISTORY_DEFAULT_TURNS as usize
        );
        assert_eq!(history.turns[0].text, "turn 20");
        assert!(history.truncated, "twenty older turns were not returned");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn agent_history_refuses_an_agent_with_no_transcript_flock_can_read() {
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:codex", "codex", "sess-codex");

        let response = history(&mut app, params(&target));

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "unsupported_for_agent");
        assert!(
            error.error.message.contains("codex"),
            "{}",
            error.error.message
        );
    }

    #[test]
    fn agent_history_refuses_a_pane_with_no_session() {
        let mut app = test_app();
        let target = focused_terminal_id(&app).to_string();

        let response = history(&mut app, params(&target));

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "no_agent_session");
    }

    #[test]
    fn agent_history_refuses_when_the_transcript_is_not_on_disk() {
        let home = claude_home_with("absent", "sess-other", &user_line("unrelated"));
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:claude", "claude", "sess-absent");

        let response = history(&mut app, params(&target));

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "transcript_not_found");
        assert!(
            error.error.message.contains("sess-absent"),
            "{}",
            error.error.message
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
