//! smithay protocol handler impls for [`State`], plus the `delegate_*` glue.
//!
//! These are the callbacks Wayland clients drive; the policy they invoke lives
//! in [`crate::toplevel`], [`crate::frame`], and the per-protocol modules.

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
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::dialog::{ToplevelDialogHint, XdgDialogHandler};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};

use smithay::delegate_compositor;
use smithay::delegate_data_device;
use smithay::delegate_input_method_manager;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_text_input_manager;
use smithay::delegate_xdg_decoration;
use smithay::delegate_xdg_dialog;
use smithay::delegate_xdg_shell;

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

        // A client presented new content; ask the DRM loop to render. Without
        // this, an app committing while the screen is otherwise idle never
        // gets its frame callback (only sent during a render), so it stalls.
        self.needs_render = true;

        // Advance popup configure/geometry state (initial configure, acks,
        // reposition) for any tracked popup in this surface's tree.
        self.popups.commit(surface);

        // A layer surface committing may change its geometry or reserved area.
        // `handle_commit` arranges the map (map/unmap + configures); we then
        // resize apps if the usable area changed.
        if self.layers.handle_commit(surface) {
            self.recompute_layers();
        }

        // Report a toplevel's committed size against the space it was given.
        // Cheap: it early-returns unless this is a tracked toplevel whose
        // geometry actually changed.
        self.log_toplevel_size(surface);

        // A commit can carry a new wp_content_type tag (playback started or
        // stopped), which is what the auto-landscape hint keys off.
        if self.app_focus_surface().as_ref() == Some(surface) {
            self.refresh_landscape_hint();
            // ...and it may be the first commit at the size a turn configured,
            // which is what ends the rotation fade's dark stretch.
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
        // Clients set the xdg `app_id` after `new_toplevel`/map, so the id
        // captured in `register_toplevel` is a `unknown_N` placeholder. Now
        // that the real id has arrived, match it against the catalog and, on a
        // hit, retag the stored toplevel + live UI and record frecency (which
        // register could not — see the note there).
        self.resolve_app_id(&surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unregister_toplevel(surface.wl_surface());
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        // `new_toplevel` fires on the `get_toplevel` request, before the client
        // has sent `set_parent`, so its first configure always saw `parent() ==
        // None` and treated even a dialog as a top-level app (→ server-side, no
        // header bar). Once the parent arrives we know it's a child toplevel, so
        // reconfigure to flip the decoration policy and restore the toolkit's
        // action buttons (e.g. a GTK file chooser's Open/Cancel).
        self.configure_maximized(&surface);
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        // A client (video player, game) asked to go truly immersive. Cover the
        // whole output — not just the usable area — force server-side (= no)
        // decorations, and set the Fullscreen state so the toolkit hides its own
        // chrome. This is the only path that hides the OSK's exclusive zone.
        //
        // Fullscreen also means landscape, so the size we hand the client is the
        // rotated (swapped) one. `self.rotation` is NOT set here: rendering only
        // turns once the client has acked this configure and committed a
        // landscape buffer, otherwise the app would be drawn sideways at its old
        // portrait size for a frame or two. See [`crate::rotation`].
        self.configure_fullscreen(&surface);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        // Back to the normal maximized app state; `refresh_rotation` turns the
        // display back to portrait once the client commits the portrait size.
        self.configure_maximized(&surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // Send the initial configure. wvkbd (and other layer-shell OSKs) create
        // an xdg_popup child and ignore ALL input until that popup is
        // configured, so without this the on-screen keyboard never registers a
        // tap.
        //
        // Honour the client's constraint_adjustment against the on-screen area
        // (flip/slide/resize) rather than `get_geometry()`'s raw anchor result —
        // otherwise a menu anchored low is configured off the bottom edge and
        // our render-time clamp drags it back over the app's own chrome.
        let target = self.popup_target(&PopupKind::Xdg(surface.clone()));
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_unconstrained_geometry(target);
        });
        if let Err(e) = surface.send_configure() {
            warn!(?e, "failed to configure popup");
        }
        // Track the popup so it gets rendered, hit-tested, and dismissed. Its
        // configure/commit lifecycle and geometry are then advanced in `commit`.
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            warn!(?e, "failed to track popup");
        }
        self.needs_render = true;
    }

    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        self.popup_grabs.insert(surface.wl_surface().clone());
        // Marked modal above: only grabbing popups capture touch and dismiss on
        // an outside press (see `active_popups`). Non-grab popups (wvkbd's hack
        // popup, tooltips) never enter that set, so they can't swallow taps.
        //
        // A client opens a popup grab from within an in-flight press (menus —
        // e.g. Firefox — grab on the button's press/release that opened them).
        // Do NOT cancel or retarget that sequence: per wl_touch/xdg-shell grab
        // semantics the originating press belongs to the surface that received
        // its `down`, and its matching `up` must be delivered there as normal.
        // The client owns the grab and won't treat that release as an
        // outside-dismiss. Cancelling it (as we used to) made Firefox read the
        // gesture as aborted and flicker the menu closed the instant it opened.
        // We dismiss manually on the *next* outside press (see `popup_press`),
        // never off the opening sequence's release.
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
        // smithay has already removed it from `known_popups`; drop our render of
        // it on the next frame and forget any grab it held.
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

    /// Point the selection devices at the newly focused client. Without this a
    /// client never receives a `wl_data_offer`, so copy/paste silently does
    /// nothing even though the globals are advertised.
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
        // The advertised formats come straight from the backend renderer's
        // importable set, so accept optimistically; the shared render path
        // imports the buffer lazily when it first composites the surface.
        let _ = notifier.successful::<State>();
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
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

