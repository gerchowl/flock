//! `events.read` (ADR-0028 §3, #1028): a pull read of the durable event log
//! from a cursor the consumer holds.
//!
//! The answer is every matching durable event with `seq > after`, from the
//! ring or the rotated files on disk, a `gap` first when the cursor is older
//! than what is retained, and `next_after` to resume from. With `wait_ms`
//! the request is held on the event hub's condvar until a matching event
//! lands, like `msg.wait_reply`, never by a client-side poll.
//!
//! A held read is a registered wait (ADR-0028 §5, #1031): while it is held,
//! the App records it against the attested caller, which is what makes an
//! agent running `flk events wait` read `waiting_on_event`.

use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::schema::{
    ErrorBody, ErrorResponse, EventEnvelope, EventGap, EventsReadParams, Method, Request,
    ResponseResult, SequencedEvent, SuccessResponse, EVENTS_READ_DEFAULT_LIMIT,
    EVENTS_READ_MAX_LIMIT,
};
use crate::api::server::{should_stop_connection, CONNECTION_POLL_INTERVAL};
use crate::api::wait_predicate::WaitPredicate;
use crate::api::{ApiRequestMessage, ApiRequestSender, EventHub};

/// A held read's open wait in the App's registry, released on drop so that
/// every way out of the hold (an answer, the deadline, a hang-up, an error)
/// closes it.
struct HeldWait<'a> {
    api_tx: &'a ApiRequestSender,
    peer: crate::platform::peer::Credentials,
    admission: crate::api::schema::WaitAdmission,
}

impl<'a> HeldWait<'a> {
    /// Register the hold. The App attests the caller from captured socket credentials and
    /// records nothing when it runs in no agent pane.
    fn open(
        api_tx: &'a ApiRequestSender,
        peer: crate::platform::peer::Credentials,
        predicate: WaitPredicate,
        hold: Duration,
    ) -> Self {
        Self::open_with_timeout(
            api_tx,
            peer,
            predicate,
            hold,
            super::server::APP_RESPONSE_TIMEOUT,
        )
    }

    // Test seam: a zero response budget forces timeout while App dispatch is gated.
    fn open_with_timeout(
        api_tx: &'a ApiRequestSender,
        peer: crate::platform::peer::Credentials,
        predicate: WaitPredicate,
        hold: Duration,
        response_timeout: Duration,
    ) -> Self {
        let hold_ms = u64::try_from(hold.as_millis()).unwrap_or(u64::MAX);
        let admission = Arc::default();
        // Cleanup authority travels in-process, never through the JSON response.
        let _response = super::server::dispatch_to_app_with_timeout(
            Request {
                id: "events.read:hold".into(),
                method: Method::WaitHoldAdmitted(predicate, hold_ms, Arc::clone(&admission)),
            },
            api_tx,
            Some(response_timeout),
            peer,
        );
        Self {
            api_tx,
            peer,
            admission,
        }
    }
}

impl Drop for HeldWait<'_> {
    fn drop(&mut self) {
        let Some(authority) = self.admission.abandon() else {
            return;
        };
        // Nobody reads the answer: the release cannot fail in a way the
        // reader could act on.
        let (respond_to, _) = std::sync::mpsc::channel();
        let _ = self.api_tx.send(ApiRequestMessage {
            request: Request {
                id: "events.read:release".into(),
                method: Method::WaitReleaseAdmitted(authority),
            },
            respond_to,
            peer_pid: self.peer.pid,
            peer_birth: self.peer.birth,
            peer_uid: self.peer.uid,
        });
    }
}

fn encode(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"internal_error","message":"encode failed"}}"#.into()
    })
}

fn refuse(id: String, code: &str, message: String) -> String {
    encode(&ErrorResponse {
        id,
        error: ErrorBody {
            data: None,
            code: code.into(),
            message,
        },
    })
}

fn matches_subject(envelope: &EventEnvelope, subjects: &[String]) -> bool {
    if subjects.is_empty() {
        return true;
    }
    let Ok(serde_json::Value::Object(data)) = serde_json::to_value(&envelope.data) else {
        return false;
    };
    data.values()
        .filter_map(serde_json::Value::as_str)
        .any(|value| subjects.iter().any(|subject| subject == value))
}

