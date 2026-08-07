//! Per-frame shell advance: spring ticking, UI-state transitions, the popup
//! geometry chains, the home-bar fade, and the animation gate the DRM loop uses
//! to decide whether to keep priming page-flips.

use smithay::desktop::PopupManager;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use tracing::debug;

use crate::layer_shell;
use crate::popups;
use crate::render;
use crate::scene::compute_scene;
use crate::state::{FramePrep, PopupRect, State};
use crate::ui_state::{self, transition, UiEvent, UiState};

impl State {
    /// Popups rooted at `root`, ordered root→leaf, as `(kind, phys_origin,
    /// phys_size)`. The origin is clamped so each popup stays fully on-screen.
    /// `root_origin` is where the root surface's `(0, 0)` is drawn, physical.
    fn popup_chain(&self, root: &WlSurface, root_origin: (i32, i32)) -> Vec<PopupRect> {
        let dpi = self.dpi;
        PopupManager::popups_for_surface(root)
            .map(|(kind, loc)| {
                let geo = kind.geometry();
                let size = (
                    (geo.size.w as f64 * dpi).round() as i32,
                    (geo.size.h as f64 * dpi).round() as i32,
                );
                let origin = (
                    root_origin.0 + (loc.x as f64 * dpi).round() as i32,
                    root_origin.1 + (loc.y as f64 * dpi).round() as i32,
                );
                let clamped = popups::clamp_origin(origin, size, self.output_size);
                (kind, clamped, size)
            })
            .collect()
    }

    /// Popups parented to the fullscreen app (menus, dropdowns), root→leaf.
    fn app_popups(&self) -> Vec<PopupRect> {
        let usable = self.layers.usable(self.dpi);
        let origin = (usable.x.round() as i32, usable.y.round() as i32);
        self.app_focus_surface()
            .map(|s| self.popup_chain(&s, origin))
            .unwrap_or_default()
    }

    /// Popups parented to a top/overlay layer surface (e.g. an OSK menu).
    fn layer_popups(&self) -> Vec<PopupRect> {
        let mut out = Vec::new();
        let (below, above) = self.layers.render_lists(self.dpi);
        for (surface, origin) in below.iter().chain(above.iter()) {
            out.extend(self.popup_chain(surface, *origin));
        }
        out
    }

    /// Popups that are on screen right now, app-rooted first then layer-rooted.
    /// Used by touch routing to hit-test and (for grabbing popups only) dismiss.
    /// A tap is routed into whichever popup it lands on regardless of grab; only
    /// a *grabbing* popup swallows an outside tap and dismisses — see
    /// `touch::popup_press`, which consults `popup_grabs` per popup.
    ///
    /// While the app is rotated the layer surfaces are not drawn (portrait
    /// chrome over a landscape app), so their popups are not on screen either
    /// and are left out — input routing uses this list, and an invisible popup
    /// must not take taps.
    pub(crate) fn active_popups(&self) -> Vec<PopupRect> {
        let mut v = self.app_popups();
        if !self.rotation.swaps_axes() {
            v.extend(self.layer_popups());
        }
        v
    }

    /// Whether `surface` is a popup that issued an `xdg_popup.grab()` (modal).
    pub(crate) fn popup_has_grab(&self, surface: &WlSurface) -> bool {
        self.popup_grabs.contains(surface)
    }

    /// Bar fade target: 0 when a Top/Overlay layer surface covers the pill,
    /// else 1. The OSK is lifted above the pill's strip (see `shift_docked`),
    /// so it no longer fades the bar; only a surface actually over the pill
    /// (e.g. a fullscreen overlay) does.
    fn bar_alpha_target(&self) -> f32 {
        let (w, h) = self.output_size_f();
        let pill = sc_layout::pill_rect(w, h);
        if self.layers.top_overlaps(pill, self.dpi) {
            0.0
        } else {
            1.0
        }
    }

