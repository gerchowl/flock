//! Per-host mechanical checks (#175 phase 4).
//!
//! The check runner OWNS the debounce state machine and emits `FireDecision`s;
//! it never fires notifications itself (P1 — mechanical gates). Actions are
//! executed by the App tick which folds `FireDecision`s into the existing
//! notification / event paths.

pub(crate) mod config;

pub(crate) use config::ChecksConfig;
