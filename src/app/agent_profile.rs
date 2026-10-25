//! The agent's profile selector, recorded with the agent (#1010).
//!
//! A profile selector such as `CLAUDE_CONFIG_DIR` relocates an agent CLI's
//! whole home, credentials included, and on a fleet that picks one with a
//! shell function it exists only in the environment of the agent process. A
//! resume spawns a fresh shell that has none of it, so a resumed agent comes
//! up on the CLI's default profile: logged out, or logged in as someone else.
//!
//! `agent.start` already records what it carried in `launch_env`. An agent the
//! operator launched by hand in a pane carried nothing through flock, so its
//! selector is read here from the agent's own process and merged into the same
//! field, which the session snapshot persists and the resume re-exports. Which
//! keys count is the fleet's `[spawn.env]` declaration, the same table the
//! spawn carry reads, so the two cannot disagree about a key.

use crate::terminal::TerminalId;

use super::App;

impl App {
    /// Read the declared selector keys from the pane's live agent process
    /// (or from `requester_pid`, a process inside it) and merge them into the
    /// terminal's `launch_env`. A key the process does not set keeps whatever
    /// was recorded: a value carried at start is better evidence than an
    /// environment that could not be read.
    pub(crate) fn record_agent_profile_env(
        &mut self,
        terminal_id: &TerminalId,
        requester_pid: Option<u32>,
    ) {
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return;
        };
        let agent = terminal
            .effective_agent_label()
            .and_then(crate::detect::parse_agent_label)
            .or_else(|| {
                terminal
                    .launch_argv
                    .as_deref()
                    .and_then(crate::spawn::env::agent_for_argv)
            });
        let keys = self.state.config.spawn.env.keys_for(agent);
        if keys.is_empty() {
            return;
        }
        let process = requester_pid.or_else(|| self.agent_process_pid(terminal_id));
        let Some(env) =
            process.and_then(|pid| crate::spawn::env::read_requester_env(pid, std::process::id()))
        else {
            return;
        };
        let found = selector_values(keys, &env);
        if found.is_empty() {
            return;
        }
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            if merge_launch_env(&mut terminal.launch_env, found) {
                self.state.mark_session_dirty();
            }
        }
    }

    /// Record the profile of a session a hook just reported, once per
    /// session. The hook runs inside the agent, so the reporting peer's
    /// ancestry is the agent's own environment: this is the moment an agent
    /// the operator launched by hand gets its profile on record.
    pub(crate) fn record_reported_session_profile(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        session: String,
    ) {
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .cloned()
        else {
            return;
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return;
        };
        if terminal.profile_env_session.as_deref() == Some(session.as_str()) {
            return;
        }
        terminal.profile_env_session = Some(session);
        self.record_agent_profile_env(&terminal_id, self.current_api_peer_pid);
    }

    /// The pane's foreground process: the agent itself when it runs in a
    /// shell, or the pane's own child when it was execed from argv.
    fn agent_process_pid(&self, terminal_id: &TerminalId) -> Option<u32> {
        let child = self.terminal_runtimes.get(terminal_id)?.child_pid()?;
        Some(crate::platform::foreground_process_group_id(child).unwrap_or(child))
    }

    /// The selector environment a resume must re-export for `plan`: what the
    /// terminal recorded, with Claude's per-session record (written by its
    /// `SessionStart` hook from inside the session) taking precedence for
    /// `CLAUDE_CONFIG_DIR`, because it names the session being resumed.
    pub(crate) fn resume_profile_env(
        &self,
        terminal_id: &TerminalId,
        plan: &crate::agent_resume::AgentResumePlan,
    ) -> Vec<(String, String)> {
        let agent = crate::detect::parse_agent_label(&plan.agent)
            .or_else(|| crate::spawn::env::agent_for_argv(&plan.argv));
        let keys = self.state.config.spawn.env.keys_for(agent);
        let mut env: Vec<(String, String)> = self
            .state
            .terminals
            .get(terminal_id)
            .map(|terminal| {
                terminal
                    .launch_env
                    .iter()
                    .filter(|(key, value)| keys.contains(key) && !value.is_empty())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if let Some(config_dir) = crate::agent_resume::claude_resume_session_id(plan)
            .and_then(crate::agent_resume::claude_config_dir_for_session)
        {
            merge_launch_env(&mut env, vec![("CLAUDE_CONFIG_DIR".into(), config_dir)]);
        }
        env
    }
}

fn selector_values(
    keys: &[String],
    env: &std::collections::BTreeMap<String, String>,
) -> Vec<(String, String)> {
    keys.iter()
        .filter_map(|key| {
            env.get(key)
                .filter(|value| !value.is_empty())
                .map(|value| (key.clone(), value.clone()))
        })
        .collect()
}

/// Upsert `found` into `env`, keeping its order. Whether anything changed.
fn merge_launch_env(env: &mut Vec<(String, String)>, found: Vec<(String, String)>) -> bool {
    let mut changed = false;
    for (key, value) in found {
        match env.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, existing)) if *existing == value => {}
            Some((_, existing)) => {
                *existing = value;
                changed = true;
            }
            None => {
                env.push((key, value));
                changed = true;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_upserts_and_reports_change() {
        let mut env = vec![("A".to_string(), "1".to_string())];
        assert!(!merge_launch_env(&mut env, vec![("A".into(), "1".into())]));
        assert!(merge_launch_env(&mut env, vec![("A".into(), "2".into())]));
        assert!(merge_launch_env(&mut env, vec![("B".into(), "3".into())]));
        assert_eq!(
            env,
            vec![("A".into(), "2".into()), ("B".into(), "3".into())]
        );
    }

    #[test]
    fn only_declared_nonempty_keys_are_read() {
        let env = std::collections::BTreeMap::from([
            ("CLAUDE_CONFIG_DIR".to_string(), "/p/work".to_string()),
            ("EMPTY".to_string(), String::new()),
            ("OTHER".to_string(), "x".to_string()),
        ]);
        let keys = vec!["CLAUDE_CONFIG_DIR".to_string(), "EMPTY".to_string()];
        assert_eq!(
            selector_values(&keys, &env),
            vec![("CLAUDE_CONFIG_DIR".to_string(), "/p/work".to_string())]
        );
    }

    fn app_with_claude_pane(launch_env: Vec<(String, String)>) -> (App, TerminalId) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("main"));
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane has a terminal");
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal");
        terminal.launch_argv = Some(vec!["claude".into()]);
        terminal.launch_env = launch_env;
        (app, terminal_id)
    }

    #[test]
    fn reload_spawn_env_changes_resume_selection_and_keeps_launch_snapshot() {
        let _env = crate::config::test_config_env_guard();
        let root = crate::test_support::TempDir::socket();
        let path = root.path().join("config.toml");
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        let snapshot = vec![
            ("CLAUDE_CONFIG_DIR".into(), "old-profile".into()),
            ("TEST_PROFILE_DIR".into(), "new-profile".into()),
        ];
        let (mut app, terminal_id) = app_with_claude_pane(snapshot.clone());
        let plan = crate::agent_resume::AgentResumePlan {
            agent: "claude".into(),
            argv: vec!["claude".into()],
            dedupe_key: "reload-fixture".into(),
        };
        assert_eq!(
            app.resume_profile_env(&terminal_id, &plan),
            vec![snapshot[0].clone()]
        );
        std::fs::write(&path, "[spawn.env]\nclaude = [\"TEST_PROFILE_DIR\"]\n").unwrap();
        let report = app.apply_config_from_disk(false);
        assert_eq!(report.status, crate::config::ConfigReloadStatus::Applied);
        assert_eq!(
            app.resume_profile_env(&terminal_id, &plan),
            vec![snapshot[1].clone()]
        );
        assert!(
            crate::spawn::allowlist::for_argv(&plan.argv, &app.state.config.spawn.env)
                .keys()
                .contains(&"TEST_PROFILE_DIR".to_string())
        );
        assert_eq!(app.state.terminals[&terminal_id].launch_env, snapshot);
        std::fs::write(&path, "[spawn.env]\nclaude = 42\n").unwrap();
        let report = app.apply_config_from_disk(false);
        assert_eq!(report.status, crate::config::ConfigReloadStatus::Partial);
        assert!(report
            .diagnostics
            .iter()
            .any(|message| message.contains("spawn")));
        assert_eq!(
            app.resume_profile_env(&terminal_id, &plan),
            vec![snapshot[1].clone()]
        );
        assert_eq!(app.state.terminals[&terminal_id].launch_env, snapshot);
    }

    #[test]
    fn resume_reexports_the_recorded_profile_and_nothing_undeclared() {
        let (app, terminal_id) = app_with_claude_pane(vec![
            ("CLAUDE_CONFIG_DIR".into(), "/p/eth".into()),
            ("UNRELATED".into(), "x".into()),
        ]);
        let plan = crate::agent_resume::AgentResumePlan {
            agent: "claude".into(),
            // Not a UUID-shaped id, so no per-session record can answer.
            argv: vec!["claude".into(), "--resume".into(), "no/such/record".into()],
            dedupe_key: "k".into(),
        };
        assert_eq!(
            app.resume_profile_env(&terminal_id, &plan),
            vec![("CLAUDE_CONFIG_DIR".to_string(), "/p/eth".to_string())]
        );
    }
}