    /// Step the home-bar fade toward its target and return the current alpha.
    /// ~0.13s fade (0.15 per 90Hz frame).
    fn tick_bar_alpha(&mut self) -> f32 {
        let target = self.bar_alpha_target();
        let step = 0.15;
        if (self.bar_alpha - target).abs() <= step {
            self.bar_alpha = target;
        } else if self.bar_alpha < target {
            self.bar_alpha += step;
        } else {
            self.bar_alpha -= step;
        }
        self.bar_alpha
    }

    /// True while the bar fade is still animating (keeps the DRM loop rendering).
    pub(crate) fn bar_fading(&self) -> bool {
        (self.bar_alpha - self.bar_alpha_target()).abs() > f32::EPSILON
    }

    /// True while anything on screen is still changing, so the DRM loop should
    /// keep priming page-flips. False on a static screen (idle home, foreground
    /// app that isn't drawing) so the vblank render loop can stop and let the
    /// CPU/GPU idle. A fresh commit, input, or animation start re-arms rendering
    /// via `needs_render` and the animation springs below.
    pub(crate) fn is_animating(&self, now: std::time::Instant) -> bool {
        self.needs_render
            || self.ui.needs_animation()
            || self.launching.is_some()
            || self.osd.is_active(now)
            || self.bar_fading()
            // A lock is engaged but not yet confirmed to the client: keep
            // page-flipping so the locked frame it is waiting on is actually
            // presented (see `session_lock::SessionLock::tick`).
            || self.session_lock.needs_frame()
            // A finger held on an icon, waiting to become a long-press. The hold
            // is checked in `advance_frame`, so without this the timer only
            // advances while some *other* input keeps the loop awake: a
            // perfectly still finger emits no further events, page-flips stop,
            // and arrange mode never engages. Real panels jitter enough to hide
            // this most of the time; synthetic input (the debug socket) does not
            // jitter at all, so it fails there every time.
            //
            // Gated on `pointer_down` — the same guard `maybe_engage_arrange_hold`
            // uses — so this can only spin while a finger is actually down. A
            // stale `icon_press` left behind by a lost touch-up would otherwise
            // pin the render loop on for good, which on a phone is a battery bug.
            || (self.pointer_down && self.icon_press.is_some())
            // A debug-input gesture/key/touch/settle in flight must keep the DRM
            // loop rendering each tick so it advances (page-flips otherwise stop
            // on an idle screen). Inert in normal runs — these are always None.
            || self.active_gesture.is_some()
            || self.active_key.is_some()
            || self.active_touch.is_some()
            || self.pending_settle.is_some()
            || self
                .grid_anim
                .values()
                .any(|(sx, sy)| !sx.is_settled() || !sy.is_settled())
            || self
                .dock_anim
                .values()
                .any(|(sx, sy)| !sx.is_settled() || !sy.is_settled())
    }

