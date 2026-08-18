//! wlr-layer-shell geometry + app-area reservation, backed by smithay's
//! [`LayerMap`].
//!
//! We used to hand-roll surface tracking, geometry and configures. That missed
//! the map/unmap lifecycle smithay's `LayerMap` gets right: configuring a
//! surface mid-unmap (after smithay resets its cached anchor/size to Default)
//! sent a `(0,0)`/no-anchor configure that the client committed back, tripping
//! the `width 0 requested without setting left and right anchors` protocol
//! error and killing the whole client. `LayerMap::arrange` only ever configures
//! *mapped* surfaces, so it can't happen.
//!
//! ## Coordinate spaces
//!
//! `LayerMap` works in **logical** coordinates at the output scale. Our output
//! scale is `Scale::Fractional(dpi)`, so logical = physical / `dpi`. The rest of
//! the compositor (render origins, touch hit-testing, the Skia home bar) works
//! in **physical** px, so every geometry we read back from the map is scaled up
//! by `dpi` here. Layer clients render at fractional scale `dpi` (advertised via
//! `wp_fractional_scale`), so the logical configures the map sends them line up
//! with the physical buffers they produce.

use sc_layout::Rect;
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::desktop::{layer_map_for_output, LayerSurface, WindowSurfaceType};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
};
use std::collections::{HashMap, HashSet};

/// `(surface, physical origin)` pairs to composite, in bottom-to-top order.
pub type RenderList = Vec<(WlSurface, (i32, i32))>;

/// Seconds a bottom-docked layer surface (the OSK) takes to slide up into place
/// after it maps. Short enough not to delay typing, long enough to read as a
/// move rather than a pop.
const SLIDE_SECS: f32 = 0.18;

/// Seconds an area *increase* (the OSK unmapping) is held back before apps are
/// resized up again. An OSK that unmaps and remaps inside this window — wvkbd
/// swapping layouts, or a client that drops `zwp_text_input` focus for a beat —
/// never reaches the apps at all, so no configure storm follows it.
const REGROW_DELAY: f32 = 0.25;

/// Window in which repeated map transitions of the same surface count as
/// flapping.
const FLAP_WINDOW: f32 = 3.0;

/// Maps within [`FLAP_WINDOW`] that trip the latch.
const FLAP_LIMIT: usize = 4;

/// How long the usable area is pinned at its smallest once flapping is seen.
/// Long enough to outlast the cycle; short enough that a keyboard genuinely
/// dismissed afterwards still gives the space back.
const FLAP_HOLD: f32 = 10.0;

