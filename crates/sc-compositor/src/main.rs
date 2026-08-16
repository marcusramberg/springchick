//! springchick compositor — a phone shell on smithay.
//!
//! Module map:
//! - [`state`] — the central `State` struct and its construction.
//! - [`handlers`] — smithay protocol handler impls + `delegate_*` glue.
//! - [`toplevel`] — app window lifecycle, focus, decoration, rotation.
//! - [`arrange`] — home-grid reflow springs + arrange-mode drag.
//! - [`frame`] — per-frame shell advance and the render snapshot.
//! - [`winit_backend`] / [`drm_backend`] — the two ways to present it.
//! - [`session`] — Wayland display/socket plumbing shared by both backends.

mod app_history;
mod arrange;
mod backend;
mod background_effect;
mod bar_hint;
mod blank;
mod capture;
mod content_type;
mod debug_input;
mod drm_backend;
mod frame;
mod frame_stats;
mod gamma_control;
mod handlers;
mod icon_menu;
mod idle_inhibit;
mod idle_notify;
mod input_common;
mod input_dispatch;
mod ipc;
mod kbd_switch;
mod keybinds;
mod launcher;
mod layer_shell;
mod mirror;
mod osd;
mod popups;
mod provenance;
mod render;
mod rotation;
pub mod scene;
mod sensor;
mod session;
mod session_lock;
mod skia_gl;
mod sleep;
mod state;
mod switcher;
mod toplevel;
mod touch;
mod touch_viz;
mod uclamp;
pub mod ui_state;
mod winit_backend;
mod wlr_screencopy;

// Re-exported so sibling modules can keep using the short `crate::State` path.
pub(crate) use arrange::{DragItem, IconPress};
pub(crate) use session::{accept_client, create_display, publish_wayland_display};
pub(crate) use state::{AppToplevel, FramePrep, State};

use tracing::info;

fn main() -> std::process::ExitCode {
    // `springchick ipc <verb> [args...]` runs as a control-socket client against
    // a running compositor instead of starting one. Handled before tracing so
    // the client's stdout stays clean.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("ipc") {
        return ipc::run_client(&args[2..]);
    }

    init_tracing();
    match backend::BackendKind::from_env() {
        backend::BackendKind::Winit => {
            info!("springchick M4 — winit dev backend");
            winit_backend::run_winit();
        }
        backend::BackendKind::Drm => {
            info!("springchick M4 — DRM device backend");
            drm_backend::run_drm();
        }
    }
    std::process::ExitCode::SUCCESS
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sc_compositor=debug"));
    let _ = fmt().with_env_filter(filter).try_init();
}
