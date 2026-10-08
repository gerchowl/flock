//! #627: drive the delegate CLI through fake pane and System One transports.
// The test owns subprocesses, rather than shipped process-execution paths.
#![allow(clippy::disallowed_methods)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Pane {
    base: PathBuf,
    registry: PathBuf,
    socket: PathBuf,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Pane {
    fn new(status: &str, screen: &str) -> Self {
        Self::frames(vec![(status.to_string(), screen.to_string())])
    }

    fn frames(frames: Vec<(String, String)>) -> Self {
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(format!("delegate-verdict-{suffix}"));
        let socket = std::env::temp_dir().join(format!("dv-{suffix}.sock"));
        std::fs::create_dir_all(&base).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let counted = calls.clone();

        let worker = thread::spawn(move || {
            let mut frame = 0usize;
            let mut sampled = 0usize;
            while !stopped.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                // Accepted sockets can inherit the listener's nonblocking mode.
                // A deadline can also close the client before setup completes.
                if stream.set_nonblocking(false).is_err()
                    || stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .is_err()
                {
                    continue;
                }
                let mut line = String::new();
                if BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .is_err()
                {
                    continue;
                }
                let Ok(request) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                let result = match request["method"].as_str().unwrap() {
                    "agent.get" => {
                        frame = sampled.min(frames.len() - 1);
                        sampled += 1;
                        let status = &frames[frame].0;
                        let cursor = if status == "idle" {
                            "term_fixture:0:1:2:i"
                        } else {
                            "term_fixture:0:1:1:w"
                        };
                        json!({"agent": {"name": "fixture", "terminal_id": "term_fixture", "pane_id": "ws_fixture:p1",
                        "agent_status": status, "turn_cursor": cursor, "revision": 0}})
                    }
                    "pane.read" => {
                        assert_eq!(request["params"]["source"], "detection");
                        json!({"read": {"text": frames[frame].1}})
                    }
                    "agent.result" => {
                        json!({"result": {"finished": true, "at_ms": 2, "status": "done", "text": "DONE: recovered"}})
                    }
                    "workspace.get" => {
                        let _ = writeln!(
                            stream,
                            "{}",
                            json!({"id": request["id"], "error": {"code": "workspace_not_found", "message": "fixture already gone"}})
                        );
                        // Error replies must also outlive the client read.
                        let _ = stream.read(&mut [0_u8; 1]);
                        continue;
                    }
                    other => panic!("unexpected request: {other}"),
                };
                let _ = writeln!(stream, "{}", json!({"id": request["id"], "result": result}));
                // Keep the response socket alive until the client finishes its read.
                let _ = stream.read(&mut [0_u8; 1]);
            }
        });
        let hash = socket
            .to_string_lossy()
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        let app = if cfg!(debug_assertions) {
            "flock-dev"
        } else {
            "flock"
        };
        let registry = base
            .join("state")
            .join(app)
            .join("delegates")
            .join(format!("{hash:016x}"));
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(registry.join("fixture.json"), json!({
            "name": "fixture", "terminal_id": "term_fixture", "pane_id": "ws_fixture:p1", "root_pane": "ws_fixture:p0",
            "workspace_id": "ws_fixture", "mode": "cwd", "worktree": null, "branch": null,
            "harness": "opencode", "model": null, "round": 1, "brief": "fixture.md", "submitted_at_ms": 1,
            "cursor": "term_fixture:0:0:0:i", "created_at_ms": 1,
        }).to_string()).unwrap();
        Self {
            registry,
            base,
            socket,
            calls,
            stop,
            worker: Some(worker),
        }
    }

    fn command(&self, args: &[&str], s1: Option<&str>) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_flk"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("FLOCK_") {
                command.env_remove(key);
            }
        }
        command
            .args(args)
            .env("HOME", &self.base)
            .env("XDG_STATE_HOME", self.base.join("state"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("FLOCK_SOCKET_PATH", &self.socket)
            .env_remove("SYSTEMONE_URL")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(url) = s1 {
            command.env("SYSTEMONE_URL", url);
        }
        command
    }

    fn json(output: &Output) -> Value {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
            panic!(
                "{err}: {:?}; stderr {}",
                output.stdout,
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct S1 {
    url: String,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl S1 {
    fn new(status: u16) -> Self {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, captured, stopped) = (calls.clone(), bodies.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let mut length = 0;
                loop {
                    line.clear();
                    assert_ne!(
                        reader.read_line(&mut line).unwrap(),
                        0,
                        "HTTP request ended before headers"
                    );
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                let mut answers = serde_json::Map::new();
                for (name, question) in body["questions"].as_object().unwrap() {
                    answers.insert(name.clone(), if question["type"] == "choice" {
                        json!({"probabilities": {"failure": 0.9, "partial": 0.1, "success": 0.0, "info": 0.0}})
                    } else { json!({"noul": 0.9}) });
                }
                captured.lock().unwrap().push(body);
                count.fetch_add(1, Ordering::SeqCst);
                let response = json!({"answers": answers}).to_string();
                let _ = write!(stream, "HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len());
            }
        });
        Self {
            url,
            calls,
            bodies,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for S1 {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[test]
fn delegate_silence_stalls_a_working_pane_and_status_remembers_the_verdict() {
    let pane = Pane::new("working", "tool has stopped changing\n\n");
    let out = pane
        .command(
            &[
                "delegate",
                "wait",
                "fixture",
                "--silence",
                "100ms",
                "--timeout",
                "5000",
                "--json",
            ],
            None,
        )
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = Pane::json(&out);
    assert_eq!(value["outcome"], "stalled");
    assert_eq!(value["reason"], "silence");
    assert_eq!(value["last_line"], "tool has stopped changing");
    assert!(value["s1"].is_null());
    let status = pane
        .command(&["delegate", "status", "fixture", "--json"], None)
        .output()
        .unwrap();
    assert_eq!(Pane::json(&status)["verdict"], "stalled");
}

#[test]
fn delegate_long_provider_retry_stalls_with_silence_disabled() {
    let pane = Pane::new("working", "■■⬝⬝⬝⬝⬝⬝ Free usage exceeded, subscribe to Go [retrying in 46m 15s attempt #1] esc interrupt");
    let out = pane
        .command(
            &[
                "delegate",
                "wait",
                "fixture",
                "--silence",
                "0",
                "--timeout",
                "5000",
                "--json",
            ],
            None,
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));
    let value = Pane::json(&out);
    assert_eq!(value["verdict"], "provider_limit");
    assert_eq!(value["reason"], "provider_limit");
    assert_eq!(value["retry_after_ms"], 2_775_000);
}

#[test]
fn delegate_ready_timeout_names_the_prompt_and_last_line_without_typing() {
    let pane = Pane::new("blocked", "Choose an option\nenter confirm\n\n");
    let brief = pane.base.join("task.md");
    std::fs::write(&brief, "fixture task").unwrap();
    let out = pane
        .command(
            &[
                "delegate",
                "send",
                "fixture",
                "--brief",
                brief.to_str().unwrap(),
                "--ready-timeout",
                "1000",
                "--json",
            ],
            None,
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let value = Pane::json(&out);
    assert_eq!(value["outcome"], "ready_timeout");
    assert_eq!(value["verdict"], "waiting_on_input");
    assert_eq!(value["last_line"], "enter confirm");
}

#[test]
fn delegate_s1_is_called_once_at_round_end_and_never_on_polls() {
    let pane = Pane::new("working", "still running");
    let s1 = S1::new(200);
    let child = pane
        .command(
            &[
                "delegate",
                "wait",
                "fixture",
                "--silence",
                "0",
                "--timeout",
                "5000",
                "--json",
            ],
            Some(&s1.url),
        )
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while pane.calls.load(Ordering::SeqCst) < 5 {
        assert!(
            std::time::Instant::now() < deadline,
            "pane polls never arrived"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(s1.calls.load(Ordering::SeqCst), 0);
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(124),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = Pane::json(&out);
    assert!(value["s1"]["fused"].as_f64().unwrap() >= 0.5);
    assert_eq!(s1.calls.load(Ordering::SeqCst), 1);
    let body = &s1.bodies.lock().unwrap()[0];
    assert_eq!(body["questions"].as_object().unwrap().len(), 10);
    assert!(body["state"].as_str().unwrap().ends_with("still running"));
}

#[test]
fn delegate_s1_failure_is_advisory_and_its_breaker_is_per_endpoint() {
    let pane = Pane::new("working", "same screen");
    let args = [
        "delegate",
        "wait",
        "fixture",
        "--silence",
        "100ms",
        "--timeout",
        "5000",
        "--json",
    ];
    let baseline = pane.command(&args, None).output().unwrap();
    let s1 = S1::new(503);
    for _ in 0..4 {
        let out = pane.command(&args, Some(&s1.url)).output().unwrap();
        assert_eq!(out.status.code(), baseline.status.code());
        assert_eq!(Pane::json(&out), Pane::json(&baseline));
    }
    assert_eq!(s1.calls.load(Ordering::SeqCst), 3);
    let healthy = S1::new(200);
    let out = pane
        .command(&args, Some(&format!("{}, {}", s1.url, healthy.url)))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));
    assert!(Pane::json(&out)["s1"].is_object());
    assert_eq!(healthy.calls.load(Ordering::SeqCst), 1);
    assert_eq!(s1.calls.load(Ordering::SeqCst), 3);
}

#[test]
fn delegate_short_provider_retry_recovers_and_settles_normally() {
    let retry = "■■⬝⬝⬝⬝⬝⬝ rate limit [retrying in 2s attempt #1] esc interrupt";
    let pane = Pane::frames(vec![
        ("working".into(), retry.into()),
        ("working".into(), retry.into()),
        ("idle".into(), "DONE: recovered".into()),
    ]);
    let out = pane
        .command(
            &[
                "delegate",
                "wait",
                "fixture",
                "--silence",
                "0",
                "--settle",
                "0",
                "--timeout",
                "5000",
                "--json",
            ],
            None,
        )
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(Pane::json(&out)["outcome"], "done");
}

#[test]
fn delegate_short_provider_retry_persisting_outlasts_silence() {
    let pane = Pane::new(
        "working",
        "■■⬝⬝⬝⬝⬝⬝ rate limit [retrying in 2s attempt #1] esc interrupt",
    );
    let out = pane
        .command(
            &[
                "delegate",
                "wait",
                "fixture",
                "--silence",
                "3s",
                "--timeout",
                "8000",
                "--json",
            ],
            None,
        )
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(Pane::json(&out)["reason"], "provider_limit");
}

#[test]
fn delegate_spinner_and_elapsed_changes_do_not_reset_silence() {
    // A changed-screen bug reaches idle after these polls instead of relying
    // on a wall-clock command timeout to distinguish it from silence.
    let pane = Pane::frames(
        (0..5)
            .map(|i| {
                (
                    "working".into(),
                    format!("tool waiting\n• Working ({i}s • esc to interrupt)"),
                )
            })
            .chain(std::iter::once(("idle".into(), "DONE: recovered".into())))
            .collect(),
    );
    let out = pane
        .command(
            &[
                "delegate",
                "wait",
                "fixture",
                "--silence",
                // The settled loop waits 200ms between samples.
                "1ms",
                "--timeout",
                "30000",
                "--json",
            ],
            None,
        )
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(Pane::json(&out)["reason"], "silence");
}

#[test]
fn delegate_new_transcript_lines_reset_silence() {
    // Every working sample advances the transcript, then a bounded number
    // of polls reaches idle. Scheduling delays cannot exhaust the frames.
    let pane = Pane::frames(
        (0..5)
            .map(|i| {
                (
                    "working".into(),
                    format!("new transcript line {i}\n• Working ({i}s • esc to interrupt)"),
                )
            })
            .chain(std::iter::once(("idle".into(), "DONE: recovered".into())))
            .collect(),
    );
    let out = pane
        .command(
            &[
                "delegate",
                "wait",
                "fixture",
                "--silence",
                "1ms",
                "--timeout",
                "30000",
                "--json",
            ],
            None,
        )
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(Pane::json(&out)["outcome"], "done");
}

#[test]
fn delegate_workspace_error_waits_for_complete_request_and_client_close() {
    let pane = Pane::new("working", "fixture");
    let mut stream = std::os::unix::net::UnixStream::connect(&pane.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    write!(
        stream,
        "{}",
        json!({"id": "fixture", "method": "workspace.get", "params": {"workspace_id": "ws_fixture"}})
    )
    .unwrap();
    let error = stream.read(&mut [0_u8; 1]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    writeln!(stream).unwrap();
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["error"]["code"], "workspace_not_found");
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let error = reader.read(&mut [0_u8; 1]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
}

#[test]
fn delegate_reap_removes_persisted_verdict() {
    let pane = Pane::new("working", "fixture");
    let verdict = pane.registry.join("fixture.verdict.json");
    std::fs::write(&verdict, "{}").unwrap();
    let out = pane
        .command(&["delegate", "reap", "fixture", "--json"], None)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!verdict.exists());
    assert!(!pane.registry.join("fixture.json").exists());
}
