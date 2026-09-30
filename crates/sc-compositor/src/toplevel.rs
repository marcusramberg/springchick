//! App window lifecycle: register/close, launch/raise, keyboard focus,
//! decorations, and rotation.

use smithay::desktop::PopupManager;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::shell::xdg::dialog::ToplevelDialogHint;
use smithay::wayland::shell::xdg::{SurfaceCachedState, ToplevelSurface, XdgToplevelSurfaceData};
use smithay::wayland::xdg_activation::{XdgActivationToken, XdgActivationTokenData};

use sc_shell_model::persist;

use tracing::{info, warn};

use crate::launcher::{spawn_app, spawn_exec};
use crate::state::{
    AppToplevel, Launching, State, LAUNCH_PULSE_TIMEOUT, SEARCH_APP_EXEC, SEARCH_APP_ID,
};
use crate::ui_state::{self, transition, ToplevelId, UiEvent, UiState, ZoomOrigin};
use crate::{content_type, keybinds, provenance, rotation};

impl State {
    pub(crate) fn handle_return_home(&mut self) {
        // End any keyboard switch, or releasing the modifier would commit a card
        // over the Home the user asked for.
        self.kbd_switch = None;
        let origin = self.home_origin();
        transition(&mut self.ui, UiEvent::ReturnHome { origin });
    }

    /// The foreground app's own icon, not `last_origin`, which may belong to
    /// another app after switching.
    pub(crate) fn home_origin(&self) -> ZoomOrigin {
        let app_id = match self
            .ui
            .foreground_toplevel()
            .and_then(|tid| self.toplevels.get(tid))
            .and_then(|s| s.as_ref())
        {
            Some(tl) => tl.app_id.clone(),
            None => return self.last_origin,
        };
        let (w, h) = self.output_size_f();
        let layout = sc_layout::compute(w, h, 0, &self.model);
        layout
            .grid
            .iter()
            .chain(layout.dock.iter())
            .find(|s| s.app_id == app_id)
            .map(|s| ZoomOrigin::icon((s.icon_rect.center_x(), s.icon_rect.center_y())))
            .unwrap_or_else(|| ZoomOrigin::icon((w / 2.0, h / 2.0)))
    }

    /// Keyboard equivalent of `fullscreen_request`. Only sends the configure;
    /// rotation and the OSK zone follow the commit, as for the client path.
    pub(crate) fn toggle_fullscreen(&mut self) {
        let Some(surface) = self.foreground_toplevel_surface() else {
            return;
        };
        if self.foreground_is_fullscreen() {
            self.configure_maximized(&surface);
        } else {
            self.configure_fullscreen(&surface);
        }
        self.needs_render = true;
    }

    /// Raises it instead if already running.
    pub(crate) fn open_search(&mut self) {
        self.search_arm = None;
        self.cancel_page_drag();
        self.pending_launch = None;
        for (idx, slot) in self.toplevels.iter().enumerate() {
            if slot.as_ref().is_some_and(|tl| tl.app_id == SEARCH_APP_ID) {
                self.raise_toplevel_centered(idx, false);
                return;
            }
        }
        // No token: identified by spawn intent (`expecting_search`).
        if let Some(child) = spawn_exec(SEARCH_APP_EXEC, &self.wayland_socket, "") {
            self.children.push(child);
            self.expecting_search = true;
        }
        self.needs_render = true;
    }

    /// Takes a window, not an app: with several open, the caller (the icon
    /// menu's per-window rows) is choosing against "most recent".
    pub(crate) fn raise_toplevel(&mut self, tid: ToplevelId, origin: ZoomOrigin) {
        let Some(Some(tl)) = self.toplevels.get(tid) else {
            return;
        };
        let app_id = tl.app_id.clone();
        self.last_origin = origin;
        self.history.push_foreground(tid);
        transition(
            &mut self.ui,
            UiEvent::AppMapped {
                toplevel: tid,
                app_id,
                origin,
                open_mode: ui_state::OpenMode::Zoom,
            },
        );
    }

