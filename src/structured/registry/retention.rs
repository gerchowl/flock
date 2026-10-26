//! Rollback-safe retained-record lifetime policy, ADR-0036 §1 and ADR-0028 §3a.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub(super) struct Retained {
    kind: RetainedKind,
    first_seen_ms: Option<u64>,
    metadata: Value,
}
impl Retained {
    pub(super) fn new(dead: bool) -> Self {
        Self {
            kind: if dead {
                RetainedKind::Dead
            } else {
                RetainedKind::Forward
            },
            first_seen_ms: None,
            metadata: serde_json::json!({}),
        }
    }
}
#[derive(Deserialize)]
struct Stamp {
    slot: usize,
    digest: String,
    first_seen_ms: u64,
}
pub(super) fn digest(raw: &Value) -> String {
    Sha256::digest(raw.to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
impl Registry {
    /// Metadata only; no retained content can become a live agent through this view.
    pub fn retained_records(&self) -> Vec<RetainedInfo> {
        self.preserved
            .iter()
            .enumerate()
            .filter(|(_, raw)| raw.is_some())
            .enumerate()
            .filter_map(|(serialized, (slot, _))| {
                self.retained.get(&slot).map(|entry| RetainedInfo {
                    slot: serialized,
                    kind: entry.kind,
                    first_seen_ms: entry.first_seen_ms,
                })
            })
            .collect()
    }

    /// Apply sidecar stamps individually. Unknown entries remain raw and logged.
    /// A future collection format freezes automatic retention instead of destroying it.
    pub fn restore_retained_stamps(&mut self, raw: &Value) {
        for entry in self.retained.values_mut() {
            entry.first_seen_ms = None;
            entry.metadata = serde_json::json!({});
        }
        self.retention_extra.clear();
        self.retention_opaque = None;
        if raw.is_null() {
            return;
        }
        let Some(entries) = raw.as_array() else {
            tracing::warn!("retained sidecar has unknown shape; automatic retention disabled; see session-state repair guide");
            self.retention_opaque = Some(raw.clone());
            return;
        };
        for (index, value) in entries.iter().enumerate() {
            if let Some(stamp) = value
                .is_object()
                .then(|| serde_json::from_value::<Stamp>(value.clone()).ok())
                .flatten()
            {
                if let (Some(entry), Some(Some(record))) = (
                    self.retained.get_mut(&stamp.slot),
                    self.preserved.get(stamp.slot),
                ) {
                    if entry.first_seen_ms.is_none() && stamp.digest == digest(record) {
                        entry.first_seen_ms = Some(stamp.first_seen_ms);
                        entry.metadata = value.clone();
                        continue;
                    }
                }
            }
            tracing::warn!(index, "retained sidecar entry skipped and preserved raw");
            self.retention_extra.push(value.clone());
        }
    }

    /// Additive snapshot sidecar: timestamps never alter preserved record JSON.
    /// Digest binding prevents a mismatched slot from inheriting another record's age.
    pub fn retained_stamps(&self) -> Value {
        if let Some(opaque) = &self.retention_opaque {
            return opaque.clone();
        }
        let mut stamps = Vec::new();
        for (serialized, (slot, _)) in self
            .preserved
            .iter()
            .enumerate()
            .filter(|(_, raw)| raw.is_some())
            .enumerate()
        {
            let Some(entry) = self.retained.get(&slot) else {
                continue;
            };
            let Some(first_seen_ms) = entry.first_seen_ms else {
                continue;
            };
            let Some(record) = &self.preserved[slot] else {
                continue;
            };
            let mut metadata = entry.metadata.clone();
            metadata["slot"] = serde_json::json!(serialized);
            metadata["digest"] = serde_json::json!(digest(record));
            metadata["first_seen_ms"] = serde_json::json!(first_seen_ms);
            stamps.push(metadata);
        }
        stamps.extend(self.retention_extra.clone());
        if stamps.is_empty() {
            Value::Null
        } else {
            Value::Array(stamps)
        }
    }

    /// Stamp at the injected wall-clock time; prune only at an actual save boundary.
    /// Capture/undo callers pass `false` so no record disappears from a snapshot.
    /// FORWARD records are never auto-pruned. Equal-age DEAD ties use original order.
    pub fn prepare_retained_save(&mut self, now_ms: u64, prune_dead: bool) {
        if self.retention_opaque.is_some() {
            tracing::warn!("automatic retained-record pruning refused for unknown sidecar shape; see session-state repair guide");
            return;
        }
        let mut dead = Vec::new();
        for (&slot, entry) in &mut self.retained {
            let first = *entry.first_seen_ms.get_or_insert(now_ms);
            if entry.kind == RetainedKind::Dead {
                dead.push((first, slot));
            }
        }
        if !prune_dead {
            return;
        }
        dead.sort_unstable();
        let expired = dead
            .iter()
            .take_while(|(first, _)| {
                now_ms.saturating_sub(*first) > RETAINED_DEAD_MAX_AGE.as_millis() as u64
            })
            .count();
        let remove = expired.max(dead.len().saturating_sub(RETAINED_DEAD_MAX));
        for (_, slot) in dead.into_iter().take(remove) {
            self.retained.remove(&slot);
            self.preserved[slot] = None;
        }
    }
}
