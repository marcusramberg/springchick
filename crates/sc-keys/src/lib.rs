#![forbid(unsafe_code)]
//! Short/long key-press timing. Pure: the compositor supplies keysyms and
//! `Instant`s. The binding types live in `sc-config`.

pub mod state;

pub use state::{KeyBindings, PressOutcome, PressTracker};
