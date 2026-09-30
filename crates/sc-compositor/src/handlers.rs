//! smithay protocol handlers and dispatch glue.

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{PopupKind, PopupManager};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{IsAlive, Rectangle, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, CompositorClientState, CompositorHandler, CompositorState,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::fractional_scale::{with_fractional_scale, FractionalScaleHandler};
use smithay::wayland::image_capture_source::{
    ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
    OutputCaptureSourceState,
};
use smithay::wayland::image_copy_capture::{
    BufferConstraints, Frame as CaptureFrame, ImageCopyCaptureHandler, ImageCopyCaptureState,
    Session as CaptureSession, SessionRef as CaptureSessionRef,
};
use smithay::wayland::input_method::{InputMethodHandler, PopupSurface as ImePopupSurface};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::wlr_data_control::{
    DataControlHandler as WlrDataControlHandler, DataControlState as WlrDataControlState,
};
use smithay::wayland::selection::ext_data_control::{DataControlHandler, DataControlState};
use smithay::wayland::selection::{SelectionHandler, SelectionTarget};
use smithay::wayland::shell::xdg::dialog::{ToplevelDialogHint, XdgDialogHandler};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::wayland::pointer_constraints::PointerConstraintsHandler;

use tracing::{debug, info, warn};

use crate::state::{ClientState, State};

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // Frame callbacks are only sent during a render, so an idle screen must be
        // woken or the client stalls.
        if self.commit_affects_frame(surface) {
            self.needs_render = true;
        }

        self.popups.commit(surface);

        if self.layers.handle_commit(surface) {
            self.recompute_layers();
        }

        self.log_toplevel_size(surface);

        // A new wp_content_type tag drives the auto-landscape hint.
        if self.app_focus_surface().as_ref() == Some(surface) {
            self.refresh_landscape_hint();
            // The first commit at the turned size ends the rotation fade.
            self.note_rotation_commit(surface);
        }
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.configure_maximized(&surface);
        self.register_toplevel(surface);
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        // The real app_id arrives after map; retag and record frecency now.
        self.resolve_app_id(&surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unregister_toplevel(surface.wl_surface());
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        // `set_parent` arrives after the first configure, so a dialog was
        // configured as a top-level app. Reconfigure to restore its CSD buttons.
        self.configure_maximized(&surface);
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        // Covers the whole output, which is the only way to hide the OSK's
        // exclusive zone. The size is the rotated one, but `self.rotation` waits
        // for a landscape buffer; see [`crate::rotation`].
        self.configure_fullscreen(&surface);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        // `refresh_rotation` turns back once the portrait size is committed.
        self.configure_maximized(&surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // wvkbd ignores all input until its popup is configured. Unconstrain
        // against the on-screen area so low menus flip instead of being clamped
        // over the app's chrome.
        let target = self.popup_target(&PopupKind::Xdg(surface.clone()));
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_unconstrained_geometry(target);
        });
        if let Err(e) = surface.send_configure() {
            warn!(?e, "failed to configure popup");
        }
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            warn!(?e, "failed to track popup");
        }
        self.needs_render = true;
    }

    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        self.popup_grabs.insert(surface.wl_surface().clone());
        // Only grabbing popups capture touch and dismiss on an outside press.
        //
        // Don't cancel the press that opened the grab: its `up` belongs to the
        // surface that got its `down`. Cancelling made Firefox menus flicker shut.
        // Dismissal happens on the next outside press (`popup_press`).
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        let target = self.popup_target(&PopupKind::Xdg(surface.clone()));
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_unconstrained_geometry(target);
        });
        surface.send_repositioned(token);
        if let Err(e) = surface.send_configure() {
            warn!(?e, "failed to configure repositioned popup");
        }
        self.needs_render = true;
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        self.popup_grabs.remove(surface.wl_surface());
        self.needs_render = true;
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    /// Without this no client gets a `wl_data_offer` and copy/paste does nothing.
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = self.dh.clone();
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(&dh, seat, client.clone());
        set_primary_focus(&dh, seat, client);
    }
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Formats come from the renderer's importable set; import happens lazily.
        let _ = notifier.successful::<State>();
    }
}

