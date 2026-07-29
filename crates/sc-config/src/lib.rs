#![forbid(unsafe_code)]
pub mod catalog;
pub mod state;
pub use catalog::{parse_desktop, xdg_data_dirs, AppEntry};
