//! App window lifecycle: registering/closing toplevels, launching and raising
//! apps, keyboard focus, decoration policy, and the fullscreen→rotation signal.

use smithay::desktop::PopupManager;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::shell::xdg::dialog::ToplevelDialogHint;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgToplevelSurfaceData};

use sc_shell_model::persist;

use tracing::{info, warn};

use crate::launcher::spawn_app;
use crate::state::{
    AppToplevel, Launching, State, LAUNCH_PULSE_TIMEOUT, SEARCH_APP_EXEC, SEARCH_APP_ID,
};
use crate::ui_state::{self, transition, ToplevelId, UiEvent, ZoomOrigin};
use crate::{content_type, keybinds, rotation};

impl State {
    pub(crate) fn handle_return_home(&mut self) {
        transition(
            &mut self.ui,
            UiEvent::ReturnHome {
                origin: self.last_origin,
            },
        );
    }

    /// Launch the pull-down search app. It is a normal Wayland client (a
    /// fullscreen xdg toplevel) that owns the search UI, keyboard, and app
    /// launching; the compositor only spawns it and gives it the usual app
    /// treatment (focus/touch/animation). Deduped: if it is already running,
    /// raise it instead of spawning a second copy.
    pub(crate) fn open_search(&mut self) {
        self.search_arm = None;
        self.page_drag_start = None;
        self.pending_launch = None;
        for (idx, slot) in self.toplevels.iter().enumerate() {
            if slot.as_ref().is_some_and(|tl| tl.app_id == SEARCH_APP_ID) {
                self.raise_toplevel_centered(idx, false);
                return;
            }
        }
        if let Some(child) = spawn_app(SEARCH_APP_EXEC, &self.wayland_socket) {
            self.children.push(child);
            self.expecting_search = true;
        }
        self.needs_render = true;
    }

    pub(crate) fn launch_or_raise(&mut self, app_id: &str, origin: ZoomOrigin) {
        self.last_origin = origin;

        // Frecency is recorded when a window actually maps (`register_toplevel`),
        // so launches from the search app — which exec apps directly rather than
        // through here — count the same as icon taps.

        // Check if already running — raise it (no zoom, instant).
        for (idx, slot) in self.toplevels.iter().enumerate() {
            if let Some(tl) = slot {
                if tl.app_id == app_id {
                    self.history.push_foreground(idx);
                    transition(
                        &mut self.ui,
                        UiEvent::RaiseApp {
                            toplevel: idx,
                            app_id: app_id.to_string(),
                        },
                    );
                    return;
                }
            }
        }

        // Launch new. Track it as `launching` so its icon pulses until the
        // window maps or the process dies. A prior in-flight launch is abandoned
        // (its child moved to the reap list) so only one icon pulses at a time.
        if let Some(entry) = self.app_catalog.get(app_id) {
            let exec = entry.exec.clone();
            if let Some(child) = spawn_app(&exec, &self.wayland_socket) {
                if let Some(prev) = self.launching.take() {
                    self.children.push(prev.child);
                }
                self.launching = Some(Launching {
                    app_id: app_id.to_string(),
                    child,
                    started: std::time::Instant::now(),
                });
            }
        }
    }

    /// Poll the pulsing launch: reap the child if it exited (launch failed) and
    /// give up after [`LAUNCH_PULSE_TIMEOUT`] (daemonized/hung launcher). Called
    /// from both backend loops; the mapped-window case is handled in
    /// `register_toplevel`.
    pub(crate) fn poll_launching(&mut self) {
        // Reap exited children so abandoned/finished launches don't zombie. The
        // `children` list is drained here (every loop tick) rather than on push,
        // since a launch's child may map, be abandoned, or time out — all funnel
        // through here.
        keybinds::reap(&mut self.children);

        let Some(l) = &mut self.launching else {
            return;
        };
        let exited = matches!(l.child.try_wait(), Ok(Some(_)) | Err(_));
        let timed_out = l.started.elapsed() >= LAUNCH_PULSE_TIMEOUT;
        if exited || timed_out {
            if let Some(prev) = self.launching.take() {
                self.children.push(prev.child);
            }
        }
    }

