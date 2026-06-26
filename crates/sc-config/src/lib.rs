#![forbid(unsafe_code)]
pub mod state;
pub mod catalog;
pub use catalog::{AppEntry, parse_desktop};
