//! Durable recovery evidence must precede destructive quarantine updates.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::types::ValueRef;
use serde_json::{json, Map, Value};
use std::io::{Read, Seek, SeekFrom, Write};

const MAX_BYTES: u64 = 64 << 20;
const FILE: &str = "mesh-quarantine.jsonl";

fn raw(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => json!({"sqlite_type": "null"}),
        ValueRef::Integer(value) => json!({"sqlite_type": "integer", "value": value}),
        ValueRef::Real(value) => json!({"sqlite_type": "real", "bits": value.to_bits()}),
        ValueRef::Text(value) => json!({"sqlite_type": "text", "base64": STANDARD.encode(value)}),
        ValueRef::Blob(value) => json!({"sqlite_type": "blob", "base64": STANDARD.encode(value)}),
    }
}

fn archive(
    connection: &Connection,
    rowid: i64,
    directory: &Path,
    reason: &str,
    wall_ms: i64,
) -> Result<()> {
    let record = connection.query_row(
        "SELECT rowid,* FROM envelopes WHERE rowid=?1",
        [rowid],
        |row| {
            let mut columns = Map::new();
            for (index, name) in row.as_ref().column_names().iter().enumerate() {
                columns.insert((*name).into(), raw(row.get_ref(index)?));
            }
            Ok(json!({
                "format": 1,
                "key": {"origin_node": columns["origin"], "message_id": columns["id"]},
                "state": columns["state"],
                "reason": reason,
                "quarantined_at": wall_ms,
                "body": columns["body"],
                "envelope": columns["metadata"],
                "raw_row": columns,
            }))
        },
    )?;
    let mut line = serde_json::to_vec(&record)?;
    line.push(b'\n');
    append(directory, &line, MAX_BYTES)?;
    Ok(())
}

/// The caller holds the store's write transaction, serializing backup and clear.
/// Failure leaves even the state untouched so the original row can be retried.
pub(super) fn row(
    connection: &Connection,
    rowid: i64,
    directory: &Path,
    reason: &str,
    wall_ms: i64,
) -> Result<()> {
    if let Err(error) = archive(connection, rowid, directory, reason, wall_ms) {
        tracing::warn!(
            rowid,
            error = error.to_string(),
            "mesh quarantine backup failed; original row retained"
        );
        return Ok(());
    }
    connection.execute(
        "UPDATE envelopes SET state='quarantined',body=X'' WHERE rowid=?1",
        [rowid],
    )?;
    Ok(())
}

pub(super) fn rows(
    connection: &Connection,
    rowids: &[i64],
    directory: &Path,
    reason: &str,
    wall_ms: i64,
) -> Result<()> {
    if rowids.is_empty() {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    for rowid in rowids {
        row(&tx, *rowid, directory, reason, wall_ms)?;
    }
    tx.commit()?;
    Ok(())
}

// One store writer holds SQLite's write lock across rotation, append and clear.
// Sync the directory as well as the file so a crash cannot lose a new filename.
pub(super) fn append(directory: &Path, line: &[u8], limit: u64) -> std::io::Result<()> {
    if line.len() as u64 > limit {
        return Err(std::io::Error::other(
            "quarantine row exceeds sidecar file limit",
        ));
    }
    let path = directory.join(FILE);
    let open = || -> std::io::Result<fs::File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    };
    let mut file = open()?;
    if file.metadata()?.len().saturating_add(line.len() as u64) > limit {
        drop(file);
        fs::rename(&path, directory.join("mesh-quarantine.jsonl.1"))?;
        file = open()?;
    }
    let previous_len = file.metadata()?.len();
    if previous_len > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last != *b"\n" {
            return Err(std::io::Error::other(
                "quarantine sidecar has an incomplete final record",
            ));
        }
    }
    let result = (|| {
        file.write_all(line)?;
        file.sync_all()?;
        fs::File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        // A failed append is never acknowledged to SQLite. Restore the last
        // complete JSONL boundary where possible, allowing a later retry.
        let _ = file.set_len(previous_len);
    }
    result
}
