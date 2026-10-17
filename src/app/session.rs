use std::time::Instant;

use super::{App, SESSION_SAVE_DEBOUNCE};

impl App {
    pub(super) fn schedule_session_save(&mut self) {
        if !self.no_session {
            self.session_save_deadline = Some(Instant::now() + SESSION_SAVE_DEBOUNCE);
        }
    }

    pub(crate) fn sync_session_save_schedule(&mut self) {
        if self.state.session_dirty {
            self.state.session_dirty = false;
            self.schedule_session_save();
        }
    }

    /// Finish owned workers and always save before returning their typed diagnostics.
    pub(crate) fn finish_session(
        &mut self,
    ) -> Result<(), crate::structured::workers::ShutdownError> {
        let result = self
            .structured_workers
            .shutdown(&mut self.state.structured_agents);
        if let Err(error) = &result {
            crate::logging::structured_worker_shutdown_failed(error);
        }
        self.save_session_now();
        result
    }

    pub(crate) fn save_session_now(&mut self) {
        if self.no_session {
            self.session_save_deadline = None;
            return;
        }

        if self.state.workspaces.is_empty() && self.state.structured_agents.is_empty() {
            crate::persist::clear();
        } else {
            let mut snap = crate::persist::capture(
                &self.state.structured_agents,
                &self.state.workspaces,
                &self.state.terminals,
                &self.terminal_runtimes,
                self.state.active,
                self.state.selected,
                self.state.panel_scopes(),
                self.state.sidebar_width,
                self.state.sidebar_section_split,
                self.state.collapsed_space_keys.clone(),
                self.state
                    .pane_id_aliases
                    .iter()
                    .map(|(old, pane)| (*old, pane.raw()))
                    .collect(),
            );
            snap.issue_draft = self.state.issue_draft_snapshot();
            let history = self.persist_pane_history.then(|| {
                crate::persist::capture_history(&self.state.workspaces, &self.terminal_runtimes)
            });
            crate::persist::save(&snap, history.as_ref());
        }

        self.session_save_deadline = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::agent_state::{Lifecycle, Phase};
    use crate::structured::registry::Record;

    struct SavedEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for SavedEnv {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    #[test]
    fn registry_only_save_and_app_startup_restore_metadata_offline() {
        let _guard = crate::config::test_config_env_guard();
        let root = crate::test_support::TempDir::socket();
        let _env = SavedEnv(vec![
            (
                crate::config::CONFIG_PATH_ENV_VAR,
                std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR),
            ),
            ("XDG_STATE_HOME", std::env::var_os("XDG_STATE_HOME")),
            ("XDG_CONFIG_HOME", std::env::var_os("XDG_CONFIG_HOME")),
        ]);
        std::env::set_var(
            crate::config::CONFIG_PATH_ENV_VAR,
            root.path().join("config.toml"),
        );
        std::env::set_var("XDG_STATE_HOME", root.path().join("state"));
        std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        assert!(crate::session::data_dir().starts_with(root.path()));
        let config = crate::config::Config::default();
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&config, true, None, rx, crate::api::EventHub::default());
        assert!(app.state.workspaces.is_empty());
        app.no_session = false;
        let record = Record {
            runtime: crate::structured::registry::StructuredRuntime::Structured,
            agent_id: "agent_fixture_structured".into(),
            name: Some("fixture-worker".into()),
            harness: "claude".into(),
            session_id: "fixture-session".into(),
            cwd: root.path().to_path_buf(),
            model: Some("fixture-model".into()),
        };
        app.state
            .structured_agents
            .register(record.clone())
            .unwrap();
        app.save_session_now();
        let persisted = crate::persist::load().expect("registry-only session must be saved");
        assert!(persisted.workspaces.is_empty());
        assert_eq!(persisted.structured_agents.records(), vec![record.clone()]);
        drop(app);
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let restored = App::new(&config, false, None, rx, crate::api::EventHub::default());
        let (metadata, view, _) = restored
            .state
            .structured_agents
            .get(&record.agent_id)
            .unwrap();
        assert_eq!(metadata, &record);
        assert_eq!(view.update.state.lifecycle, Lifecycle::Offline);
        assert_eq!(view.update.state.phase, Phase::Offline);
        assert!(restored.state.terminals.is_empty());
        assert_eq!(restored.terminal_runtimes.values().count(), 0);
        assert!(view.update.needs_human.is_empty());
        assert!(view.update.observe_at.is_none());
    }
    #[tokio::test]
    async fn worker_timeout_still_saves_the_whole_session_before_returning() {
        use crate::api::error_code::ErrorCode;
        use crate::structured::workers::tests::{event, Case};
        let _guard = crate::config::test_config_env_guard();
        let root = crate::test_support::TempDir::socket();
        let _env = SavedEnv(vec![
            (
                crate::config::CONFIG_PATH_ENV_VAR,
                std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR),
            ),
            ("XDG_STATE_HOME", std::env::var_os("XDG_STATE_HOME")),
            ("XDG_CONFIG_HOME", std::env::var_os("XDG_CONFIG_HOME")),
        ]);
        std::env::set_var(
            crate::config::CONFIG_PATH_ENV_VAR,
            root.path().join("config.toml"),
        );
        std::env::set_var("XDG_STATE_HOME", root.path().join("state"));
        std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let notify = app.render_notify.clone();
        let case = Case::start(
            &mut app.state.structured_agents,
            &mut app.structured_workers,
            41,
            "worker-stubborn",
            vec![event("control_request")],
        );
        case.human(
            &mut app.state.structured_agents,
            &mut app.structured_workers,
            &notify,
            Instant::now(),
        )
        .await;
        let timeout = case.force_shutdown(
            &mut app.state.structured_agents,
            &mut app.structured_workers,
        );
        assert!(timeout
            .0
            .contains(&(case.id.clone(), ErrorCode::StructuredWorkerTimeout)));
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("worker-save")];
        app.state.ensure_test_terminals();
        app.no_session = false;
        let error = app.finish_session().unwrap_err();
        assert!(error
            .0
            .contains(&(case.id.clone(), ErrorCode::StructuredWorkerTimeout)));
        let saved = crate::persist::load().expect("timeout must not skip the session save");
        assert_eq!(saved.workspaces.len(), 1);
        assert_eq!(
            saved.workspaces[0].id.as_deref(),
            Some(app.state.workspaces[0].id.as_str())
        );
        assert_eq!(
            saved.structured_agents.records(),
            app.state.structured_agents.records()
        );
        assert_eq!(
            saved
                .structured_agents
                .get(&case.id)
                .unwrap()
                .1
                .update
                .state
                .lifecycle,
            Lifecycle::Offline
        );
        case.assert_shutdown(&app.state.structured_agents, &app.structured_workers);
    }

    #[test]
    fn unknown_only_session_survives_real_startup_and_save() {
        let _guard = crate::config::test_config_env_guard();
        let root = crate::test_support::TempDir::socket();
        let _env = SavedEnv(vec![
            (
                crate::config::CONFIG_PATH_ENV_VAR,
                std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR),
            ),
            ("XDG_STATE_HOME", std::env::var_os("XDG_STATE_HOME")),
            ("XDG_CONFIG_HOME", std::env::var_os("XDG_CONFIG_HOME")),
        ]);
        std::env::set_var(
            crate::config::CONFIG_PATH_ENV_VAR,
            root.path().join("config.toml"),
        );
        std::env::set_var("XDG_STATE_HOME", root.path().join("state"));
        std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
        std::fs::create_dir_all(crate::config::config_dir()).unwrap();
        assert!(crate::session::data_dir().starts_with(root.path()));
        let path = crate::session::data_dir().join("session.json");
        for raw in [
            serde_json::json!([{"runtime":"future-runtime","future_data":[1,{"kept":true}]}]),
            serde_json::json!({"future_collection":["kept"]}),
        ] {
            std::fs::write(
                &path,
                serde_json::json!({
                    "workspaces":[],"active":null,"selected":0,"structured_agents":raw,
                })
                .to_string(),
            )
            .unwrap();
            let (_, rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                &crate::config::Config::default(),
                false,
                None,
                rx,
                crate::api::EventHub::default(),
            );
            assert!(app.state.workspaces.is_empty());
            assert!(app.state.structured_agents.records().is_empty());
            assert_eq!(app.state.structured_agents.retained_count(), 1);
            app.save_session_now();
            let saved: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(saved["structured_agents"], raw);
        }
    }
}
