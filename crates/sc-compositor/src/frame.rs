//! Per-frame shell advance: spring ticking, UI-state transitions, the popup
//! geometry chains, the home-bar fade, and the animation gate the DRM loop uses
//! to decide whether to keep priming page-flips.

use smithay::desktop::{
    find_popup_root_surface, get_popup_toplevel_coords, PopupKind, PopupManager,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};

use tracing::debug;

use crate::layer_shell;
use crate::popups;
use crate::render;
use crate::scene::compute_scene;
use crate::state::{AppToplevel, FramePrep, Launching, PopupRect, State, SEARCH_APP_ID};
use crate::ui_state::{self, transition, UiEvent, UiState};

use std::collections::{HashMap, HashSet};

/// Refill `out` from the reflow springs in `springs`, shifted left by `scroll`
/// (the live page scroll for the grid; zero for the dock, which doesn't page).
///
/// Updates in place: existing keys keep their `String` allocation and only their
/// value is overwritten, so a steady grid costs no allocation per frame.
fn sync_positions(
    out: &mut HashMap<String, (f32, f32)>,
    springs: &HashMap<String, (sc_anim::Spring, sc_anim::Spring)>,
    scroll: f32,
) {
    out.retain(|app, _| springs.contains_key(app));
    for (app, (sx, sy)) in springs {
        let pos = (sx.value - scroll, sy.value);
        match out.get_mut(app) {
            Some(slot) => *slot = pos,
            None => {
                out.insert(app.clone(), pos);
            }
        }
    }
}

/// Refill `out` with the app ids that have at least one open window, for the
/// running dots. Placeholder ids are skipped: `unknown_N` matches no icon, so it
/// would only ever be a wasted lookup.
///
/// In place, as [`sync_positions`]. `toplevels` holds a handful of entries, so
/// the nested scan is cheaper than rebuilding the set.
fn sync_running_apps(out: &mut HashSet<String>, toplevels: &[Option<AppToplevel>]) {
    let shown = |id: &str| !id.starts_with("unknown_") && id != SEARCH_APP_ID;
    out.retain(|id| toplevels.iter().flatten().any(|tl| tl.app_id == *id));
    for tl in toplevels.iter().flatten() {
        if shown(&tl.app_id) && !out.contains(tl.app_id.as_str()) {
            out.insert(tl.app_id.clone());
        }
    }
}

/// Refill `out` with `(app_id, seconds since spawn)` for every launch still
/// waiting on its window. Positional and in place: `launching` is short and
/// ordered, so slot `i` almost always already holds the right id and only the
/// elapsed time changes.
fn sync_launch_pulses(out: &mut Vec<(String, f32)>, launching: &[Launching]) {
    out.truncate(launching.len());
    for (i, l) in launching.iter().enumerate() {
        let elapsed = l.started.elapsed().as_secs_f32();
        match out.get_mut(i) {
            Some(slot) => {
                if slot.0 != l.app_id {
                    slot.0.clear();
                    slot.0.push_str(&l.app_id);
                }
                slot.1 = elapsed;
            }
            None => out.push((l.app_id.clone(), elapsed)),
        }
    }
}