    /// Advance the shell by one frame and produce the render snapshot: tick the
    /// springs, apply any resulting effect, refresh `page_count`, compute the
    /// scene, and gather the app surface, OSD, bar fade, and layer lists. Shared
    /// by the winit and DRM backends, which differ only in how they present the
    /// resulting frame.
    pub(crate) fn advance_frame(&mut self, dt: f32) -> FramePrep {
        // Confirm a pending lock once the frame that hid the session has been
        // presented. Done first so the snapshot below reflects the same lock
        // state the confirmation is about.
        self.session_lock.tick();

        self.maybe_engage_arrange_hold();

        // Lazy-seed the grid-reflow springs on first use so they snap to the
        // current order instead of animating in from (0,0).
        if self.grid_anim.is_empty() {
            self.reflow_grid();
        }
        if self.dock_anim.is_empty() {
            self.reflow_dock();
        }
        // Live reorder: retarget springs to the working order each frame while
        // an icon is being dragged. Gated on the drag itself (not `hover`) so the
        // dragged app is dropped from `grid_anim` immediately on pickup and while
        // over the dock — otherwise it double-draws (in-slot + ghost).
        if self.arrange.as_ref().is_some_and(|a| a.drag.is_some()) {
            self.reflow_grid();
            self.reflow_dock();
        }

        self.tick_edge_page_flip();

        for (sx, sy) in self.grid_anim.values_mut() {
            sx.step(dt);
            sy.step(dt);
        }
        for (sx, sy) in self.dock_anim.values_mut() {
            sx.step(dt);
            sy.step(dt);
        }

        let effect = transition(&mut self.ui, UiEvent::Tick { dt });
        match effect {
            ui_state::Effect::CloseToplevel { toplevel } => {
                self.close_toplevel(toplevel, false);
            }
            ui_state::Effect::EnterSwitcher => {
                let cards = self.history.deck_order();
                debug!(target: "springchick::debug", "Effect::EnterSwitcher deck={:?}", cards);
                transition(&mut self.ui, UiEvent::EnterSwitcher { cards });
            }
            _ => {}
        }

        // Animations that settle to home reset page_count to 1; restore from the model.
        if let UiState::Home { page_count, .. } = &mut self.ui {
            *page_count = self.model.pages.len().max(1);
        }

        let usable = self.layers.usable(self.dpi);
        let scene = compute_scene(
            &self.ui,
            self.output_size,
            (usable.x, usable.y),
            self.card_radius,
        );
        self.switcher_cards = scene.cards.clone();
        let disc = (
            std::mem::discriminant(&self.ui),
            ui_state::desired_focus(&self.ui),
        );
        if self.last_log_state != Some(disc) {
            self.last_log_state = Some(disc);
            debug!(target: "springchick::debug", "state changed to {:?} cards={}", self.ui, scene.cards.len());
        }

        let app_surface = scene.window.as_ref().and_then(|(tid, _)| {
            self.toplevels
                .get(*tid)
                .and_then(|slot| slot.as_ref())
                .map(|tl| tl.surface.wl_surface().clone())
        });

        let frame_time = self.start_time.elapsed().as_millis() as u32;
        let osd_now = std::time::Instant::now();
        let osd_view = self
            .osd
            .is_active(osd_now)
            .then(|| (self.osd.level, self.osd.muted, self.osd.alpha(osd_now)));
        let bar_alpha = self.tick_bar_alpha();
        let (layers_below, layers_above) = self.layers.render_lists(self.dpi);
        // `origin` is the popup's on-screen geometry top-left (used for clamp and
        // hit-test). The buffer is drawn from its (0,0), which sits `geometry.loc`
        // above-left of the geometry rect (client-side shadow/margin), so shift
        // the render origin back by it — matching smithay's own popup placement.
        let dpi = self.dpi;
        let to_render_list = |chain: Vec<PopupRect>| {
            chain
                .into_iter()
                .map(|(kind, origin, _)| {
                    let gloc = kind.geometry().loc;
                    let render_origin = (
                        origin.0 - (gloc.x as f64 * dpi).round() as i32,
                        origin.1 - (gloc.y as f64 * dpi).round() as i32,
                    );
                    (kind.wl_surface().clone(), render_origin)
                })
                .collect::<layer_shell::RenderList>()
        };
        let app_popups = to_render_list(self.app_popups());
        let layer_popups = to_render_list(self.layer_popups());

        // Touch indicator overlay: prune expired rings, then snapshot the marks
        // for this frame. Empty (and cheap) unless `show_touches` is on.
        let touch_marks = if self.show_touches {
            self.touch_viz.prune(osd_now);
            // Keep the vblank-driven DRM loop awake while rings are still fading.
            if self.touch_viz.is_active(osd_now) {
                self.needs_render = true;
            }
            self.touch_viz.marks(osd_now)
        } else {
            Vec::new()
        };

        // Animated icon centers in screen space. Grid springs are global (page 0
        // origin), so subtract the live page scroll here; the dock doesn't page.
        let page_scroll = match &self.ui {
            UiState::Home { page_spring, .. } => page_spring.value,
            _ => 0.0,
        };
        let (out_w, _) = self.output_size_f();
        let grid_positions = self
            .grid_anim
            .iter()
            .map(|(app, (sx, sy))| (app.clone(), (sx.value - page_scroll * out_w, sy.value)))
            .collect();
        let dock_positions = self
            .dock_anim
            .iter()
            .map(|(app, (sx, sy))| (app.clone(), (sx.value, sy.value)))
            .collect();

        FramePrep {
            scene,
            app_surface,
            frame_time,
            osd_view,
            bar_alpha,
            layers_below,
            layers_above,
            app_popups,
            layer_popups,
            touch_marks,
            lock_view: self.session_lock.view(),
            lock_surface: self.session_lock.wl_surface().cloned(),
            grid_positions,
            dock_positions,
        }
    }