/// Answer one `events.read`. `None` when the client hung up while it waited.
pub(super) fn read_events(
    request_id: String,
    params: EventsReadParams,
    stream: &mut UnixStream,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    api_tx: &ApiRequestSender,
    peer: crate::platform::peer::Credentials,
) -> std::io::Result<Option<String>> {
    if let Some(kind) = params.kinds.iter().find(|kind| !kind.is_persisted()) {
        return Ok(Some(refuse(
            request_id,
            "invalid_params",
            format!("{kind:?} is not durable; events.read serves durable kinds only"),
        )));
    }
    if params.after > event_hub.current_sequence() {
        // Suggest the durable head: a later sequence may belong to an
        // ephemeral event that a restart hands out again.
        // `data.head` is the structured form a client resumes from (#1029).
        let head = event_hub.durable_sequence();
        return Ok(Some(encode(&serde_json::json!({
            "id": request_id,
            "error": {
                "code": "cursor_ahead",
                "message": format!(
                    "cursor {} is past this server's newest event {head}; read from 0 or {head}",
                    params.after
                ),
                "data": {"head": head},
            },
        }))));
    }
    let limit = params
        .limit
        .unwrap_or(EVENTS_READ_DEFAULT_LIMIT)
        .clamp(1, EVENTS_READ_MAX_LIMIT) as usize;
    let wait_ms = params.wait_ms.unwrap_or(0);
    let wait = Duration::from_millis(wait_ms.min(event_hub.read_max_wait_ms()));
    let deadline = Instant::now() + wait;
    let keep = |envelope: &EventEnvelope| {
        (params.kinds.is_empty() || params.kinds.contains(&envelope.event))
            && matches_subject(envelope, &params.subjects)
    };
    let mut after = params.after;
    let mut held: Option<HeldWait> = None;
    loop {
        let read = event_hub.read(after, limit, keep);
        let found = !read.events.is_empty() || read.gap.is_some();
        if found || Instant::now() >= deadline {
            return Ok(Some(encode(&SuccessResponse {
                id: request_id,
                result: ResponseResult::EventsRead {
                    gap: read.gap.map(|(from, to)| EventGap { from, to }),
                    events: read
                        .events
                        .into_iter()
                        .map(|(seq, envelope)| SequencedEvent { seq, envelope })
                        .collect(),
                    next_after: read.next_after.max(params.after),
                },
            })));
        }
        // Nothing matched up to here: skip it rather than rescan it.
        after = read.scanned;
        if held.is_none() {
            let predicate = WaitPredicate::Events {
                kinds: params.kinds.clone(),
                subjects: params.subjects.clone(),
                after: params.after,
            };
            held = Some(HeldWait::open(api_tx, peer, predicate, wait));
        }
        if should_stop_connection(stream, running)? {
            return Ok(None);
        }
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(CONNECTION_POLL_INTERVAL);
        event_hub.wait_after(after, slice);
    }
}

#[cfg(test)]
mod attribution_tests {
    use super::*;

    #[tokio::test]
    async fn timed_out_hold_admitted_after_reader_exit_is_released_by_id() {
        for uid in [7, 42] {
            let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
            let (_events, events_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = crate::app::App::new(
                &crate::config::Config::default(),
                true,
                None,
                events_rx,
                EventHub::default(),
            );
            app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
                operator: 7,
                dedicated: Ok(Some(42)).into(),
            };
            app.state.workspaces = vec![crate::workspace::Workspace::test_new("late-held-read")];
            app.state.ensure_test_terminals();
            let pane = app.state.workspaces[0].focused_pane_id().unwrap();
            let terminal_id = app.state.workspaces[0]
                .pane_state(pane)
                .unwrap()
                .attached_terminal_id
                .clone();
            app.state
                .terminals
                .get_mut(&terminal_id)
                .unwrap()
                .launch_argv = Some(vec!["claude".into()]);
            let agent_id = app
                .state
                .terminals
                .get(&terminal_id)
                .unwrap()
                .agent_id
                .to_string();
            crate::test_support::attest_pane_caller(&mut app, pane);
            let peer = crate::platform::peer::Credentials {
                pid: app.current_api_peer_pid,
                birth: app
                    .current_api_peer_pid
                    .and_then(crate::platform::process_start_time),
                uid: Some(uid),
            };
            let predicate = || WaitPredicate::Events {
                kinds: vec![],
                subjects: vec![],
                after: 0,
            };
            let (finished, finish) = std::sync::mpsc::channel();
            let driver = std::thread::spawn(move || {
                let held = HeldWait::open_with_timeout(
                    &api_tx,
                    peer,
                    predicate(),
                    Duration::from_secs(600),
                    Duration::ZERO,
                );
                drop(held);
                finished.send(()).unwrap();
            });
            let hold = tokio::time::timeout(Duration::from_secs(2), api_rx.recv())
                .await
                .unwrap()
                .unwrap();
            // The App has not handled the queued hold. Timeout and Drop finish first.
            finish.recv_timeout(Duration::from_secs(2)).unwrap();
            driver.join().unwrap();
            let (other, _) = app
                .waits
                .open(
                    agent_id.clone(),
                    pane,
                    predicate(),
                    Duration::from_secs(600),
                    Instant::now(),
                    0,
                )
                .unwrap();
            app.handle_api_request_message(hold);
            assert!(
                app.waits.close(&other).is_some(),
                "another hold must survive"
            );
            assert!(
                app.waits.next_deadline().is_none(),
                "uid={uid}: late hold must close immediately"
            );
            assert!(matches!(
                app.waits.waiting(&agent_id),
                crate::api::agent_state::Waiting::None
            ));
            app.tick_waits(Instant::now() + Duration::from_secs(3600));
            assert!(!app
                .event_hub
                .events_after(0)
                .iter()
                .any(|(_, event)| { event.event == crate::api::schema::EventKind::WaitExpired }));
        }
    }

