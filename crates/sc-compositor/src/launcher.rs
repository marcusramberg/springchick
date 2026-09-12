//! App launching: resolve a catalog entry to a command line and spawn it.

use sc_catalog::{launch_command, AppEntry};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use tracing::{error, info, warn};

/// Parent slice for app scopes. Exists in every systemd user manager.
const APP_SLICE: &str = "app.slice";

/// Spawn a bare Exec line (our own bundled helpers, not a catalog entry).
pub fn spawn_exec(exec: &str, wayland_display: &str, token: &str) -> Option<Child> {
    let entry = AppEntry {
        exec: exec.to_string(),
        ..Default::default()
    };
    spawn_app(&entry, wayland_display, token, None)
}

/// Whether launches can be wrapped in a transient systemd scope: there has to
/// be a systemd user manager to talk to. Absent in the nested-winit dev setup
/// on a non-systemd host and in a bare container, where launching must still
/// work — so this gates the wrap rather than failing the launch.
fn scopes_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") else {
            return false;
        };
        // The socket `systemctl --user` itself connects to.
        std::path::Path::new(&dir).join("systemd/private").exists()
    })
}

/// Name for the transient scope of one launch.
///
/// Scoped by our own pid so a restarted compositor cannot collide with a scope
/// left behind by its predecessor (systemd refuses a duplicate unit name while
/// the old one is alive), and by a counter so two launches of the same app
/// differ. Only `[A-Za-z0-9:_.\-]` survives from the app id; systemd rejects the
/// rest.
fn scope_name(app_id: &str, pid: u32, seq: u64) -> String {
    let id: String = app_id
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | ':' | '_' | '.' | '-' => c,
            _ => '_',
        })
        .take(64)
        .collect();
    format!("springchick-{pid}-{seq}-{id}.scope")
}

/// A scope name for the next launch of `app_id`, or `None` when there is no
/// user manager to register it with.
pub fn next_scope(app_id: &str) -> Option<String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    scopes_available().then(|| {
        scope_name(
            app_id,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        )
    })
}

/// Spawn a Wayland client for `entry`, pointing at our socket.
///
/// `token` is an xdg-activation token minted for this launch: a client that
/// hands it back names the launch it came from, which is how the shell tags the
/// window with the right app (see [`crate::provenance`]). Both the Wayland and
/// the X11-era spelling are set, since toolkits read one or the other.
///
/// `scope` (from [`next_scope`]) wraps the launch in a transient systemd scope
/// under [`APP_SLICE`], so the app *and everything it forks* end up in one
/// cgroup that resource limits can be applied to. `systemd-run --scope` execs in
/// the caller's context, so the environment set below still reaches the app and
/// the pid we get back is the app's own — attribution and reaping are unchanged.
pub fn spawn_app(
    entry: &AppEntry,
    wayland_display: &str,
    token: &str,
    scope: Option<&str>,
) -> Option<Child> {
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

    info!(program, ?args, wayland_display, scope, "launching app");

    let spawn = |scope: Option<&str>| {
        let mut builder = match scope {
            Some(unit) => {
                let mut b = Command::new("systemd-run");
                b.args([
                    "--user",
                    "--scope",
                    "--quiet",
                    // Reap the unit when it fails, instead of leaving a failed
                    // scope behind that nothing will ever `reset-failed`.
                    "--collect",
                    &format!("--slice={APP_SLICE}"),
                    &format!("--unit={unit}"),
                    "--",
                ]);
                b.arg(program);
                b
            }
            None => Command::new(program),
        };
        if let Some(cwd) = &command.cwd {
            builder.current_dir(cwd);
        }
        builder
            .args(args)
            .env("WAYLAND_DISPLAY", wayland_display)
            .env("GDK_BACKEND", "wayland")
            .env("QT_QPA_PLATFORM", "wayland")
            .env("XDG_ACTIVATION_TOKEN", token)
            .env("DESKTOP_STARTUP_ID", token)
            // ensure zwp_text_input_v3 works.
            .env_remove("QT_IM_MODULE")
            .env_remove("DISPLAY") // prevent X11 fallback
            .spawn()
    };

    match spawn(scope) {
        Ok(child) => Some(child),
        // No systemd-run on PATH despite a user manager being there. Losing the
        // scope costs resource control, not the launch. A systemd-run that runs
        // but *fails* (a name collision, a refused unit) can't be caught here;
        // it surfaces as a launch that exits without mapping, which
        // `poll_launching` already handles.
        Err(e) if scope.is_some() => {
            warn!(%e, "systemd-run unavailable; launching without a scope");
            spawn(None)
                .inspect_err(|e| error!(%e, program, "failed to spawn app"))
                .ok()
        }
        Err(e) => {
            error!(%e, program, "failed to spawn app");
            None
        }
    }
}

// Exec parsing is covered by sc-catalog's unit tests, where parse_exec lives.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_names_are_unique_and_systemd_safe() {
        let a = scope_name("org.gnome.Fractal", 42, 0);
        assert_eq!(a, "springchick-42-0-org.gnome.Fractal.scope");
        // Same app, same compositor: the counter separates them.
        assert_ne!(a, scope_name("org.gnome.Fractal", 42, 1));
        // Same app, restarted compositor: the pid separates them.
        assert_ne!(a, scope_name("org.gnome.Fractal", 43, 0));
    }

    #[test]
    fn scope_name_sanitizes_app_id() {
        assert_eq!(
            scope_name("weird id/with@chars", 1, 2),
            "springchick-1-2-weird_id_with_chars.scope"
        );
    }
}
