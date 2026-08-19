//! App window lifecycle: registering/closing toplevels, launching and raising
//! apps, keyboard focus, decoration policy, and the fullscreen→rotation signal.

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
use crate::ui_state::{self, transition, ToplevelId, UiEvent, ZoomOrigin};
use crate::{content_type, keybinds, provenance, rotation};

impl State {
    pub(crate) fn handle_return_home(&mut self) {
        // Going Home ends any keyboard switching session, or letting the
        // modifier go afterwards would commit a card on top of the exit the
        // user just asked for — Super+h then releasing Super landed back in an
        // app instead of Home.
        self.kbd_switch = None;
        transition(
            &mut self.ui,
            UiEvent::ReturnHome {
                origin: self.last_origin,
            },
        );
    }

    /// Flip the foreground app between immersive fullscreen and the normal
    /// maximized state — the keyboard equivalent of the client asking for it
    /// (`fullscreen_request`), for apps that offer no way to ask.
    ///
    /// Only the configure is sent here. Rotation and the OSK's exclusive zone
    /// follow on the client's commit, via `refresh_landscape_hint`, exactly as
    /// they do for a client-initiated fullscreen.
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

    /// Launch the pull-down search app. It is a normal Wayland client (a
    /// fullscreen xdg toplevel) that owns the search UI, keyboard, and app
    /// launching; the compositor only spawns it and gives it the usual app
    /// treatment (focus/touch/animation). Deduped: if it is already running,
    /// raise it instead of spawning a second copy.
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
        // No activation token: the search app is identified by spawn intent
        // (`expecting_search`), not by attribution, and it never pulses an icon.
        if let Some(child) = spawn_exec(SEARCH_APP_EXEC, &self.wayland_socket, "") {
            self.children.push(child);
            self.expecting_search = true;
        }
        self.needs_render = true;
    }

    /// Raise `tid` to the foreground, zooming from `origin`. The window picked
    /// becomes the most recent, since choosing it is a deliberate activation.
    ///
    /// Unlike [`Self::launch_or_raise`] this takes a window, not an app: with
    /// several open, "the app's most recent window" is what the caller (the icon
    /// menu's per-window rows) is choosing *against*.
    pub(crate) fn raise_toplevel(&mut self, tid: ToplevelId, origin: ZoomOrigin) {
        let Some(Some(tl)) = self.toplevels.get(tid) else {
            return;
        };
        let app_id = tl.app_id.clone();
        self.last_origin = origin;
        self.history.push_foreground(tid);
        transition(
            &mut self.ui,
            UiEvent::RaiseApp {
                toplevel: tid,
                app_id,
            },
        );
    }

    /// A window's client-set title, empty when it never set one.
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

    /// Every window of `app_id`, most-recently-used first.
    ///
    /// Ordered off the MRU history rather than the toplevel vec so "raise the
    /// app" lands on the window the user last looked at, not the oldest one.
    /// Windows that never entered the history (only the search app) are absent.
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

    /// Tap on an icon: raise the app's most recent window if it has one,
    /// otherwise start it.
    pub(crate) fn launch_or_raise(&mut self, app_id: &str, origin: ZoomOrigin) {
        self.last_origin = origin;

        // Frecency is recorded when a window actually maps (`register_toplevel`),
        // so launches from the search app — which exec apps directly rather than
        // through here — count the same as icon taps.
        if let Some(&idx) = self.instances(app_id).first() {
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

        self.spawn_instance(app_id, origin);
    }

    /// Start another window of `app_id`, whether or not it is already running.
    /// Backs the icon menu's "New window"; `launch_or_raise` falls through to it
    /// when the app isn't running at all.
    ///
    /// The launch is tracked in `launching` so the icon pulses until the window
    /// maps or the process dies, and so the mapped window can be attributed back
    /// to it. Several launches may pulse at once.
    pub(crate) fn spawn_instance(&mut self, app_id: &str, origin: ZoomOrigin) {
        self.last_origin = origin;
        let Some(entry) = self.app_catalog.get(app_id).cloned() else {
            return;
        };
        let token = self.mint_activation_token(app_id);
        if let Some(child) = spawn_app(&entry, &self.wayland_socket, &token) {
            self.launching.push(Launching {
                app_id: app_id.to_string(),
                pid: child.id() as i32,
                child,
                token,
                started: std::time::Instant::now(),
            });
        }
    }

    /// Mint an xdg-activation token tagged with the app being launched, for the
    /// child's environment. The `app_id` we record is our catalog id, which is
    /// exactly what attribution needs back.
    fn mint_activation_token(&mut self, app_id: &str) -> String {
        let data = XdgActivationTokenData {
            app_id: Some(app_id.to_string()),
            ..Default::default()
        };
        let (token, _) = self.xdg_activation_state.create_external_token(data);
        token.as_str().to_string()
    }

    /// Take the launch a newly mapped client belongs to, if any.
    ///
    /// Tried in order of how much we trust them: an activation token the client
    /// presented for this exact surface, then the process ancestry (which
    /// survives terminals and shell wrappers that drop the token). Removing the
    /// entry both stops its pulse and stops a second window claiming the same
    /// launch.
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

        let mut done = Vec::new();
        for (i, l) in self.launching.iter_mut().enumerate() {
            let exited = matches!(l.child.try_wait(), Ok(Some(_)) | Err(_));
            let timed_out = l.started.elapsed() >= LAUNCH_PULSE_TIMEOUT;
            if exited || timed_out {
                done.push(i);
            }
        }
        // Back to front so the earlier indices stay valid.
        for i in done.into_iter().rev() {
            self.give_up_launch(i);
        }
    }

    /// Drop launch `i`: stop its pulse, hand the child to the reap list, and
    /// forget the token so the pool doesn't grow for the life of the session.
    fn give_up_launch(&mut self, i: usize) {
        let l = self.launching.remove(i);
        self.forget_token(&l.token);
        self.children.push(l.child);
    }

    /// Drop a minted token from the activation pool.
    fn forget_token(&mut self, token: &str) {
        self.xdg_activation_state
            .remove_token(&XdgActivationToken::from(token.to_string()));
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

        // Identity comes from the launch when a launch claims this window: the
        // client's own id is wrong for anything started through a wrapper (a
        // `Terminal=true` entry reports `foot`) or by a runner that hosts many
        // apps under one id (a PWA shell). Claiming also stops that launch's
        // pulse, so only the icon that is genuinely still waiting keeps
        // breathing. `unknown_N` remains for windows nothing claimed and whose
        // client id isn't a catalog app; `resolve_app_id` may still fix those up.
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

        // A launch-claimed window is resolved right here. Otherwise frecency is
        // recorded in `app_id_changed`: clients set their xdg `app_id` *after*
        // the toplevel maps, so `app_id` above is a placeholder that never
        // catalog-matches, and recording only fires once the real id arrives.
        if id_from_launch {
            self.record_launch(&app_id);
        }

        // Enter the output so the client receives its scale factor (`[main].dpi`)
        // and renders a HiDPI buffer instead of 1:1.
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
        let mut closed = None;
        for (idx, slot) in self.toplevels.iter_mut().enumerate() {
            if let Some(tl) = slot {
                if tl.surface.wl_surface() == surface {
                    // Read the dialog hint before dropping the slot — the xdg
                    // resource is gone by the time close_toplevel runs.
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

    /// Close a toplevel by id (remove from vec, notify UI state).
    ///
    /// `was_dialog` decides where the screen goes if this was the foreground
    /// toplevel. A dialog is a transient thing on behalf of another app — a
    /// portal file chooser is a whole separate process, so dismissing it must
    /// hand the screen back rather than drop the app that asked for it. A real
    /// app closing still goes Home, which is the Springboard model.
    pub(crate) fn close_toplevel(&mut self, id: ToplevelId, was_dialog: bool) {
        self.detach_toplevel(id);
        let next = if was_dialog { self.mru_app() } else { None };
        transition(&mut self.ui, UiEvent::ToplevelClosed { toplevel: id, next });
    }

    /// The app to fall back to when a dialog goes away: the front of the MRU
    /// history, which `detach_toplevel` has already pruned of the closed id.
    /// `None` when nothing is left to return to.
    fn mru_app(&self) -> Option<(ToplevelId, String)> {
        let id = *self.history.stack.first()?;
        let tl = self.toplevels.get(id)?.as_ref()?;
        Some((id, tl.app_id.clone()))
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
        // Deliberately quitting the front app is an app close, not a dialog
        // dismissal: go Home.
        self.detach_toplevel(id);
        transition(
            &mut self.ui,
            UiEvent::ToplevelClosed {
                toplevel: id,
                next: None,
            },
        );
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

    /// Slide an already-running app in from the right edge — the Home-bar
    /// rightward swipe. Unlike [`Self::raise_toplevel_centered`] this plays an
    /// entrance animation (Home travels leftwards off the app), and it counts as
    /// a deliberate activation, so it reorders the MRU stack.
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
                // Unused by the slide, but the state carries an origin for the
                // return trip home.
                origin: self.last_origin,
                open_mode: ui_state::OpenMode::SlideFromLeft,
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
        // Only a placeholder id is worth replacing. A launch-owned id is
        // authoritative and must survive whatever the client announces — a
        // `Terminal=true` entry's window says `foot`, and taking that would hand
        // the terminal's icon someone else's window. Anything else already stuck
        // (the search app, or a client that set app_id before mapping).
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

    /// Record an app opening in the frecency model and persist it.
    fn record_launch(&mut self, app_id: &str) {
        self.model
            .frecency
            .record_launch(app_id, sc_shell_model::unix_now());
        if let Err(e) = persist::save(&self.model, &persist::state_path()) {
            warn!(%e, "failed to save shell model after launch");
        }
    }

    /// Recompute layer-surface geometry + reserved area. If the area apps may
    /// use changed, resize the toplevels to fit around it (e.g. above an OSK).
    pub(crate) fn recompute_layers(&mut self) {
        // Hold the app at its old size while a layer surface slides in: shrinking
        // it the instant the OSK maps leaves a bare strip that the keyboard then
        // slides up into. `advance_frame` calls back here when the slide lands.
        if self.layers.sliding() {
            return;
        }
        if self.layers.usable_changed(self.dpi).is_some() {
            self.reconfigure_toplevels();
            // The area popups may occupy moved with it (OSK up/down): re-solve
            // their positioners so an open menu flips instead of being covered.
            self.reconstrain_popups();
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
        // A locked session gives the keyboard to the lock surface — that is how
        // a password reaches the lock client and, just as importantly, how it
        // stops reaching the app underneath. Locked with no surface means no
        // keyboard focus at all rather than a fallback to the app.
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
            .or_else(|| app.clone());
        if want == self.focused_surface {
            return;
        }
        tracing::info!(
            to_popup = want.is_some() && want != app,
            "keyboard focus changed"
        );
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
        // The home pill's fullscreen policy keys off exactly the same signal as
        // rotation does: what the client has actually committed. Idempotent, so
        // the per-commit call rate does not matter.
        self.bar_hint
            .set_fullscreen(fullscreen, std::time::Instant::now());
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
        // The accelerometer is only worth powering while something can act on
        // it, which is exactly while an app is fullscreen.
        if let Some(sensor) = &mut self.sensor {
            sensor.set_wanted(fullscreen);
        }
        // A rotation change here means the app only just became (or stopped
        // being) fullscreen while the device was already turned, so it is still
        // drawing at the old size — re-configure it.
        if self.refresh_rotation(fullscreen) {
            if let Some(surface) = self.foreground_toplevel_surface() {
                if fullscreen {
                    self.configure_fullscreen(&surface);
                } else {
                    self.configure_maximized(&surface);
                }
            }
        }
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

    /// Rotation follows the device, but only while an app is fullscreen — see
    /// [`rotation::desired_rotation`]. Derived (rather than latched on the
    /// fullscreen request) so it can only be on while a client is really drawing
    /// at the rotated size — including when the app unmaps, is switched away
    /// from, or leaves fullscreen without asking.
    ///
    /// Returns whether the rotation changed, so the caller can re-configure the
    /// app: turning the phone changes the size the client must draw at.
    fn refresh_rotation(&mut self, fullscreen: bool) -> bool {
        let want = rotation::desired_rotation(self.device_orientation, fullscreen);
        if want == self.rotation {
            return false;
        }
        self.rotation = want;
        self.needs_render = true;
        info!(target: "springchick::debug", "rotation {want:?}");
        true
    }

    /// The device was reported as turned — by the accelerometer, or by the
    /// `orientation` control verb. Nothing turns yet: the reading has to hold
    /// still for `rotation_settle_ms` first (see [`rotation::Settle`]), because
    /// the sensor flips the instant the phone crosses the diagonal and a hand
    /// that wobbles past it used to re-configure the app twice in a few frames.
    ///
    /// [`Self::tick_rotation`] is what eventually acts on it.
    pub(crate) fn set_device_orientation(&mut self, orientation: rotation::DeviceOrientation) {
        let before = self.orientation_settle.is_pending();
        self.orientation_settle
            .observe(orientation, std::time::Instant::now());
        if self.orientation_settle.is_pending() != before {
            // A candidate appeared (or was cancelled): wake the render loop so
            // `tick_rotation` actually runs out the hold — on an idle screen
            // nothing else would ask for a frame.
            self.needs_render = true;
        }
    }

    /// Per-frame rotation work: run out the orientation debounce, then advance
    /// the fade that covers a turn. Called by `advance_frame`, so both backends
    /// get it.
    pub(crate) fn tick_rotation(&mut self, now: std::time::Instant) {
        if let Some(orientation) = self.orientation_settle.poll(now) {
            self.apply_device_orientation(orientation, now);
        }
        match self.rotation_fade.tick(now) {
            // The screen is black now: swap under cover of it.
            rotation::FadeStep::Apply => self.swap_rotation(now),
            rotation::FadeStep::Done => self.rotation_await_size = None,
            rotation::FadeStep::None => {}
        }
        if self.rotation_fade.is_active() || self.orientation_settle.is_pending() {
            self.needs_render = true;
        }
    }

    /// A settled orientation. If it changes how the app should be turned, start
    /// the fade — or swap straight away when transitions are off.
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
        let fullscreen = self.foreground_is_fullscreen();
        if rotation::desired_rotation(orientation, fullscreen) == self.rotation {
            return;
        }
        // `begin` returns true when there is no dark stretch to wait for.
        if self.rotation_fade.begin(now) {
            self.swap_rotation(now);
        }
        self.needs_render = true;
    }

    /// Turn the app: re-derive the rotation and hand the fullscreen app the size
    /// its new orientation implies. Called with the screen already dark (or with
    /// fades disabled), because the client keeps drawing its old buffer until it
    /// gets round to the resize.
    fn swap_rotation(&mut self, now: std::time::Instant) {
        let fullscreen = self.foreground_is_fullscreen();
        let changed = self.refresh_rotation(fullscreen);
        let configured = changed
            .then(|| self.foreground_toplevel_surface())
            .flatten()
            .map(|surface| self.configure_fullscreen(&surface));
        self.rotation_await_size = configured;
        if configured.is_none() {
            // Nothing was asked to redraw (the app left fullscreen or went away
            // mid-fade), so there is nothing to wait for: come straight back
            // rather than sit out the whole `Fade::MAX_WAIT` on black.
            self.rotation_fade.content_ready(now);
        }
    }

    /// A foreground-app commit arrived. If it is the first one at the size the
    /// turn configured, the new orientation is really on screen and the fade can
    /// come back up.
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

    /// Hand the sensor's latest reading to the debounce. Called once a tick by
    /// both frame loops; a no-op when there is no sensor, or nothing changed.
    pub(crate) fn drain_sensor(&mut self) {
        let Some(latest) = self.sensor.as_ref().and_then(crate::sensor::Sensor::latest) else {
            return;
        };
        self.set_device_orientation(latest);
    }

    /// The foreground app's `ToplevelSurface`, if there is one.
    fn foreground_toplevel_surface(&self) -> Option<ToplevelSurface> {
        ui_state::desired_focus(&self.ui)
            .and_then(|tid| self.toplevels.get(tid))
            .and_then(|slot| slot.as_ref())
            .map(|tl| tl.surface.clone())
    }

    /// Whether anything visible is holding an idle inhibitor: the foreground app
    /// (video playing, navigation running) or a mapped layer surface (a shell
    /// relaying a D-Bus screensaver inhibit). Inhibitors from backgrounded apps
    /// and unmapped layer surfaces don't count — see [`crate::idle_inhibit`].
    pub(crate) fn is_idle_inhibited(&mut self) -> bool {
        let visible = self.app_focus_surface();
        let layers = &self.layers;
        self.idle_inhibit
            .is_inhibited(visible.as_ref(), |s| layers.is_mapped_layer(s))
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
    pub(crate) fn configure_maximized(&mut self, surface: &ToplevelSurface) {
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
        self.record_toplevel_rotation(surface, rotation::Rotation::None);
    }

    /// Log a toplevel's client-set xdg window geometry against the logical size
    /// it was configured for, and whether it overflows.
    ///
    /// A client may legally ignore the size in a configure, and GTK does: its
    /// file chooser clamps to the widget's minimum width (well over the ~360
    /// logical px a phone has) and commits that instead, so the picker runs off
    /// the screen edge. Nothing in xdg-shell can force it narrower — the fix is
    /// a backend that is actually adaptive. This line is the oracle the
    /// `vm-portal` check asserts on to prove one is in use.
    ///
    /// Only fires when the geometry changes, not on every commit.
    pub(crate) fn log_toplevel_size(&mut self, surface: &WlSurface) {
        let Some(idx) = self.toplevels.iter().position(|t| {
            t.as_ref()
                .is_some_and(|t| t.surface.wl_surface() == surface)
        }) else {
            return;
        };
        // The client's own window geometry (logical px, excluding its shadows),
        // which is what it wants to occupy. Absent until the first real commit.
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

    /// Configure a toplevel truly fullscreen: the whole output, no decorations,
    /// and at whatever size the *current* device orientation implies.
    ///
    /// The size follows [`Self::rotation`] rather than assuming landscape. An
    /// upright phone gives a fullscreen app the portrait output size, which is
    /// what it is about to be drawn at; handing it the swapped size instead is
    /// what left the pull-down search wider than the screen and its blur region
    /// short of the bottom.
    ///
    /// Returns the logical size the client was asked for, which the rotation
    /// fade matches commits against to know when the turn is really on screen.
    pub(crate) fn configure_fullscreen(&mut self, surface: &ToplevelSurface) -> (i32, i32) {
        let (ow, oh) = self.rotation.app_size(self.output_size);
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

    /// Remember the orientation a window was just configured at, so its card can
    /// be drawn the right way up after the shell itself has gone back to
    /// portrait. See [`crate::state::AppToplevel::rotation`].
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
