use crate::api::schema::{
    AgentHistoryParams, AgentHistoryResult, AgentRenameParams, AgentResultInfo, AgentResultParams,
    AgentSendParams, AgentStartParams, AgentTarget, ErrorBody, HistoryTurnInfo, PaneReadResult,
    ReadFormat, ReadSource, ResponseResult, AGENT_HISTORY_DEFAULT_TURNS, AGENT_HISTORY_MAX_TURNS,
    AGENT_RESULT_DEFAULT_CHARS, AGENT_RESULT_MAX_CHARS,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

/// opencode messages `agent.result` reads to find the newest reply: a turn
/// is a user message plus one assistant message per step, so this reaches
/// back past a long tool-using turn without reading the session.
const RESULT_OPENCODE_MESSAGES: usize = 64;

/// Where a pane's conversation lives (#575).
enum ConversationStore {
    Claude {
        session_id: String,
        path: std::path::PathBuf,
    },
    Codex {
        session_id: String,
        path: std::path::PathBuf,
    },
    Opencode {
        session_id: String,
        db: std::path::PathBuf,
    },
}

impl ConversationStore {
    fn session_id(&self) -> &str {
        match self {
            Self::Claude { session_id, .. }
            | Self::Opencode { session_id, .. }
            | Self::Codex { session_id, .. } => session_id,
        }
    }

    fn agent(&self) -> &'static str {
        match self {
            Self::Claude { .. } => "claude",
            Self::Opencode { .. } => "opencode",
            Self::Codex { .. } => "codex",
        }
    }

    /// The refusal for a store that exists and could not be read. The error
    /// never carries CONTENT: these hold everything the agent read and was told.
    fn unreadable(&self, id: String, err: &crate::agent_transcript::TranscriptError) -> String {
        crate::logging::transcript_unreadable(self.session_id(), &format!("{err:?}"));
        encode_error(
            id,
            "transcript_unreadable",
            format!(
                "conversation for session {} could not be read",
                self.session_id()
            ),
        )
    }
}

