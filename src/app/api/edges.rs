//! Bind the relay process used by mesh and fleet requests.
use super::responses::{encode_error, encode_success};
use crate::api::schema::ResponseResult;
use crate::app::App;

const SSH_ANCESTRY_DEPTH: usize = 16;

/// The ancestors a relay may descend from, matched against the process name.
///
/// - `sshd`, as a prefix, which also matches macOS's `sshd-session` and
///   Linux's `sshd:` privsep child.
/// - `tailscaled`, exactly: Tailscale SSH has no sshd, and a session's shell
///   descends from `tailscaled be-child ssh` under the `tailscaled` daemon
///   (#976). Tailscale's SSH server runs on Linux, and on macOS only with the
///   open-source `tailscaled` daemon. The App Store and standalone apps cannot
///   accept Tailscale SSH sessions, so there is no other macOS process to match.
///
/// Both are matched the same way, by name: a renamed copy of a shell passes
/// for either one equally, and neither is trusted more. The caller's
/// `SSH_CONNECTION` is not consulted, since the caller sets its own
/// environment and a variable proves nothing about its ancestry.
///
/// Debug builds let the multi-node test harness name a different one: its
/// fake ssh runs the relay under the polling node's own server, and macOS
/// refuses to run a copied system shell renamed to stand in for sshd. A
/// release build never reads the variable.
fn relay_ancestor_override() -> Option<String> {
    #[cfg(debug_assertions)]
    if let Ok(name) = std::env::var("FLOCK_TEST_RELAY_ANCESTOR") {
        return Some(name);
    }
    None
}

/// Whether a process named `name` is an ssh session's server process.
fn is_ssh_server_process(name: &str, override_ancestor: Option<&str>) -> bool {
    match override_ancestor {
        Some(ancestor) => name.starts_with(ancestor),
        None => name.starts_with("sshd") || name == "tailscaled",
    }
}

/// Whether `pid` was started by an ssh session: some ancestor is an ssh
/// server process (see [`is_ssh_server_process`]).
fn descends_from_ssh(pid: u32) -> bool {
    let override_ancestor = relay_ancestor_override();
    ancestry_reaches_ssh_server(
        pid,
        override_ancestor.as_deref(),
        crate::platform::process_parent_id,
        crate::platform::process_name,
    )
}

/// The ancestry walk behind [`descends_from_ssh`], over an injected process
/// table so the tests can describe one.
fn ancestry_reaches_ssh_server(
    pid: u32,
    override_ancestor: Option<&str>,
    parent_of: impl Fn(u32) -> Option<u32>,
    name_of: impl Fn(u32) -> Option<String>,
) -> bool {
    let mut current = pid;
    for _ in 0..SSH_ANCESTRY_DEPTH {
        let Some(parent) = parent_of(current) else {
            return false;
        };
        if parent <= 1 || parent == current {
            return false;
        }
        if name_of(parent).is_some_and(|name| is_ssh_server_process(&name, override_ancestor)) {
            return true;
        }
        current = parent;
    }
    false
}

