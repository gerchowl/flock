//! Transactional schema upgrades, including the original unversioned store.
use super::{Connection, Error, Result, TransactionBehavior};

pub(super) const VERSION: i64 = 11;

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
        version = 2;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 2 {
        tx.execute_batch(
            r#"
            ALTER TABLE identity_pins RENAME TO legacy_identity_pins;
            CREATE TABLE identity_pins (
                source TEXT NOT NULL, peer TEXT NOT NULL, node_id TEXT NOT NULL,
                public_key BLOB NOT NULL, PRIMARY KEY(source,peer),
                UNIQUE(source,node_id), UNIQUE(source,public_key));
            INSERT INTO identity_pins SELECT 'configured',peer,node_id,public_key
                FROM legacy_identity_pins;
            DROP TABLE legacy_identity_pins;
        "#,
        )?;
        version = 3;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 3 {
        tx.execute_batch(r#"
            ALTER TABLE identity_pins RENAME TO directional_identity_pins;
            CREATE TABLE identity_pins (
                source TEXT NOT NULL, peer TEXT NOT NULL, node_id TEXT NOT NULL,
                public_key BLOB NOT NULL, PRIMARY KEY(source,peer));
            INSERT INTO identity_pins SELECT * FROM directional_identity_pins;
            DROP TABLE directional_identity_pins;
            CREATE UNIQUE INDEX inbound_node_name ON identity_pins(node_id) WHERE source='inbound';
            CREATE UNIQUE INDEX inbound_key_name ON identity_pins(public_key) WHERE source='inbound';
        "#)?;
        version = 4;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 4 {
        tx.execute_batch(
            "ALTER TABLE identity_pins ADD COLUMN origin TEXT NOT NULL DEFAULT 'unknown';",
        )?;
        version = 5;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 5 {
        tx.execute_batch("CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY);")?;
        version = 6;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 6 {
        tx.execute_batch("CREATE TABLE writer_generation (singleton INTEGER PRIMARY KEY CHECK(singleton=1), generation INTEGER NOT NULL); INSERT INTO writer_generation VALUES(1,0);")?;
        version = 7;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 7 {
        tx.execute_batch(
            "ALTER TABLE envelopes ADD COLUMN collect_at INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE envelopes ADD COLUMN collect_attempts INTEGER NOT NULL DEFAULT 0;
            CREATE INDEX collect_ready ON envelopes(origin,collect_at,custody_deadline);",
        )?;
        version = 8;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 8 {
        tx.execute_batch(
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
             CREATE INDEX collection_token ON envelopes(collection_token);
             CREATE INDEX request_answers ON envelopes(request_origin,request_id,origin,id);
             CREATE TABLE collect_acks (
               request_origin TEXT NOT NULL, request_id TEXT NOT NULL,
               answer_origin TEXT NOT NULL, answer_id TEXT NOT NULL,
               PRIMARY KEY(request_origin,request_id,answer_origin,answer_id),
               FOREIGN KEY(request_origin,request_id) REFERENCES envelopes(origin,id) ON DELETE CASCADE);
             UPDATE envelopes AS request SET collect_done=1 WHERE EXISTS (
               SELECT 1 FROM envelopes AS answer WHERE answer.request_origin=request.origin
               AND answer.request_id=request.id AND answer.state IN ('inbox','read')
               AND answer.correlation NOT GLOB '*:deferred');
             CREATE INDEX open_collections ON envelopes(origin,delivered,reply_expected,collect_at);",
        )?;
        version = 9;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 9 {
        // Routing lives outside immutable envelopes. Earlier drafts left
        // collect-only answers in the ordinary outbox retry queue.
        tx.execute_batch(
            "UPDATE envelopes SET state='held',lease_until=0
            WHERE state='custody' AND request_origin IS NOT NULL;
            CREATE INDEX held_recipient ON envelopes(state,origin,recipient_node);
            DROP TRIGGER usage_insert;
            DROP TRIGGER usage_delete;
            DROP TRIGGER usage_update;
            DROP TABLE usage;",
        )?;
        tx.execute_batch(HELD_ACCOUNTING)?;
        version = 10;
        tx.pragma_update(None, "user_version", version)?;
    }
    if version == 10 {
        tx.execute_batch("ALTER TABLE envelopes ADD COLUMN remote_state TEXT;
            ALTER TABLE envelopes ADD COLUMN receipt_sent TEXT;
            CREATE INDEX status_correlation ON envelopes(correlation,origin,id);
            CREATE TABLE delivery_attempts (id TEXT PRIMARY KEY, evidence TEXT NOT NULL,
              queued_at INTEGER NOT NULL, finished INTEGER NOT NULL, state TEXT NOT NULL,
              correlations TEXT NOT NULL);
            CREATE INDEX delivery_attempt_age ON delivery_attempts(queued_at,id);
            CREATE TABLE status_signer (singleton INTEGER PRIMARY KEY CHECK(singleton=1), secret BLOB NOT NULL);")?;
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|_| Error::InvalidState)?;
        tx.execute(
            "INSERT INTO status_signer VALUES(1,?1)",
            [secret.as_slice()],
        )?;
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

const HELD_ACCOUNTING: &str = r#"
CREATE TABLE usage (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1), total_bytes INTEGER NOT NULL,
    body_bytes INTEGER NOT NULL, active INTEGER NOT NULL);
INSERT INTO usage SELECT 1, COALESCE(SUM(metadata_bytes+length(body)),0),
    COALESCE(SUM(length(body)),0), COALESCE(SUM(state IN ('custody','held','inbox')),0) FROM envelopes;
CREATE TRIGGER usage_insert AFTER INSERT ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes+NEW.metadata_bytes+length(NEW.body),
        body_bytes=body_bytes+length(NEW.body),active=active+(NEW.state IN ('custody','held','inbox')) WHERE singleton=1;
END;
CREATE TRIGGER usage_delete AFTER DELETE ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes-OLD.metadata_bytes-length(OLD.body),
        body_bytes=body_bytes-length(OLD.body),active=active-(OLD.state IN ('custody','held','inbox')) WHERE singleton=1;
END;
CREATE TRIGGER usage_update AFTER UPDATE OF metadata_bytes,body,state ON envelopes BEGIN
    UPDATE usage SET total_bytes=total_bytes+NEW.metadata_bytes+length(NEW.body)-OLD.metadata_bytes-length(OLD.body),
        body_bytes=body_bytes+length(NEW.body)-length(OLD.body),
        active=active+(NEW.state IN ('custody','held','inbox'))-(OLD.state IN ('custody','held','inbox')) WHERE singleton=1;
END;
"#;
