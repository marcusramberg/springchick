#![forbid(unsafe_code)]
//! Short/long key-press timing for springchick.
//!
//! Pure logic: no wayland, no xkb, no clock of its own. The compositor resolves
//! keysym names and supplies `Instant`s, so every rule here is unit-testable.
//! The binding/config *types* it operates on live in `sc-config`.

pub mod state;

pub use state::{KeyBindings, PressOutcome, PressTracker};
