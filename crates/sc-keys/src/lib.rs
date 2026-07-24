#![forbid(unsafe_code)]
//! Keybinding config and short/long press logic for springchick.
//!
//! Pure logic: no wayland, no xkb, no clock of its own. The compositor resolves
//! keysym names and supplies `Instant`s, so every rule here is unit-testable.

pub mod config;
pub mod state;

pub use config::{Action, Binding, Config, ModMask, PressKind};
pub use state::{KeyBindings, PressOutcome, PressTracker};