/// An `agent.history` page over an opencode session (#575). The cursor is a
/// message's creation time in ms where a Claude cursor is a byte offset;
/// both are opaque to the caller, and both mean "strictly after this".
fn opencode_history(
    db: &std::path::Path,
    session_id: &str,
    detail: crate::agent_transcript::TranscriptDetail,
    cursor: Option<u64>,
    limit: usize,
) -> Result<crate::agent_transcript::HistoryPage, crate::agent_transcript::TranscriptError> {
    use crate::opencode_transcript::{events, read_session, Window};
    let mut read = match cursor {
        Some(ms) => read_session(db, session_id, Window::After { ms, n: limit })?,
        None => read_session(db, session_id, Window::Last(limit))?,
    };
    // A cursor past the newest message means the session was replaced under
    // the caller; restart from the tail, as a Claude cursor past EOF does.
    if cursor.is_some_and(|ms| ms > read.newest_ms) {
        read = read_session(db, session_id, Window::Last(limit))?;
    }
    let mut turns = Vec::new();
    let mut compaction_pending = false;
    for message in &read.messages {
        for event in events(std::slice::from_ref(message)) {
            if matches!(event, crate::agent_transcript::TranscriptEvent::Compacted) {
                compaction_pending = !turns.is_empty() || read.oldest_ms < message.created_ms;
                continue;
            }
            for (role, text, at) in
                crate::agent_transcript::turns_at_level(std::slice::from_ref(&event), detail)
            {
                turns.push(crate::agent_transcript::HistoryTurn {
                    role,
                    text,
                    at,
                    after_compaction: std::mem::take(&mut compaction_pending),
                });
            }
        }
    }
    let first = read.messages.first().map(|m| m.created_ms);
    let cursor = first.map_or(cursor.unwrap_or(read.newest_ms), |ms| ms.saturating_sub(1));
    Ok(crate::agent_transcript::HistoryPage {
        turns,
        cursor,
        next_cursor: read.resume_after().unwrap_or(cursor),
        // `more` (next_cursor < len) means "page again NOW". A message still
        // being written is re-sent from behind the cursor, but waiting for it
        // is a poll, not a page — so only rows past the page count.
        len: if read.has_more {
            read.newest_ms
        } else {
            read.resume_after().unwrap_or(cursor)
        },
        truncated: first.is_some_and(|ms| ms > read.oldest_ms),
    })
}

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
        // WHO is asking, resolved once and by the same classifier the spawn
        // ceiling uses (#398). Placement asks one question of it — whether the
        // active workspace may stand in for a placement this caller did not
        // name — and a second walk of the peer's ancestry would be a second
        // answer to the same question, which is the drift #124 / #197 /
        // #199-#210 are.
        let caller = self.spawn_caller();
        let result = match self.start_agent(params, &caller) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, result)
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
        let mut unfaint = None;
        let text = match params.format {
            ReadFormat::Text => match params.source {
                ReadSource::Visible => pane.visible_text(),
                ReadSource::Recent => pane.recent_text(requested_lines),
                ReadSource::RecentUnwrapped => pane.recent_unwrapped_text(requested_lines),
                ReadSource::Detection => pane.detection_text(),
                ReadSource::DetectionUnfaint => {
                    let (text, blanked) = pane.detection_text_and_unfaint();
                    unfaint = Some(blanked);
                    text
                }
            },
            ReadFormat::Ansi => match params.source {
                ReadSource::Visible => pane.visible_ansi(),
                ReadSource::Recent => pane.recent_ansi(requested_lines),
                ReadSource::RecentUnwrapped => pane.recent_unwrapped_ansi(requested_lines),
                ReadSource::Detection => pane.detection_ansi(),
                ReadSource::DetectionUnfaint => {
                    unfaint = Some(pane.detection_text_and_unfaint().1);
                    pane.detection_ansi()
                }
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
                    unfaint,
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
        let (resolved, store) = match self.conversation_store(&params.target) {
            Ok(found) => found,
            Err(body) => return encode_error_body(id, body),
        };
        let limit = params
            .limit
            .unwrap_or(AGENT_HISTORY_DEFAULT_TURNS)
            .clamp(1, AGENT_HISTORY_MAX_TURNS) as usize;
        let page = match &store {
            ConversationStore::Claude { path, .. } => {
                crate::agent_transcript::read_history(path, params.detail, params.cursor, limit)
            }
            ConversationStore::Codex { .. } => {
                return encode_error(
                    id,
                    "unsupported_for_agent",
                    "Codex supports agent.result, but agent.history is not supported",
                );
            }
            ConversationStore::Opencode { db, session_id } => {
                opencode_history(db, session_id, params.detail, params.cursor, limit)
            }
        };
        let page = match page {
            Ok(page) => page,
            Err(err) => return store.unreadable(id, &err),
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
                    workspace_id: self.public_workspace_id(resolved.ws_idx),
                    session_id: store.session_id().to_string(),
                    detail: params.detail,
                    turns: page
                        .turns
                        .into_iter()
                        .map(|turn| HistoryTurnInfo {
                            role: turn.role,
                            text: turn.text,
                            at_ms: turn.at.and_then(unix_ms),
                            after_compaction: turn.after_compaction,
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

    /// `agent.result` (#575): the reply an agent's newest turn ended on, and
    /// the `DONE:` / `BLOCKED:` / `VERDICT:` line it ends with — the most
    /// common supervision step, which supervisors were doing with `sqlite3`
    /// against opencode's private schema and a `substr` offset.
    ///
    /// Same properties as `agent.history`: no pane state touched (absent from
    /// `request_changes_ui`), bounded reads (a 1 MiB transcript tail, a
    /// handful of opencode rows), and nothing logged but the failure kind.
    pub(super) fn handle_agent_result(&mut self, id: String, params: AgentResultParams) -> String {
        let (resolved, store) = match self.conversation_store(&params.target) {
            Ok(found) => found,
            Err(body) => return encode_error_body(id, body),
        };
        // The harness's own records say whether the newest turn LOOKS over, and
        // flock's hook-reported pane state says whether it IS. Claude writes
        // one transcript entry per content block, so its record alone looks
        // settled for a moment between a narration and the tool call after
        // it. Unknown (no hook yet, hibernated) defers to the record.
        let pane_says_running = matches!(
            self.state
                .workspaces
                .get(resolved.ws_idx)
                .and_then(|ws| ws.pane_state(resolved.pane_id))
                .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
                .map(|terminal| terminal.state),
            Some(crate::detect::AgentState::Working | crate::detect::AgentState::Blocked)
        );
        let read = match &store {
            ConversationStore::Codex { path, .. } => crate::codex_transcript::read_tail(path),
            ConversationStore::Claude { path, .. } => {
                crate::agent_transcript::read_tail(path).map(|read| {
                    let over = crate::agent_transcript::finished(&read.events);
                    (read.events, over)
                })
            }
            ConversationStore::Opencode { db, session_id } => {
                crate::opencode_transcript::read_session(
                    db,
                    session_id,
                    crate::opencode_transcript::Window::Last(RESULT_OPENCODE_MESSAGES),
                )
                .map(|read| {
                    (
                        crate::opencode_transcript::events(&read.messages),
                        crate::opencode_transcript::finished(&read.messages),
                    )
                })
            }
        };
        let read = read.map(|(events, recorded_over)| {
            let finished = recorded_over && !pane_says_running;
            (
                crate::agent_transcript::turn_result(&events, finished),
                finished,
            )
        });
        let (reply, finished) = match read {
            Ok(found) => found,
            Err(err) => return store.unreadable(id, &err),
        };
        let Some(reply) = reply else {
            return encode_error(
                id,
                "no_result",
                format!("session {} has no assistant reply yet", store.session_id()),
            );
        };
        let (status, status_text) = crate::agent_transcript::sentinel(&reply.text).unzip();
        let max_chars = params
            .max_chars
            .unwrap_or(AGENT_RESULT_DEFAULT_CHARS)
            .clamp(1, AGENT_RESULT_MAX_CHARS) as usize;
        let total = reply.text.chars().count();
        let offset = (params.offset.unwrap_or(0) as usize).min(total);
        let text: String = reply.text.chars().skip(offset).take(max_chars).collect();
        let end = offset + text.chars().count();
        encode_success(
            id,
            ResponseResult::AgentResult {
                result: AgentResultInfo {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id: self.public_workspace_id(resolved.ws_idx),
                    agent: store.agent().to_string(),
                    session_id: store.session_id().to_string(),
                    finished,
                    status,
                    status_text,
                    text,
                    offset: u32::try_from(offset).unwrap_or(u32::MAX),
                    total_chars: u32::try_from(total).unwrap_or(u32::MAX),
                    next_offset: (end < total).then(|| u32::try_from(end).unwrap_or(u32::MAX)),
                    at_ms: reply.at.and_then(unix_ms),
                },
            },
        )
    }

    /// Where a pane's conversation can be read (#276, #575), or the refusal
    /// that says why not.
    ///
    /// Resolved from the workspace rather than through `lookup_runtime`: a
    /// hibernated agent has no live runtime and still has a transcript, and
    /// its history is exactly what an operator wants before waking it.
    fn conversation_store(
        &self,
        target: &str,
    ) -> Result<
        (
            crate::app::terminal_targets::TerminalTarget,
            ConversationStore,
        ),
        ErrorBody,
    > {
        let resolved = self
            .resolve_terminal_target(target)
            .map_err(|err| self.agent_target_error_body(err))?;
        let refuse = |code: &str, message: String| ErrorBody {
            code: code.into(),
            message,
        };
        let terminal = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|ws| ws.pane_state(resolved.pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id));
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);

        if let Some(session_id) =
            terminal.and_then(crate::terminal::TerminalState::claude_session_id)
        {
            let path = home
                .and_then(|home| crate::agent_resume::claude_transcript_path(&home, &session_id));
            let Some(path) = path else {
                return Err(refuse(
                    "transcript_not_found",
                    format!(
                        "no transcript on disk for session {session_id} — transcript saving \
                         may be disabled, or the agent has not written its first turn yet"
                    ),
                ));
            };
            return Ok((resolved, ConversationStore::Claude { session_id, path }));
        }
        if let Some(session_id) =
            terminal.and_then(crate::terminal::TerminalState::opencode_session_id)
        {
            let xdg = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from);
            let db =
                home.and_then(|home| crate::opencode_transcript::db_path(&home, xdg.as_deref()));
            let Some(db) = db else {
                return Err(refuse(
                    "transcript_not_found",
                    format!(
                        "no opencode database found for session {session_id} \
                         (looked under $XDG_DATA_HOME/opencode and ~/.local/share/opencode)"
                    ),
                ));
            };
            return Ok((resolved, ConversationStore::Opencode { session_id, db }));
        }
        if let Some(info) = terminal.and_then(super::super::creation::terminal_agent_session_info) {
            if info.agent == "codex" && info.source == "flock:codex" {
                let codex_home = std::env::var_os("CODEX_HOME")
                    .map(std::path::PathBuf::from)
                    .or_else(|| home.map(|home| home.join(".codex")));
                let path = codex_home
                    .map(|home| crate::codex_transcript::rollout_path(&home, &info.value))
                    .transpose()
                    .map_err(|_| {
                        refuse(
                            "transcript_unreadable",
                            format!(
                                "Codex rollouts for session {} could not be read",
                                info.value
                            ),
                        )
                    })?
                    .flatten();
                let Some(path) = path else {
                    return Err(refuse(
                        "transcript_not_found",
                        format!("no Codex rollout on disk for session {}", info.value),
                    ));
                };
                return Ok((
                    resolved,
                    ConversationStore::Codex {
                        session_id: info.value,
                        path,
                    },
                ));
            }
            return Err(refuse(
                "unsupported_for_agent",
                format!(
                    "{} ({}) has no conversation flock can read: only claude and codex transcripts \
                     and opencode's session database are read",
                    info.agent, info.source
                ),
            ));
        }
        Err(refuse(
            "no_agent_session",
            format!("agent target {target} has no known session — check `flk integration status`"),
        ))
    }

    /// `agent.hibernate` (#175 C3): park a pane. Refuses with a typed code
    /// when the agent has no resumable session (data loss guard) or the pane
    /// is already hibernated. Emits `AgentHibernated` on success.
    pub(super) fn handle_agent_restart(
        &mut self,
        id: String,
        params: crate::api::schema::AgentRestartParams,
    ) -> String {
        match self.queue_agent_restart(params, std::time::Instant::now()) {
            Ok((pane_id, session, stop_only)) => encode_success(
                id,
                ResponseResult::AgentRestartQueued {
                    pane_id,
                    session,
                    stop_only,
                },
            ),
            Err(err) => encode_error_body(id, err),
        }
    }

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
        if let Some(terminal) = self
            .state
            .terminals
            .values_mut()
            .find(|t| t.id.to_string() == resolved.terminal_id)
        {
            if terminal.restart_stopped {
                terminal.restart_stopped = false;
                terminal.restart_in_progress = false;
            }
        }
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
        if params.submit {
            let Some(pane) = self.public_pane_id(resolved.ws_idx, resolved.pane_id) else {
                return agent_not_found(id, &params.target);
            };
            return self.handle_pane_submit(
                id,
                crate::api::schema::PaneSubmitParams {
                    self_submit_confirmed: None,
                    pane_id: pane,
                    text: params.text,
                    if_session: None,
                    min_age_secs: 0,
                },
            );
        }
        self.begin_paste(
            id,
            resolved.ws_idx,
            resolved.pane_id,
            params.text,
            (
                "agent_not_found",
                format!("agent target {} not found", params.target),
            ),
            "agent_send_failed",
        )
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
        AgentHistoryParams, AgentResultParams, ErrorResponse, Method, Request, ResponseResult,
        SuccessResponse,
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

    #[test]
    #[should_panic(expected = "deferred agent submit must be consumed by respond_or_park")]
    fn agent_send_submit_cannot_be_silently_cleared_by_the_next_request() {
        let mut app = test_app();
        app.pending_agent_submit = Some((
            "send".into(),
            "1:p1".into(),
            crate::app::guarded_submit::Attempt::test_new(),
        ));
        app.handle_api_request(Request {
            id: "next".into(),
            method: Method::Ping(crate::api::schema::PingParams {}),
        });
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
        let target = stamp_session(&mut app, "flock:kimi", "kimi", "sess-kimi");

        let response = history(&mut app, params(&target));

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "unsupported_for_agent");
        assert!(
            error.error.message.contains("kimi"),
            "{}",
            error.error.message
        );
    }

    #[test]
    fn codex_result_reports_not_yet_codes_and_reads_the_reported_session() {
        let mut app = test_app();
        let home = std::env::temp_dir().join(format!("flock-codex-api-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("CODEX_HOME", &home);
        let target = focused_terminal_id(&app).to_string();
        let read = |app: &mut App| {
            app.handle_agent_result(
                "req".into(),
                AgentResultParams {
                    target: target.clone(),
                    max_chars: None,
                    offset: None,
                },
            )
        };
        let error: ErrorResponse = serde_json::from_str(&read(&mut app)).unwrap();
        assert_eq!(error.error.code, "no_agent_session");
        stamp_session(&mut app, "flock:codex", "codex", "sess-codex");
        let error: ErrorResponse = serde_json::from_str(&read(&mut app)).unwrap();
        assert_eq!(error.error.code, "transcript_not_found");
        let dir = home.join("sessions/2026/01/01");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-01-01T00-00-00-sess-codex.jsonl");
        std::fs::write(&path, "invalid JSON\n").unwrap();
        let error: ErrorResponse = serde_json::from_str(&read(&mut app)).unwrap();
        assert_eq!(error.error.code, "transcript_unreadable");
        std::fs::write(
            &path,
            include_str!("../../../tests/fixtures/codex/in-progress.jsonl"),
        )
        .unwrap();
        let error: ErrorResponse = serde_json::from_str(&read(&mut app)).unwrap();
        assert_eq!(error.error.code, "no_result");
        std::fs::write(
            &path,
            include_str!("../../../tests/fixtures/codex/finished.jsonl"),
        )
        .unwrap();
        let response: serde_json::Value = serde_json::from_str(&read(&mut app)).unwrap();
        let result = &response["result"]["result"];
        assert_eq!(result["agent"], "codex", "{response}");
        assert_eq!(result["text"], "The fixture is sound.\nDONE: checked");
        assert_eq!(result["finished"], true);
        assert_eq!(result["at_ms"], 1_767_225_603_125_u64);
        std::fs::write(
            &path,
            concat!(
                include_str!("../../../tests/fixtures/codex/finished.jsonl"),
                include_str!("../../../tests/fixtures/codex/in-progress.jsonl")
            ),
        )
        .unwrap();
        let response: serde_json::Value = serde_json::from_str(&read(&mut app)).unwrap();
        assert_eq!(response["result"]["result"]["finished"], false);
        assert_eq!(response["result"]["result"]["at_ms"], 1_767_225_603_125_u64);
        std::env::remove_var("CODEX_HOME");
        std::fs::remove_dir_all(home).unwrap();
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

    // ── #575: agent.result, and opencode sessions ──

    fn result(app: &mut App, target: &str, max_chars: Option<u32>, offset: Option<u32>) -> String {
        app.handle_api_request_after_internal_events_drained(Request {
            id: "req".into(),
            method: Method::AgentResult(crate::api::schema::AgentResultParams {
                target: target.into(),
                max_chars,
                offset,
            }),
        })
    }

    fn result_info(response: &str) -> crate::api::schema::AgentResultInfo {
        let success: crate::api::schema::SuccessResponse =
            serde_json::from_str(response).expect(response);
        let ResponseResult::AgentResult { result } = success.result else {
            panic!("expected agent_result: {response}");
        };
        result
    }

    fn assistant_line(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
            })
        )
    }

    #[test]
    fn agent_result_reads_a_claude_reply_and_its_sentinel() {
        let body = user_line("merge it")
            + &assistant_line("Merged after CI went green.\nDONE: PR #12 merged");
        let home = claude_home_with("result", "sess-result", &body);
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:claude", "claude", "sess-result");

        let info = result_info(&result(&mut app, &target, None, None));
        assert_eq!(info.agent, "claude");
        assert!(info.finished);
        assert_eq!(info.status.as_deref(), Some("done"));
        assert_eq!(info.status_text.as_deref(), Some("PR #12 merged"));
        assert!(info.text.ends_with("DONE: PR #12 merged"));
        assert_eq!(info.next_offset, None);

        // Paged by characters, and the status comes from the WHOLE reply.
        let first = result_info(&result(&mut app, &target, Some(6), None));
        assert_eq!(
            (first.text.as_str(), first.next_offset),
            ("Merged", Some(6))
        );
        assert_eq!(first.status.as_deref(), Some("done"));
        let rest = result_info(&result(&mut app, &target, Some(10_000), first.next_offset));
        assert_eq!(format!("{}{}", first.text, rest.text), info.text);
        let _ = std::fs::remove_dir_all(home);
    }

    /// Review finding: mid-turn, Claude's transcript can END on this turn's
    /// narration (one entry per content block) and look settled. The pane's
    /// hook-reported state says the turn is running, so the result is the
    /// previous turn's reply and `finished` is false.
    #[test]
    fn a_working_claude_pane_reports_the_previous_turn() {
        let body = user_line("first")
            + &assistant_line("DONE: first shipped")
            + &user_line("second")
            + &assistant_line("Running the tests.");
        let home = claude_home_with("working", "sess-working", &body);
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:claude", "claude", "sess-working");
        let terminal_id = focused_terminal_id(&app);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal")
            .state = crate::detect::AgentState::Working;
        let info = result_info(&result(&mut app, &target, None, None));
        assert!(!info.finished);
        assert_eq!(info.text, "DONE: first shipped");
        assert_eq!(info.status.as_deref(), Some("done"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn agent_result_with_no_reply_yet_is_refused_as_no_result() {
        let home = claude_home_with("no-result", "sess-empty", &user_line("hello?"));
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:claude", "claude", "sess-empty");
        let error: crate::api::schema::ErrorResponse =
            serde_json::from_str(&result(&mut app, &target, None, None)).unwrap();
        assert_eq!(error.error.code, "no_result");
        let _ = std::fs::remove_dir_all(home);
    }

    /// An opencode session database in a fixture HOME, in opencode's own
    /// schema: a finished turn ending on a verdict, then one still running.
    fn opencode_home_with(name: &str, session_id: &str, running: bool) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("flock-opencode-home-{}-{name}", std::process::id()));
        let dir = home.join(".local/share/opencode");
        std::fs::create_dir_all(&dir).expect("fixture data dir");
        let conn = rusqlite::Connection::open(dir.join("opencode-stable.db")).expect("fixture db");
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, \
             time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, \
             session_id TEXT NOT NULL, time_created INTEGER NOT NULL, \
             time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .expect("fixture schema");
        let mut rows = vec![
            (
                "m1",
                1_000,
                r#"{"role":"user"}"#,
                r#"{"type":"text","text":"review PR 12"}"#,
            ),
            (
                "m2",
                2_000,
                r#"{"role":"assistant","finish":"stop","time":{"created":2000,"completed":2500}}"#,
                r#"{"type":"text","text":"Read the diff; tests cover it.\nVERDICT: approve"}"#,
            ),
        ];
        if running {
            rows.push((
                "m3",
                3_000,
                r#"{"role":"user"}"#,
                r#"{"type":"text","text":"now PR 13"}"#,
            ));
            rows.push((
                "m4",
                4_000,
                r#"{"role":"assistant","time":{"created":4000}}"#,
                r#"{"type":"tool","tool":"bash","state":{"status":"running"}}"#,
            ));
        }
        for (id, at, data, part) in rows {
            conn.execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)",
                rusqlite::params![id, session_id, at, data],
            )
            .expect("message row");
            conn.execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                rusqlite::params![format!("{id}-p0"), id, session_id, at, part],
            )
            .expect("part row");
        }
        std::env::set_var("HOME", &home);
        std::env::remove_var("XDG_DATA_HOME");
        home
    }

    #[test]
    fn agent_result_reads_an_opencode_session() {
        let home = opencode_home_with("result", "ses_fixture", false);
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:opencode", "opencode", "ses_fixture");
        let info = result_info(&result(&mut app, &target, None, None));
        assert_eq!(
            (info.agent.as_str(), info.session_id.as_str()),
            ("opencode", "ses_fixture")
        );
        assert!(info.finished);
        assert_eq!(info.status.as_deref(), Some("verdict"));
        assert_eq!(info.status_text.as_deref(), Some("approve"));
        assert_eq!(info.at_ms, Some(2_000));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_running_opencode_turn_reports_the_last_finished_reply_as_unfinished() {
        let home = opencode_home_with("running", "ses_running", true);
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:opencode", "opencode", "ses_running");
        let info = result_info(&result(&mut app, &target, None, None));
        assert!(!info.finished, "a tool is still running");
        assert_eq!(
            info.status.as_deref(),
            Some("verdict"),
            "the earlier turn's reply"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn agent_history_reads_an_opencode_session_and_its_cursor_resumes() {
        let home = opencode_home_with("history", "ses_history", true);
        let mut app = test_app();
        let target = stamp_session(&mut app, "flock:opencode", "opencode", "ses_history");

        let mut first = params(&target);
        first.limit = Some(2);
        let success: crate::api::schema::SuccessResponse =
            serde_json::from_str(&history(&mut app, first)).unwrap();
        let ResponseResult::AgentHistory { history: page } = success.result else {
            panic!("expected agent_history");
        };
        let texts: Vec<&str> = page.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["now PR 13"],
            "the newest two messages; a running tool call has no reply text"
        );
        assert!(page.truncated, "older messages exist");
        assert!(!page.more);

        let mut from_start = params(&target);
        from_start.cursor = Some(0);
        let success: crate::api::schema::SuccessResponse =
            serde_json::from_str(&history(&mut app, from_start)).unwrap();
        let ResponseResult::AgentHistory { history: page } = success.result else {
            panic!("expected agent_history");
        };
        assert_eq!(page.turns.len(), 3);
        assert_eq!(page.turns[1].at_ms, Some(2_000));
        assert_eq!(
            page.next_cursor, 3_999,
            "never past m4, which is still being written"
        );

        let mut caught_up = params(&target);
        caught_up.cursor = Some(page.next_cursor);
        let success: crate::api::schema::SuccessResponse =
            serde_json::from_str(&history(&mut app, caught_up)).unwrap();
        let ResponseResult::AgentHistory { history: page } = success.result else {
            panic!("expected agent_history");
        };
        assert!(
            page.turns.iter().all(|t| t.at_ms == Some(4_000)),
            "only the still-running m4 again, until it completes"
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