impl App {
    pub(crate) fn respond_or_park(
        &mut self,
        respond_to: std::sync::mpsc::Sender<String>,
        response: String,
    ) {
        let respond_to = match self.input_writes.park(respond_to) {
            Ok(()) => {
                self.start_ready_pastes();
                return;
            }
            Err(respond_to) => respond_to,
        };
        if let Some((request, caller)) = self.pending_recovery_park.take() {
            self.park_until_mesh_recovered(
                request,
                caller.pid,
                caller.birth,
                caller.uid,
                respond_to,
            );
            return;
        }
        if let Some(mut relay) = self.message_relays.pending.take() {
            relay.respond_to = Some(respond_to);
            self.enqueue_message_relay(relay.into_work());
            return;
        }
        if let Some((request_id, pane_id, attempt)) = self.pending_agent_submit.take() {
            self.schedule_guarded_request(request_id, pane_id, attempt, respond_to);
            return;
        }
        if let Some(install) = self.pending_integration_install.take() {
            self.spawn_integration_install(install, respond_to);
            return;
        }
        let _ = respond_to.send(response);
    }
    pub(crate) fn expire_relayed_entries(&mut self) {
        self.state.evict_expired_relayed_entries();
    }
    /// Bind only a hello Begin from an attestable SSH relay outside panes.
    pub(super) fn attach_inbound_edge(&mut self, id: String) -> String {
        let Some(pid) = self.current_api_peer_pid else {
            return encode_error(
                id,
                "relay_unattestable",
                "this platform did not report the caller's pid, so no relay can be bound",
            );
        };
        if self.caller_pane_candidate().is_some() {
            return encode_error(
                id,
                "not_from_a_pane",
                "a relay is started by the hub's ssh session, never from inside a pane",
            );
        }
        let Some(started) = crate::platform::process_start_time(pid) else {
            return encode_error(
                id,
                "relay_unattestable",
                "the caller's start time could not be read, so it cannot be told from a pid reuser",
            );
        };
        if !descends_from_ssh(pid) {
            return encode_error(
                id,
                "relay_not_from_ssh",
                "a relay is started by the hub's ssh session; this caller has no sshd or tailscaled ancestor",
            );
        }
        match self
            .inbound
            .attach(pid, started, crate::platform::process_start_time)
        {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(code) => encode_error(
                id,
                code,
                "inbound edge attachment unavailable; retry mesh.hello",
            ),
        }
    }

