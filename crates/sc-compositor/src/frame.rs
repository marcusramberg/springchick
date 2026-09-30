//! Per-frame shell advance: springs, UI transitions, popup chains, the bar
//! fade, and the "keep rendering" gate for the DRM loop.

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

fn bar_mode(ui: &UiState) -> crate::bar_hint::BarMode {
    use crate::bar_hint::BarMode;
    match ui {
        UiState::Home { .. } | UiState::AppClosing { .. } => BarMode::Off,
        UiState::App { .. } | UiState::AppOpening { .. } => BarMode::Auto,
        _ => BarMode::Shown,
    }
}

/// Shifted left by `scroll` (zero for the dock). In place, so a steady grid
/// allocates nothing per frame.
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

/// App ids with an open window, for the running dots. In place, as
/// [`sync_positions`].
fn sync_running_apps(out: &mut HashSet<String>, toplevels: &[Option<AppToplevel>]) {
    let shown = |id: &str| !id.starts_with("unknown_") && id != SEARCH_APP_ID;
    out.retain(|id| toplevels.iter().flatten().any(|tl| tl.app_id == *id));
    for tl in toplevels.iter().flatten() {
        if shown(&tl.app_id) && !out.contains(tl.app_id.as_str()) {
            out.insert(tl.app_id.clone());
        }
    }
}

