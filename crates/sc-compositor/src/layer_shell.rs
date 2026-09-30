//! wlr-layer-shell on smithay's [`LayerMap`], plus the app-area reservation.
//! `LayerMap::arrange` only configures mapped surfaces; configuring one
//! mid-unmap sends a zero-size configure that kills the client.
//!
//! The map is logical (physical / `dpi`); everything read back is scaled to
//! physical here.

use sc_layout::Rect;
use smithay::backend::renderer::utils::{with_renderer_surface_state, Buffer};
use smithay::desktop::{layer_map_for_output, LayerSurface, WindowSurfaceType};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::compositor::{with_states, SurfaceAttributes};
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
};
use std::collections::{HashMap, HashSet};

pub type RenderList = Vec<(WlSurface, (i32, i32))>;

/// OSK slide-in duration after it maps.
const SLIDE_SECS: f32 = 0.18;

/// Hold area increases this long, so an OSK that unmaps and remaps (wvkbd
/// swapping layouts, a brief text-input focus drop) never resizes apps.
const REGROW_DELAY: f32 = 0.25;

const FLAP_WINDOW: f32 = 3.0;

const FLAP_LIMIT: usize = 4;

/// How long the usable area stays pinned at its smallest after flapping.
const FLAP_HOLD: f32 = 10.0;

pub fn is_mapped(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

/// Logical px.
pub fn buffer_size(surface: &WlSurface) -> Option<(i32, i32)> {
    with_renderer_surface_state(surface, |state| state.buffer_size().map(|s| (s.w, s.h)))
        .unwrap_or(None)
}

fn to_physical(r: Rectangle<i32, Logical>, dpi: f64) -> Rect {
    Rect {
        x: (r.loc.x as f64 * dpi) as f32,
        y: (r.loc.y as f64 * dpi) as f32,
        w: (r.size.w as f64 * dpi) as f32,
        h: (r.size.h as f64 * dpi) as f32,
    }
}

/// One layer surface, for the `layers` IPC dump.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerInfo {
    pub namespace: String,
    pub layer: &'static str,
    /// Logical `(x, y, w, h)`; `None` until arranged.
    pub geo: Option<(i32, i32, i32, i32)>,
    /// Physical, slide offset included.
    pub placed: Option<Rect>,
    /// Logical px. `None` = nothing to draw.
    pub buffer: Option<(i32, i32)>,
    /// Waiting for the commit that maps it.
    pub pending_map: bool,
    pub anchor: u32,
    /// `>0` reserved, `0` neutral, `-1` don't care.
    pub exclusive: i32,
    pub slide: Option<f32>,
}

fn layer_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Background => "background",
        Layer::Bottom => "bottom",
        Layer::Top => "top",
        Layer::Overlay => "overlay",
    }
}

fn fmt_rect(r: Rect) -> String {
    format!(
        "{},{}+{}x{}",
        r.x.round(),
        r.y.round(),
        r.w.round(),
        r.h.round()
    )
}

pub fn format_layer(idx: usize, l: &LayerInfo) -> String {
    let mut s = format!("#{idx} ns={} layer={}", l.namespace, l.layer);
    match l.geo {
        Some((x, y, w, h)) => s.push_str(&format!(" geo={x},{y}+{w}x{h}")),
        None => s.push_str(" geo=none"),
    }
    match l.placed {
        Some(r) => s.push_str(&format!(" draw={}", fmt_rect(r))),
        None => s.push_str(" draw=none"),
    }
    match l.buffer {
        Some((w, h)) => s.push_str(&format!(" buf={w}x{h}")),
        None => s.push_str(" buf=none"),
    }
    if l.pending_map {
        s.push_str(" pending-map");
    }
    s.push_str(&format!(" anchor={} excl={}", l.anchor, l.exclusive));
    if let Some(p) = l.slide {
        s.push_str(&format!(" slide={p:.2}"));
    }
    s
}

/// A bottom-docked surface (the OSK) the client just hid, still drawn from
/// its last buffer while it slides out. The buffer is captured in a
/// pre-commit/destruction hook before smithay resets it; holding the clone
/// withholds `wl_buffer.release` until the animation ends.
struct ClosingLayer {
    /// Identity only, to spot a remap of the same surface. May be destroyed.
    surface: WlSurface,
    buffer: Buffer,
    /// Physical; a mid-flight slide-in continues from here.
    rect: Rect,
    progress: f32,
}