impl State {
    /// Popups rooted at `root`, ordered root→leaf, as `(kind, phys_origin,
    /// phys_size)`. The origin is clamped so each popup stays fully on-screen.
    /// `root_origin` is where the root surface's `(0, 0)` is drawn, physical.
    /// `bound` is the space the origin is clamped into — the output, except for
    /// popups of a rotated app, which live in the app's turned space.
    fn popup_chain(
        &self,
        root: &WlSurface,
        root_origin: (i32, i32),
        bound: (i32, i32),
    ) -> Vec<PopupRect> {
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
                let clamped = popups::clamp_origin(origin, size, bound);
                (kind, clamped, size)
            })
            .collect()
    }

    /// The rect a new/repositioned popup's positioner is unconstrained against,
    /// in the popup parent's logical space. See [`popups::unconstrain_target`].
    ///
    /// App-rooted popups get [`State::app_popup_space`] — the usable area, or
    /// the app's turned space while it is rotated (so a Firefox menu in
    /// landscape is solved against the landscape height, not the portrait one);
    /// layer-rooted popups (OSK menus) get the whole output, since the layer
    /// surface itself lives in the reserved strip.
    pub(crate) fn popup_target(&self, kind: &PopupKind) -> Rectangle<i32, Logical> {
        let root = find_popup_root_surface(kind).ok();
        let app_rooted = root.is_some() && root == self.app_focus_surface();
        let (area, root_origin) = if app_rooted {
            let (o, (w, h)) = self.app_popup_space();
            ((o.0, o.1, w, h), o)
        } else {
            let (below, above) = self.layers.render_lists(self.dpi);
            let origin = root
                .and_then(|r| {
                    below
                        .iter()
                        .chain(above.iter())
                        .find(|(s, _)| *s == r)
                        .map(|(_, o)| *o)
                })
                .unwrap_or((0, 0));
            ((0, 0, self.output_size.0, self.output_size.1), origin)
        };
        let tc = get_popup_toplevel_coords(kind);
        let (x, y, w, h) = popups::unconstrain_target(area, root_origin, (tc.x, tc.y), self.dpi);
        Rectangle::new((x, y).into(), (w, h).into())
    }

    /// Re-run the positioner against the current [`State::popup_target`] for
    /// every live popup and configure the ones whose geometry moved.
    ///
    /// Called when the area popups may occupy changes under them — the OSK
    /// mapping/unmapping, an exclusive zone appearing. Without it a menu opened
    /// against the full screen height stays where it was and the keyboard slides
    /// up over it.
    ///
    /// Only *reactive* popups (`xdg_positioner.set_reactive`) may be
    /// reconfigured after their initial configure; `send_pending_configure`
    /// enforces that and errors otherwise, which is the expected outcome for a
    /// static popup, not a problem — it keeps its original placement and the
    /// render-time [`popups::clamp_origin`] keeps it on screen.
    pub(crate) fn reconstrain_popups(&mut self) {
        let mut roots: Vec<WlSurface> = self
            .toplevels
            .iter()
            .flatten()
            .map(|slot| slot.surface.wl_surface().clone())
            .collect();
        let (below, above) = self.layers.render_lists(self.dpi);
        roots.extend(below.into_iter().chain(above).map(|(s, _)| s));

        let kinds: Vec<PopupKind> = roots
            .iter()
            .flat_map(|r| PopupManager::popups_for_surface(r).map(|(kind, _)| kind))
            .collect();

        for kind in kinds {
            // Input-method popups are positioned by the text cursor rectangle,
            // not an xdg_positioner; nothing to re-solve.
            let PopupKind::Xdg(popup) = kind else {
                continue;
            };
            let target = self.popup_target(&PopupKind::Xdg(popup.clone()));
            popup.with_pending_state(|state| {
                state.geometry = state.positioner.get_unconstrained_geometry(target);
            });
            if let Ok(Some(_)) = popup.send_pending_configure() {
                self.needs_render = true;
            }
        }
    }

    /// The space app-rooted popups are laid out in, as `(origin, size)` in
    /// physical px: normally the usable area, but a rotated app fills the output
    /// and lives in its own turned space, starting at that space's own origin.
    /// Everything downstream (clamp, unconstrain target, hit-test, draw) has to
    /// agree on this or the popup lands somewhere the app never asked for.
    pub(crate) fn app_popup_space(&self) -> ((i32, i32), (i32, i32)) {
        if self.rotation.swaps_axes() {
            return ((0, 0), self.rotation.app_size(self.output_size));
        }
        let u = self.layers.usable(self.dpi);
        (
            (u.x.round() as i32, u.y.round() as i32),
            (u.w.round() as i32, u.h.round() as i32),
        )
    }

    /// Popups parented to the fullscreen app (menus, dropdowns), root→leaf.
    fn app_popups(&self) -> Vec<PopupRect> {
        let (origin, bound) = self.app_popup_space();
        // A rotated app's popups are clamped into the app's own space, which
        // starts at its origin; an unrotated one keeps clamping to the output so
        // a menu may still overhang the usable area's edges (bar strip).
        let bound = if self.rotation.swaps_axes() {
            bound
        } else {
            self.output_size
        };
        self.app_focus_surface()
            .map(|s| self.popup_chain(&s, origin, bound))
            .unwrap_or_default()
    }

    /// Popups parented to a top/overlay layer surface (e.g. an OSK menu).
    fn layer_popups(&self) -> Vec<PopupRect> {
        let mut out = Vec::new();
        let (below, above) = self.layers.render_lists(self.dpi);
        for (surface, origin) in below.iter().chain(above.iter()) {
            out.extend(self.popup_chain(surface, *origin, self.output_size));
        }
        out
    }

    /// One-line snapshot of every layer surface and layer-rooted popup the
    /// compositor would composite, answered by `springchick ipc layers`.
    ///
    /// This exists for the "two keyboards on screen, one wvkbd process" class of
    /// bug: it shows what the render lists actually carry, so a surface that is
    /// still drawn after its client moved on (stale buffer, stale geometry, or a
    /// popup left behind) is visible without a rebuild. Fields are `key=value`,
    /// entries separated by ` | ` — the ipc client prints one entry per line.
    pub(crate) fn layers_dump(&self) -> String {
        let infos = self.layers.dump(self.dpi);
        let popups = self.layer_popups();
        // Layer surfaces and their popups are not drawn while the app is turned
        // (portrait chrome over a landscape app), so say so rather than let the
        // reader wonder why the dump lists surfaces they cannot see.
        let rotated = if self.rotation.swaps_axes() {
            " rotated=yes(layers-not-drawn)"
        } else {
            ""
        };
        let mut parts = vec![format!(
            "out={}x{} dpi={} {}{} layers={} layer-popups={}",
            self.output_size.0,
            self.output_size.1,
            self.dpi,
            self.layers.dump_header(self.dpi),
            rotated,
            infos.len(),
            popups.len(),
        )];
        parts.extend(
            infos
                .iter()
                .enumerate()
                .map(|(i, l)| layer_shell::format_layer(i, l)),
        );
        for (kind, origin, size) in popups {
            // `size` is the popup's *geometry*, which a client may leave at zero
            // while still committing a buffer (wvkbd's key-preview popup does),
            // so the buffer size is reported alongside it.
            let surface = kind.wl_surface().clone();
            let buf = match layer_shell::buffer_size(&surface) {
                Some((w, h)) => format!("{w}x{h}"),
                None => "none".into(),
            };
            parts.push(format!(
                "popup at={},{} geo={}x{} buf={buf}",
                origin.0, origin.1, size.0, size.1,
            ));
        }
        parts.join(" | ")
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

    /// Step the home-bar fade toward its target and return the alpha to draw.
    /// ~0.13s fade (0.15 per 90Hz frame).
    ///
    /// Two independent things dim the pill, and they multiply: `bar_alpha` is
    /// the occlusion fade above, while [`crate::bar_hint`] owns the fullscreen
    /// policy (blink once on the way in, then stay out of the way). Keeping
    /// them separate means neither has to know about the other's timing.
    fn tick_bar_alpha(&mut self, now: std::time::Instant) -> f32 {
        let target = self.bar_alpha_target();
        let step = 0.15;
        if (self.bar_alpha - target).abs() <= step {
            self.bar_alpha = target;
        } else if self.bar_alpha < target {
            self.bar_alpha += step;
        } else {
            self.bar_alpha -= step;
        }
        self.bar_hint.advance(now);
        self.bar_alpha * self.bar_hint.alpha(now)
    }

    /// True while the bar's drawn alpha is still changing — either fade — so the
    /// DRM loop keeps rendering and the partial-damage fast path stays off.
    pub(crate) fn bar_fading(&self) -> bool {
        (self.bar_alpha - self.bar_alpha_target()).abs() > f32::EPSILON
            || self.bar_hint.is_animating(std::time::Instant::now())
    }

    /// True while anything on screen is still changing, so the DRM loop should
    /// keep priming page-flips. False on a static screen (idle home, foreground
    /// app that isn't drawing) so the vblank render loop can stop and let the
    /// CPU/GPU idle. A fresh commit, input, or animation start re-arms rendering
    /// via `needs_render` and the animation springs below.
    pub(crate) fn is_animating(&self, now: std::time::Instant) -> bool {
        self.needs_render
            || self.ui.needs_animation()
            || !self.launching.is_empty()
            || self.osd.is_active(now)
            || self.bar_fading()
            // A layer surface (the OSK) sliding up into place.
            || self.layers.sliding()
            // An app resize held back while the OSK's unmap is debounced: the
            // deadline is only checked from a frame, so keep them coming.
            || self.layers.regrow_pending()
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
            || (self.pointer_down && (self.icon_press.is_some() || self.bg_press.is_some()))
            // The icon menu's open animation.
            || self
                .icon_menu
                .as_ref()
                .is_some_and(|m| !m.open.is_settled())
            // A debug-input gesture/key/touch/settle in flight must keep the DRM
            // loop rendering each tick so it advances (page-flips otherwise stop
            // on an idle screen). Inert in normal runs — these are always None.
            || self.active_gesture.is_some()
            || self.active_key.is_some()
            || self.active_touch.is_some()
            || self.pending_settle.is_some()
            // A turn waiting out its debounce, or the fade covering one. Without
            // this an orientation reported to an otherwise idle screen never
            // gets a frame in which to settle, so nothing turns at all.
            || self.orientation_settle.is_pending()
            || self.rotation_fade.is_active()
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

        // Orientation debounce and the fade that covers a turn. Before the scene
        // is computed, so a rotation that lands this frame is the one drawn.
        self.tick_rotation(std::time::Instant::now());

        self.maybe_engage_arrange_hold();
        self.maybe_open_icon_menu();
        if let Some(menu) = &mut self.icon_menu {
            menu.open.step(dt);
        }

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

        // Slide a freshly-mapped OSK up into place. Purely a render offset — the
        // client is never told about it. The app keeps its old size for the
        // duration (`recompute_layers` bails while sliding) so the keyboard rises
        // *over* it rather than into a strip vacated ahead of it; the resize and
        // the popup re-solve both land on the frame the slide finishes.
        let was_sliding = self.layers.sliding();
        if !self.layers.tick_slides(dt) && was_sliding {
            self.recompute_layers();
        }

        // An OSK unmap whose regrow is still debounced: re-ask every frame so
        // the resize lands once the deadline passes (or never, if the keyboard
        // comes back first).
        if self.layers.regrow_pending() {
            self.recompute_layers();
        }

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

        // A held-modifier switch (Super+Tab) may have queued steps — or the
        // release itself — while the deck was still animating in.
        self.poll_kbd_switch();

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

        let frame_time = self.clock.now().as_millis();
        let osd_now = std::time::Instant::now();
        let osd_view = self
            .osd
            .is_active(osd_now)
            .then(|| (self.osd.level, self.osd.muted, self.osd.alpha(osd_now)));
        let bar_alpha = self.tick_bar_alpha(std::time::Instant::now());
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
        //
        // All four overlays are refilled in place (see [`IconOverlays`]) — the
        // app ids keying them are stable across frames, so rebuilding them would
        // re-allocate every key at 90 Hz.
        let page_scroll = self.home_page_scroll();
        let (out_w, _) = self.output_size_f();
        sync_positions(
            &mut self.icon_overlays.grid_positions,
            &self.grid_anim,
            page_scroll * out_w,
        );
        sync_positions(&mut self.icon_overlays.dock_positions, &self.dock_anim, 0.0);
        sync_running_apps(&mut self.icon_overlays.running_apps, &self.toplevels);
        sync_launch_pulses(&mut self.icon_overlays.launch_pulses, &self.launching);

        let icon_menu = self.icon_menu.as_ref().map(|m| {
            let (w, h) = self.output_size_f();
            render::MenuView {
                layout: m.layout(w, h),
                items: m
                    .items
                    .iter()
                    .map(|i| (i.label.clone(), i.action.is_destructive()))
                    .collect(),
                pressed: m.pressed,
                anchor: m.anchor,
                progress: m.open.value,
            }
        });

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
            cursor: if self.cursor_overlay && self.cursor_visible {
                self.last_pointer_pos
            } else {
                None
            },
            lock_view: self.session_lock.view(),
            lock_surface: self.session_lock.wl_surface().cloned(),
            icon_menu,
            dim: self.rotation_fade.dim(std::time::Instant::now()),
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
        sinks: &'a mut render::FrameSinks,
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

        // When the frame being composited is expected to land: one refresh
        // interval out. Commits aimed at this frame are released against it,
        // and aiming at "now" instead would hold each one back a frame.
        let frame_target = self.clock.now() + self.output_refresh_interval();

        render::DrawCtx {
            frame_target,
            sinks,
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
            cursor: prep.cursor,
            lock_view: prep.lock_view,
            lock_surface: prep.lock_surface.as_ref(),
            layers_below: &prep.layers_below,
            layers_above: &prep.layers_above,
            app_popups: &prep.app_popups,
            layer_popups: &prep.layer_popups,
            bar_alpha: prep.bar_alpha,
            pressed_app,
            launch_pulses: &self.icon_overlays.launch_pulses,
            running_apps: &self.icon_overlays.running_apps,
            arrange,
            icon_menu: prep.icon_menu.as_ref(),
            dim: prep.dim,
            report_partial_damage,
            last_present: &mut self.last_present,
            grid_positions: &self.icon_overlays.grid_positions,
            dock_positions: &self.icon_overlays.dock_positions,
            rounded_tex_shader,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn springs(
        entries: &[(&str, f32, f32)],
    ) -> HashMap<String, (sc_anim::Spring, sc_anim::Spring)> {
        entries
            .iter()
            .map(|(app, x, y)| {
                (
                    (*app).to_string(),
                    (sc_anim::Spring::new(*x), sc_anim::Spring::new(*y)),
                )
            })
            .collect()
    }

    #[test]
    fn sync_positions_fills_an_empty_map() {
        let mut out = HashMap::new();
        sync_positions(&mut out, &springs(&[("a", 10.0, 20.0)]), 0.0);
        assert_eq!(out.get("a"), Some(&(10.0, 20.0)));
    }

    #[test]
    fn sync_positions_subtracts_the_page_scroll() {
        let mut out = HashMap::new();
        sync_positions(&mut out, &springs(&[("a", 10.0, 20.0)]), 4.0);
        assert_eq!(out.get("a"), Some(&(6.0, 20.0)));
    }

    #[test]
    fn sync_positions_drops_apps_the_springs_no_longer_have() {
        let mut out = HashMap::new();
        sync_positions(&mut out, &springs(&[("a", 1.0, 1.0), ("b", 2.0, 2.0)]), 0.0);
        sync_positions(&mut out, &springs(&[("b", 3.0, 3.0)]), 0.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out.get("b"), Some(&(3.0, 3.0)));
    }

    /// The point of syncing in place: a steady grid must not re-allocate its
    /// keys every frame. Same `String` buffer, so the same heap pointer.
    #[test]
    fn sync_positions_reuses_the_key_allocations() {
        let s = springs(&[("a", 1.0, 1.0)]);
        let mut out = HashMap::new();
        sync_positions(&mut out, &s, 0.0);
        let first = out.keys().next().unwrap().as_ptr();
        sync_positions(&mut out, &springs(&[("a", 9.0, 9.0)]), 0.0);
        assert_eq!(out.get("a"), Some(&(9.0, 9.0)));
        assert_eq!(out.keys().next().unwrap().as_ptr(), first);
    }
}