/// `(app_id, seconds since spawn)` per pending launch. Positional, in place.
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
    /// Popups under `root`, root→leaf, as `(kind, phys_origin, phys_size)`, each
    /// clamped into `bound` (the output, or a rotated app's turned space).
    /// smithay yields topmost-first, so this reverses it.
    fn popup_chain(
        &self,
        root: &WlSurface,
        root_origin: (i32, i32),
        bound: (i32, i32),
    ) -> Vec<PopupRect> {
        let dpi = self.dpi;
        let mut chain: Vec<PopupRect> = PopupManager::popups_for_surface(root)
            .map(|(kind, loc)| {
                // Geometry, not the drawn bbox: wvkbd's key preview has no geometry but a
                // buffer covering the whole keyboard, and must hit-test as nothing.
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
            .collect();
        chain.reverse();
        chain
    }

    /// App-rooted popups get [`State::app_popup_space`]; layer-rooted ones
    /// (OSK menus) the whole output.
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
            ((0, 0, self.panel_size.0, self.panel_size.1), origin)
        };
        let tc = get_popup_toplevel_coords(kind);
        let (x, y, w, h) = popups::unconstrain_target(area, root_origin, (tc.x, tc.y), self.dpi);
        Rectangle::new((x, y).into(), (w, h).into())
    }

    /// Re-solve every live popup when the space they may occupy changes (OSK
    /// map/unmap). Only reactive popups can be reconfigured; others error in
    /// `send_pending_configure`, which is expected, and stay clamped.
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
            // IME popups follow the text cursor, not a positioner.
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

    /// `(origin, size)` in view px: the usable area, or the whole view while
    /// turned. Clamp, unconstrain, hit-test and draw must all agree on this.
    pub(crate) fn app_popup_space(&self) -> ((i32, i32), (i32, i32)) {
        if self.view_rotation().swaps_axes() {
            return ((0, 0), self.output_size());
        }
        let u = self.layers.usable(self.dpi);
        (
            (u.x.round() as i32, u.y.round() as i32),
            (u.w.round() as i32, u.h.round() as i32),
        )
    }

    fn app_popups(&self) -> Vec<PopupRect> {
        // Clamped to the whole view, so a menu may overhang the bar strip.
        let (origin, _) = self.app_popup_space();
        let bound = self.output_size();
        self.app_focus_surface()
            .map(|s| self.popup_chain(&s, origin, bound))
            .unwrap_or_default()
    }

    fn layer_popups(&self) -> Vec<PopupRect> {
        let mut out = Vec::new();
        let (below, above) = self.layers.render_lists(self.dpi);
        for (surface, origin) in below.iter().chain(above.iter()) {
            out.extend(self.popup_chain(surface, *origin, self.panel_size));
        }
        out
    }

    /// `springchick ipc home`: what the grid actually draws. `!` = not in the
    /// catalog (draws nothing).
    pub(crate) fn home_dump(&self) -> String {
        let (page, page_count) = match &self.ui {
            UiState::Home {
                page, page_count, ..
            } => (*page, *page_count),
            _ => (0, self.home_page_count()),
        };
        // `!` = not in the catalog. `~` = no reflow spring, so it can't be placed.
        let slot = |id: &String, anim: &HashMap<String, (sc_anim::Spring, sc_anim::Spring)>| {
            let mut s = id.clone();
            if !self.app_catalog.contains_key(id) {
                s.push('!');
            }
            if !anim.contains_key(id) {
                s.push('~');
            }
            s
        };
        let lens: Vec<String> = self
            .model
            .pages
            .iter()
            .map(|p| p.len().to_string())
            .collect();
        let mut parts = vec![format!(
            "ui={} page={page}/{page_count} pages=[{}] arrange={} gen={}",
            format!("{:?}", self.ui)
                .split([' ', '{', '('])
                .next()
                .unwrap_or("?")
                .to_string(),
            lens.join(","),
            self.arrange.is_some(),
            self.catalog_gen,
        )];
        parts.push(format!(
            "dock={}",
            self.model
                .dock
                .iter()
                .map(|id| slot(id, &self.dock_anim))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        for (i, p) in self.model.pages.iter().enumerate() {
            parts.push(format!(
                "page{i}={}",
                p.iter()
                    .map(|id| slot(id, &self.grid_anim))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        parts.join(" | ")
    }

    /// `springchick ipc layers`: every layer surface and layer-rooted popup
    /// being composited, to find surfaces drawn after their client moved on.
    pub(crate) fn layers_dump(&self) -> String {
        let infos = self.layers.dump(self.dpi);
        let popups = self.active_popups();
        let rotated = if self.view_rotation().swaps_axes() {
            " rotated=yes(layers-not-drawn)"
        } else {
            ""
        };
        let mut parts = vec![format!(
            "out={}x{} dpi={} {}{} layers={} popups={}",
            self.panel_size.0,
            self.panel_size.1,
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
            // Geometry may be zero while a buffer is committed (wvkbd's key preview).
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

    /// On-screen popups, app-rooted first. Every hit popup takes the tap; only
    /// grabbing ones swallow outside taps (`touch::popup_press`). Layer popups
    /// are left out while turned, since layers aren't drawn.
    pub(crate) fn active_popups(&self) -> Vec<PopupRect> {
        let mut v = self.app_popups();
        if !self.view_rotation().swaps_axes() {
            v.extend(self.layer_popups());
        }
        v
    }

    pub(crate) fn popup_has_grab(&self, surface: &WlSurface) -> bool {
        self.popup_grabs.contains(surface)
    }

    /// 0 when a Top/Overlay surface covers the pill. The OSK is lifted above it
    /// and doesn't count.
    fn bar_alpha_target(&self) -> f32 {
        if self.view_rotation().swaps_axes() {
            return 1.0;
        }
        let (w, h) = self.output_size_f();
        let pill = sc_layout::pill_rect(w, h);
        if self.layers.top_overlaps(pill, self.dpi) {
            0.0
        } else {
            1.0
        }
    }

    /// ~0.13s fade. Multiplied with [`crate::bar_hint`]'s visibility policy.
    fn tick_bar_alpha(&mut self, now: std::time::Instant) -> f32 {
        self.bar_hint.set_mode(bar_mode(&self.ui), now);
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

    /// Also keeps the partial-damage fast path off.
    pub(crate) fn bar_fading(&self) -> bool {
        (self.bar_alpha - self.bar_alpha_target()).abs() > f32::EPSILON
            || self.bar_hint.is_animating(std::time::Instant::now())
    }

    /// False on a static screen so the vblank loop can stop and the GPU idle.
    /// `needs_render` and the springs re-arm it.
    pub(crate) fn is_animating(&self, now: std::time::Instant) -> bool {
        self.needs_render
            || self.ui.needs_animation()
            || !self.launching.is_empty()
            || self.osd.is_active(now)
            || self.bar_fading()
            || self.layers.sliding()
            // The held-back regrow is only checked from a frame.
            || self.layers.regrow_pending()
            // The lock confirmation waits for a presented frame.
            || self.session_lock.needs_frame()
            // A still finger sends no events, so the long-press timer needs frames.
            // Gated on `pointer_down`: a stale `icon_press` from a lost touch-up
            // would otherwise pin the loop on (battery).
            || (self.pointer_down && (self.icon_press.is_some() || self.bg_press.is_some()))
            || self
                .icon_menu
                .as_ref()
                .is_some_and(|m| !m.open.is_settled())
            || self.folder.as_ref().is_some_and(|f| !f.open.is_settled())
            // Deck fades outlive the deck on the way out.
            || self.card_chrome.is_animating()
            // Debug-input playback; always None in normal runs.
            || self.active_gesture.is_some()
            || self.active_key.is_some()
            || self.active_touch.is_some()
            || self.pending_settle.is_some()
            // An orientation change needs frames to settle.
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

    /// Tick springs, apply effects, compute the scene, and gather the render
    /// snapshot. Shared by both backends.
    pub(crate) fn advance_frame(&mut self, dt: f32) -> FramePrep {
        // First, so the snapshot matches the lock state being confirmed.
        self.session_lock.tick();

        // Before the scene, so a rotation landing this frame is the one drawn.
        self.tick_rotation(std::time::Instant::now());

        self.maybe_engage_arrange_hold();
        self.maybe_open_icon_menu();
        if let Some(menu) = &mut self.icon_menu {
            menu.open.step(dt);
        }
        if let Some(f) = &mut self.folder {
            f.open.step(dt);
            if f.closing && f.open.is_settled() {
                self.folder = None;
            }
        }

        // Seed lazily so springs snap to the current order, not in from (0,0).
        if self.grid_anim.is_empty() {
            self.reflow_grid();
        }
        if self.dock_anim.is_empty() {
            self.reflow_dock();
        }
        // Gated on the drag, not `hover`, so the dragged app leaves `grid_anim`
        // at once instead of double-drawing.
        if self.arrange.as_ref().is_some_and(|a| a.drag.is_some()) {
            self.reflow_grid();
            self.reflow_dock();
        }

        self.tick_edge_page_flip();

        // OSK slide-in is a render offset only. The app keeps its size until the
        // slide ends, so the keyboard rises over it; the resize and popup re-solve
        // land on the final frame.
        let was_sliding = self.layers.sliding();
        if !self.layers.tick_slides(dt) && was_sliding {
            self.recompute_layers();
        }

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

        // Super+Tab steps or release may have queued while the deck animated in.
        self.poll_kbd_switch();

        // Settling home resets page_count to 1; restore it.
        let pages = self.home_page_count();
        if let UiState::Home { page_count, .. } = &mut self.ui {
            *page_count = pages;
        }

        let scene = compute_scene(
            &self.ui,
            self.output_size(),
            self.app_origin(),
            self.card_radius,
        );
        self.switcher_cards = scene.cards.clone();
        // A commit from a toplevel absent here gets no frame (`commit_affects_frame`).
        self.drawn_toplevels.clear();
        self.drawn_toplevels
            .extend(scene.window.as_ref().map(|(tid, _)| *tid));
        self.drawn_toplevels
            .extend(scene.cards.iter().map(|c| c.toplevel));
        let disc = (
            std::mem::discriminant(&self.ui),
            ui_state::desired_focus(&self.ui),
        );
        if self.last_log_state != Some(disc) {
            self.last_log_state = Some(disc);
            debug!(target: "springchick::debug", "state changed to {:?} cards={}", self.ui, scene.cards.len());
        }

        // No-op except on the frame focus settles.
        self.apply_resource_tiers();

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
        // `origin` is the geometry top-left; the buffer's (0,0) sits `geometry.loc`
        // above-left of it (client shadows), as in smithay's placement.
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

        let touch_marks = if self.show_touches {
            self.touch_viz.prune(osd_now);
            if self.touch_viz.is_active(osd_now) {
                self.needs_render = true;
            }
            self.touch_viz.marks(osd_now)
        } else {
            Vec::new()
        };

        // Grid springs are global; subtract the page scroll. Overlays refill in
        // place since their keys are stable.
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

        // `CardChrome` owns its title copy so it survives the fade-out.
        let focused = match &self.ui {
            UiState::Switcher { cards, scroll, .. } => {
                crate::switcher::focused_card(cards, scroll.value)
            }
            _ => None,
        };
        let focused_title = focused.map(|tid| (tid, self.toplevel_title(tid)));
        self.card_chrome.advance(
            dt,
            focused_title
                .as_ref()
                .map(|(tid, title)| (*tid, title.as_str())),
        );
        let card_chrome = render::CardChromeView {
            icon_alpha: self.card_chrome.icon_alpha(),
            title: self
                .card_chrome
                .title()
                .map(|(tid, text, _)| (tid, text.to_string())),
            title_alpha: self.card_chrome.title().map_or(0.0, |(_, _, a)| a),
        };

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

        let library = scene.show_home.then(|| {
            let tiles = self.library_tiles();
            let previews = self
                .folders
                .iter()
                .map(|f| f.apps.iter().take(4).cloned().collect())
                .collect();
            render::LibraryView {
                tiles,
                previews,
                x_offset: self.library_x_offset(),
            }
        });
        let folder = self.folder_panel().map(|layout| render::FolderView {
            layout,
            title: self
                .folder
                .as_ref()
                .and_then(|f| self.folders.get(f.index))
                .map_or(String::new(), |f| f.name.to_string()),
            pressed: self.folder.as_ref().and_then(|f| f.pressed),
            anchor: self.folder.as_ref().map_or((0.0, 0.0), |f| f.anchor),
            progress: self.folder.as_ref().map_or(1.0, |f| f.open.value),
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
            library,
            folder,
            closing: self.layers.closing_view(),
            card_chrome,
            dim: self.rotation_fade.dim(std::time::Instant::now()),
        }
    }

    /// The render context for [`crate::render::draw_scene`]. One builder for
    /// both backends; inlined copies drifted.
    pub(crate) fn draw_ctx<'a>(
        &'a mut self,
        prep: &'a FramePrep,
        transform: smithay::utils::Transform,
        skia_flip_y: bool,
        report_partial_damage: bool,
        rounded_tex_shader: &'a smithay::backend::renderer::gles::GlesTexProgram,
        sinks: &'a mut render::FrameSinks,
    ) -> render::DrawCtx<'a> {
        // Resolved before the literal: `skia` and `last_present` borrow `self`
        // mutably.
        let (ox, oy) = self.app_origin();
        let app_origin = (ox.round() as i32, oy.round() as i32);
        let rotation = self.view_rotation();
        let dock_zone = self.arrange.as_ref().map(|_| {
            let (w, h) = self.output_size_f();
            sc_layout::compute(w, h, self.current_home_page(), &self.model).dock_zone
        });
        let arrange = self.arrange.as_ref().map(|a| {
            let drag = a.drag.as_ref();
            // Dock→dock is a no-op; only highlight drags that can pin.
            let over_dock = drag.is_some_and(|d| {
                d.source != crate::input_dispatch::IconSource::Dock
                    && dock_zone.is_some_and(|z| z.contains(d.cur.0, d.cur.1))
            });
            render::ArrangeView {
                drag_app: drag.map(|d| d.app_id.as_str()),
                drag_pos: drag.map(|d| d.cur),
                over_dock,
            }
        });
        let pressed_app = self.pending_launch.as_ref().map(|p| p.app_id.as_str());

        // One refresh out. Targeting "now" would hold each commit back a frame.
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
            catalog_gen: self.catalog_gen,
            toplevels: &self.toplevels,
            app_scale: self.dpi,
            app_origin,
            transform,
            rotation,
            skia_flip_y,
            frame_time: prep.frame_time,
            osd: prep.osd_view,
            touches: &prep.touch_marks,
            cursor: prep.cursor,
            lock_view: prep.lock_view,
            lock_surface: prep.lock_surface.as_ref(),
            layers_below: &prep.layers_below,
            layers_above: &prep.layers_above,
            closing: prep.closing.as_ref().map(|(b, r)| (b, *r)),
            app_popups: &prep.app_popups,
            layer_popups: &prep.layer_popups,
            bar_alpha: prep.bar_alpha,
            pill_anchor: None,
            pressed_app,
            launch_pulses: &self.icon_overlays.launch_pulses,
            running_apps: &self.icon_overlays.running_apps,
            arrange,
            icon_menu: prep.icon_menu.as_ref(),
            library: prep.library.as_ref(),
            folder: prep.folder.as_ref(),
            card_chrome: &prep.card_chrome,
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