impl SelectionHandler for State {
    /// We only own the selection for a screenshot, so this is the PNG.
    type SelectionUserData = std::sync::Arc<Vec<u8>>;

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        data: &Self::SelectionUserData,
    ) {
        crate::screenshot::serve(fd, data.clone());
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DataControlHandler for State {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}

impl WlrDataControlHandler for State {
    fn data_control_state(&mut self) -> &mut WlrDataControlState {
        &mut self.wlr_data_control_state
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

// No server-initiated DnD; the default cancels the source.
impl WaylandDndGrabHandler for State {}

impl OutputHandler for State {}

impl FractionalScaleHandler for State {
    /// Single output, so one send at creation suffices.
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.dpi;
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // The decoration VM test greps this line.
        debug!(target: "springchick::debug", "xdg-decoration negotiated");
        self.apply_decoration(&toplevel, None);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        self.apply_decoration(&toplevel, Some(mode));
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.apply_decoration(&toplevel, None);
    }
}

impl XdgDialogHandler for State {
    fn dialog_hint_changed(&mut self, toplevel: ToplevelSurface, _hint: ToplevelDialogHint) {
        // The dialog hint postdates the first configure, as in `parent_changed`.
        self.configure_maximized(&toplevel);
    }
}

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // A mapped window is raised. Otherwise the token is identity: it names
        // the launch. The surface may not be registered yet, so park it for
        // `register_toplevel`.
        if self.raise_activated_surface(&surface) {
            return;
        }
        self.pending_activation
            .insert(surface, token.as_str().to_string());
    }
}

impl ImageCaptureSourceHandler for State {}

impl OutputCaptureSourceHandler for State {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ImageCopyCaptureHandler for State {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        let output = source
            .user_data()
            .get::<smithay::output::WeakOutput>()?
            .upgrade()?;
        let mode = output.current_mode()?;
        let size = mode
            .size
            .to_logical(1)
            .to_buffer(1, smithay::utils::Transform::Normal);
        // dmabuf when the DRM backend supplied formats, shm always.
        let dma = self.capture_formats.as_ref().map(|(node, formats)| {
            smithay::wayland::image_copy_capture::DmabufConstraints {
                node: *node,
                formats: formats.clone(),
            }
        });
        Some(BufferConstraints {
            size,
            shm: vec![
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888,
            ],
            dma,
        })
    }

    fn new_session(&mut self, session: CaptureSession) {
        // Dropping a `Session` sends `stopped`; keep it for the capture's lifetime.
        self.capture_sessions.retain(|s| s.alive());
        self.capture_sessions.push(session);
    }

    fn frame(&mut self, _session: &CaptureSessionRef, frame: CaptureFrame) {
        // The render loop owns the renderer and does `success`/`fail`.
        self.pending_captures.push(frame);
        self.needs_render = true;
    }
}

impl InputMethodHandler for State {
    fn new_popup(&mut self, surface: ImePopupSurface) {
        // Parented to the focused app surface, so `app_popups()` finds it.
        if let Err(e) = self.popups.track_popup(PopupKind::from(surface)) {
            warn!(?e, "failed to track input-method popup");
        }
        self.needs_render = true;
    }

    fn dismiss_popup(&mut self, surface: ImePopupSurface) {
        if let Some(parent) = surface.get_parent().map(|p| p.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::from(surface));
        }
        self.needs_render = true;
    }

    fn popup_repositioned(&mut self, _surface: ImePopupSurface) {}

    fn parent_geometry(&self, _parent: &WlSurface) -> Rectangle<i32, smithay::utils::Logical> {
        // Logical coords, so the IME can place its popup over the field.
        let u = self.layers.usable(self.dpi);
        Rectangle::from_size(
            (
                (u.w as f64 / self.dpi).round() as i32,
                (u.h as f64 / self.dpi).round() as i32,
            )
                .into(),
        )
    }
}

impl smithay::wayland::shell::wlr_layer::WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut smithay::wayland::shell::wlr_layer::WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: smithay::wayland::shell::wlr_layer::LayerSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        layer: smithay::wayland::shell::wlr_layer::Layer,
        namespace: String,
    ) {
        info!(%namespace, ?layer, "new layer surface");
        // Holds the last buffer before a null commit resets it, so the OSK
        // dismissal can animate.
        smithay::wayland::compositor::add_pre_commit_hook::<State, _>(
            surface.wl_surface(),
            |state, _dh, surface| {
                state.layers.note_hide(surface, state.dpi);
            },
        );
        self.layers.new_surface(surface, namespace);
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        if self.layers.destroyed(&surface, self.dpi) {
            self.recompute_layers();
        }
        // A destroy is not a commit; the OSK slide-out needs a frame.
        self.needs_render = true;
    }
}

/// Required by `PointerTarget for WlSurface`; no global is advertised.
impl PointerConstraintsHandler for State {}

// Hand-rolled `Dispatch for State` modules (gamma_control, wlr_screencopy,
// idle_notify) use disjoint interfaces and don't overlap this.
smithay::delegate_dispatch2!(State);
