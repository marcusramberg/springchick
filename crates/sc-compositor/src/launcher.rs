//! App launching: strip field codes from Exec lines and spawn processes.

use sc_config::strip_field_codes;
use std::process::{Child, Command};
use tracing::{error, info};

/// Spawn a Wayland client with the given exec line, pointing at our socket.
pub fn spawn_app(exec: &str, wayland_display: &str) -> Option<Child> {
    let clean = strip_field_codes(exec);
    let parts: Vec<&str> = clean.split_whitespace().collect();
    if parts.is_empty() {
        error!("empty exec line after stripping field codes");
        return None;
    }

    let program = parts[0];
    let args = &parts[1..];

    info!(program, ?args, wayland_display, "launching app");

    match Command::new(program)
        .args(args)
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_basic_field_codes() {
        assert_eq!(strip_field_codes("gnome-maps %U"), "gnome-maps");
        assert_eq!(strip_field_codes("firefox %u %F"), "firefox");
        assert_eq!(
            strip_field_codes("env VAR=1 app %f --flag"),
            "env VAR=1 app --flag"
        );
    }

    #[test]
    fn strip_preserves_no_code_lines() {
        assert_eq!(strip_field_codes("foot"), "foot");
        assert_eq!(strip_field_codes("app --arg val"), "app --arg val");
    }

    #[test]
    fn strip_consecutive_codes() {
        assert_eq!(strip_field_codes("%i%c%k app"), "app");
    }
}
