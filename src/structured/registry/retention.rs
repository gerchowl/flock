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

    /// Unknown registry collection formats require the documented offline repair path.
    pub fn check_retained_collection(&self) -> Result<(), RegistryError> {
        if self.unparsed.is_some() {
            return Err(RegistryError::UnsupportedCollectionShape);
        }
        Ok(())
    }

    /// Explicit operator selection; missing timestamps never satisfy an age filter.
    /// This changes only inactive records, preserving active workers and observations.
    /// Reclassify remaining copies so removing an earlier duplicate cannot orphan DEAD data.
    pub fn purge_retained(&mut self, all: bool, older_than_ms: Option<u64>, now_ms: u64) -> usize {
        let before = self.retained.len();
        self.retained.retain(|slot, entry| {
            let selected = (all || entry.kind == RetainedKind::Dead)
                && older_than_ms.is_none_or(|age| {
                    entry
                        .first_seen_ms
                        .is_some_and(|first| now_ms.saturating_sub(first) > age)
                });
            if selected {
                self.preserved[*slot] = None;
            }
            !selected
        });
        let mut seen = BTreeSet::new();
        for (slot, raw) in self.preserved.iter().enumerate() {
            let Some(raw) = raw else { continue };
            let duplicate = !seen.insert(digest(raw));
            if let Some(entry) = self.retained.get_mut(&slot) {
                entry.kind = if !raw.is_object() || duplicate {
                    RetainedKind::Dead
                } else {
                    RetainedKind::Forward
                };
            }
        }
        before - self.retained.len()
    }

    /// Install the saved snapshot's inactive records and sidecar after durable commit.
    /// The snapshot must come from this registry during synchronous maintenance,
    /// without intervening registry mutations. Active entries, generations and worker
    /// observations remain owned by the live registry (ADR-0036 §1).
    pub(crate) fn swap_retained_state(&mut self, saved: &mut Self) {
        // A fresh snapshot keeps preserved slots bound to the same active entries.
        debug_assert!(
            self.entries.keys().eq(saved.entries.keys()),
            "retained snapshot must have the same live entries"
        );
        std::mem::swap(&mut self.preserved, &mut saved.preserved);
        std::mem::swap(&mut self.retained, &mut saved.retained);
        std::mem::swap(&mut self.retention_extra, &mut saved.retention_extra);
        std::mem::swap(&mut self.retention_opaque, &mut saved.retention_opaque);
    }

    /// Stamp missing first-seen times without pruning capture/undo records.
    /// Unknown sidecar shapes remain untouched (ADR-0028 §3a).
    pub fn stamp_retained_records(&mut self, now_ms: u64) {
        if self.retention_opaque.is_none() {
            for entry in self.retained.values_mut() {
                entry.first_seen_ms.get_or_insert(now_ms);
            }
        }
    }

    /// Stamp and prune at an actual save boundary, using the injected wall clock.
    /// FORWARD records are never auto-pruned. Equal-age DEAD ties use original order.
    pub fn prepare_retained_save(&mut self, now_ms: u64) {
        if self.retention_opaque.is_some() {
            tracing::warn!("automatic retained-record pruning refused for unknown sidecar shape; see session-state repair guide");
            return;
        }
        self.stamp_retained_records(now_ms);
        let mut dead: Vec<_> = self
            .retained
            .iter()
            .filter_map(|(&slot, entry)| {
                (entry.kind == RetainedKind::Dead)
                    .then_some((entry.first_seen_ms.unwrap_or(now_ms), slot))
            })
            .collect();
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
