//! Lenient decoding of the additive, mixed-version structured peer directory.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::schema::PeerStructuredAgentSummary;

/// Keep supported rows when a newer peer sends unknown runtime or status values.
/// Invalid rows are skipped with a diagnostic; malformed collections are empty.
pub(crate) fn decode(value: Value) -> Vec<PeerStructuredAgentSummary> {
    let Value::Array(rows) = value else {
        tracing::warn!("ignoring malformed structured peer discovery collection");
        return Vec::new();
    };
    rows.into_iter()
        .enumerate()
        .filter_map(|(index, row)| match serde_json::from_value(row) {
            Ok(row) => Some(row),
            Err(_) => {
                tracing::warn!(
                    index,
                    "skipping unsupported or invalid structured peer discovery row"
                );
                None
            }
        })
        .collect()
}

/// Apply the same row boundary to typed summaries and relayed fleet entries.
/// Missing legacy fields use their serde default without a diagnostic.
pub(crate) fn deserialize<'de, D>(
    deserializer: D,
) -> Result<Vec<PeerStructuredAgentSummary>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(decode(Value::deserialize(deserializer)?))
}
