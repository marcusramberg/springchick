use std::sync::Arc;

use smithay::reexports::wayland_server::{Display, ListeningSocket};

use tracing::{debug, info, warn};

use crate::state::{ClientState, State};

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

/// Export `WAYLAND_DISPLAY` to our children, and with `import_to_systemd` to
/// the systemd/dbus activation env so user services (wvkbd) find us. winit
/// skips the import so it doesn't clobber the host session.
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

pub(crate) fn accept_client(display: &Display<State>, listener: &ListeningSocket) {
    if let Some(stream) = listener.accept().ok().flatten() {
        debug!("new wayland client connected");
        let _ = display
            .handle()
            .insert_client(stream, Arc::new(ClientState::default()));
    }
}