pub struct LayerShell {
    output: Output,
    /// Created but not yet mapped; drives the initial configure and map/unmap
    /// transitions (mirrors niri).
    unmapped: HashSet<WlSurface>,
    last_usable: Rect,
    /// Slide-in progress `[0, 1)`; only bottom-docked surfaces move.
    slides: HashMap<WlSurface, f32>,
    output_h: f32,
    regrow: RegrowGuard,
    /// Per-surface map timestamps within [`FLAP_WINDOW`].
    map_events: HashMap<WlSurface, Vec<f32>>,
    /// An `OnDemand` surface only takes keyboard focus after a tap on it.
    focus_tap: Option<WlSurface>,
    /// One at a time; a second unmap replaces the first.
    closing: Option<ClosingLayer>,
}

/// Rate limit on growing the app area back. Resizing an app can make it drop
/// text-input focus, which unmaps the OSK, which grows the app: an endless
/// show/hide cycle. Increases wait [`REGROW_DELAY`], and a flapping surface
/// blocks them for [`FLAP_HOLD`].
#[derive(Debug, Default)]
struct RegrowGuard {
    /// Monotonic seconds, arbitrary origin.
    now: f32,
    /// When a held-back increase may apply.
    pending: Option<f32>,
    /// Increases are refused until then.
    flap_until: f32,
}

impl RegrowGuard {
    fn tick(&mut self, dt: f32) {
        self.now += dt;
    }

    /// Starts or continues the debounce when not yet allowed.
    fn allow_grow(&mut self) -> bool {
        if self.now < self.flap_until {
            return false;
        }
        match self.pending {
            None => {
                self.pending = Some(self.now + REGROW_DELAY);
                false
            }
            Some(deadline) => self.now >= deadline,
        }
    }

    fn resolved(&mut self) {
        self.pending = None;
    }

    /// Pre-arm the debounce to expire when an OSK slide-out ends.
    fn hold_until(&mut self, deadline: f32) {
        self.pending = Some(deadline);
    }

    fn latch(&mut self) {
        self.flap_until = self.now + FLAP_HOLD;
    }

    /// Keeps the frame loop drawing so the hold has a frame to expire on;
    /// otherwise the app stays shrunk until an unrelated commit.
    fn holding(&self) -> bool {
        self.pending.is_some() || self.now < self.flap_until
    }
}

