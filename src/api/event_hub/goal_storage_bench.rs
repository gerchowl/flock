//! ADR-0031's measurement (#1033): goal state as a fold over the durable
//! event log vs a SQLite table, on the Stop-hook hot path.
//!
//! Ignored by default. Run it in release, on the machine you want numbers for:
//!
//! ```text
//! cargo test --release --bin flk goal_storage_bench -- --ignored --nocapture
//! ```
//!
//! `FLOCK_BENCH_EVENTS_PER_FILE` sizes each of the five log files (four
//! rotated plus the active one), default 100000, the event-count rotation
//! bound. The goal events are stand-ins: an existing persisted kind carrying
//! a verdict-sized payload, since the `Goal*` kinds do not exist yet.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{EventHub, PersistedEvent, MAX_ROTATED};
use crate::api::schema::{EventData, EventEnvelope, EventKind};

const GOAL_AGENT: &str = "goal:agent_bench_long";
const VERDICTS: usize = 400;
/// The active file stops this many events short of a full one, so the
/// measured verdict writes never cross the 32 MiB byte bound and no rotation
/// lands inside the write samples.
const ACTIVE_HEADROOM: usize = 2_000;

fn envelope(payload: String) -> EventEnvelope {
    EventEnvelope {
        event: EventKind::WorkspaceClosed,
        data: EventData::WorkspaceClosed {
            workspace_id: payload,
        },
    }
}

/// A verdict-sized payload: the scores, thresholds version and turn cursor
/// G3c records, about 400 bytes per log line.
fn verdict_payload(agent: &str, index: usize) -> String {
    let scores: Vec<String> = (0..24)
        .map(|q| format!("q{q}=0.{:03}", (index * 7 + q) % 1000))
        .collect();
    format!(
        "{agent}|turn={index}|thresholds=v3|verdict=continue|{}",
        scores.join(",")
    )
}

/// The live record a fold keeps per agent: counters plus the newest verdict,
/// which is what `goal.status` returns. The verdict history stays in the log.
#[derive(Clone, Default, serde::Serialize)]
struct GoalRecord {
    verdicts: u64,
    last_seq: u64,
    last_verdict: String,
}

fn fold<'a>(events: impl Iterator<Item = (u64, &'a EventEnvelope)>) -> HashMap<String, GoalRecord> {
    let mut goals: HashMap<String, GoalRecord> = HashMap::new();
    for (seq, envelope) in events {
        if let EventData::WorkspaceClosed { workspace_id } = &envelope.data {
            if let Some((agent, _)) = workspace_id.split_once('|') {
                if agent.starts_with("goal:") {
                    let record = goals.entry(agent.to_string()).or_default();
                    record.verdicts += 1;
                    record.last_seq = seq;
                    record.last_verdict.clone_from(workspace_id);
                }
            }
        }
    }
    goals
}

fn rotated(path: &Path, index: usize) -> PathBuf {
    path.with_file_name(format!("event-log.{index}.jsonl"))
}

/// Write four full rotated files and a nearly full active one directly (no
/// fsync per line): filler lines of about 200 bytes, a little above a live
/// log's average, with the goal's verdicts spread evenly across all of them.
fn seed_log(path: &Path, per_file: usize) -> u64 {
    let total = per_file * (MAX_ROTATED + 1) - ACTIVE_HEADROOM;
    let every = total / VERDICTS;
    let mut seq = 0u64;
    let mut verdict = 0usize;
    for file_index in (0..=MAX_ROTATED).rev() {
        let target = if file_index == 0 {
            path.to_path_buf()
        } else {
            rotated(path, file_index)
        };
        let mut out = std::io::BufWriter::new(std::fs::File::create(&target).unwrap());
        let lines = if file_index == 0 {
            per_file - ACTIVE_HEADROOM
        } else {
            per_file
        };
        for _ in 0..lines {
            seq += 1;
            let payload = if (seq as usize).is_multiple_of(every) && verdict < VERDICTS {
                verdict += 1;
                verdict_payload(GOAL_AGENT, verdict)
            } else {
                format!("w{seq:x}-filler-{}", "x".repeat(60))
            };
            let line = PersistedEvent {
                seq,
                ts_ms: 1_791_751_125_296 + seq,
                envelope: envelope(payload),
            };
            serde_json::to_writer(&mut out, &line).unwrap();
            out.write_all(b"\n").unwrap();
        }
        out.flush().unwrap();
    }
    seq
}