// Phone shell: no server-initiated DnD. The default `dnd_requested` cancels the
// source, which is what we want.
impl WaylandDndGrabHandler for State {}

impl OutputHandler for State {}

impl FractionalScaleHandler for State {
    /// A client bound `wp_fractional_scale` for a surface: tell it to render at
    /// `dpi`. Constant here (single output), so one send at creation suffices.
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
        // A client created an xdg-decoration object — i.e. it speaks the protocol
        // and will honor whatever mode we hand back (Qt does; GTK never gets
        // here). Logged so the decoration nix test can tell "negotiated" from
        // "self-decorated" apart.
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
        // A client (often a portal file chooser with no in-process parent) just
        // flagged this toplevel as a dialog/modal. Like `parent_changed`, the
        // initial configure predates the hint, so reconfigure to flip the
        // decoration policy to client-side and restore its action buttons.
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
        // Springboard decides what is in front, so an activation request never
        // raises anything by itself. What it is good for is identity: a client
        // presenting a token we minted at spawn time names the launch it came
        // from, which beats guessing from its xdg `app_id`. The surface may not
        // be a registered toplevel yet (clients commonly activate before their
        // first commit), so park it for `register_toplevel` to claim.
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
        // Stash the output on the source so `capture_constraints` can recover it.
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ImageCopyCaptureHandler for State {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        // Only our output is a valid source; recover it from the source's data.
        let output = source
            .user_data()
            .get::<smithay::output::WeakOutput>()?
            .upgrade()?;
        let mode = output.current_mode()?;
        let size = mode
            .size
            .to_logical(1)
            .to_buffer(1, smithay::utils::Transform::Normal);
        // Advertise dmabuf (zero-copy blit) when the DRM backend supplied formats,
        // plus shm as a universal fallback so a client can always allocate.
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
        // `Session` is owned: dropping it sends `stopped` and fails every frame
        // the client asks for, so it must be kept alive for the capture's
        // lifetime. Drop the ones whose client object died while we're here.
        self.capture_sessions.retain(|s| s.alive());
        self.capture_sessions.push(session);
    }

    fn frame(&mut self, _session: &CaptureSessionRef, frame: CaptureFrame) {
        // Defer the actual capture to the render loop, which owns the renderer and
        // the just-composited scene. `success`/`fail` happens there.
        self.pending_captures.push(frame);
        self.needs_render = true;
    }
}

impl InputMethodHandler for State {
    fn new_popup(&mut self, surface: ImePopupSurface) {
        // Track the IME popup like an xdg popup so it renders + hit-tests. It is
        // parented to the focused app surface, so `app_popups()` picks it up
        // automatically (PopupManager::popups_for_surface yields all kinds).
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
        // Apps are fullscreen within the usable area; report it in logical coords
        // (client space) so the IME positions its popup over the focused field.
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
        // smithay's LayerMap tracks geometry + reservations and sends the
        // initial configure on the surface's first commit.
        self.layers.new_surface(surface, namespace);
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        if self.layers.destroyed(&surface) {
            self.recompute_layers();
        }
    }
}

delegate_compositor!(State);
smithay::delegate_dmabuf!(State);
delegate_xdg_shell!(State);
delegate_xdg_dialog!(State);
delegate_seat!(State);
delegate_shm!(State);
delegate_data_device!(State);
smithay::delegate_ext_data_control!(State);
smithay::delegate_data_control!(State);
smithay::delegate_primary_selection!(State);
delegate_output!(State);
delegate_xdg_decoration!(State);
smithay::delegate_layer_shell!(State);
smithay::delegate_virtual_keyboard_manager!(State);
smithay::delegate_fractional_scale!(State);
smithay::delegate_viewporter!(State);
delegate_text_input_manager!(State);
delegate_input_method_manager!(State);
smithay::delegate_image_capture_source!(State);
smithay::delegate_output_capture_source!(State);
smithay::delegate_image_copy_capture!(State);
smithay::delegate_content_type!(State);
smithay::delegate_xdg_activation!(State);
