//! Structured harness adapters, separate from flock state events (ADR-0036 §11).

pub mod blobs;
pub mod claude;
pub mod history;
pub mod launch;
pub mod lease;
pub mod permissions;
pub mod registry;
pub mod runtime;
pub mod state;
pub mod supervision;
pub mod transport;
pub mod workers;

pub(crate) mod identity;
