//! Transactional schema upgrades, including the original unversioned store.
use super::{Connection, Error, Result, TransactionBehavior};

pub(super) const VERSION: i64 = 2;

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

pub(super) fn migrate(connection: &mut Connection) -> Result<()> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_version(&tx)?;
    let mut version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version == 0 {
        tx.execute_batch(BASE)?;
        version = 1;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 1 {
        tx.execute_batch(ACCOUNTING_AND_CLAIMS)?;
        tx.pragma_update(None, "user_version", VERSION)?;
    }
    tx.commit()?;
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
CREATE TABLE usage (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1), total_bytes INTEGER NOT NULL,
    body_bytes INTEGER NOT NULL, active INTEGER NOT NULL);
INSERT INTO usage SELECT 1, COALESCE(SUM(metadata_bytes+length(body)),0),
    COALESCE(SUM(length(body)),0), COALESCE(SUM(state IN ('custody','inbox')),0) FROM envelopes;
CREATE TRIGGER usage_insert AFTER INSERT ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes+NEW.metadata_bytes+length(NEW.body),
        body_bytes=body_bytes+length(NEW.body),active=active+(NEW.state IN ('custody','inbox')) WHERE singleton=1;
END;
CREATE TRIGGER usage_delete AFTER DELETE ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes-OLD.metadata_bytes-length(OLD.body),
        body_bytes=body_bytes-length(OLD.body),active=active-(OLD.state IN ('custody','inbox')) WHERE singleton=1;
END;
CREATE TRIGGER usage_update AFTER UPDATE OF metadata_bytes,body,state ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes+NEW.metadata_bytes+length(NEW.body)-OLD.metadata_bytes-length(OLD.body),
        body_bytes=body_bytes+length(NEW.body)-length(OLD.body),
        active=active+(NEW.state IN ('custody','inbox'))-(OLD.state IN ('custody','inbox')) WHERE singleton=1;
END;
CREATE INDEX request_key ON envelopes(json_extract(metadata,'$.request_key.origin_node'),json_extract(metadata,'$.request_key.message_id'));
CREATE INDEX collection_token ON envelopes(json_extract(metadata,'$.return_binding.collection_token'));
CREATE INDEX terminal_gc ON envelopes(outcome_until,dedupe_until);
"#;
