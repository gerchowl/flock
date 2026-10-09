//! Durable mesh identity and transport foundations (ADR-0026).
//! Custody is projected into app mailboxes and transferred over enrolled edges.

pub mod clock;
pub mod hello;
pub(crate) mod identity;
pub mod key;
pub mod store;

pub mod delivery;

pub(crate) mod runtime_store;

pub mod collect;