/// Records a map and reports [`FLAP_LIMIT`] maps within [`FLAP_WINDOW`].
/// Clears the history when it trips.
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
            focus_tap: None,
            closing: None,
        }
    }

    /// Returns true while anything is moving. Dropping a finished `closing`
    /// releases its buffer to the client.
    pub fn tick_slides(&mut self, dt: f32) -> bool {
        self.regrow.tick(dt);
        self.slides.retain(|_, p| {
            *p = (*p + dt / SLIDE_SECS).min(1.0);
            *p < 1.0
        });
        if let Some(c) = &mut self.closing {
            c.progress = (c.progress + dt / SLIDE_SECS).min(1.0);
        }
        if self.closing.as_ref().is_some_and(|c| c.progress >= 1.0) {
            self.closing = None;
        }
        !self.slides.is_empty() || self.closing.is_some()
    }

    /// For when frames stop (panel blanked): nothing would tick the slides, and
    /// a slide-out would hold the client's buffer until wake.
    pub fn end_slides(&mut self) {
        self.slides.clear();
        self.closing = None;
    }

    pub fn sliding(&self) -> bool {
        !self.slides.is_empty() || self.closing.is_some()
    }

    /// Pre-commit hook for a null commit: the last buffer is still intact. A
    /// mapped bottom-docked surface keeps it and slides out; others just vanish.
    pub fn note_hide(&mut self, surface: &WlSurface, dpi: f64) {
        let removing = with_states(surface, |states| {
            matches!(
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer,
                Some(smithay::wayland::compositor::BufferAssignment::Removed)
            )
        });
        if !removing || self.unmapped.contains(surface) {
            return;
        }
        let Some(buffer) = with_renderer_surface_state(surface, |s| s.buffer().cloned()).flatten()
        else {
            return;
        };
        let map = layer_map_for_output(&self.output);
        let rect = map
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .and_then(|l| map.layer_geometry(l))
            .filter(|geo| self.is_docked(to_physical(*geo, dpi)))
            .map(|geo| self.place(surface, geo, dpi));
        drop(map);
        let Some(rect) = rect else { return };
        self.closing = Some(ClosingLayer {
            surface: surface.clone(),
            buffer,
            rect,
            progress: 0.0,
        });
        // `recompute_layers` bails while a slide runs, so the debounce must already
        // be expired when the slide-out lands.
        self.regrow.hold_until(self.regrow.now + SLIDE_SECS);
    }

    pub fn closing_view(&self) -> Option<(Buffer, Rect)> {
        let c = self.closing.as_ref()?;
        let mut rect = c.rect;
        let dist = self.output_h - rect.y;
        // Ease-out on the way out too; the slide-in curve reversed hangs then snaps.
        rect.y += sc_anim::ease_out_cubic(c.progress) * dist;
        Some((c.buffer.clone(), rect))
    }

    /// `Some(new)` when the usable area changed. Increases go through
    /// [`RegrowGuard`].
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

    pub fn regrow_pending(&self) -> bool {
        self.regrow.holding()
    }

    fn record_map(&mut self, surface: &WlSurface) {
        let now = self.regrow.now;
        let events = self.map_events.entry(surface.clone()).or_default();
        if flapped(events, now) {
            tracing::warn!("layer surface mapping repeatedly; holding app size for {FLAP_HOLD}s");
            self.regrow.latch();
        }
    }

    /// Geometry and the initial configure follow on its first commit.
    pub fn new_surface(&mut self, surface: WlrLayerSurface, namespace: String) {
        self.unmapped.insert(surface.wl_surface().clone());
        let mut map = layer_map_for_output(&self.output);
        // Only fails if already mapped.
        let _ = map.map_layer(&LayerSurface::new(surface, namespace));
    }

    /// wvkbd 0.20 hides by destroying the layer surface. The `wl_surface` is
    /// still alive (its hook runs after the role's), so the last buffer can be
    /// held for a slide-out. Returns true if it was mapped.
    pub fn destroyed(&mut self, surface: &WlrLayerSurface, dpi: f64) -> bool {
        let wl = surface.wl_surface();
        let was_mapped = !self.unmapped.contains(wl);
        self.unmapped.remove(wl);
        self.slides.remove(wl);
        // A client relaunching its keyboard isn't the flap cycle.
        self.map_events.remove(wl);
        if self.focus_tap.as_ref() == Some(wl) {
            self.focus_tap = None;
        }
        let mut map = layer_map_for_output(&self.output);
        let Some(layer) = map.layers().find(|l| l.layer_surface() == surface).cloned() else {
            return false;
        };
        // Before `unmap_layer`, which drops the geometry.
        let geo = map.layer_geometry(&layer).filter(|_| was_mapped);
        let buffer = was_mapped
            .then(|| with_renderer_surface_state(wl, |s| s.buffer().cloned()))
            .flatten()
            .flatten();
        map.unmap_layer(&layer);
        drop(map);
        if let (Some(geo), Some(buffer)) = (geo, buffer) {
            let phys = to_physical(geo, dpi);
            if self.is_docked(phys) {
                self.closing = Some(ClosingLayer {
                    surface: wl.clone(),
                    buffer,
                    rect: self.place(wl, geo, dpi),
                    progress: 0.0,
                });
                self.regrow.hold_until(self.regrow.now + SLIDE_SECS);
            }
        }
        true
    }

    /// Returns true if it belonged to a layer surface. Mirrors niri's flow.
    pub fn handle_commit(&mut self, surface: &WlSurface) -> bool {
        let mut map = layer_map_for_output(&self.output);
        if map
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .is_none()
        {
            return false;
        }

        // Arrange before the initial configure so the requested size is respected.
        map.arrange();
        let layer = map
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .unwrap()
            .clone();
        // Release the map guard; it borrows `self.output`.
        drop(map);

        if is_mapped(surface) {
            // Only the unmapped → mapped transition starts a slide-in.
            if self.unmapped.remove(surface) {
                self.slides.insert(surface.clone(), 0.0);
                self.record_map(surface);
                // Back mid slide-out (wvkbd swapping layouts): drop the held buffer.
                if self.closing.as_ref().is_some_and(|c| &c.surface == surface) {
                    self.closing = None;
                    self.regrow.resolved();
                }
            }
        } else if !self.unmapped.contains(surface) {
            // Unmapped by a null commit: must redo the initial configure to remap.
            self.unmapped.insert(surface.clone());
            self.slides.remove(surface);
            if self.focus_tap.as_ref() == Some(surface) {
                self.focus_tap = None;
            }
        } else {
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

    /// For [`crate::idle_inhibit`]: counts only while on screen.
    pub fn is_mapped_layer(&self, surface: &WlSurface) -> bool {
        if self.unmapped.contains(surface) {
            return false;
        }
        layer_map_for_output(&self.output)
            .layer_for_surface(surface, WindowSurfaceType::ALL)
            .is_some()
    }

    /// Output minus exclusive zones, physical.
    pub fn usable(&self, dpi: f64) -> Rect {
        let zone = layer_map_for_output(&self.output).non_exclusive_zone();
        let mut r = to_physical(zone, dpi);
        // Always reserve the home bar zone. Bottom-docked surfaces are lifted by
        // the same amount (`shift_docked`), so the pill's strip stays below them.
        r.h = (r.h - self.gesture_zone()).max(0.0);
        r
    }

    fn gesture_zone(&self) -> f32 {
        sc_layout::gesture_exclusive_zone(self.output_h)
    }

    /// Lift a bottom-docked rect clear of the home pill. Fullscreen surfaces are
    /// left alone.
    fn shift_docked(&self, mut r: Rect) -> Rect {
        if self.is_docked(r) {
            r.y -= self.gesture_zone();
        }
        r
    }

    fn is_docked(&self, r: Rect) -> bool {
        r.y + r.h >= self.output_h - 1.0 && r.y > 1.0
    }

    /// Scaled to physical, lifted clear of the pill, offset by the remaining
    /// slide-in. Only bottom-docked surfaces slide.
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

    /// Below the app (background, bottom) and above it (top, overlay), each
    /// bottom-to-top.
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

    /// Unfiltered on purpose: the dump is for finding surfaces drawn that
    /// shouldn't be.
    pub fn dump(&self, dpi: f64) -> Vec<LayerInfo> {
        let map = layer_map_for_output(&self.output);
        let mut out = Vec::new();
        for wanted in [Layer::Background, Layer::Bottom, Layer::Top, Layer::Overlay] {
            for layer in map.layers().filter(|l| l.layer() == wanted) {
                let surface = layer.wl_surface();
                let geo = map.layer_geometry(layer);
                let state = layer.cached_state();
                out.push(LayerInfo {
                    namespace: layer.namespace().to_string(),
                    layer: layer_name(wanted),
                    geo: geo.map(|g| (g.loc.x, g.loc.y, g.size.w, g.size.h)),
                    placed: geo.map(|g| self.place(surface, g, dpi)),
                    buffer: buffer_size(surface),
                    pending_map: self.unmapped.contains(surface),
                    anchor: state.anchor.bits(),
                    exclusive: state.exclusive_zone.into(),
                    slide: self.slides.get(surface).copied(),
                });
            }
        }
        out
    }

    pub fn dump_header(&self, dpi: f64) -> String {
        let u = self.usable(dpi);
        let closing = self
            .closing
            .as_ref()
            .map(|c| format!(" closing={:.2}", c.progress))
            .unwrap_or_default();
        format!(
            "usable={} regrow={} sliding={}{closing}",
            fmt_rect(u),
            if self.regrow.now < self.regrow.flap_until {
                "flap-latched"
            } else if self.regrow.pending.is_some() {
                "debounced"
            } else {
                "idle"
            },
            self.slides.len(),
        )
    }

    /// Hides the home bar when the OSK covers it.
    pub fn top_overlaps(&self, rect: Rect, dpi: f64) -> bool {
        let map = layer_map_for_output(&self.output);
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

    /// Overlay above Top; within a layer, later-created is on top.
    pub fn hit_test(&self, x: f32, y: f32, dpi: f64) -> Option<(WlSurface, (i32, i32))> {
        let map = layer_map_for_output(&self.output);
        for wanted in [Layer::Overlay, Layer::Top] {
            let candidates: Vec<LayerSurface> = map
                .layers()
                .filter(|l| l.layer() == wanted)
                .cloned()
                .collect();
            for layer in candidates.iter().rev() {
                if let Some(geo) = map.layer_geometry(layer) {
                    // The animated position, so a tap lands where the surface is drawn.
                    let rect = self.place(layer.wl_surface(), geo, dpi);
                    if rect.contains(x, y) {
                        return Some((layer.wl_surface().clone(), (rect.x as i32, rect.y as i32)));
                    }
                }
            }
        }
        None
    }

    /// A tap anywhere else clears it.
    pub fn note_tap(&mut self, surface: &WlSurface) {
        // Focus belongs to the layer surface, not the tapped subsurface.
        self.focus_tap = layer_map_for_output(&self.output)
            .layer_for_surface(surface, WindowSurfaceType::ALL)
            .map(|l| l.wl_surface().clone());
    }

    /// `Exclusive` takes focus outright (topmost first); `OnDemand` once tapped.
    /// Without this `zwp_text_input` never focuses a shell dialog and the OSK
    /// can't auto-show.
    pub fn keyboard_focus(&self) -> Option<WlSurface> {
        let map = layer_map_for_output(&self.output);
        let mut on_demand = None;
        for wanted in [Layer::Overlay, Layer::Top] {
            let candidates: Vec<LayerSurface> = map
                .layers()
                .filter(|l| l.layer() == wanted && !self.unmapped.contains(l.wl_surface()))
                .cloned()
                .collect();
            for layer in candidates.iter().rev() {
                match layer.cached_state().keyboard_interactivity {
                    KeyboardInteractivity::Exclusive => return Some(layer.wl_surface().clone()),
                    KeyboardInteractivity::OnDemand
                        if on_demand.is_none()
                            && self.focus_tap.as_ref() == Some(layer.wl_surface()) =>
                    {
                        on_demand = Some(layer.wl_surface().clone());
                    }
                    _ => {}
                }
            }
        }
        on_demand
    }
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> LayerInfo {
        LayerInfo {
            namespace: "wvkbd".into(),
            layer: "overlay",
            geo: Some((0, 1730, 437, 250)),
            placed: Some(Rect {
                x: 0.0,
                y: 4844.0,
                w: 1224.0,
                h: 700.0,
            }),
            buffer: Some((437, 250)),
            pending_map: false,
            anchor: 14,
            exclusive: 250,
            slide: None,
        }
    }

    #[test]
    fn format_layer_reports_geometry_and_buffer() {
        assert_eq!(
            format_layer(0, &info()),
            "#0 ns=wvkbd layer=overlay geo=0,1730+437x250 draw=0,4844+1224x700 buf=437x250 \
             anchor=14 excl=250"
        );
    }

    #[test]
    fn format_layer_marks_unmapped_and_sliding() {
        let l = LayerInfo {
            geo: None,
            placed: None,
            buffer: None,
            pending_map: true,
            slide: Some(0.25),
            ..info()
        };
        assert_eq!(
            format_layer(2, &l),
            "#2 ns=wvkbd layer=overlay geo=none draw=none buf=none pending-map \
             anchor=14 excl=250 slide=0.25"
        );
    }

    #[test]
    fn brief_unmap_never_grows_the_app() {
        let mut g = RegrowGuard::default();
        assert!(!g.allow_grow(), "first ask starts the debounce");
        g.tick(REGROW_DELAY / 2.0);
        assert!(!g.allow_grow());
        g.resolved();
        g.tick(1.0);
        assert!(!g.allow_grow(), "a later unmap starts a fresh debounce");
    }

    #[test]
    fn settled_unmap_grows_after_the_delay() {
        let mut g = RegrowGuard::default();
        assert!(!g.allow_grow());
        g.tick(REGROW_DELAY + 0.01);
        assert!(g.allow_grow());
    }

    #[test]
    fn hold_until_expires_at_the_slide_end() {
        let mut g = RegrowGuard::default();
        g.hold_until(g.now + SLIDE_SECS);
        assert!(!g.allow_grow(), "the slide is still running");
        g.tick(SLIDE_SECS + 0.01);
        assert!(g.allow_grow());
    }

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
        assert!(!g.allow_grow());
        g.tick(REGROW_DELAY + 0.01);
        assert!(g.allow_grow());
        g.resolved();
        assert!(!g.holding());
    }

    #[test]
    fn flap_trips_only_on_a_fast_burst() {
        let mut events = Vec::new();
        let mut t = 0.0;
        for _ in 0..10 {
            t += FLAP_WINDOW + 0.1;
            assert!(!flapped(&mut events, t));
        }
        // FLAP_LIMIT maps inside one window trips it; the last slow map counts.
        for i in 2..FLAP_LIMIT {
            assert!(!flapped(&mut events, t + i as f32 * 0.1), "map {i}");
        }
        assert!(flapped(&mut events, t + (FLAP_LIMIT - 1) as f32 * 0.1));
        assert!(!flapped(&mut events, t + 10.0));
    }
}