    pub(crate) fn register_toplevel(&mut self, surface: ToplevelSurface) -> ToplevelId {
        // Try to match by app_id from the surface.
        let wl_app_id = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| d.app_id.clone()))
        })
        .unwrap_or_default();

        // The pull-down search app is a normal toplevel but gets special
        // treatment: a slide-up open, no frecency, and hidden from the switcher.
        // Detect it by the spawn intent (the app_id isn't set yet at this point)
        // or, as a fallback, its id. The flag is consumed by the first map.
        let is_search = self.expecting_search || wl_app_id == SEARCH_APP_ID;
        self.expecting_search = false;
        let app_id = if is_search {
            SEARCH_APP_ID.to_string()
        } else if !wl_app_id.is_empty() && self.app_catalog.contains_key(&wl_app_id) {
            wl_app_id
        } else {
            format!("unknown_{}", self.toplevels.len())
        };

        // Frecency is recorded in `app_id_changed`, not here: clients set their
        // xdg `app_id` *after* the toplevel maps, so `app_id` above is a
        // placeholder at this point and never catalog-matches. Recording only
        // fires once the real id arrives. The rare client that sets app_id
        // before mapping resolves here instead — log it either way.
        if self.app_catalog.contains_key(&app_id) {
            info!(
                toplevel = self.toplevels.len(),
                app_id, "toplevel app_id resolved"
            );
        }

        // Enter the output so the client receives its scale factor (`[main].dpi`)
        // and renders a HiDPI buffer instead of 1:1.
        self.output.enter(surface.wl_surface());

        // Window opened — stop the launch pulse. Keep the child handle for
        // reaping. The mapping client's `wl_app_id` is usually still empty at
        // first map (so `app_id` here is `unknown_N`), meaning an app_id match
        // is unreliable. Since only one launch pulses at a time and the search
        // app is handled separately, stop the pulse on the first non-search map.
        if !is_search && self.launching.is_some() {
            if let Some(prev) = self.launching.take() {
                self.children.push(prev.child);
            }
        }

        let id = self.toplevels.len();
        self.toplevels.push(Some(AppToplevel {
            surface,
            app_id: app_id.clone(),
        }));

        // Keep the search app out of the MRU / task switcher.
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

    pub(crate) fn unregister_toplevel(&mut self, surface: &WlSurface) {
        let mut closed_id = None;
        for (idx, slot) in self.toplevels.iter_mut().enumerate() {
            if let Some(tl) = slot {
                if tl.surface.wl_surface() == surface {
                    closed_id = Some(idx);
                    *slot = None;
                    break;
                }
            }
        }
        if let Some(id) = closed_id {
            self.close_toplevel(id);
        }
    }

    /// Close a toplevel by id (remove from vec, notify UI state).
    pub(crate) fn close_toplevel(&mut self, id: ToplevelId) {
        self.detach_toplevel(id);
        transition(&mut self.ui, UiEvent::ToplevelClosed { toplevel: id });
    }

    /// Ask the client to close, without emitting a `ToplevelClosed` UI
    /// transition. Used when the UI has already removed the card (e.g. switcher
    /// swipe-to-close). The slot is kept until the client actually exits and
    /// `toplevel_destroyed` fires — dropping the `ToplevelSurface` here would
    /// destroy the xdg resource before the queued `close` event is flushed.
    pub(crate) fn detach_toplevel(&mut self, id: ToplevelId) {
        if let Some(Some(tl)) = self.toplevels.get(id) {
            tl.surface.send_close();
        }
        self.history.remove(id);
    }

    /// Close whatever app is in front, if any. Backs the `close-app` binding.
    pub(crate) fn close_front_app(&mut self) {
        let Some(id) = ui_state::desired_focus(&self.ui) else {
            return;
        };
        self.detach_toplevel(id);
        transition(&mut self.ui, UiEvent::ToplevelClosed { toplevel: id });
    }

    /// Raise `tid` to the foreground with a screen-centered zoom origin. Backs
    /// the bar swipe-up and the bar horizontal quick-switch, which differ only
    /// in which toplevel they pick. `reorder` records it as the most-recent app
    /// (swipe-up, a deliberate jump); quick-switch passes `false` so browsing
    /// left/right does not shuffle the MRU order.
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

    /// Retag a mapped toplevel once its real xdg `app_id` arrives, recording the
    /// launch in frecency. See `XdgShellHandler::app_id_changed`.
    pub(crate) fn resolve_app_id(&mut self, surface: &ToplevelSurface) {
        let wl_surface = surface.wl_surface().clone();
        let Some(id) = self.toplevels.iter().position(|s| {
            s.as_ref()
                .is_some_and(|t| t.surface.wl_surface() == &wl_surface)
        }) else {
            return;
        };
        // Only a placeholder id is worth replacing; a real match already stuck
        // (e.g. the search app, or a client that set app_id before mapping).
        if !self.toplevels[id]
            .as_ref()
            .is_some_and(|t| t.app_id.starts_with("unknown_"))
        {
            return;
        }
        let new_id = smithay::wayland::compositor::with_states(&wl_surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| d.app_id.clone()))
        })
        .unwrap_or_default();
        if new_id.is_empty() || !self.app_catalog.contains_key(&new_id) {
            return;
        }
        if let Some(tl) = self.toplevels[id].as_mut() {
            tl.app_id = new_id.clone();
        }
        info!(toplevel = id, app_id = %new_id, "toplevel app_id resolved");
        self.model
            .frecency
            .record_launch(&new_id, sc_shell_model::unix_now());
        if let Err(e) = persist::save(&self.model, &persist::state_path()) {
            warn!(%e, "failed to save shell model after launch");
        }
        self.ui.retag_app(id, &new_id);
    }

    /// Recompute layer-surface geometry + reserved area. If the area apps may
    /// use changed, resize the toplevels to fit around it (e.g. above an OSK).
    pub(crate) fn recompute_layers(&mut self) {
        if self.layers.usable_changed(self.dpi).is_some() {
            self.reconfigure_toplevels();
        }
    }

    /// Send every app toplevel a configure at the current usable size, so apps
    /// render within the area not covered by exclusive-zone layer surfaces.
    fn reconfigure_toplevels(&mut self) {
        // Logical size: client scales its buffer up by `dpi`.
        let usable = self.layers.usable(self.dpi);
        let size = (
            (usable.w as f64 / self.dpi).round() as i32,
            (usable.h as f64 / self.dpi).round() as i32,
        );
        for slot in self.toplevels.iter().flatten() {
            slot.surface.with_pending_state(|state| {
                state.size = Some(size.into());
            });
            slot.surface.send_configure();
        }
    }

    /// Push `desired_focus` into the seat keyboard when it changed. Cheap enough
    /// to call every frame; the comparison keeps it from re-sending focus.
    pub(crate) fn sync_keyboard_focus(&mut self) {
        let app = self.app_focus_surface();
        // Only a *grabbing* popup takes keyboard focus — a real menu that
        // requested the grab and drives keyboard nav from it. Redirecting focus
        // to a NON-grab popup makes clients (Firefox) read the toplevel's
        // wl_keyboard.leave as "app deactivated" and instantly roll the popup
        // back up. So focus the topmost grabbing popup in the app's chain if any,
        // else keep focus on the app itself. Both fall out of recomputing `want`
        // every frame, so focus restores to the app when the grab chain closes.
        let want = app
            .as_ref()
            .and_then(|s| {
                PopupManager::popups_for_surface(s)
                    .filter(|(kind, _)| self.popup_grabs.contains(kind.wl_surface()))
                    .last()
                    .map(|(kind, _)| kind.wl_surface().clone())
            })
            .or(app);
        if want == self.focused_surface {
            return;
        }
        self.focused_surface = want.clone();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, want, SERIAL_COUNTER.next_serial());
        // A different app is in front now; its content type is a different
        // answer (and a backgrounded video stops counting).
        self.refresh_landscape_hint();
    }

    /// The wl_surface of the currently focused foreground app, if any.
    pub(crate) fn app_focus_surface(&self) -> Option<WlSurface> {
        ui_state::desired_focus(&self.ui)
            .and_then(|tid| self.toplevels.get(tid))
            .and_then(|slot| slot.as_ref())
            .map(|tl| tl.surface.wl_surface().clone())
    }

    /// Recompute [`State::landscape_hint`] from the foreground app's content
    /// type and fullscreen state. Cheap (two surface-state lookups), called on
    /// commits by the foreground app and whenever focus moves.
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
            // Logged, not acted on: rotation keys off fullscreen alone (see
            // `refresh_rotation`); the content type is kept as the finer signal
            // for policy that wants to tell video from a fullscreen text app.
            info!(target: "springchick::debug", "landscape hint {hint}");
        }
        self.refresh_rotation(fullscreen);
    }

    /// Whether the foreground app has *committed* the Fullscreen state — i.e.
    /// acked our fullscreen configure and drawn at that size.
    fn foreground_is_fullscreen(&self) -> bool {
        ui_state::desired_focus(&self.ui)
            .and_then(|tid| self.toplevels.get(tid))
            .and_then(|slot| slot.as_ref())
            .is_some_and(|tl| {
                tl.surface.with_committed_state(|state| {
                    state.is_some_and(|s| s.states.contains(xdg_toplevel::State::Fullscreen))
                })
            })
    }

    /// Rotation follows fullscreen: the foreground app is landscape while it is
    /// fullscreen and portrait otherwise. Derived (rather than latched on the
    /// fullscreen request) so it can only be on while a client is really drawing
    /// at the rotated size — including when the app unmaps, is switched away
    /// from, or leaves fullscreen without asking.
    fn refresh_rotation(&mut self, fullscreen: bool) {
        let want = if fullscreen {
            rotation::Rotation::Landscape
        } else {
            rotation::Rotation::None
        };
        if want != self.rotation {
            self.rotation = want;
            self.needs_render = true;
            info!(target: "springchick::debug", "rotation {want:?}");
        }
    }

    /// Whether the foreground app is holding an idle inhibitor (video playing,
    /// navigation running). Inhibitors from backgrounded apps don't count — see
    /// [`crate::idle_inhibit`].
    pub(crate) fn is_idle_inhibited(&mut self) -> bool {
        let visible = self.app_focus_surface();
        self.idle_inhibit.is_inhibited(visible.as_ref())
    }

    /// Whether a toplevel is a dialog and so must keep client-side decorations.
    ///
    /// Two independent signals, either sufficient:
    /// - `set_parent`: a child toplevel of another window (in-process dialogs).
    /// - the xdg-dialog `dialog`/`modal` hint: how a portal file chooser — a
    ///   separate process with no in-process parent — announces itself, so it's
    ///   the only signal that catches those.
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

    /// Dialogs always keep client-side decorations: their toolkit draws the
    /// action buttons (a GTK file chooser's Open/Cancel) into its own header bar,
    /// which vanishes under server-side decorations. Top-level app windows follow
    /// `prefer_no_csd` — server-side (borderless) by default, otherwise honoring
    /// whatever the client asked for.
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

    /// Resolve and send a toplevel's decoration mode. Called for every
    /// xdg-decoration request (create / set / unset).
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

    /// Configure a toplevel as maximized to the usable area (output minus any
    /// exclusive-zone reservations, e.g. the OSK), so an app that opens while an
    /// OSK is up already fits above it. xdg sizes are logical; the client scales
    /// its buffer up by `dpi`.
    ///
    /// Maximized — not Fullscreen — is the normal app state: it fills the screen
    /// but leaves a client-side header bar visible (toolkits hide it in the
    /// Fullscreen state), which is what keeps a dialog's buttons on screen.
    pub(crate) fn configure_maximized(&self, surface: &ToplevelSurface) {
        let usable = self.layers.usable(self.dpi);
        let w = (usable.w as f64 / self.dpi).round() as i32;
        let h = (usable.h as f64 / self.dpi).round() as i32;
        let deco = self.decoration_for(surface, None);
        // Dialogs keep client-side decorations so the toolkit's action buttons
        // stay on screen; top-level apps go borderless (server-side). Logged so
        // the nix xdg-dialog test can assert the policy from the journal.
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
    }

    /// Configure a toplevel truly fullscreen: the whole output, no decorations,
    /// and at the rotated (landscape) size. See `XdgShellHandler::
    /// fullscreen_request` for why `self.rotation` is not set here.
    pub(crate) fn configure_fullscreen(&self, surface: &ToplevelSurface) {
        let (ow, oh) = rotation::Rotation::Landscape.app_size(self.output_size);
        let w = (ow as f64 / self.dpi).round() as i32;
        let h = (oh as f64 / self.dpi).round() as i32;
        info!(target: "springchick::debug", "fullscreen request; configure {w}x{h} landscape");
        surface.with_pending_state(|state| {
            state.size = Some((w, h).into());
            state.decoration_mode = Some(DecorationMode::ServerSide);
            state.states.unset(xdg_toplevel::State::Maximized);
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
    }
}
