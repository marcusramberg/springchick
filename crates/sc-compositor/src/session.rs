//! Wayland display + socket plumbing shared by both backends: creating the
//! listening socket, publishing `WAYLAND_DISPLAY`, and accepting clients.

use std::sync::Arc;

use smithay::reexports::wayland_server::{Display, ListeningSocket};

use tracing::{debug, info, warn};

use crate::state::{ClientState, State};

/// Create the Wayland display + an auto-bound listening socket. Shared by the
/// winit and DRM backends.
pub(crate) fn create_display(
) -> Result<(Display<State>, ListeningSocket, String), Box<dyn std::error::Error>> {
    let display: Display<State> = Display::new()?;
    let listener = ListeningSocket::bind_auto("springchick", 0..32)?;
    let socket_name = listener
        .socket_name()
        .ok_or("wayland socket has no name")?
        .to_string_lossy()
        .to_string();
    info!(%socket_name, "wayland socket listening");
    Ok((display, listener, socket_name))
}

/// Publish the compositor's Wayland socket so clients can find it.
///
/// `WAYLAND_DISPLAY` goes into our own environment so directly-spawned children
/// (launched apps, keybinding commands) inherit it. When running as a real
/// session (`import_to_systemd`), it is also pushed into the systemd/dbus user
/// activation environment so user services — e.g. the on-screen keyboard
/// `wvkbd-mobintl` — connect to us instead of failing with "Failed to create
/// display". The winit dev backend skips the systemd import so it does not
/// clobber the host session's value.
pub(crate) fn publish_wayland_display(socket_name: &str, import_to_systemd: bool) {
    std::env::set_var("WAYLAND_DISPLAY", socket_name);
    if !import_to_systemd {
        return;
    }
    for (program, args) in [
        (
            "systemctl",
            vec!["--user", "import-environment", "WAYLAND_DISPLAY"],
        ),
        (
            "dbus-update-activation-environment",
            vec!["--systemd", "WAYLAND_DISPLAY"],
        ),
    ] {
        match std::process::Command::new(program).args(&args).status() {
            Ok(s) if s.success() => {}
            Ok(s) => warn!(program, code = ?s.code(), "activation-environment update failed"),
            Err(e) => warn!(%e, program, "could not run activation-environment update"),
        }
    }
}

/// Accept one pending client on the listener, if any.
pub(crate) fn accept_client(display: &Display<State>, listener: &ListeningSocket) {
    if let Some(stream) = listener.accept().ok().flatten() {
        debug!("new wayland client connected");
        let _ = display
            .handle()
            .insert_client(stream, Arc::new(ClientState::default()));
    }
}