    #[tokio::test]
    async fn held_read_release_keeps_captured_peer_and_closes_the_wait() {
        let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_events, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            events_rx,
            EventHub::default(),
        );
        app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
            operator: 7,
            dedicated: Ok(Some(42)).into(),
        };
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("held-read")];
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[0].focused_pane_id().unwrap();
        let terminal = app.state.workspaces[0]
            .pane_state(pane)
            .unwrap()
            .attached_terminal_id
            .clone();
        app.state.terminals.get_mut(&terminal).unwrap().launch_argv = Some(vec!["claude".into()]);
        crate::test_support::attest_pane_caller(&mut app, pane);
        let peer = crate::platform::peer::Credentials {
            pid: app.current_api_peer_pid,
            birth: app
                .current_api_peer_pid
                .and_then(crate::platform::process_start_time),
            uid: Some(7),
        };
        let (finish, release) = std::sync::mpsc::channel();
        let driver = std::thread::spawn(move || {
            let held = HeldWait::open(
                &api_tx,
                peer,
                WaitPredicate::Events {
                    kinds: vec![],
                    subjects: vec![],
                    after: 0,
                },
                Duration::from_secs(1),
            );
            release.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(held);
        });
        let hold = api_rx.recv().await.unwrap();
        app.handle_api_request_message(hold);
        let due = app.waits.next_deadline().expect("hold was attributed");
        finish.send(()).unwrap();
        let release = api_rx.recv().await.unwrap();
        app.handle_api_request_message(release);
        driver.join().unwrap();
        assert_eq!(
            app.waits.next_deadline(),
            None,
            "release must close the attributed wait"
        );
        app.tick_waits(due);
        assert!(!app
            .event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| event.event == crate::api::schema::EventKind::WaitExpired));
    }
    #[tokio::test]
    async fn exited_caller_hangup_releases_its_admitted_wait() {
        for uid in [7, 42] {
            let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
            let (_events, events_rx) = tokio::sync::mpsc::unbounded_channel();
            let hub = EventHub::default();
            let mut app = crate::app::App::new(
                &crate::config::Config::default(),
                true,
                None,
                events_rx,
                hub.clone(),
            );
            app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
                operator: 7,
                dedicated: Ok(Some(42)).into(),
            };
            app.state.workspaces = vec![crate::workspace::Workspace::test_new("exited-reader")];
            app.state.ensure_test_terminals();
            let pane = app.state.workspaces[0].focused_pane_id().unwrap();
            let terminal_id = app.state.workspaces[0]
                .pane_state(pane)
                .unwrap()
                .attached_terminal_id
                .clone();
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.launch_argv = Some(vec!["claude".into()]);
            let now = Instant::now();
            for at in [now - Duration::from_secs(30), now] {
                terminal.set_detected_state_with_screen_signals_at(
                    Some(crate::detect::Agent::Claude),
                    crate::detect::AgentState::Idle,
                    false,
                    true,
                    false,
                    false,
                    at,
                );
            }
            let agent_id = terminal.agent_id.to_string();
            let (mut control, pid, _directory) = spawn_reader_in_pane(&mut app, pane, &terminal_id);
            let peer = crate::platform::peer::Credentials {
                pid: Some(pid),
                birth: crate::platform::process_start_time(pid),
                uid: Some(uid),
            };
            assert!(peer.birth.is_some());
            let (client, mut server) = UnixStream::pair().unwrap();
            let driver = std::thread::spawn(move || {
                read_events(
                    "exited-reader".into(),
                    serde_json::from_value(
                        serde_json::json!({"after":0,"kinds":["message_queued"],"wait_ms":600_000}),
                    )
                    .unwrap(),
                    &mut server,
                    &hub,
                    &Arc::new(AtomicBool::new(true)),
                    &api_tx,
                    peer,
                )
            });
            let hold = tokio::time::timeout(Duration::from_secs(2), api_rx.recv())
                .await
                .unwrap()
                .unwrap();
            app.handle_api_request_message(hold);
            let due = app.waits.next_deadline().expect("admitted hold");
            assert_eq!(visible_agent_phase(&mut app, &agent_id), "waiting_on_event");
            assert!(matches!(
                app.waits.waiting(&agent_id),
                crate::api::agent_state::Waiting::On { .. }
            ));
            use std::io::{BufRead, Write};
            control.get_mut().write_all(b"x").unwrap();
            let mut exited = String::new();
            control.read_line(&mut exited).unwrap();
            assert_eq!(exited, "exited\n", "parent reaped the caller");
            assert!(crate::platform::process_start_time(pid).is_none());
            assert!(app
                .terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .child_pid()
                .is_some());
            drop(client);
            let release = tokio::time::timeout(Duration::from_secs(2), api_rx.recv())
                .await
                .unwrap()
                .unwrap();
            app.handle_api_request_message(release);
            assert!(
                driver.join().unwrap().unwrap().is_none(),
                "hang-up ends the read"
            );
            assert!(
                matches!(
                    app.waits.waiting(&agent_id),
                    crate::api::agent_state::Waiting::None
                ),
                "uid={uid}: exited caller's hold must close"
            );
            assert_ne!(visible_agent_phase(&mut app, &agent_id), "waiting_on_event");
            app.tick_waits(due);
            assert!(!app
                .event_hub
                .events_after(0)
                .iter()
                .any(|(_, event)| event.event == crate::api::schema::EventKind::WaitExpired));
        }
    }
    fn spawn_reader_in_pane(
        app: &mut crate::app::App,
        pane: crate::layout::PaneId,
        terminal_id: &crate::terminal::TerminalId,
    ) -> (
        std::io::BufReader<UnixStream>,
        u32,
        crate::test_socket_dir::SocketDir,
    ) {
        use std::io::BufRead;
        let directory = crate::test_socket_dir::SocketDir::reserve("held-reader");
        let path = directory.path().join("control.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (connected, connection) = std::sync::mpsc::channel();
        let acceptor = std::thread::spawn(move || {
            let _ = connected.send(listener.accept());
        });
        let parent = r#"import socket,subprocess,sys
control=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
control.connect(sys.argv[1])
child=subprocess.Popen([sys.executable,"-c",sys.argv[2],str(control.fileno())],pass_fds=(control.fileno(),))
child.wait()
control.sendall(b"exited\n")
sys.stdin.buffer.read()
"#;
        let child = r#"import os,socket,sys
control=socket.socket(fileno=int(sys.argv[1]))
control.sendall((str(os.getpid())+"\n").encode())
control.recv(1)
"#;
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        let command = format!(
            "exec {} -c {} {} {}",
            quote(&crate::test_support::program_path_string("python3")),
            quote(parent),
            quote(path.to_str().unwrap()),
            quote(child)
        );
        let runtime = crate::terminal::TerminalRuntime::spawn_shell_command(
            pane,
            24,
            80,
            std::env::temp_dir(),
            &command,
            &[],
            4096,
            Default::default(),
            app.event_tx.clone(),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);
        let (control, _) = connection
            .recv_timeout(Duration::from_secs(2))
            .expect("reader connected")
            .unwrap();
        acceptor.join().unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut control = std::io::BufReader::new(control);
        let mut pid = String::new();
        control.read_line(&mut pid).unwrap();
        let pid = pid.trim().parse::<u32>().expect("caller PID");
        (control, pid, directory)
    }

    fn visible_agent_phase(app: &mut crate::app::App, agent: &str) -> String {
        let (respond_to, reply) = std::sync::mpsc::channel();
        let pid = std::process::id();
        app.handle_api_request_message(ApiRequestMessage {
            request: Request {
                id: "observe-held-read".into(),
                method: Method::AgentGet(crate::api::schema::AgentTarget {
                    target: agent.into(),
                }),
            },
            respond_to,
            peer_pid: Some(pid),
            peer_birth: crate::platform::process_start_time(pid),
            peer_uid: Some(7),
        });
        let value: serde_json::Value =
            serde_json::from_str(&reply.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
        value["result"]["agent"]["state"]["phase"]
            .as_str()
            .expect("visible agent phase")
            .into()
    }
}
