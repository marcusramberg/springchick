#![forbid(unsafe_code)]
pub mod catalog;
pub mod search;
pub mod state;
pub use catalog::{parse_desktop, scan_apps, strip_field_codes, xdg_data_dirs, AppEntry};
pub use search::rank;
