//! Transactional schema upgrades, including the original unversioned store.
use super::{Connection, Error, Result, TransactionBehavior};

pub(super) const VERSION: i64 = 12;

pub(super) fn check_version(connection: &Connection) -> Result<()> {
    let found: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if found > VERSION {
        return Err(Error::NewerSchema {
            found,
            supported: VERSION,
        });
    }
    Ok(())
}

pub(super) fn migrate(
    connection: &mut Connection,
    directory: &std::path::Path,
    wall_ms: i64,
) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_version(&tx)?;
    tx.execute_batch(LOCAL_RECIPIENTS)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS mesh_meta (name TEXT PRIMARY KEY, value INTEGER NOT NULL);",
    )?;
    let mut version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let existing: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='envelopes')",
        [],
        |r| r.get(0),
    )?;
    if version == 0 && !existing {
        tx.execute_batch(BASELINE)?;
        ensure_signer(&tx)?;
        tx.pragma_update(None, "user_version", VERSION)?;
        tx.commit()?;
        return Ok(());
    }
    if version == 0 {
        upgrade(&tx, BASE)?;
        version = 1;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 1 {
        upgrade(&tx, ACCOUNTING_AND_CLAIMS)?;
        version = 2;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 2 && !has_column(&tx, "identity_pins", "source")? {
        upgrade(
            &tx,
            r#"
            ALTER TABLE identity_pins RENAME TO legacy_identity_pins;
            CREATE TABLE IF NOT EXISTS identity_pins (
                source TEXT NOT NULL, peer TEXT NOT NULL, node_id TEXT NOT NULL,
                public_key BLOB NOT NULL, PRIMARY KEY(source,peer),
                UNIQUE(source,node_id), UNIQUE(source,public_key));
            INSERT INTO identity_pins SELECT 'configured',peer,node_id,public_key
                FROM legacy_identity_pins;
            DROP TABLE IF EXISTS legacy_identity_pins;
        "#,
        )?;
        version = 3;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 2 {
        version = 3;
    }
    if version == 3 {
        let origin = if has_column(&tx, "identity_pins", "origin")? {
            "origin"
        } else {
            "'unknown'"
        };
        upgrade(
            &tx,
            &format!(
                r#"
            ALTER TABLE identity_pins RENAME TO directional_identity_pins;
            CREATE TABLE IF NOT EXISTS identity_pins (
                source TEXT NOT NULL, peer TEXT NOT NULL, node_id TEXT NOT NULL,
                public_key BLOB NOT NULL, origin TEXT NOT NULL DEFAULT 'unknown', PRIMARY KEY(source,peer));
            INSERT INTO identity_pins SELECT source,peer,node_id,public_key,{origin} FROM directional_identity_pins;
            DROP TABLE IF EXISTS directional_identity_pins;
            CREATE UNIQUE INDEX IF NOT EXISTS inbound_node_name ON identity_pins(node_id) WHERE source='inbound';
            CREATE UNIQUE INDEX IF NOT EXISTS inbound_key_name ON identity_pins(public_key) WHERE source='inbound';
        "#
            ),
        )?;
        version = 4;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 4 {
        upgrade(
            &tx,
            "ALTER TABLE identity_pins ADD COLUMN origin TEXT NOT NULL DEFAULT 'unknown';",
        )?;
        version = 5;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 5 {
        upgrade(
            &tx,
            "CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY);",
        )?;
        version = 6;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 6 {
        upgrade(&tx, "CREATE TABLE IF NOT EXISTS writer_generation (singleton INTEGER PRIMARY KEY CHECK(singleton=1), generation INTEGER NOT NULL); INSERT OR IGNORE INTO writer_generation VALUES(1,0);")?;
        version = 7;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 7 {
        upgrade(&tx,
            "ALTER TABLE envelopes ADD COLUMN collect_at INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE envelopes ADD COLUMN collect_attempts INTEGER NOT NULL DEFAULT 0;
            CREATE INDEX IF NOT EXISTS collect_ready ON envelopes(origin,collect_at,custody_deadline);",
        )?;
        version = 8;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 8 {
        upgrade(&tx,
            "DROP INDEX IF EXISTS request_key;
             DROP INDEX IF EXISTS collection_token;
             ALTER TABLE envelopes ADD COLUMN collection_token TEXT;
             ALTER TABLE envelopes ADD COLUMN request_origin TEXT;
             ALTER TABLE envelopes ADD COLUMN request_id TEXT;
             ALTER TABLE envelopes ADD COLUMN recipient_node TEXT NOT NULL DEFAULT '';
             ALTER TABLE envelopes ADD COLUMN reply_expected INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE envelopes ADD COLUMN collect_done INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE envelopes ADD COLUMN collect_failures INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE envelopes ADD COLUMN collect_error TEXT;
             UPDATE envelopes SET
               collection_token=json_extract(metadata,'$.return_binding.collection_token'),
               request_origin=json_extract(metadata,'$.request_key.origin_node'),
               request_id=json_extract(metadata,'$.request_key.message_id'),
               recipient_node=COALESCE(json_extract(metadata,'$.return_binding.recipient_node'),''),
               reply_expected=json_extract(metadata,'$.intent') IN ('\"needs_reply\"','\"blocking\"')
               WHERE json_valid(metadata);
             CREATE INDEX IF NOT EXISTS collection_token ON envelopes(collection_token);
             CREATE INDEX IF NOT EXISTS request_answers ON envelopes(request_origin,request_id,origin,id);
             CREATE TABLE IF NOT EXISTS collect_acks (
               request_origin TEXT NOT NULL, request_id TEXT NOT NULL,
               answer_origin TEXT NOT NULL, answer_id TEXT NOT NULL,
               PRIMARY KEY(request_origin,request_id,answer_origin,answer_id),
               FOREIGN KEY(request_origin,request_id) REFERENCES envelopes(origin,id) ON DELETE CASCADE);
             UPDATE envelopes AS request SET collect_done=1 WHERE EXISTS (
               SELECT 1 FROM envelopes AS answer WHERE answer.request_origin=request.origin
               AND answer.request_id=request.id AND answer.state IN ('inbox','read')
               AND answer.correlation NOT GLOB '*:deferred');
             CREATE INDEX IF NOT EXISTS open_collections ON envelopes(origin,delivered,reply_expected,collect_at);",
        )?;
        version = 9;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 9 {
        // Routing lives outside immutable envelopes. Earlier drafts left
        // collect-only answers in the ordinary outbox retry queue.
        upgrade(
            &tx,
            "UPDATE envelopes SET state='held',lease_until=0
            WHERE state='custody' AND request_origin IS NOT NULL;
            CREATE INDEX IF NOT EXISTS held_recipient ON envelopes(state,origin,recipient_node);
            DROP TRIGGER IF EXISTS usage_insert;
            DROP TRIGGER IF EXISTS usage_delete;
            DROP TRIGGER IF EXISTS usage_update;
            DROP TABLE IF EXISTS usage;",
        )?;
        upgrade(&tx, HELD_ACCOUNTING)?;
        version = 10;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 10 {
        upgrade(&tx, "ALTER TABLE envelopes ADD COLUMN remote_state TEXT;
            ALTER TABLE envelopes ADD COLUMN receipt_sent TEXT;
            CREATE INDEX IF NOT EXISTS status_correlation ON envelopes(correlation,origin,id);
            CREATE TABLE IF NOT EXISTS delivery_attempts (id TEXT PRIMARY KEY, evidence TEXT NOT NULL,
              queued_at INTEGER NOT NULL, finished INTEGER NOT NULL, state TEXT NOT NULL,
              correlations TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS delivery_attempt_age ON delivery_attempts(queued_at,id);
            CREATE TABLE IF NOT EXISTS status_signer (singleton INTEGER PRIMARY KEY CHECK(singleton=1), secret BLOB NOT NULL);")?;
        ensure_signer(&tx)?;
        version = 11;
    }
    if version == 11 {
        repair_columns(&tx, true)?;
        upgrade(&tx, STEP2)?;
        tx.execute("UPDATE envelopes SET next_hop=recipient_node WHERE next_hop='' AND state IN ('held','custody')", [])?;
    }
    repair_columns(&tx, false)?;
    let rowids = {
        let mut statement =
            tx.prepare("SELECT rowid FROM envelopes WHERE state='quarantined' AND length(body)>0")?;
        let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for rowid in rowids {
        super::quarantine::row(
            &tx,
            rowid,
            directory,
            "existing quarantine during migration",
            wall_ms,
        )?;
    }
    if version != VERSION {
        tx.pragma_update(None, "user_version", VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

/// Only structural migration failures warrant advice to replace the store.
/// Operational failures (busy, locked, IO, disk full) can recover in place.
pub(super) fn is_schema_failure(error: &Error) -> bool {
    match error {
        Error::SchemaRepair(_) => true,
        Error::Sql(rusqlite::Error::SqliteFailure(error, _)) => matches!(
            error.code,
            rusqlite::ErrorCode::Unknown
                | rusqlite::ErrorCode::DatabaseCorrupt
                | rusqlite::ErrorCode::NotADatabase
                | rusqlite::ErrorCode::ConstraintViolation
                | rusqlite::ErrorCode::TypeMismatch
        ),
        _ => false,
    }
}

/// Derive repairs from the same DDL used for fresh stores, including tables
/// whose columns were added by an in-place edit without a version bump.
fn repair_columns(connection: &Connection, allow_missing_tables: bool) -> Result<()> {
    let baseline = Connection::open_in_memory()?;
    baseline.execute_batch(BASELINE)?;
    baseline.execute_batch(LOCAL_RECIPIENTS)?;
    let mut tables =
        baseline.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let tables = tables.query_map([], |row| row.get::<_, String>(0))?;
    for table in tables {
        let table = table?;
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [&table],
            |row| row.get(0),
        )?;
        if !exists {
            if allow_missing_tables {
                continue;
            }
            return Err(Error::SchemaRepair(format!(
                "missing table {table}: cannot reconstruct durable data"
            )));
        }
        let mut columns = baseline
            .prepare("SELECT name,type,[notnull],dflt_value,pk FROM pragma_table_info(?1)")?;
        let columns = columns.query_map([&table], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for column in columns {
            let (name, kind, required, default, primary_key) = column?;
            if has_column(connection, &table, &name)? {
                continue;
            }
            if primary_key != 0 || (required && default.is_none()) {
                return Err(Error::SchemaRepair(format!(
                    "missing column {table}.{name}: cannot restore a primary key or required value without a baseline default"
                )));
            }
            let nullable = if required { " NOT NULL" } else { "" };
            let default = default
                .map(|value| format!(" DEFAULT {value}"))
                .unwrap_or_default();
            // Identifiers and defaults come exclusively from the static baseline.
            connection.execute_batch(&format!(
                "ALTER TABLE \"{table}\" ADD COLUMN \"{name}\" {kind}{nullable}{default}"
            ))?;
        }
    }
    Ok(())
}

pub(super) const BASE: &str = r#"
            CREATE TABLE IF NOT EXISTS clock (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                wall INTEGER NOT NULL, elapsed INTEGER NOT NULL, paused INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS envelopes (
                origin TEXT NOT NULL, id TEXT NOT NULL, correlation TEXT NOT NULL,
                metadata TEXT NOT NULL, fingerprint BLOB NOT NULL, body BLOB NOT NULL,
                state TEXT NOT NULL, custody_deadline INTEGER NOT NULL,
                inbox_deadline INTEGER, dedupe_until INTEGER NOT NULL,
                outcome_until INTEGER, delivered INTEGER NOT NULL DEFAULT 0,
                retry_at INTEGER NOT NULL, metadata_bytes INTEGER NOT NULL,
                PRIMARY KEY(origin,id));
            CREATE INDEX IF NOT EXISTS conversation ON envelopes(origin,correlation);
            CREATE INDEX IF NOT EXISTS expiry ON envelopes(state,custody_deadline);
            CREATE TABLE IF NOT EXISTS inbox_imports (
                origin TEXT NOT NULL, id TEXT NOT NULL,
                PRIMARY KEY(origin,id), FOREIGN KEY(origin,id) REFERENCES envelopes(origin,id) ON DELETE CASCADE);
            CREATE TABLE IF NOT EXISTS identity_pins (
                peer TEXT PRIMARY KEY, node_id TEXT NOT NULL UNIQUE, public_key BLOB NOT NULL);
"#;

const ACCOUNTING_AND_CLAIMS: &str = r#"
ALTER TABLE envelopes ADD COLUMN lease_until INTEGER NOT NULL DEFAULT 0;
CREATE TABLE IF NOT EXISTS usage (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1), total_bytes INTEGER NOT NULL,
    body_bytes INTEGER NOT NULL, active INTEGER NOT NULL);
INSERT OR IGNORE INTO usage SELECT 1, COALESCE(SUM(metadata_bytes+length(body)),0),
    COALESCE(SUM(length(body)),0), COALESCE(SUM(state IN ('custody','inbox')),0) FROM envelopes;
CREATE TRIGGER IF NOT EXISTS usage_insert AFTER INSERT ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes+NEW.metadata_bytes+length(NEW.body),
        body_bytes=body_bytes+length(NEW.body),active=active+(NEW.state IN ('custody','inbox')) WHERE singleton=1;
END;
CREATE TRIGGER IF NOT EXISTS usage_delete AFTER DELETE ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes-OLD.metadata_bytes-length(OLD.body),
        body_bytes=body_bytes-length(OLD.body),active=active-(OLD.state IN ('custody','inbox')) WHERE singleton=1;
END;
CREATE TRIGGER IF NOT EXISTS usage_update AFTER UPDATE OF metadata_bytes,body,state ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes+NEW.metadata_bytes+length(NEW.body)-OLD.metadata_bytes-length(OLD.body),
        body_bytes=body_bytes+length(NEW.body)-length(OLD.body),
        active=active+(NEW.state IN ('custody','inbox'))-(OLD.state IN ('custody','inbox')) WHERE singleton=1;
END;
CREATE INDEX IF NOT EXISTS request_key ON envelopes(json_extract(metadata,'$.request_key.origin_node'),json_extract(metadata,'$.request_key.message_id'));
CREATE INDEX IF NOT EXISTS collection_token ON envelopes(json_extract(metadata,'$.return_binding.collection_token'));
CREATE INDEX IF NOT EXISTS terminal_gc ON envelopes(outcome_until,dedupe_until);
"#;

const HELD_ACCOUNTING: &str = r#"
CREATE TABLE IF NOT EXISTS usage (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1), total_bytes INTEGER NOT NULL,
    body_bytes INTEGER NOT NULL, active INTEGER NOT NULL);
INSERT OR IGNORE INTO usage SELECT 1, COALESCE(SUM(metadata_bytes+length(body)),0),
    COALESCE(SUM(length(body)),0), COALESCE(SUM(state IN ('custody','held','inbox')),0) FROM envelopes;
CREATE TRIGGER IF NOT EXISTS usage_insert AFTER INSERT ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes+NEW.metadata_bytes+length(NEW.body),
        body_bytes=body_bytes+length(NEW.body),active=active+(NEW.state IN ('custody','held','inbox')) WHERE singleton=1;
END;
CREATE TRIGGER IF NOT EXISTS usage_delete AFTER DELETE ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes-OLD.metadata_bytes-length(OLD.body),
        body_bytes=body_bytes-length(OLD.body),active=active-(OLD.state IN ('custody','held','inbox')) WHERE singleton=1;
END;
CREATE TRIGGER IF NOT EXISTS usage_update AFTER UPDATE OF metadata_bytes,body,state ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes+NEW.metadata_bytes+length(NEW.body)-OLD.metadata_bytes-length(OLD.body),
        body_bytes=body_bytes+length(NEW.body)-length(OLD.body),
        active=active+(NEW.state IN ('custody','held','inbox'))-(OLD.state IN ('custody','held','inbox')) WHERE singleton=1;
END;
"#;

fn has_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
        [table, column],
        |r| r.get(0),
    )?)
}

/// Execute historical DDL while tolerating columns introduced by dev builds.
fn upgrade(connection: &Connection, sql: &str) -> Result<()> {
    let mut rest = sql;
    while let Some(start) = rest.find("ALTER TABLE ") {
        connection.execute_batch(&rest[..start])?;
        rest = &rest[start..];
        let end = rest.find(';').ok_or(Error::InvalidState)? + 1;
        let statement = &rest[..end];
        let words: Vec<_> = statement.split_whitespace().collect();
        if words.get(3) != Some(&"ADD") || !has_column(connection, words[2], words[5])? {
            connection.execute_batch(statement)?;
        }
        rest = &rest[end..];
    }
    connection.execute_batch(rest)?;
    Ok(())
}

fn ensure_signer(connection: &Connection) -> Result<()> {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|_| Error::InvalidState)?;
    connection.execute(
        "INSERT OR IGNORE INTO status_signer VALUES(1,?1)",
        [secret.as_slice()],
    )?;
    Ok(())
}

const STEP2: &str = r#"
ALTER TABLE envelopes ADD COLUMN next_hop TEXT NOT NULL DEFAULT '';
ALTER TABLE envelopes ADD COLUMN hops_left INTEGER NOT NULL DEFAULT 8;
ALTER TABLE envelopes ADD COLUMN visited TEXT NOT NULL DEFAULT '[]';
ALTER TABLE envelopes ADD COLUMN kind TEXT NOT NULL DEFAULT 'message';
ALTER TABLE envelopes ADD COLUMN mailbox_ttl_ms INTEGER NOT NULL DEFAULT 86400000;
CREATE INDEX IF NOT EXISTS push_ready ON envelopes(state,next_hop,retry_at);
CREATE INDEX IF NOT EXISTS unrouted ON envelopes(state,next_hop) WHERE next_hop='';
CREATE TABLE IF NOT EXISTS agent_tombstones (agent_id TEXT NOT NULL, session TEXT NOT NULL, reason TEXT NOT NULL, at INTEGER NOT NULL, until INTEGER NOT NULL, PRIMARY KEY(agent_id,session));
CREATE TABLE IF NOT EXISTS agent_owners (agent_id TEXT PRIMARY KEY, node_id TEXT NOT NULL, name TEXT NOT NULL, seen INTEGER NOT NULL);
"#;

const LOCAL_RECIPIENTS: &str = "CREATE TABLE IF NOT EXISTS local_recipients (
    origin TEXT NOT NULL, id TEXT NOT NULL, agent TEXT NOT NULL, session TEXT NOT NULL,
    PRIMARY KEY(origin,id),
    FOREIGN KEY(origin,id) REFERENCES envelopes(origin,id) ON DELETE CASCADE);";

const BASELINE: &str = r#"
CREATE TABLE IF NOT EXISTS agent_owners (agent_id TEXT PRIMARY KEY, node_id TEXT NOT NULL, name TEXT NOT NULL, seen INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS agent_tombstones (agent_id TEXT NOT NULL, session TEXT NOT NULL, reason TEXT NOT NULL, at INTEGER NOT NULL, until INTEGER NOT NULL, PRIMARY KEY(agent_id,session));
CREATE TABLE IF NOT EXISTS clock (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                wall INTEGER NOT NULL, elapsed INTEGER NOT NULL, paused INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS collect_acks (
               request_origin TEXT NOT NULL, request_id TEXT NOT NULL,
               answer_origin TEXT NOT NULL, answer_id TEXT NOT NULL,
               PRIMARY KEY(request_origin,request_id,answer_origin,answer_id),
               FOREIGN KEY(request_origin,request_id) REFERENCES envelopes(origin,id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS delivery_attempts (id TEXT PRIMARY KEY, evidence TEXT NOT NULL,
              queued_at INTEGER NOT NULL, finished INTEGER NOT NULL, state TEXT NOT NULL,
              correlations TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS envelopes (
                origin TEXT NOT NULL, id TEXT NOT NULL, correlation TEXT NOT NULL,
                metadata TEXT NOT NULL, fingerprint BLOB NOT NULL, body BLOB NOT NULL,
                state TEXT NOT NULL, custody_deadline INTEGER NOT NULL,
                inbox_deadline INTEGER, dedupe_until INTEGER NOT NULL,
                outcome_until INTEGER, delivered INTEGER NOT NULL DEFAULT 0,
                retry_at INTEGER NOT NULL, metadata_bytes INTEGER NOT NULL, lease_until INTEGER NOT NULL DEFAULT 0, collect_at INTEGER NOT NULL DEFAULT 0, collect_attempts INTEGER NOT NULL DEFAULT 0, collection_token TEXT, request_origin TEXT, request_id TEXT, recipient_node TEXT NOT NULL DEFAULT '', reply_expected INTEGER NOT NULL DEFAULT 0, collect_done INTEGER NOT NULL DEFAULT 0, collect_failures INTEGER NOT NULL DEFAULT 0, collect_error TEXT, remote_state TEXT, receipt_sent TEXT, next_hop TEXT NOT NULL DEFAULT '', hops_left INTEGER NOT NULL DEFAULT 8, visited TEXT NOT NULL DEFAULT '[]', kind TEXT NOT NULL DEFAULT 'message', mailbox_ttl_ms INTEGER NOT NULL DEFAULT 86400000,
                PRIMARY KEY(origin,id));
CREATE TABLE IF NOT EXISTS identity_pins (
                source TEXT NOT NULL, peer TEXT NOT NULL, node_id TEXT NOT NULL,
                public_key BLOB NOT NULL, origin TEXT NOT NULL DEFAULT 'unknown', PRIMARY KEY(source,peer));
CREATE TABLE IF NOT EXISTS inbox_imports (
                origin TEXT NOT NULL, id TEXT NOT NULL,
                PRIMARY KEY(origin,id), FOREIGN KEY(origin,id) REFERENCES envelopes(origin,id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS status_signer (singleton INTEGER PRIMARY KEY CHECK(singleton=1), secret BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS usage (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1), total_bytes INTEGER NOT NULL,
    body_bytes INTEGER NOT NULL, active INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS writer_generation (singleton INTEGER PRIMARY KEY CHECK(singleton=1), generation INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS collect_ready ON envelopes(origin,collect_at,custody_deadline);
CREATE INDEX IF NOT EXISTS collection_token ON envelopes(collection_token);
CREATE INDEX IF NOT EXISTS conversation ON envelopes(origin,correlation);
CREATE INDEX IF NOT EXISTS delivery_attempt_age ON delivery_attempts(queued_at,id);
CREATE INDEX IF NOT EXISTS expiry ON envelopes(state,custody_deadline);
CREATE INDEX IF NOT EXISTS held_recipient ON envelopes(state,origin,recipient_node);
CREATE UNIQUE INDEX IF NOT EXISTS inbound_key_name ON identity_pins(public_key) WHERE source='inbound';
CREATE UNIQUE INDEX IF NOT EXISTS inbound_node_name ON identity_pins(node_id) WHERE source='inbound';
CREATE INDEX IF NOT EXISTS open_collections ON envelopes(origin,delivered,reply_expected,collect_at);
CREATE INDEX IF NOT EXISTS push_ready ON envelopes(state,next_hop,retry_at);
CREATE INDEX IF NOT EXISTS request_answers ON envelopes(request_origin,request_id,origin,id);
CREATE INDEX IF NOT EXISTS status_correlation ON envelopes(correlation,origin,id);
CREATE INDEX IF NOT EXISTS terminal_gc ON envelopes(outcome_until,dedupe_until);
CREATE INDEX IF NOT EXISTS unrouted ON envelopes(state,next_hop) WHERE next_hop='';
CREATE TRIGGER IF NOT EXISTS usage_delete AFTER DELETE ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes-OLD.metadata_bytes-length(OLD.body),
        body_bytes=body_bytes-length(OLD.body),active=active-(OLD.state IN ('custody','held','inbox')) WHERE singleton=1;
END;
CREATE TRIGGER IF NOT EXISTS usage_insert AFTER INSERT ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes+NEW.metadata_bytes+length(NEW.body),
        body_bytes=body_bytes+length(NEW.body),active=active+(NEW.state IN ('custody','held','inbox')) WHERE singleton=1;
END;
CREATE TRIGGER IF NOT EXISTS usage_update AFTER UPDATE OF metadata_bytes,body,state ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes+NEW.metadata_bytes+length(NEW.body)-OLD.metadata_bytes-length(OLD.body),
        body_bytes=body_bytes+length(NEW.body)-length(OLD.body),
        active=active+(NEW.state IN ('custody','held','inbox'))-(OLD.state IN ('custody','held','inbox')) WHERE singleton=1;
END;
INSERT INTO usage VALUES(1,0,0,0);
INSERT INTO writer_generation VALUES(1,0);
"#;
