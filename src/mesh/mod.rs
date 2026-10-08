//! Durable mesh primitives. Opening the store is deliberately owned by the
//! future handoff integration, not application construction.
pub mod clock;
pub mod key;
pub mod store;
