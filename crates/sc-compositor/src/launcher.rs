//! App launching: resolve a catalog entry to a command line and spawn it.

use sc_catalog::{launch_command, AppEntry};
use std::process::{Child, Command};
use tracing::{error, info};

/// Spawn a bare Exec line (our own bundled helpers, not a catalog entry).
pub fn spawn_exec(exec: &str, wayland_display: &str) -> Option<Child> {
    let entry = AppEntry {
        exec: exec.to_string(),
        ..Default::default()
    };
    spawn_app(&entry, wayland_display)
}

/// Spawn a Wayland client for `entry`, pointing at our socket.
pub fn spawn_app(entry: &AppEntry, wayland_display: &str) -> Option<Child> {
    let Some(command) = launch_command(entry) else {
        error!(
            id = entry.id,
            exec = entry.exec,
            terminal = entry.terminal,
            "nothing runnable for entry (empty exec, or terminal app with no terminal emulator)"
        );
        return None;
    };
    let Some((program, args)) = command.argv.split_first() else {
        error!(id = entry.id, "empty exec line after stripping field codes");
        return None;
    };

    info!(program, ?args, wayland_display, "launching app");

    let mut builder = Command::new(program);
    if let Some(cwd) = &command.cwd {
        builder.current_dir(cwd);
    }
    match builder
        .args(args)
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        // ensure zwp_text_input_v3 works.
        .env_remove("QT_IM_MODULE")
        .env_remove("DISPLAY") // prevent X11 fallback
        .spawn()
    {
        Ok(child) => Some(child),
        Err(e) => {
            error!(%e, program, "failed to spawn app");
            None
        }
    }
}

// Exec parsing is covered by sc-catalog's unit tests, where parse_exec lives.