    /// Build the render context both backends feed to [`crate::render::draw_scene`].
    ///
    /// Everything here is backend-independent; the four parameters are the only
    /// things the winit and DRM paths actually disagree about. Keeping one
    /// builder is the point — when this was inlined in both backends the copies
    /// drifted (the dock drop-zone highlight was guarded in one and not the
    /// other).
    pub(crate) fn draw_ctx<'a>(
        &'a mut self,
        prep: &'a FramePrep,
        transform: smithay::utils::Transform,
        skia_flip_y: bool,
        report_partial_damage: bool,
        rounded_tex_shader: &'a smithay::backend::renderer::gles::GlesTexProgram,
    ) -> render::DrawCtx<'a> {
        // Resolved before the struct literal so nothing here borrows `self`
        // while `skia` and `last_present` hold mutable borrows of it.
        let usable = self.layers.usable(self.dpi);
        let app_origin = (usable.x.round() as i32, usable.y.round() as i32);
        // Resolved to an owned rect first, so the `arrange` closure below borrows
        // nothing but `self.arrange` itself.
        let dock_zone = self.arrange.as_ref().map(|_| {
            let (w, h) = self.output_size_f();
            sc_layout::compute(w, h, self.current_home_page(), &self.model).dock_zone
        });
        let arrange = self.arrange.as_ref().map(|a| {
            let drag = a.drag.as_ref();
            // Only a grid-sourced drag can pin, so only highlight the dock drop
            // target for those (a dock→dock drag is a no-op).
            let over_dock = drag.is_some_and(|d| {
                d.source == crate::input_dispatch::IconSource::Grid
                    && dock_zone.is_some_and(|z| z.contains(d.cur.0, d.cur.1))
            });
            render::ArrangeView {
                drag_app: drag.map(|d| d.app_id.as_str()),
                drag_pos: drag.map(|d| d.cur),
                over_dock,
            }
        });
        let pressed_app = self.pending_launch.as_ref().map(|p| p.app_id.as_str());
        let launching_app = self.launching.as_ref().map(|l| l.app_id.as_str());
        let launching_elapsed = self
            .launching
            .as_ref()
            .map_or(0.0, |l| l.started.elapsed().as_secs_f32());

        render::DrawCtx {
            scene: &prep.scene,
            app_surface: prep.app_surface.as_ref(),
            skia: &mut self.skia,
            model: &self.model,
            icon_cache: &self.icon_cache,
            app_catalog: &self.app_catalog,
            toplevels: &self.toplevels,
            app_scale: self.dpi,
            app_origin,
            transform,
            rotation: self.rotation,
            skia_flip_y,
            frame_time: prep.frame_time,
            osd: prep.osd_view,
            touches: &prep.touch_marks,
            lock_view: prep.lock_view,
            lock_surface: prep.lock_surface.as_ref(),
            layers_below: &prep.layers_below,
            layers_above: &prep.layers_above,
            app_popups: &prep.app_popups,
            layer_popups: &prep.layer_popups,
            bar_alpha: prep.bar_alpha,
            pressed_app,
            launching_app,
            launching_elapsed,
            arrange,
            report_partial_damage,
            last_present: &mut self.last_present,
            grid_positions: &prep.grid_positions,
            dock_positions: &prep.dock_positions,
            rounded_tex_shader,
        }
    }
}
