use crate::api::schema::{IntegrationInstallResult, IntegrationUninstallResult, ResponseResult};
use crate::app::App;

use super::responses::{encode_error, encode_success};

/// See `App::claude_version_probe`.
pub(crate) type VersionProbe =
    std::sync::Arc<dyn Fn() -> Option<crate::update::Version> + Send + Sync>;

/// An `integration.install` waiting for its worker.
#[derive(Debug)]
pub(crate) struct PendingInstall {
    pub(crate) id: String,
    pub(crate) target: crate::api::schema::IntegrationTarget,
    pub(crate) human_tools: Vec<String>,
}

impl App {
    /// Answered from a worker thread (see `respond_or_park`): installing
    /// Claude probes `claude --version`, which may take seconds, and the App
    /// loop must never wait on it. The live `[questions] human_tools` is read
    /// here, on the loop.
    pub(super) fn handle_integration_install(
        &mut self,
        id: String,
        params: crate::api::schema::IntegrationInstallParams,
    ) -> String {
        self.pending_integration_install = Some(PendingInstall {
            id,
            target: params.target,
            human_tools: self.state.config.questions.human_tools.clone(),
        });
        String::new()
    }

    /// Run a parked `integration.install` and answer it from the worker.
    pub(crate) fn spawn_integration_install(
        &self,
        install: PendingInstall,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let probe = self.claude_version_probe.clone();
        std::thread::spawn(move || {
            let PendingInstall {
                id,
                target,
                human_tools,
            } = install;
            let question = || crate::integration::ClaudeQuestionHook::new(human_tools, probe());
            let response = match crate::integration::install_target(target, question) {
                Ok(messages) => encode_success(
                    id,
                    ResponseResult::IntegrationInstall {
                        target,
                        details: IntegrationInstallResult { messages },
                    },
                ),
                Err(err) => encode_error(id, "integration_install_failed", err.to_string()),
            };
            let _ = respond_to.send(response);
        });
    }

    pub(super) fn handle_integration_uninstall(
        &mut self,
        id: String,
        params: crate::api::schema::IntegrationUninstallParams,
    ) -> String {
        let target = params.target;
        let messages = match crate::integration::uninstall_target(target) {
            Ok(messages) => messages,
            Err(err) => return encode_error(id, "integration_uninstall_failed", err.to_string()),
        };

        encode_success(
            id,
            ResponseResult::IntegrationUninstall {
                target,
                details: IntegrationUninstallResult { messages },
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::api::schema::{IntegrationInstallParams, IntegrationTarget, Method, Request};
    use crate::api::{ApiRequestMessage, EventHub};
    use crate::app::delegation::handler_tests::{app_on, shutdown, temp_log};

    /// #1180 review: `integration.install` for Claude probes `claude
    /// --version`, which can take seconds. The loop hands the request to a
    /// worker and returns at once; the answer arrives once the probe does,
    /// built from the LIVE `[questions] human_tools`, not the file on disk.
    #[tokio::test]
    async fn installing_claude_never_waits_on_the_app_loop() {
        let _lock = crate::integration::integration_env_lock();
        let log = temp_log("integration-install-off-loop");
        let claude_dir = log.parent().expect("dir").join("claude");
        std::fs::create_dir_all(&claude_dir).expect("claude dir");
        std::env::set_var("CLAUDE_CONFIG_DIR", &claude_dir);

        let mut app = app_on(EventHub::with_persistence(log));
        app.state.config.questions.human_tools = vec!["mcp__live__ask".into()];
        let (entered_tx, entered) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let released = std::sync::Mutex::new(released);
        app.claude_version_probe = std::sync::Arc::new(move || {
            let _ = entered_tx.send(());
            let _ = released.lock().expect("probe lock").recv();
            crate::update::Version::parse("2.1.296")
        });

        let (respond_to, answer) = mpsc::channel();
        app.handle_api_request_message(ApiRequestMessage {
            request: Request {
                id: "install".into(),
                method: Method::IntegrationInstall(IntegrationInstallParams {
                    target: IntegrationTarget::Claude,
                }),
            },
            respond_to,
            peer_pid: None,
        });

        // The loop is back while the probe is still held.
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker probes");
        assert!(answer.try_recv().is_err(), "answered before the probe");
        release.send(()).expect("release the probe");
        let response: serde_json::Value = serde_json::from_str(
            &answer
                .recv_timeout(Duration::from_secs(10))
                .expect("an answer"),
        )
        .expect("json");
        assert_eq!(
            response["result"]["type"], "integration_install",
            "{response}"
        );

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude_dir.join("settings.json")).expect("settings"),
        )
        .expect("json");
        let entry = &settings["hooks"]["PreToolUse"][0];
        assert_eq!(
            entry["matcher"],
            "^(AskUserQuestion|ExitPlanMode|mcp__live__ask)$"
        );
        assert_eq!(entry["hooks"][0]["onFailure"], "block");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        shutdown(app);
    }
}