/// Whether a layer surface's `wl_surface` is currently mapped (has a buffer).
fn is_mapped(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

/// Scale a logical rectangle up to a physical [`Rect`].
fn to_physical(r: Rectangle<i32, Logical>, dpi: f64) -> Rect {
    Rect {
        x: (r.loc.x as f64 * dpi) as f32,
        y: (r.loc.y as f64 * dpi) as f32,
        w: (r.size.w as f64 * dpi) as f32,
        h: (r.size.h as f64 * dpi) as f32,
    }
}

/// Owns the per-output [`LayerMap`] handle and the not-yet-mapped surface set.
pub struct LayerShell {
    output: Output,
    /// Surfaces created but not yet mapped (no buffer committed). Drives the
    /// initial configure and the map/unmap transitions, mirroring niri.
    unmapped: HashSet<WlSurface>,
    /// Last physical usable area handed to apps; compared to detect changes.
    last_usable: Rect,
    /// Slide-in progress in `[0, 1)` for surfaces that just mapped, keyed by
    /// `wl_surface`. Entries are added on the unmapped→mapped transition and
    /// dropped once they reach 1 (and on unmap/destroy). Only *bottom-docked*
    /// surfaces actually move — see `place`.
    slides: HashMap<WlSurface, f32>,
    /// Physical output height, for the home-bar bottom exclusive zone.
    output_h: f32,
    /// Debounce + flap latch on area increases. See [`RegrowGuard`].
    regrow: RegrowGuard,
    /// Timestamps of recent unmapped→mapped transitions, per surface, trimmed
    /// to [`FLAP_WINDOW`]. Feeds the flap latch.
    map_events: HashMap<WlSurface, Vec<f32>>,
}

/// Rate limiting on *growing* the app area back.
///
/// Resizing an app is what makes some clients drop `zwp_text_input` focus,
/// which unmaps the OSK, which grows the app back — a cycle that on device
/// looks like the keyboard being summoned and dismissed forever. So an increase
/// waits out [`REGROW_DELAY`] (an OSK that returns inside it never reaches the
/// apps at all), and once a surface is seen flapping, increases are refused
/// entirely for [`FLAP_HOLD`]. Holding the app at the smaller size costs
/// nothing — the space it gives up is empty.
#[derive(Debug, Default)]
struct RegrowGuard {
    /// Monotonic seconds, advanced by [`RegrowGuard::tick`]. Only differences
    /// matter, so the origin is arbitrary.
    now: f32,
    /// Deadline at which a held-back increase may be applied. Set the first
    /// time the area grows, cleared when applied or when it shrinks again.
    pending: Option<f32>,
    /// While `now` is below this, increases are refused outright.
    flap_until: f32,
}

impl RegrowGuard {
    fn tick(&mut self, dt: f32) {
        self.now += dt;
    }

    /// Whether an area increase may be applied now. Starts (or continues) the
    /// debounce when it may not.
    fn allow_grow(&mut self) -> bool {
        if self.now < self.flap_until {
            return false;
        }
        match self.pending {
            // First frame of the increase: start the debounce.
            None => {
                self.pending = Some(self.now + REGROW_DELAY);
                false
            }
            Some(deadline) => self.now >= deadline,
        }
    }

    /// The area settled (grew and was applied, or shrank): drop the debounce.
    fn resolved(&mut self) {
        self.pending = None;
    }

    /// A surface flapped: pin the area at its smallest for [`FLAP_HOLD`].
    fn latch(&mut self) {
        self.flap_until = self.now + FLAP_HOLD;
    }

    /// Whether an increase is being held back right now, either by the debounce
    /// or the flap latch. The frame loop keeps drawing while this is true so the
    /// hold has a frame to expire on — otherwise a keyboard that stops flapping
    /// and stays gone leaves the app shrunk until some unrelated commit.
    fn holding(&self) -> bool {
        self.pending.is_some() || self.now < self.flap_until
    }
}

/// Record a map at `now` in `events` and report whether the surface has now
/// mapped [`FLAP_LIMIT`] times inside [`FLAP_WINDOW`]. Trips at most once per
/// burst: a run that trips clears its history.
fn flapped(events: &mut Vec<f32>, now: f32) -> bool {
    events.retain(|t| now - *t <= FLAP_WINDOW);
    events.push(now);
    let tripped = events.len() >= FLAP_LIMIT;
    if tripped {
        events.clear();
    }
    tripped
}

impl LayerShell {
    pub fn new(output: Output, output_w: f32, output_h: f32) -> Self {
        LayerShell {
            output,
            unmapped: HashSet::new(),
            last_usable: Rect {
                x: 0.0,
                y: 0.0,
                w: output_w,
                h: output_h,
            },
            slides: HashMap::new(),
            output_h,
            regrow: RegrowGuard::default(),
            map_events: HashMap::new(),
        }
    }

    /// Advance every in-flight slide by `dt` seconds, dropping the ones that
    /// finished. Returns true while any is still moving, so the frame loop keeps
    /// presenting.
    pub fn tick_slides(&mut self, dt: f32) -> bool {
        self.regrow.tick(dt);
        self.slides.retain(|_, p| {
            *p = (*p + dt / SLIDE_SECS).min(1.0);
            *p < 1.0
        });
        !self.slides.is_empty()
    }

    /// True while any layer surface is still sliding in.
    pub fn sliding(&self) -> bool {
        !self.slides.is_empty()
    }

    /// Recompute the usable area after an arrange. Returns `Some(new)` if it
    /// changed since the last call (so the caller resizes app toplevels).
    ///
    /// Increases are debounced, and refused outright while a client is flapping
    /// its keyboard — see [`RegrowGuard`].
    pub fn usable_changed(&mut self, dpi: f64) -> Option<Rect> {
        let now = self.usable(dpi);
        if now == self.last_usable {
            return None;
        }
        if now.h > self.last_usable.h && !self.regrow.allow_grow() {
            return None;
        }
        self.regrow.resolved();
        self.last_usable = now;
        Some(now)
    }

    /// True while an area increase is being held back (debounce or flap latch),
    /// so the frame loop keeps rendering long enough to apply it.
    pub fn regrow_pending(&self) -> bool {
        self.regrow.holding()
    }

    /// Record an unmapped→mapped transition, latching the guard if this surface
    /// is cycling.
    fn record_map(&mut self, surface: &WlSurface) {
        let now = self.regrow.now;
        let events = self.map_events.entry(surface.clone()).or_default();
        if flapped(events, now) {
            tracing::warn!("layer surface mapping repeatedly; holding app size for {FLAP_HOLD}s");
            self.regrow.latch();
        }
    }

    /// A new layer surface was created. Track it as unmapped and register it
    /// with the map; geometry + initial configure follow on its first commit.
    pub fn new_surface(&mut self, surface: WlrLayerSurface, namespace: String) {
        self.unmapped.insert(surface.wl_surface().clone());
        let mut map = layer_map_for_output(&self.output);
        // Only fails if already mapped, which a fresh surface never is.
        let _ = map.map_layer(&LayerSurface::new(surface, namespace));
    }

    /// A layer surface was destroyed. Returns true if it was mapped (so the
    /// caller recomputes the app area).
    pub fn destroyed(&mut self, surface: &WlrLayerSurface) -> bool {
        self.unmapped.remove(surface.wl_surface());
        self.slides.remove(surface.wl_surface());
        // A client that exits and relaunches its keyboard is not the cycle this
        // guards against, so its history goes with it.
        self.map_events.remove(surface.wl_surface());
        let mut map = layer_map_for_output(&self.output);
        let Some(layer) = map.layers().find(|l| l.layer_surface() == surface).cloned() else {
            return false;
        };
        map.unmap_layer(&layer);
        true
    }

    /// Handle a `wl_surface` commit. Returns true if it belonged to a layer
    /// surface (so the caller recomputes the app area / redraws). Arranges the
    /// map and drives the map/unmap transition, mirroring niri's flow.
    pub fn handle_commit(&mut self, surface: &WlSurface) -> bool {
        let mut map = layer_map_for_output(&self.output);
        if map
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .is_none()
        {
            return false;
        }

        // Arrange before the initial configure so the client's requested size is
        // respected. `arrange` only configures mapped surfaces.
        map.arrange();
        let layer = map
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .unwrap()
            .clone();
        // Everything below works off the clone; dropping the guard here keeps
        // `&mut self` usable (the map borrows `self.output`).
        drop(map);

        if is_mapped(surface) {
            // Unmapped → mapped: start the slide-in. Commits on an
            // already-mapped surface (the OSK redrawing) must not restart it.
            if self.unmapped.remove(surface) {
                self.slides.insert(surface.clone(), 0.0);
                self.record_map(surface);
            }
        } else if !self.unmapped.contains(surface) {
            // Was mapped, now unmapped via a null commit: it must redo the
            // initial configure sequence before mapping again.
            self.unmapped.insert(surface.clone());
            self.slides.remove(surface);
        } else {
            // Still unmapped. If we haven't sent the initial configure, do so;
            // otherwise `arrange` already sent any needed configure.
            let initial_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<LayerSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            });
            if !initial_sent {
                layer.layer_surface().send_configure();
            }
        }
        true
    }

    /// Whether `surface` belongs to a currently *mapped* layer surface.
    ///
    /// Used by [`crate::idle_inhibit`]: an idle inhibitor on a shell layer
    /// surface counts, but only while that surface is actually on screen.
    pub fn is_mapped_layer(&self, surface: &WlSurface) -> bool {
        if self.unmapped.contains(surface) {
            return false;
        }
        layer_map_for_output(&self.output)
            .layer_for_surface(surface, WindowSurfaceType::ALL)
            .is_some()
    }

    /// Physical usable area (the output minus exclusive-zone reservations).
    pub fn usable(&self, dpi: f64) -> Rect {
        let zone = layer_map_for_output(&self.output).non_exclusive_zone();
        let mut r = to_physical(zone, dpi);
        // Reserve the home gesture bar's zone off the bottom (physical px, so
        // it matches the pill draw_bar renders at physical framebuffer size).
        // Always reserved: bottom-docked layer surfaces (the OSK) are lifted by
        // the same amount (see `shift_docked`), so the pill's strip stays clear
        // beneath them rather than the gap landing above the keyboard.
        r.h = (r.h - self.gesture_zone()).max(0.0);
        r
    }

    /// Physical height of the home gesture bar's bottom reservation.
    fn gesture_zone(&self) -> f32 {
        sc_layout::gesture_exclusive_zone(self.output_h)
    }

    /// Lift a physical layer rect docked to the screen bottom (the OSK, bottom
    /// bars) up by the gesture zone, so the home pill's strip stays clear
    /// beneath it. Fullscreen surfaces (reaching the top edge too) are left be.
    fn shift_docked(&self, mut r: Rect) -> Rect {
        if self.is_docked(r) {
            r.y -= self.gesture_zone();
        }
        r
    }

    /// Whether a physical layer rect is docked to the bottom edge (and isn't a
    /// fullscreen surface reaching the top too).
    fn is_docked(&self, r: Rect) -> bool {
        r.y + r.h >= self.output_h - 1.0 && r.y > 1.0
    }

    /// Where a layer surface is drawn right now: its logical geometry scaled to
    /// physical, lifted clear of the home pill, and pushed back down by however
    /// much of its slide-in remains.
    ///
    /// Only bottom-docked surfaces slide — a top bar or fullscreen overlay would
    /// travel the wrong way, so they just appear.
    fn place(&self, surface: &WlSurface, geo: Rectangle<i32, Logical>, dpi: f64) -> Rect {
        let phys = to_physical(geo, dpi);
        let mut r = self.shift_docked(phys);
        if self.is_docked(phys) {
            if let Some(&p) = self.slides.get(surface) {
                r.y += sc_anim::slide_in_offset(p, r.h);
            }
        }
        r
    }

    /// `(surface, physical origin)` pairs for the render pass, split into those
    /// drawn below the app (background, bottom) and above it (top, overlay),
    /// each in bottom-to-top order.
    pub fn render_lists(&self, dpi: f64) -> (RenderList, RenderList) {
        let map = layer_map_for_output(&self.output);
        let collect = |layers: &[Layer]| {
            let mut v = Vec::new();
            for &wanted in layers {
                for layer in map.layers().filter(|l| l.layer() == wanted) {
                    if let Some(geo) = map.layer_geometry(layer) {
                        let r = self.place(layer.wl_surface(), geo, dpi);
                        v.push((layer.wl_surface().clone(), (r.x as i32, r.y as i32)));
                    }
                }
            }
            v
        };
        (
            collect(&[Layer::Background, Layer::Bottom]),
            collect(&[Layer::Top, Layer::Overlay]),
        )
    }

    /// Whether any Top/Overlay surface overlaps `rect` (physical) — used to hide
    /// the home bar when the on-screen keyboard covers it.
    pub fn top_overlaps(&self, rect: Rect, dpi: f64) -> bool {
        let map = layer_map_for_output(&self.output);
        // Collect first so the `layers()` borrow ends before `layer_geometry`.
        let tops: Vec<LayerSurface> = map
            .layers()
            .filter(|l| matches!(l.layer(), Layer::Top | Layer::Overlay))
            .cloned()
            .collect();
        tops.iter().any(|l| {
            map.layer_geometry(l)
                .is_some_and(|g| rects_overlap(self.place(l.wl_surface(), g, dpi), rect))
        })
    }

    /// The topmost hit-testable (Top/Overlay) surface containing the physical
    /// point, with its physical origin. Overlay is above Top; within a layer,
    /// later-created is on top.
    pub fn hit_test(&self, x: f32, y: f32, dpi: f64) -> Option<(WlSurface, (i32, i32))> {
        let map = layer_map_for_output(&self.output);
        for wanted in [Layer::Overlay, Layer::Top] {
            // Collect (ending the `layers()` borrow), then `.rev()` on insertion
            // order gives the topmost (latest-created) match within the layer.
            let candidates: Vec<LayerSurface> = map
                .layers()
                .filter(|l| l.layer() == wanted)
                .cloned()
                .collect();
            for layer in candidates.iter().rev() {
                if let Some(geo) = map.layer_geometry(layer) {
                    // The animated position, so a tap lands where the surface is
                    // drawn rather than where it will settle.
                    let rect = self.place(layer.wl_surface(), geo, dpi);
                    if rect.contains(x, y) {
                        return Some((layer.wl_surface().clone(), (rect.x as i32, rect.y as i32)));
                    }
                }
            }
        }
        None
    }
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An OSK that unmaps and comes back inside the debounce never resizes the
    /// app: the increase is asked for, refused, and dropped when it shrinks.
    #[test]
    fn brief_unmap_never_grows_the_app() {
        let mut g = RegrowGuard::default();
        assert!(!g.allow_grow(), "first ask starts the debounce");
        g.tick(REGROW_DELAY / 2.0);
        assert!(!g.allow_grow());
        // OSK back: the area shrank again, so the pending grow is dropped.
        g.resolved();
        g.tick(1.0);
        assert!(!g.allow_grow(), "a later unmap starts a fresh debounce");
    }

    /// A keyboard really dismissed gives the space back once the debounce ends.
    #[test]
    fn settled_unmap_grows_after_the_delay() {
        let mut g = RegrowGuard::default();
        assert!(!g.allow_grow());
        g.tick(REGROW_DELAY + 0.01);
        assert!(g.allow_grow());
    }

    /// While latched, no increase is applied however long it is asked for.
    #[test]
    fn flap_latch_refuses_grows_then_releases() {
        let mut g = RegrowGuard::default();
        g.latch();
        for _ in 0..20 {
            g.tick(FLAP_HOLD / 20.0);
            assert!(!g.allow_grow(), "latched");
        }
        assert!(g.holding());
        g.tick(0.1);
        // Latch expired: the normal debounce takes over, then the grow lands.
        assert!(!g.allow_grow());
        g.tick(REGROW_DELAY + 0.01);
        assert!(g.allow_grow());
        g.resolved();
        assert!(!g.holding());
    }

    #[test]
    fn flap_trips_only_on_a_fast_burst() {
        let mut events = Vec::new();
        // Slow, legitimate keyboard use: each map is outside the window of the
        // one before it, so nothing accumulates.
        let mut t = 0.0;
        for _ in 0..10 {
            t += FLAP_WINDOW + 0.1;
            assert!(!flapped(&mut events, t));
        }
        // A cycling client: FLAP_LIMIT maps inside one window trips it. The
        // last slow map above is still inside the window, so it counts as the
        // first of the burst.
        for i in 2..FLAP_LIMIT {
            assert!(!flapped(&mut events, t + i as f32 * 0.1), "map {i}");
        }
        assert!(flapped(&mut events, t + (FLAP_LIMIT - 1) as f32 * 0.1));
        // History cleared, so the same burst has to build up again.
        assert!(!flapped(&mut events, t + 10.0));
    }
}
