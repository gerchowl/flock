//! Durable mesh identity and transport foundations (ADR-0026).
//! Opening the custody store is owned by the future handoff integration.

pub mod clock;
pub(crate) mod identity;
pub mod key;
pub mod store;
