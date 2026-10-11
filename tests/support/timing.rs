//! Per-test phase timing for the multi-process harness (#992).
//!
//! nextest runs each test in its own process, so process-global accumulators
//! are per-test accumulators. Harness helpers open a [`Span`] around the work
//! they do for a test: starting servers, waiting for their sockets, client
//! handshakes, route convergence, labelled polls, teardown. When the process
//! exits, one line goes to stderr:
//!
//! ```text
//! FLOCK_TEST_TIMING {"binary":"mesh_step1","test":"...","total_ms":...,"phases":{...},"waits":{...}}
//! ```
//!
//! and, when `FLOCK_TEST_TIMING_FILE` is set, the same JSON is appended to that
//! file. nextest captures stderr of passing tests, so the file is how a whole
//! run is collected. Nothing here changes what a test does.
//!
//! Only the outermost span on a thread is counted, so a socket wait inside a
//! server start is charged to the start and never twice. `total_ms` runs from
//! the first harness call to process exit, which includes the exit-time
//! cleanup sweep.

#![allow(dead_code)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

/// What a span is spent on. `Wait` is a labelled poll that is not already
/// inside another phase, usually a test body's own assertion wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Starting a server process and waiting until it answers.
    Spawn,
    /// Restarting a server mid-test, which is the test's own work.
    Respawn,
    /// Client handshakes and fixture workspaces.
    Handshake,
    /// Waiting for a mesh route or enrollment to appear.
    Converge,
    /// Stopping servers and removing sandboxes.
    Teardown,
}

impl Phase {
    fn key(self) -> &'static str {
        match self {
            Phase::Spawn => "spawn",
            Phase::Respawn => "respawn",
            Phase::Handshake => "handshake",
            Phase::Converge => "converge",
            Phase::Teardown => "teardown",
        }
    }
}

#[derive(Default)]
struct Totals {
    phases: BTreeMap<&'static str, Duration>,
    waits: BTreeMap<String, Duration>,
}

static START: OnceLock<Instant> = OnceLock::new();
static TOTALS: OnceLock<Mutex<Totals>> = OnceLock::new();
static REGISTER: Once = Once::new();

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Start the clock and register the exit-time report. The cleanup hooks call
/// this before registering their own `atexit`, and `atexit` runs in reverse,
/// so the report is written after that cleanup has been timed.
pub fn init() {
    START.get_or_init(Instant::now);
    REGISTER.call_once(|| unsafe {
        libc::atexit(report_at_exit);
    });
}

/// An open span. Dropping it charges the elapsed time when it is outermost.
pub struct Span {
    started: Instant,
    target: Option<Target>,
}

enum Target {
    Phase(Phase),
    Wait(String),
}

impl Drop for Span {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        let Some(target) = self.target.take() else {
            return;
        };
        let elapsed = self.started.elapsed();
        let mut totals = totals();
        match target {
            Target::Phase(phase) => *totals.phases.entry(phase.key()).or_default() += elapsed,
            Target::Wait(label) => *totals.waits.entry(label).or_default() += elapsed,
        }
    }
}

fn open(target: Target) -> Span {
    init();
    let outermost = DEPTH.with(|depth| {
        let current = depth.get();
        depth.set(current + 1);
        current == 0
    });
    Span {
        started: Instant::now(),
        target: outermost.then_some(target),
    }
}

/// Time a harness phase until the returned span drops.
pub fn phase(phase: Phase) -> Span {
    open(Target::Phase(phase))
}

/// Time a labelled poll until the returned span drops.
pub fn wait(label: &str) -> Span {
    open(Target::Wait(label.to_owned()))
}

fn totals() -> std::sync::MutexGuard<'static, Totals> {
    TOTALS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The test binary's crate name: `mesh_step1-0123abcd` becomes `mesh_step1`.
fn binary_name() -> String {
    let stem = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    match stem.rsplit_once('-') {
        Some((name, hash)) if hash.chars().all(|c| c.is_ascii_hexdigit()) => name.to_owned(),
        _ => stem,
    }
}

/// nextest runs `<binary> --exact <name> --nocapture`, so the one argument
/// that is not a flag names the test.
fn test_name() -> String {
    std::env::args()
        .skip(1)
        .find(|arg| !arg.starts_with('-'))
        .unwrap_or_default()
}

fn ms(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

extern "C" fn report_at_exit() {
    let Some(start) = START.get() else {
        return;
    };
    let total = start.elapsed();
    let totals = totals();
    let phases: serde_json::Map<String, serde_json::Value> = totals
        .phases
        .iter()
        .map(|(key, duration)| ((*key).to_owned(), ms(*duration).into()))
        .collect();
    let waits: serde_json::Map<String, serde_json::Value> = totals
        .waits
        .iter()
        .map(|(label, duration)| (label.clone(), ms(*duration).into()))
        .collect();
    let line = serde_json::json!({
        "binary": binary_name(),
        "test": test_name(),
        "total_ms": ms(total),
        "phases": phases,
        "waits": waits,
    })
    .to_string();
    let _ = writeln!(std::io::stderr(), "FLOCK_TEST_TIMING {line}");
    if let Some(path) = std::env::var_os("FLOCK_TEST_TIMING_FILE") {
        // One short append per process. O_APPEND keeps concurrent writers'
        // lines whole.
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}
