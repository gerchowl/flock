//! S10c pure components (ADR-0036 §11); composed and wired by S10c2/S10d (#1430).
// Components are reviewed independently before their pane and input consumers land.
#![allow(dead_code)]
pub(crate) mod controls;
mod markdown;
mod pane;
pub(crate) mod transcript;
pub(crate) use pane::Pane;

mod dispatch;
pub(crate) use dispatch::{prepare, render_native, validate, Prepared};