fn pcts(label: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let at = |q: usize| samples[(samples.len() * q / 100).min(samples.len() - 1)];
    let report = format!(
        "{label:<44} n={:<6} p50={:>10.3?} p99={:>10.3?} max={:>10.3?}",
        samples.len(),
        at(50),
        at(99),
        samples[samples.len() - 1]
    );
    eprintln!("{report}"); // guardrails-ok: the benchmark's report is its output
}

fn time<T>(f: impl FnOnce() -> T) -> (Duration, T) {
    let start = Instant::now();
    let value = f();
    (start.elapsed(), value)
}

#[test]
#[ignore = "benchmark for ADR-0031; run by hand in release"]
fn goal_storage_bench() {
    let per_file = std::env::var("FLOCK_BENCH_EVENTS_PER_FILE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100_000);
    let dir = std::env::temp_dir().join(format!("flock-goal-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("event-log.jsonl");
    let events = seed_log(&path, per_file);
    let bytes: u64 = (0..=MAX_ROTATED)
        .map(|i| {
            if i == 0 {
                path.clone()
            } else {
                rotated(&path, i)
            }
        })
        .map(|file| std::fs::metadata(file).map(|meta| meta.len()).unwrap_or(0))
        .sum();
    let report = format!(
        "log: {events} events in 5 files, {:.1} MiB, {VERDICTS} verdicts for one goal",
        bytes as f64 / 1048576.0
    );
    eprintln!("{report}"); // guardrails-ok: the benchmark's report is its output

    // (a) Event fold.
    let mut starts = Vec::new();
    let mut folds = Vec::new();
    let mut hub = EventHub::default();
    let mut goals = HashMap::new();
    for _ in 0..5 {
        let (open, opened) = time(|| EventHub::with_persistence(path.clone()));
        let (fold_cost, folded) = time(|| {
            let restored = opened.persisted_events_after(0);
            fold(restored.iter().map(|(seq, _, envelope)| (*seq, envelope)))
        });
        starts.push(open);
        folds.push(fold_cost);
        hub = opened;
        goals = folded;
    }
    assert_eq!(
        goals.get(GOAL_AGENT).map(|record| record.verdicts),
        Some(VERDICTS as u64)
    );
    pcts("fold: cold start, existing hub open", starts);
    pcts("fold: cold start, extra full-log fold", folds);

    let live = std::sync::Mutex::new(goals);
    let reads: Vec<Duration> = (0..10_000)
        .map(|_| {
            time(|| {
                let record = live.lock().unwrap().get(GOAL_AGENT).cloned();
                serde_json::to_vec(&record).unwrap()
            })
            .0
        })
        .collect();
    pcts("fold: goal.status, in-memory record", reads);

    let rescans: Vec<Duration> = (0..5)
        .map(|_| {
            time(|| {
                let restored = hub.persisted_events_after(0);
                fold(restored.iter().map(|(seq, _, envelope)| (*seq, envelope)))
            })
            .0
        })
        .collect();
    pcts("fold: goal.status, re-fold from disk", rescans);

    // The fold alone, over a log another startup pass has already read.
    let restored = hub.persisted_events_after(0);
    let shared: Vec<Duration> = (0..20)
        .map(|_| time(|| fold(restored.iter().map(|(seq, _, envelope)| (*seq, envelope)))).0)
        .collect();
    drop(restored);
    pcts("fold: cold start, fold in a shared pass", shared);

    let newest_rotated = std::fs::metadata(rotated(&path, 1)).map(|meta| meta.len());
    let writes: Vec<Duration> = (0..VERDICTS)
        .map(|index| {
            let event = envelope(verdict_payload(GOAL_AGENT, VERDICTS + index));
            time(|| {
                hub.push(event.clone());
                if let Some(record) = live.lock().unwrap().get_mut(GOAL_AGENT) {
                    record.verdicts += 1;
                    record.last_verdict = verdict_payload(GOAL_AGENT, index);
                }
            })
            .0
        })
        .collect();
    assert_eq!(
        std::fs::metadata(rotated(&path, 1))
            .map(|meta| meta.len())
            .ok(),
        newest_rotated.ok(),
        "a rotation landed inside the write samples"
    );
    pcts("fold: verdict write (append + sync_data)", writes);

    // (b) SQLite, the mesh store's pragmas (src/mesh/store.rs).
    let db = dir.join("goals.sqlite");
    let open = |fullfsync: bool| {
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch(&format!(
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync={};
                 CREATE TABLE IF NOT EXISTS goals (agent TEXT PRIMARY KEY, verdicts INTEGER NOT NULL, last_verdict TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS verdicts (id INTEGER PRIMARY KEY, agent TEXT NOT NULL, ts_ms INTEGER NOT NULL, body TEXT NOT NULL);",
                if fullfsync { "ON" } else { "OFF" }
            ))
            .unwrap();
        connection
    };
    let write = |connection: &mut rusqlite::Connection, index: usize| {
        let body = verdict_payload(GOAL_AGENT, index);
        let tx = connection.transaction().unwrap();
        tx.execute(
            "INSERT INTO verdicts (agent, ts_ms, body) VALUES (?1, ?2, ?3)",
            rusqlite::params![GOAL_AGENT, index as i64, body],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO goals (agent, verdicts, last_verdict) VALUES (?1, 1, ?2)
             ON CONFLICT(agent) DO UPDATE SET verdicts = verdicts + 1, last_verdict = excluded.last_verdict",
            rusqlite::params![GOAL_AGENT, body],
        )
        .unwrap();
        tx.commit().unwrap();
    };
    let read = |connection: &rusqlite::Connection| {
        let mut statement = connection
            .prepare_cached("SELECT verdicts, last_verdict FROM goals WHERE agent = ?1")
            .unwrap();
        let record = statement
            .query_row([GOAL_AGENT], |row| {
                Ok(GoalRecord {
                    verdicts: row.get::<_, i64>(0)? as u64,
                    last_seq: 0,
                    last_verdict: row.get(1)?,
                })
            })
            .unwrap();
        serde_json::to_vec(&record).unwrap()
    };
    {
        let mut connection = open(false);
        let tx = connection.transaction().unwrap();
        for index in 0..VERDICTS {
            tx.execute(
                "INSERT INTO verdicts (agent, ts_ms, body) VALUES (?1, ?2, ?3)",
                rusqlite::params![GOAL_AGENT, index as i64, verdict_payload(GOAL_AGENT, index)],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO goals (agent, verdicts, last_verdict) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                GOAL_AGENT,
                VERDICTS as i64,
                verdict_payload(GOAL_AGENT, VERDICTS)
            ],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let colds: Vec<Duration> = (0..20).map(|_| time(|| read(&open(false))).0).collect();
    pcts("sqlite: cold start, open + first status", colds);
    let connection = open(false);
    let reads: Vec<Duration> = (0..10_000).map(|_| time(|| read(&connection)).0).collect();
    pcts("sqlite: goal.status, warm connection", reads);
    drop(connection);
    for fullfsync in [false, true] {
        let mut connection = open(fullfsync);
        let writes: Vec<Duration> = (0..VERDICTS)
            .map(|index| time(|| write(&mut connection, VERDICTS + index)).0)
            .collect();
        pcts(
            if fullfsync {
                "sqlite: verdict write, fullfsync=ON"
            } else {
                "sqlite: verdict write, mesh pragmas"
            },
            writes,
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
