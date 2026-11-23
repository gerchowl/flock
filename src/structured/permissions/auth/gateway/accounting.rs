//! Counted-issuance integrity lives with A1, outside the replaceable J1 directory.
//! The committed watermark never precedes journal sync. A pending reservation is
//! not a committed use: it fences uncertainty until its exact claim is recovered.
//! If history is lost, the operator must revoke and re-add the grant, assigning a
//! new issuance. Never erase this evidence to repair J1. Standing grants do not
//! use this fence. Entries share A1's one-MiB bound and leave with their issuance.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stamp {
    pub total: u64,
    pub digest: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mark {
    committed: Stamp,
    pending: Option<Stamp>,
}
pub(crate) struct Fence<'a> {
    store: &'a Store,
    value: Value,
}
impl Store {
    /// Keep generation, integrity evidence, J1 sync and publication under one lease.
    pub(crate) fn with_accounting<R>(
        &self,
        action: impl FnOnce(&Listing, &mut Fence<'_>) -> Result<R, E>,
    ) -> Result<R, E> {
        let (_lease, value) = self.snapshot()?;
        action(&Self::listing(&value), &mut Fence { store: self, value })
    }
}
impl Fence<'_> {
    /// Missing/newer/invalid evidence refuses only the affected counted issuance.
    /// A recovered synced pending claim may advance its committed watermark.
    pub(crate) fn check(&mut self, id: &str, observed: Stamp) -> Result<bool, E> {
        let Some(accounting) = self.value.get("accounting") else {
            return Ok(false);
        };
        let Some(entries) = accounting.as_object() else {
            return Ok(false);
        };
        let Some(raw) = entries.get(id) else {
            return Ok(false);
        };
        let Ok(mark) = serde_json::from_value::<Mark>(raw.clone()) else {
            tracing::warn!("counted grant accounting skipped and fenced: invalid or newer format");
            return Ok(false);
        };
        if mark.pending == Some(observed) && observed.total > mark.committed.total {
            self.commit(id, observed)?;
            return Ok(true);
        }
        Ok(mark.pending.is_none() && mark.committed == observed)
    }
    pub(crate) fn prepare(&mut self, id: &str, committed: Stamp, pending: Stamp) -> Result<(), E> {
        self.write(
            id,
            Mark {
                committed,
                pending: Some(pending),
            },
        )
    }
    pub(crate) fn commit(&mut self, id: &str, committed: Stamp) -> Result<(), E> {
        self.write(
            id,
            Mark {
                committed,
                pending: None,
            },
        )
    }
    fn write(&mut self, id: &str, mark: Mark) -> Result<(), E> {
        if self.value.get("accounting").is_none() {
            self.value["accounting"] = json!({});
        }
        self.value["accounting"]
            .as_object_mut()
            .ok_or(E::InternalError)?
            .insert(id.to_owned(), serde_json::to_value(mark).map_err(storage)?);
        self.store.persist(&self.value)
    }
}

/// New counted issuances start with durable zero evidence. Older counted
/// issuances without evidence require explicit operator re-issuance.
pub(super) fn initialize(value: &mut Value, id: &str) -> Result<(), E> {
    if value.get("accounting").is_none() {
        value["accounting"] = json!({});
    }
    value["accounting"]
        .as_object_mut()
        .ok_or(E::InternalError)?
        .insert(
            id.to_owned(),
            serde_json::to_value(Mark {
                committed: Stamp::default(),
                pending: None,
            })
            .map_err(storage)?,
        );
    Ok(())
}
