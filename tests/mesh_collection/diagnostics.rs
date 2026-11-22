//! Write attribution for this integration test's sandbox database only.
use rusqlite::Connection;
use std::time::Instant;

pub struct WriteTimeline {
    start: Instant,
    last_event: i64,
    last_sample: Option<(String, i64, u64)>,
}

impl WriteTimeline {
    pub fn install(db: &Connection) -> Self {
        let tables = db
            .prepare(
                "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        db.execute_batch(
            "CREATE TABLE test_write_trace (
                sequence INTEGER PRIMARY KEY, operation TEXT, table_name TEXT,
                writer_total_changes INTEGER, old_values TEXT, new_values TEXT);",
        )
        .unwrap();
        for table in tables {
            let columns = db
                .prepare("SELECT name FROM pragma_table_info(?1)")
                .unwrap()
                .query_map([&table], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let values = |row: &str| {
                let fields = columns
                    .iter()
                    // Payloads and key material add noise, not write attribution.
                    .filter(|c| {
                        !matches!(
                            c.as_str(),
                            "body" | "metadata" | "fingerprint" | "secret" | "public_key"
                        )
                    })
                    .map(|c| {
                        format!(
                            "'{}',quote({row}.\"{}\")",
                            c.replace('\'', "''"),
                            c.replace('"', "\"\"")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!("json_object({fields})")
            };
            for operation in ["INSERT", "UPDATE", "DELETE"] {
                let old = if operation == "INSERT" {
                    "NULL".into()
                } else {
                    values("OLD")
                };
                let new = if operation == "DELETE" {
                    "NULL".into()
                } else {
                    values("NEW")
                };
                let identifier = table.replace('"', "\"\"");
                let literal = table.replace('\'', "''");
                db.execute_batch(&format!(
                    "CREATE TRIGGER \"test_trace_{operation}_{identifier}\" AFTER {operation} ON \"{identifier}\"
                     BEGIN INSERT INTO test_write_trace(operation,table_name,writer_total_changes,old_values,new_values)
                     VALUES('{operation}','{literal}',total_changes(),{old},{new}); END;"
                )).unwrap();
            }
        }
        Self {
            start: Instant::now(),
            last_event: 0,
            last_sample: None,
        }
    }

    #[expect(clippy::print_stderr, reason = "captured nextest failure diagnostics")]
    pub fn sample(&mut self, db: &Connection, phase: &str) -> i64 {
        // One read snapshot groups visible writes with the observed data_version.
        // data_version is an observer counter, not an exact count of commits.
        // Writer total_changes is connection-local and includes trace inserts.
        db.execute_batch("BEGIN DEFERRED").unwrap();
        let version = db
            .pragma_query_value(None, "data_version", |r| r.get::<_, i64>(0))
            .unwrap();
        let sample = (phase.to_owned(), version, db.total_changes());
        if self.last_sample.as_ref() != Some(&sample) {
            eprintln!("mesh write timeline +{:?}: phase={phase}, data_version={version}, observer_total_changes={}", self.start.elapsed(), sample.2);
            self.last_sample = Some(sample);
        }
        let mut statement = db.prepare("SELECT sequence,operation,table_name,writer_total_changes,old_values,new_values FROM test_write_trace WHERE sequence>?1 ORDER BY sequence").unwrap();
        let rows = statement
            .query_map([self.last_event], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (sequence, operation, table, changes, old, new) = row.unwrap();
            eprintln!("  committed write #{sequence}: {operation} {table}, writer_total_changes={changes}, old={old:?}, new={new:?}");
            self.last_event = sequence;
        }
        db.execute_batch("COMMIT").unwrap();
        version
    }
}

#[test]
fn timeline_attributes_committed_writes_and_excludes_rollbacks() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "flock-write-trace-{}-{unique}.sqlite",
        std::process::id()
    ));
    let observer = Connection::open(&path).unwrap();
    observer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE envelopes (state TEXT, lease_until INTEGER); INSERT INTO envelopes VALUES('custody',1);").unwrap();
    let mut timeline = WriteTimeline::install(&observer);
    let initial = timeline.sample(&observer, "baseline");
    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch("BEGIN; UPDATE envelopes SET state='held',lease_until=0; COMMIT;")
        .unwrap();
    assert_ne!(timeline.sample(&observer, "held"), initial);
    assert_eq!(timeline.last_event, 1);
    let event: (String, String, i64, String, String) = observer.query_row("SELECT operation,table_name,writer_total_changes,old_values,new_values FROM test_write_trace", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(event.0, "UPDATE");
    assert_eq!(event.1, "envelopes");
    assert_eq!(event.2, 0);
    assert!(event.3.contains("custody"));
    assert!(event.4.contains("held"));
    writer
        .execute_batch("BEGIN; UPDATE envelopes SET state='custody'; ROLLBACK;")
        .unwrap();
    timeline.sample(&observer, "rolled back");
    assert_eq!(timeline.last_event, 1);
    drop(writer);
    drop(observer);
    std::fs::remove_file(path).unwrap();
}
