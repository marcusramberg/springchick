use sc_catalog::{launch_command, AppEntry};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use tracing::{error, info, warn};

const APP_SLICE: &str = "app.slice";

/// Spawn a bare Exec line (our own bundled helpers).
pub fn spawn_exec(exec: &str, wayland_display: &str, token: &str) -> Option<Child> {
    let entry = AppEntry {
        exec: exec.to_string(),
        ..Default::default()
    };
    spawn_app(&entry, wayland_display, token, None)
}

/// Needs a systemd user manager; absent nested on non-systemd hosts, where
/// launches still go unscoped.
fn scopes_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") else {
            return false;
        };
        std::path::Path::new(&dir).join("systemd/private").exists()
    })
}

/// Includes our pid (systemd refuses a duplicate name while a predecessor's
/// scope lives) and a counter. Only `[A-Za-z0-9:_.\-]` survives from the id.
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

/// `token` is the xdg-activation token that ties the window back to this
/// launch ([`crate::provenance`]); set under both spellings.
///
/// `scope` puts the app and its children in one cgroup under [`APP_SLICE`].
/// `systemd-run --scope` execs in place, so env and pid are the app's own.
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
                    // Don't leave failed scopes nobody will `reset-failed`.
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
            // Needed for zwp_text_input_v3.
            .env_remove("QT_IM_MODULE")
            .env_remove("DISPLAY")
            .spawn()
    };

    match spawn(scope) {
        Ok(child) => Some(child),
        // No systemd-run on PATH: launch unscoped. A systemd-run that fails later
        // shows up as an unmapped launch, which `poll_launching` handles.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_names_are_unique_and_systemd_safe() {
        let a = scope_name("org.gnome.Fractal", 42, 0);
        assert_eq!(a, "springchick-42-0-org.gnome.Fractal.scope");
        assert_ne!(a, scope_name("org.gnome.Fractal", 42, 1));
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
