#![forbid(unsafe_code)]
pub mod catalog;
pub mod search;
pub use catalog::{
    launch_command, parse_desktop, parse_desktop_in, parse_exec, parse_exec_with, scan_apps,
    xdg_data_dirs, AppEntry, DesktopEnv, ExecContext, LaunchCommand,
};
pub use search::rank;