    /// The refusal for a relay-only method called by anyone but the relay.
    pub(super) fn refuse_unless_edge(&mut self, id: &str, method: &str) -> Option<String> {
        let caller = self.current_api_peer_pid;
        (self
            .inbound
            .edge(caller, crate::platform::process_start_time)
            .is_none())
        .then(|| {
            encode_error(
                id.to_string(),
                "not_an_edge",
                format!("{method} is accepted only from an edge bound by mesh.hello"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process table of `(pid, parent, name)` rows.
    fn walk(pid: u32, table: &[(u32, u32, &str)]) -> bool {
        ancestry_reaches_ssh_server(
            pid,
            None,
            |pid| table.iter().find(|row| row.0 == pid).map(|row| row.1),
            |pid| {
                table
                    .iter()
                    .find(|row| row.0 == pid)
                    .map(|row| row.2.to_string())
            },
        )
    }

    #[test]
    fn ssh_server_names_are_accepted() {
        for name in ["sshd", "sshd-session", "sshd: alice@pts/0", "tailscaled"] {
            assert!(is_ssh_server_process(name, None), "{name}");
        }
    }

    #[test]
    fn other_names_are_refused() {
        for name in [
            "bash",
            "zsh",
            "flk",
            "flock",
            "ssh",
            "tailscale",
            "tailscaled-impostor",
            "my-tailscaled",
            "launchd",
            "",
        ] {
            assert!(!is_ssh_server_process(name, None), "{name}");
        }
    }

    #[test]
    fn the_test_override_replaces_the_real_ancestors() {
        assert!(is_ssh_server_process("flk", Some("flk")));
        assert!(!is_ssh_server_process("sshd", Some("flk")));
        assert!(!is_ssh_server_process("tailscaled", Some("flk")));
    }

    #[test]
    fn an_openssh_session_is_accepted() {
        let table = [
            (40, 30, "flk"),
            (30, 20, "bash"),
            (20, 10, "sshd-session"),
            (10, 1, "sshd"),
        ];
        assert!(walk(40, &table));
    }

    #[test]
    fn a_linux_privsep_session_is_accepted() {
        let table = [
            (40, 30, "flk"),
            (30, 20, "bash"),
            (20, 10, "sshd: alice@notty"),
            (10, 1, "sshd"),
        ];
        assert!(walk(40, &table));
    }

    #[test]
    fn a_tailscale_ssh_session_is_accepted() {
        // The ancestry reported in #976: bash -> tailscaled -> tailscaled -> init.
        let table = [
            (40, 30, "flk"),
            (30, 20, "bash"),
            (20, 10, "tailscaled"),
            (10, 1, "tailscaled"),
        ];
        assert!(walk(40, &table));
    }

    #[test]
    fn a_shell_renamed_to_resemble_tailscaled_is_refused() {
        let table = [
            (40, 30, "flk"),
            (30, 20, "tailscaled-sh"),
            (20, 1, "launchd"),
        ];
        assert!(!walk(40, &table));
    }

    #[test]
    fn a_pane_process_is_refused() {
        // A locally started server has no SSH ancestor. A server started over
        // SSH does; attach_inbound_edge must independently refuse pane candidates.
        let table = [
            (50, 40, "flk"),
            (40, 30, "zsh"),
            (30, 20, "flk"),
            (20, 10, "alacritty"),
            (10, 1, "launchd"),
        ];
        assert!(!walk(50, &table));
    }

    #[test]
    fn the_caller_itself_is_not_its_own_ancestor() {
        let table = [(40, 30, "sshd"), (30, 1, "launchd")];
        assert!(!walk(40, &table));
    }

    #[test]
    fn a_vanished_parent_or_cycle_ends_the_walk() {
        assert!(!walk(40, &[(40, 30, "flk")]));
        assert!(!walk(40, &[(40, 40, "flk")]));
    }

    #[test]
    fn an_ancestor_beyond_the_depth_limit_is_not_reached() {
        let depth = SSH_ANCESTRY_DEPTH as u32;
        let mut table: Vec<(u32, u32, &str)> = (0..depth + 1)
            .map(|step| (100 + step, 101 + step, "bash"))
            .collect();
        table.push((101 + depth, 1, "sshd"));
        assert!(!walk(100, &table));
        assert!(walk(100 + 1, &table));
    }
    #[tokio::test]
    async fn unproven_pane_is_refused_as_an_ssh_relay() {
        const CHILD: &str = "FLOCK_TEST_OWNED_SSH_DRIVER";
        if std::env::var_os(CHILD).is_none() {
            let root = crate::test_support::TempDir::socket();
            let executable = root.path().join("sshd");
            // Rename our own test executable: copied system shells cannot stand
            // in for sshd under the macOS process-name restrictions.
            std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
            let mut child =
                crate::process::TracedCommand::new(&executable, "test:ssh-pane-refusal")
                    .args([
                        "--exact",
                        "app::api::edges::tests::unproven_pane_is_refused_as_an_ssh_relay",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env_remove("FLOCK_TEST_RELAY_ANCESTOR")
                    .spawn_traced()
                    .unwrap();
            let pid = child.id();
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let _ = tx.send(child.wait());
            });
            let result = rx.recv_timeout(std::time::Duration::from_secs(10));
            if result.is_err() {
                crate::platform::signal_processes(&[pid], crate::platform::Signal::Kill);
                // Reap if possible, but never join a live silent waiter.
                let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
            }
            assert!(
                result
                    .as_ref()
                    .is_ok_and(|r| r.as_ref().is_ok_and(|status| status.success())),
                "owned SSH driver failed: {result:?}"
            );
            worker.join().unwrap();
            let fixture_path = root.path().to_owned();
            drop(root);
            assert!(
                !fixture_path.exists(),
                "owned executable fixture must be removed"
            );
            return;
        }
        assert!(crate::platform::process_name(std::process::id())
            .unwrap()
            .starts_with("sshd"));
        for mismatch in [false, true] {
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                &crate::config::Config::default(),
                true,
                None,
                rx,
                crate::api::EventHub::default(),
            );
            app.state.workspaces = vec![crate::workspace::Workspace::test_new("ssh-pane")];
            app.state.ensure_test_terminals();
            let pane = app.state.workspaces[0].focused_pane_id().unwrap();
            crate::test_support::adopt_unproven_pane_caller(&mut app, pane, mismatch);
            assert!(descends_from_ssh(app.current_api_peer_pid.unwrap()));
            assert_eq!(app.caller_pane(), None, "unproven pane grants no authority");
            let response: serde_json::Value =
                serde_json::from_str(&app.attach_inbound_edge("relay".into())).unwrap();
            assert_eq!(response["error"]["code"], "not_from_a_pane", "{response}");
            assert!(!app.caller_is_local_operator());
        }
    }
}