    pub(crate) fn toplevel_title(&self, tid: ToplevelId) -> String {
        let Some(Some(tl)) = self.toplevels.get(tid) else {
            return String::new();
        };
        smithay::wayland::compositor::with_states(tl.surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| d.title.clone()))
        })
        .unwrap_or_default()
    }

    /// MRU first, so "raise the app" lands on the last-used window. The search
    /// app never enters the history and is absent.
    pub(crate) fn instances(&self, app_id: &str) -> Vec<ToplevelId> {
        self.history
            .stack
            .iter()
            .copied()
            .filter(|id| {
                self.toplevels
                    .get(*id)
                    .and_then(|s| s.as_ref())
                    .is_some_and(|tl| tl.app_id == app_id)
            })
            .collect()
    }

    pub(crate) fn launch_or_raise(&mut self, app_id: &str, origin: ZoomOrigin) {
        self.last_origin = origin;

        // Frecency is recorded at map (`register_toplevel`), so launches that bypass
        // this count too.
        if let Some(&idx) = self.instances(app_id).first() {
            self.history.push_foreground(idx);
            transition(
                &mut self.ui,
                UiEvent::AppMapped {
                    toplevel: idx,
                    app_id: app_id.to_string(),
                    origin,
                    open_mode: ui_state::OpenMode::Zoom,
                },
            );
            return;
        }

        self.spawn_instance(app_id, origin);
    }

    /// Tracked in `launching` for the pulse and for attribution. Several may be
    /// in flight.
    pub(crate) fn spawn_instance(&mut self, app_id: &str, origin: ZoomOrigin) {
        self.last_origin = origin;
        let Some(entry) = self.app_catalog.get(app_id).cloned() else {
            return;
        };
        let token = self.mint_activation_token(app_id);
        let scope = crate::launcher::next_scope(app_id);
        if let Some(child) = spawn_app(&entry, &self.wayland_socket, &token, scope.as_deref()) {
            self.launching.push(Launching {
                app_id: app_id.to_string(),
                pid: child.id() as i32,
                child,
                token,
                started: std::time::Instant::now(),
            });
        }
    }

    /// Tagged with our catalog id, which is what attribution needs back.
    fn mint_activation_token(&mut self, app_id: &str) -> String {
        let data = XdgActivationTokenData {
            app_id: Some(app_id.to_string()),
            ..Default::default()
        };
        let (token, _) = self.xdg_activation_state.create_external_token(data);
        token.as_str().to_string()
    }

    /// Activation token for this surface first, then process ancestry (which
    /// survives wrappers that drop the token). Removing the entry stops the
    /// pulse and prevents a second window claiming it.
    fn claim_launch(&mut self, surface: &WlSurface) -> Option<Launching> {
        if let Some(token) = self.pending_activation.remove(surface) {
            if let Some(i) = self.launching.iter().position(|l| l.token == token) {
                return Some(self.launching.remove(i));
            }
        }
        let pid = surface.client()?.get_credentials(&self.dh).ok()?.pid;
        let chain = provenance::ancestry(pid);
        let pids: Vec<i32> = self.launching.iter().map(|l| l.pid).collect();
        let i = provenance::match_ancestry(&pids, &chain)?;
        Some(self.launching.remove(i))
    }

    /// Reaps failed launches and gives up after [`LAUNCH_PULSE_TIMEOUT`]. Mapped
    /// windows are handled in `register_toplevel`.
    pub(crate) fn poll_launching(&mut self) {
        // Every launch outcome funnels through here, so reap here.
        keybinds::reap(&mut self.children);

        if !self.uninstalling.is_empty() {
            let before = self.uninstalling.len();
            keybinds::reap(&mut self.uninstalling);
            if self.uninstalling.len() < before {
                self.reload_catalog();
                self.needs_render = true;
            }
        }

        let mut done = Vec::new();
        for (i, l) in self.launching.iter_mut().enumerate() {
            let exited = matches!(l.child.try_wait(), Ok(Some(_)) | Err(_));
            let timed_out = l.started.elapsed() >= LAUNCH_PULSE_TIMEOUT;
            if exited || timed_out {
                done.push(i);
            }
        }
        // Back to front so earlier indices stay valid.
        for i in done.into_iter().rev() {
            self.give_up_launch(i);
        }
    }

    /// Forgets the token too, so the pool doesn't grow all session.
    fn give_up_launch(&mut self, i: usize) {
        let l = self.launching.remove(i);
        self.forget_token(&l.token);
        self.children.push(l.child);
    }

    fn forget_token(&mut self, token: &str) {
        self.xdg_activation_state
            .remove_token(&XdgActivationToken::from(token.to_string()));
    }

    pub(crate) fn register_toplevel(&mut self, surface: ToplevelSurface) -> ToplevelId {
        let wl_app_id = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| d.app_id.clone()))
        })
        .unwrap_or_default();

        // The search app: slide-up open, no frecency, not in the switcher. Its
        // app_id isn't set yet, so spawn intent decides.
        let is_search = self.expecting_search || wl_app_id == SEARCH_APP_ID;
        self.expecting_search = false;

        // Identity comes from the launch when one claims this window (see
        // `provenance`). Unclaimed windows get `unknown_N` unless the client id
        // matches the catalog; `resolve_app_id` may still fix them.
        let claimed_app_id = (!is_search)
            .then(|| self.claim_launch(surface.wl_surface()))
            .flatten()
            .map(|l| {
                info!(
                    toplevel = self.toplevels.len(),
                    app_id = %l.app_id, wl_app_id = %wl_app_id,
                    "toplevel attributed to launch"
                );
                self.forget_token(&l.token);
                self.children.push(l.child);
                l.app_id
            });
        let id_from_launch = claimed_app_id.is_some();
        let app_id = if is_search {
            SEARCH_APP_ID.to_string()
        } else if let Some(id) = claimed_app_id {
            id
        } else if !wl_app_id.is_empty() && self.app_catalog.contains_key(&wl_app_id) {
            wl_app_id.clone()
        } else {
            format!("unknown_{}", self.toplevels.len())
        };

        // Otherwise frecency waits for the real app_id in `app_id_changed`.
        if id_from_launch {
            self.record_launch(&app_id);
        }

        // So the client learns the scale and renders HiDPI.
        self.output.enter(surface.wl_surface());

        let id = self.toplevels.len();
        self.toplevels.push(Some(AppToplevel {
            surface,
            app_id: app_id.clone(),
            id_from_launch,
            wl_app_id,
            logged_size: None,
            rotation: rotation::Rotation::None,
        }));

        if !is_search {
            self.history.push_foreground(id);
        }
        let open_mode = if is_search {
            ui_state::OpenMode::SlideUp
        } else {
            ui_state::OpenMode::Zoom
        };
        transition(
            &mut self.ui,
            UiEvent::AppMapped {
                toplevel: id,
                app_id,
                origin: self.last_origin,
                open_mode,
            },
        );

        id
    }

    /// A backgrounded app's commits can't change the frame. waydroid keeps
    /// committing on its own vsync, and rendering each one pins the DRM loop.
    /// Only tracked toplevels are judged; everything else renders.
    pub(crate) fn commit_affects_frame(&self, surface: &WlSurface) -> bool {
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        let owner = self.toplevels.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|tl| tl.surface.wl_surface() == &root)
        });
        commit_needs_frame(owner, &self.drawn_toplevels)
    }

    pub(crate) fn unregister_toplevel(&mut self, surface: &WlSurface) {
        let mut closed = None;
        for (idx, slot) in self.toplevels.iter_mut().enumerate() {
            if let Some(tl) = slot {
                if tl.surface.wl_surface() == surface {
                    // The xdg resource is gone by the time close_toplevel runs.
                    closed = Some((idx, Self::is_dialog(&tl.surface)));
                    *slot = None;
                    break;
                }
            }
        }
        if let Some((id, was_dialog)) = closed {
            self.close_toplevel(id, was_dialog);
        }
    }

    /// `was_dialog`: a dialog (e.g. a portal file chooser, a separate process)
    /// hands the screen back to the app behind it; an app close goes Home.
    pub(crate) fn close_toplevel(&mut self, id: ToplevelId, was_dialog: bool) {
        self.detach_toplevel(id);
        let next = if was_dialog { self.mru_app() } else { None };
        transition(&mut self.ui, UiEvent::ToplevelClosed { toplevel: id, next });
    }

    /// `detach_toplevel` has already pruned the closed id.
    fn mru_app(&self) -> Option<(ToplevelId, String)> {
        let id = *self.history.stack.first()?;
        let tl = self.toplevels.get(id)?.as_ref()?;
        Some((id, tl.app_id.clone()))
    }

    /// Ask the client to close without a UI transition (the card is already
    /// gone). The slot stays until `toplevel_destroyed`: dropping the surface
    /// now would destroy the resource before `close` is flushed.
    pub(crate) fn detach_toplevel(&mut self, id: ToplevelId) {
        if let Some(Some(tl)) = self.toplevels.get(id) {
            tl.surface.send_close();
        }
        self.history.remove(id);
    }

    pub(crate) fn close_front_app(&mut self) {
        let Some(id) = ui_state::desired_focus(&self.ui) else {
            return;
        };
        // Quitting the front app goes Home, even for a dialog.
        self.detach_toplevel(id);
        transition(
            &mut self.ui,
            UiEvent::ToplevelClosed {
                toplevel: id,
                next: None,
            },
        );
    }

    /// Centered zoom. `reorder = false` lets quick-switch browse without
    /// shuffling the MRU order.
    pub(crate) fn raise_toplevel_centered(&mut self, tid: ToplevelId, reorder: bool) {
        let Some(Some(tl)) = self.toplevels.get(tid) else {
            return;
        };
        let app_id = tl.app_id.clone();
        let (w, h) = self.output_size_f();
        self.last_origin = ZoomOrigin::icon((w / 2.0, h / 2.0));
        if reorder {
            self.history.push_foreground(tid);
        }
        transition(
            &mut self.ui,
            UiEvent::RaiseApp {
                toplevel: tid,
                app_id,
            },
        );
    }

    /// xdg-activation of a running app (e.g. a browser handed a URL).
    pub(crate) fn raise_activated_surface(&mut self, surface: &WlSurface) -> bool {
        let Some(tid) = self.toplevels.iter().position(|s| {
            s.as_ref()
                .is_some_and(|t| t.surface.wl_surface() == surface)
        }) else {
            return false;
        };
        self.raise_toplevel_activated(tid);
        true
    }

    /// With another app fullscreen, slide in like a quick-switch; otherwise
    /// zoom from centre.
    pub(crate) fn raise_toplevel_activated(&mut self, tid: ToplevelId) {
        let (current, current_app) = match &self.ui {
            ui_state::UiState::App { toplevel, app_id } if *toplevel != tid => {
                (*toplevel, app_id.clone())
            }
            _ => return self.raise_toplevel_centered(tid, true),
        };
        let Some(Some(tl)) = self.toplevels.get(tid) else {
            return;
        };
        let target = (tid, tl.app_id.clone());
        self.history.push_foreground(tid);
        let mut offset = sc_anim::Spring::new(0.0);
        offset.stiffness = 280.0;
        offset.damping = 32.0;
        offset.retarget(-1.0);
        self.ui = ui_state::UiState::QuickSwitch {
            current,
            current_app,
            prev: None,
            next: Some(target.clone()),
            offset,
            commit: Some(target),
            releasing: true,
            start_x: 0.0,
            origin: sc_input::Pt { x: 0.5, y: 1.0 },
        };
        self.needs_render = true;
    }

    /// The Home-bar rightward swipe. Reorders the MRU stack.
    pub(crate) fn slide_toplevel_from_home(&mut self, tid: ToplevelId) {
        let Some(Some(tl)) = self.toplevels.get(tid) else {
            return;
        };
        let app_id = tl.app_id.clone();
        let (w, h) = self.output_size_f();
        self.last_origin = ZoomOrigin::icon((w / 2.0, h / 2.0));
        self.history.push_foreground(tid);
        transition(
            &mut self.ui,
            UiEvent::AppMapped {
                toplevel: tid,
                app_id,
                // Unused by the slide; kept for the trip home.
                origin: self.last_origin,
                open_mode: ui_state::OpenMode::SlideFromLeft,
            },
        );
    }

    /// See `XdgShellHandler::app_id_changed`.
    pub(crate) fn resolve_app_id(&mut self, surface: &ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        let Some(id) = self.toplevels.iter().position(|s| {
            s.as_ref()
                .is_some_and(|t| t.surface.wl_surface() == &wl_surface)
        }) else {
            return;
        };
        let new_id = smithay::wayland::compositor::with_states(&wl_surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| d.app_id.clone()))
        })
        .unwrap_or_default();
        if let Some(tl) = self.toplevels[id].as_mut() {
            tl.wl_app_id = new_id.clone();
        }
        // Only replace placeholders. A launch-owned id survives whatever the client
        // announces (`foot` for a `Terminal=true` entry).
        if !self.toplevels[id]
            .as_ref()
            .is_some_and(|t| !t.id_from_launch && t.app_id.starts_with("unknown_"))
        {
            return;
        }
        if new_id.is_empty() || !self.app_catalog.contains_key(&new_id) {
            return;
        }
        if let Some(tl) = self.toplevels[id].as_mut() {
            tl.app_id = new_id.clone();
        }
        info!(toplevel = id, app_id = %new_id, "toplevel app_id resolved");
        self.record_launch(&new_id);
        self.ui.retag_app(id, &new_id);
    }

    fn record_launch(&mut self, app_id: &str) {
        self.model
            .frecency
            .record_launch(app_id, sc_shell_model::unix_now());
        if let Err(e) = persist::save(&self.model, &persist::state_path()) {
            warn!(%e, "failed to save shell model after launch");
        }
    }

    /// Resize toplevels when the usable area changes (e.g. OSK up).
    pub(crate) fn recompute_layers(&mut self) {
        // Hold the app's size while a layer slides in, or the keyboard slides up
        // into a bare strip. `advance_frame` calls back when it lands.
        if self.layers.sliding() {
            return;
        }
        if self.layers.usable_changed(self.dpi).is_some() {
            self.reconfigure_toplevels();
            // Re-solve popups so an open menu flips instead of being covered.
            self.reconstrain_popups();
        }
    }

    fn reconfigure_toplevels(&mut self) {
        let usable = self.layers.usable(self.dpi);
        let size = (
            (usable.w as f64 / self.dpi).round() as i32,
            (usable.h as f64 / self.dpi).round() as i32,
        );
        let turned = self.view_rotation().swaps_axes();
        for slot in self.toplevels.iter_mut().flatten() {
            // Sized for the turned view; the upright usable area is the wrong width.
            if turned && slot.rotation.swaps_axes() {
                continue;
            }
            slot.surface.with_pending_state(|state| {
                state.size = Some(size.into());
            });
            slot.surface.send_configure();
            // A stale landscape record would make `sync_anchor_orientation` skip the
            // resize later.
            slot.rotation = rotation::Rotation::None;
        }
    }

    /// Cheap enough per frame.
    pub(crate) fn sync_keyboard_focus(&mut self) {
        // Locked: the lock surface or nothing. Never the app.
        if self.session_lock.is_locked() {
            let want = self.session_lock.wl_surface().cloned();
            if want != self.focused_surface {
                self.focused_surface = want.clone();
                let keyboard = self.keyboard.clone();
                keyboard.set_focus(self, want, SERIAL_COUNTER.next_serial());
            }
            return;
        }
        let app = self.app_focus_surface();
        // A layer asking for focus (shell dialog) outranks the app. Otherwise the
        // topmost grabbing popup, else the app. Focusing a non-grab popup makes
        // Firefox read the leave as deactivation and close it.
        let want = self.layers.keyboard_focus().or_else(|| {
            app.as_ref()
                .and_then(|s| {
                    // Topmost-first, so the first grab is the deepest menu.
                    PopupManager::popups_for_surface(s)
                        .find(|(kind, _)| self.popup_grabs.contains(kind.wl_surface()))
                        .map(|(kind, _)| kind.wl_surface().clone())
                })
                .or(app)
        });
        if want == self.focused_surface {
            return;
        }
        self.focused_surface = want.clone();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, want, SERIAL_COUNTER.next_serial());
        self.refresh_landscape_hint();
    }

    /// A fullscreen app picked from the deck may be configured for the other
    /// orientation. Same axes just needs the record fixed; swapped needs a new
    /// size.
    fn sync_anchor_orientation(&mut self) {
        let Some(tid) = self.anchor_fullscreen() else {
            return;
        };
        let Some(tl) = self.toplevels.get_mut(tid).and_then(|s| s.as_mut()) else {
            return;
        };
        if tl.rotation == self.rotation {
            return;
        }
        if tl.rotation.swaps_axes() == self.rotation.swaps_axes() {
            tl.rotation = self.rotation;
        } else {
            let surface = tl.surface.clone();
            self.configure_fullscreen(&surface);
        }
    }

    pub(crate) fn app_focus_surface(&self) -> Option<WlSurface> {
        ui_state::desired_focus(&self.ui)
            .and_then(|tid| self.toplevels.get(tid))
            .and_then(|slot| slot.as_ref())
            .map(|tl| tl.surface.wl_surface().clone())
    }

    pub(crate) fn refresh_landscape_hint(&mut self) {
        let fullscreen = self.foreground_is_fullscreen();
        let hint = ui_state::desired_focus(&self.ui)
            .and_then(|tid| self.toplevels.get(tid))
            .and_then(|slot| slot.as_ref())
            .is_some_and(|tl| {
                content_type::wants_landscape(content_type::of(tl.surface.wl_surface()), fullscreen)
            });
        if hint != self.landscape_hint {
            self.landscape_hint = hint;
            // Logged only; rotation keys off fullscreen and the device.
            info!(target: "springchick::debug", "landscape hint {hint}");
        }
        self.sync_sensor_claim();
        // It just became or stopped being fullscreen while turned; reconfigure.
        if self.refresh_rotation() {
            if let Some(surface) = self.foreground_toplevel_surface() {
                if fullscreen {
                    self.configure_fullscreen(&surface);
                } else {
                    self.configure_maximized(&surface);
                }
            }
        }
    }

    /// A claim keeps the accelerometer powered, so hold one only while an app
    /// is fullscreen and the panel is lit. Idempotent.
    pub(crate) fn sync_sensor_claim(&mut self) {
        // A turned view outlives the app and still needs the sensor to turn back.
        let wanted = (self.foreground_is_fullscreen() || self.rotation.swaps_axes())
            && !self.blank.is_blanked();
        if let Some(sensor) = &mut self.sensor {
            sensor.set_wanted(wanted);
        }
    }

    /// Committed, not just requested.
    fn foreground_is_fullscreen(&self) -> bool {
        ui_state::desired_focus(&self.ui).is_some_and(|tid| self.is_fullscreen(tid))
    }

    fn is_fullscreen(&self, tid: ToplevelId) -> bool {
        self.toplevels
            .get(tid)
            .and_then(|slot| slot.as_ref())
            .is_some_and(|tl| {
                tl.surface.with_committed_state(|state| {
                    state.is_some_and(|s| s.states.contains(xdg_toplevel::State::Fullscreen))
                })
            })
    }

    /// `Some(Some(_))` for an app, `Some(None)` for Home, `None` in between
    /// (grab, switcher, quick switch, closing), where the view holds its
    /// rotation so cmd-tab between landscape apps stays landscape.
    fn rotation_anchor(&self) -> Option<Option<ToplevelId>> {
        match &self.ui {
            UiState::App { toplevel, .. } | UiState::AppOpening { toplevel, .. } => {
                Some(Some(*toplevel))
            }
            UiState::Home { .. } => Some(None),
            _ => None,
        }
    }

    fn anchor_fullscreen(&self) -> Option<ToplevelId> {
        self.rotation_anchor()
            .flatten()
            .filter(|&tid| self.is_fullscreen(tid))
    }

    fn desired_view_rotation(&self) -> rotation::Rotation {
        let fullscreen = match self.rotation_anchor() {
            Some(Some(tid)) => self.is_fullscreen(tid),
            Some(None) => false,
            None => self.rotation.swaps_axes(),
        };
        if self.blank.is_blanked() && self.external_display {
            return rotation::mirror_only_rotation(self.rotation, fullscreen);
        }
        rotation::desired_rotation(self.device_orientation, fullscreen)
    }

    /// Derived, not latched, so it's only on while a client really draws at the
    /// rotated size. Returns whether it changed.
    fn refresh_rotation(&mut self) -> bool {
        let want = self.desired_view_rotation();
        if want == self.rotation {
            return false;
        }
        self.rotation = want;
        self.needs_render = true;
        info!(target: "springchick::debug", "rotation {want:?}");
        true
    }

    /// Feeds the debounce; [`Self::tick_rotation`] acts on it.
    pub(crate) fn set_device_orientation(&mut self, orientation: rotation::DeviceOrientation) {
        let before = self.orientation_settle.is_pending();
        self.orientation_settle
            .observe(orientation, std::time::Instant::now());
        if self.orientation_settle.is_pending() != before {
            // Nothing else would ask for a frame on an idle screen.
            self.needs_render = true;
        }
    }

    pub(crate) fn tick_rotation(&mut self, now: std::time::Instant) {
        if let Some(orientation) = self.orientation_settle.poll(now) {
            self.apply_device_orientation(orientation, now);
        }
        // The shell moved to something wanting the other orientation.
        if !self.rotation_fade.is_active()
            && self.desired_view_rotation() != self.rotation
            && self.rotation_fade.begin(now)
        {
            self.swap_rotation(now);
        }
        self.sync_anchor_orientation();
        match self.rotation_fade.tick(now) {
            rotation::FadeStep::Apply => self.swap_rotation(now),
            rotation::FadeStep::Done => self.rotation_await_size = None,
            rotation::FadeStep::None => {}
        }
        if self.rotation_fade.is_active() || self.orientation_settle.is_pending() {
            self.needs_render = true;
        }
    }

    /// Starts the fade, or swaps at once with fades off.
    fn apply_device_orientation(
        &mut self,
        orientation: rotation::DeviceOrientation,
        now: std::time::Instant,
    ) {
        if orientation == self.device_orientation {
            return;
        }
        info!(target: "springchick::debug", "device orientation {orientation:?}");
        self.device_orientation = orientation;
        if self.desired_view_rotation() == self.rotation {
            return;
        }
        if self.rotation_fade.begin(now) {
            self.swap_rotation(now);
        }
        self.needs_render = true;
    }

    /// Called with the screen dark (or fades off).
    fn swap_rotation(&mut self, now: std::time::Instant) {
        let changed = self.refresh_rotation();
        self.sync_sensor_claim();
        let configured = changed
            .then(|| self.anchor_fullscreen())
            .flatten()
            .and_then(|tid| {
                self.toplevels
                    .get(tid)?
                    .as_ref()
                    .map(|tl| tl.surface.clone())
            })
            .map(|surface| self.configure_fullscreen(&surface));
        self.rotation_await_size = configured;
        if configured.is_none() {
            // Nothing to wait for; don't sit out `Fade::MAX_WAIT` on black.
            self.rotation_fade.content_ready(now);
        }
    }

    /// The first commit at the configured size ends the fade.
    pub(crate) fn note_rotation_commit(&mut self, surface: &WlSurface) {
        let Some(want) = self.rotation_await_size else {
            return;
        };
        if self.app_focus_surface().as_ref() != Some(surface) {
            return;
        }
        let size = smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
            s.surface_size()
        })
        .flatten();
        let Some(size) = size else { return };
        if (size.w, size.h) != want {
            return;
        }
        self.rotation_await_size = None;
        self.rotation_fade.content_ready(std::time::Instant::now());
        self.needs_render = true;
    }

    pub(crate) fn drain_sensor(&mut self) {
        let Some(latest) = self.sensor.as_ref().and_then(crate::sensor::Sensor::latest) else {
            return;
        };
        self.set_device_orientation(latest);
    }

    fn foreground_toplevel_surface(&self) -> Option<ToplevelSurface> {
        ui_state::desired_focus(&self.ui)
            .and_then(|tid| self.toplevels.get(tid))
            .and_then(|slot| slot.as_ref())
            .map(|tl| tl.surface.clone())
    }

    /// Foreground app or a mapped layer surface; see [`crate::idle_inhibit`].
    pub(crate) fn is_idle_inhibited(&mut self) -> bool {
        let visible = self.app_focus_surface();
        let layers = &self.layers;
        self.idle_inhibit
            .is_inhibited(visible.as_ref(), |s| layers.is_mapped_layer(s))
    }

    /// `set_parent` (in-process dialogs) or the xdg-dialog hint (portal file
    /// choosers, a separate process).
    fn is_dialog(toplevel: &ToplevelSurface) -> bool {
        if toplevel.parent().is_some() {
            return true;
        }
        smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .map(|d| d.lock().unwrap().dialog_hint != ToplevelDialogHint::Unknown)
                .unwrap_or(false)
        })
    }

    /// Dialogs keep CSD: GTK draws Open/Cancel in its header bar. Apps follow
    /// `prefer_no_csd`.
    fn decoration_for(
        &self,
        toplevel: &ToplevelSurface,
        requested: Option<DecorationMode>,
    ) -> DecorationMode {
        if Self::is_dialog(toplevel) {
            DecorationMode::ClientSide
        } else if self.prefer_no_csd {
            DecorationMode::ServerSide
        } else {
            requested.unwrap_or(DecorationMode::ClientSide)
        }
    }

    pub(crate) fn apply_decoration(
        &self,
        toplevel: &ToplevelSurface,
        requested: Option<DecorationMode>,
    ) {
        let mode = self.decoration_for(toplevel, requested);
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_configure();
    }

    /// Maximized to the usable area (logical size). Maximized, not
    /// Fullscreen, so dialogs keep their header bar.
    pub(crate) fn configure_maximized(&mut self, surface: &ToplevelSurface) {
        let usable = self.layers.usable(self.dpi);
        let w = (usable.w as f64 / self.dpi).round() as i32;
        let h = (usable.h as f64 / self.dpi).round() as i32;
        let deco = self.decoration_for(surface, None);
        // The VM xdg-dialog test asserts this line.
        info!(
            target: "springchick::debug",
            "configure toplevel dialog={} decoration={:?}",
            Self::is_dialog(surface),
            deco,
        );
        surface.with_pending_state(|state| {
            state.size = Some((w, h).into());
            state.decoration_mode = Some(deco);
            state.states.unset(xdg_toplevel::State::Fullscreen);
            state.states.set(xdg_toplevel::State::Maximized);
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
        self.record_toplevel_rotation(surface, rotation::Rotation::None);
    }

    /// Logs committed geometry against the configured size, on change. GTK's file
    /// chooser ignores the configure and overflows a phone; the `vm-portal` check
    /// asserts on this line.
    pub(crate) fn log_toplevel_size(&mut self, surface: &WlSurface) {
        let Some(idx) = self.toplevels.iter().position(|t| {
            t.as_ref()
                .is_some_and(|t| t.surface.wl_surface() == surface)
        }) else {
            return;
        };
        // Logical px, excluding shadows.
        let geo = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceCachedState>()
                .current()
                .geometry
        });
        let Some(geo) = geo else { return };
        let size = (geo.size.w, geo.size.h);
        if size == (0, 0) {
            return;
        }
        let usable = self.layers.usable(self.dpi);
        let avail_w = (usable.w as f64 / self.dpi).round() as i32;
        let avail_h = (usable.h as f64 / self.dpi).round() as i32;
        let Some(tl) = self.toplevels[idx].as_mut() else {
            return;
        };
        if tl.logged_size == Some(size) {
            return;
        }
        tl.logged_size = Some(size);
        let app_id = tl.app_id.clone();
        info!(
            target: "springchick::debug",
            "toplevel size app_id={} geometry={}x{} available={}x{} oversize={}",
            app_id,
            size.0,
            size.1,
            avail_w,
            avail_h,
            size.0 > avail_w || size.1 > avail_h,
        );
    }

    /// Sized by the current [`Self::rotation`], not assumed landscape. Returns
    /// the logical size requested, which the rotation fade waits for.
    pub(crate) fn configure_fullscreen(&mut self, surface: &ToplevelSurface) -> (i32, i32) {
        let (ow, oh) = self.rotation.app_size(self.panel_size);
        let w = (ow as f64 / self.dpi).round() as i32;
        let h = (oh as f64 / self.dpi).round() as i32;
        let orientation = self.rotation;
        info!(target: "springchick::debug", "fullscreen request; configure {w}x{h} {orientation:?}");
        surface.with_pending_state(|state| {
            state.size = Some((w, h).into());
            state.decoration_mode = Some(DecorationMode::ServerSide);
            state.states.unset(xdg_toplevel::State::Maximized);
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
        self.record_toplevel_rotation(surface, orientation);
        (w, h)
    }

    /// See [`crate::state::AppToplevel::rotation`].
    fn record_toplevel_rotation(
        &mut self,
        surface: &ToplevelSurface,
        rotation: rotation::Rotation,
    ) {
        if let Some(tl) = self
            .toplevels
            .iter_mut()
            .flatten()
            .find(|tl| tl.surface.wl_surface() == surface.wl_surface())
        {
            tl.rotation = rotation;
        }
    }
}

fn commit_needs_frame(owner: Option<ToplevelId>, drawn: &[ToplevelId]) -> bool {
    match owner {
        Some(id) => drawn.contains(&id),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_undrawn_toplevels_skip_the_frame() {
        assert!(commit_needs_frame(Some(2), &[0, 2]), "drawn app renders");
        assert!(
            !commit_needs_frame(Some(1), &[0, 2]),
            "backgrounded app does not"
        );
        assert!(
            !commit_needs_frame(Some(0), &[]),
            "nothing drawn: no app commit renders"
        );
        assert!(commit_needs_frame(None, &[]));
    }
}
