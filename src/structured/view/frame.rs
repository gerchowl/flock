//! Controller-owned copy-on-write storage; frozen Snapshot fields stay unchanged.
use super::contracts::Snapshot;
use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};
/// Every Model mutation passes through make_mut; sharing grants no mutable access.
pub(crate) struct Frame(pub(super) Arc<Snapshot>);
impl From<Snapshot> for Frame {
    fn from(snapshot: Snapshot) -> Self {
        Self(Arc::new(snapshot))
    }
}
impl Deref for Frame {
    type Target = Snapshot;
    fn deref(&self) -> &Snapshot {
        &self.0
    }
}
impl DerefMut for Frame {
    fn deref_mut(&mut self) -> &mut Snapshot {
        Arc::make_mut(&mut self.0)
    }
}
