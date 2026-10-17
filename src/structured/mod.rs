//! Structured harness adapters, separate from flock state events (ADR-0036 §11).

pub mod claude;
pub mod launch;
pub mod lease;
pub mod permissions;
pub mod queue;
pub mod runtime;
pub mod state;
pub mod supervision;
pub mod transport;

pub(crate) mod identity;
