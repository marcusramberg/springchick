//! wlr-layer-shell: protocol state, geometry, and app-area reservation.
//!
//! The pure geometry lives in `sc_layout::layer`; this module translates the
//! smithay protocol state into those plain inputs, tracks mapped surfaces, sends
//! configures, and reports the usable area back so the app windows can be sized
//! around reserved space (e.g. an on-screen keyboard).

use sc_layout::layer::{self, Anchor as LAnchor, Edge, Margins as LMargins, Reservation};
use sc_layout::Rect;
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::wayland::shell::wlr_layer::{
    Anchor, ExclusiveZone, Layer, LayerSurface, LayerSurfaceCachedState,
};

/// Whether a layer surface is currently mapped (has committed a buffer).
///
/// On unmap, smithay's `wlr_layer` pre-commit hook resets the surface's cached
/// state to `Default` (empty anchor, zero size). If we then reconfigure it —
/// e.g. because a commit triggered `recompute` — we'd send a `(0,0)` configure
/// with no anchors; the client commits that back and smithay kills it with
/// "width 0 requested without setting left and right anchors". So we only
/// arrange and configure mapped surfaces, matching smithay's `LayerMap`.
fn is_mapped(surface: &LayerSurface) -> bool {
    with_renderer_surface_state(surface.wl_surface(), |state| state.buffer().is_some())
        .unwrap_or(false)
}

/// `(surface, origin)` pairs to composite, in bottom-to-top order.
pub type RenderList = Vec<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    (i32, i32),
)>;

/// A layer surface we are tracking, with its last computed on-screen rect.
pub struct MappedLayer {
    pub surface: LayerSurface,
    pub layer: Layer,
    pub rect: Rect,
}

/// All tracked layer surfaces plus the last usable area handed to app windows.
pub struct LayerShell {
    pub surfaces: Vec<MappedLayer>,
    pub usable: Rect,
}

impl LayerShell {
    pub fn new(output_w: f32, output_h: f32) -> Self {
        LayerShell {
            surfaces: Vec::new(),
            usable: Rect {
                x: 0.0,
                y: 0.0,
                w: output_w,
                h: output_h,
            },
        }
    }

    /// Track a newly created layer surface (rect filled in on the next commit).
    pub fn add(&mut self, surface: LayerSurface, layer: Layer) {
        self.surfaces.push(MappedLayer {
            surface,
            layer,
            rect: Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
        });
    }

    /// Drop a destroyed layer surface. Returns true if it was tracked.
    pub fn remove(&mut self, surface: &LayerSurface) -> bool {
        let before = self.surfaces.len();
        self.surfaces.retain(|m| &m.surface != surface);
        self.surfaces.len() != before
    }

    /// Recompute every surface's geometry and the usable area from the clients'
    /// current cached state, sending a configure to any surface whose size
    /// changed. Returns the new usable area (compare against the old to decide
    /// whether app windows need reconfiguring).
    ///
    /// `output_w`/`output_h` and the stored rects are in **physical** px. Layer
    /// clients are told a fractional scale of `dpi` (via `wp_fractional_scale`),
    /// so the sizes they request (`cs.size`, margins, exclusive zone) and the
    /// size we configure are **logical**: we scale requests up by `dpi` for the
    /// internal physical geometry and divide the configured size back down. The
    /// returned `usable` area stays physical (app sizing divides it by `dpi`).
    pub fn recompute(&mut self, output_w: f32, output_h: f32, dpi: i32) -> Rect {
        let scale = dpi as f32;

        // 1. Reservations from surfaces with a valid single-edge exclusive zone.
        //    The client's exclusive zone is logical → scale to physical.
        let mut reservations = Vec::new();
        for m in &self.surfaces {
            if !is_mapped(&m.surface) {
                continue;
            }
            let cs = cached_state(&m.surface);
            if let Some(mut res) = reservation(&cs) {
                res.size *= scale;
                reservations.push(res);
            }
        }
        let usable = layer::usable_area(output_w, output_h, &reservations);

        // 2. Each surface's rect (against the full output) + a configure.
        for m in &mut self.surfaces {
            // Skip unmapped surfaces: their cached state was reset to Default on
            // unmap, so configuring them sends a bogus `(0,0)`/no-anchor state
            // that the client commits back and gets killed for. See `is_mapped`.
            if !is_mapped(&m.surface) {
                continue;
            }
            let cs = cached_state(&m.surface);
            let anchor = to_anchor(cs.anchor);
            let mut margins = to_margins(cs.margin);
            margins.top *= scale;
            margins.bottom *= scale;
            margins.left *= scale;
            margins.right *= scale;
            let rect = layer::layer_rect(
                output_w,
                output_h,
                anchor,
                cs.size.w as f32 * scale,
                cs.size.h as f32 * scale,
                margins,
            );
            m.rect = rect;
            m.layer = cs.layer;

            // Configure carries logical size (the client renders at fractional
            // scale `dpi`), so divide the physical rect back down.
            let w = (rect.w / scale).round() as i32;
            let h = (rect.h / scale).round() as i32;
            let changed = m.surface.with_pending_state(|state| {
                let new = Some((w, h).into());
                let changed = state.size != new;
                state.size = new;
                changed
            });
            if changed {
                m.surface.send_configure();
            }
        }

        self.usable = usable;
        usable
    }

    /// Mapped surfaces on the given layer, in tracking order (creation order),
    /// which is the intra-layer stacking order.
    pub fn on_layer(&self, layer: Layer) -> impl Iterator<Item = &MappedLayer> {
        self.surfaces.iter().filter(move |m| m.layer == layer)
    }

    /// `(surface, origin)` pairs for the render pass, split into those drawn
    /// below the app (background, bottom) and above it (top, overlay), each in
    /// bottom-to-top order.
    pub fn render_lists(&self) -> (RenderList, RenderList) {
        let collect = |layers: &[Layer]| {
            let mut v = Vec::new();
            for &layer in layers {
                for m in self.on_layer(layer) {
                    v.push((
                        m.surface.wl_surface().clone(),
                        (m.rect.x.round() as i32, m.rect.y.round() as i32),
                    ));
                }
            }
            v
        };
        (
            collect(&[Layer::Background, Layer::Bottom]),
            collect(&[Layer::Top, Layer::Overlay]),
        )
    }

    /// Whether any Top/Overlay surface overlaps `rect` — used to hide the
    /// home-bar when the on-screen keyboard covers it, independent of whether
    /// the keyboard reserved an exclusive zone.
    pub fn top_overlaps(&self, rect: Rect) -> bool {
        self.surfaces
            .iter()
            .any(|m| matches!(m.layer, Layer::Top | Layer::Overlay) && rects_overlap(m.rect, rect))
    }

    /// The topmost hit-testable (Top/Overlay) surface containing the point, if
    /// any. Overlay is above Top; within a layer, later-created is on top.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<&MappedLayer> {
        for layer in [Layer::Overlay, Layer::Top] {
            // `.last()` gives the topmost (latest-created) match within the layer.
            if let Some(m) = self
                .on_layer(layer)
                .filter(|m| m.rect.contains(x, y))
                .last()
            {
                return Some(m);
            }
        }
        None
    }
}

/// Axis-aligned rectangle overlap (touching edges do not count).
fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// Read a layer surface's current (committed) cached state.
fn cached_state(surface: &LayerSurface) -> LayerSurfaceCachedState {
    smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
        *states
            .cached_state
            .get::<LayerSurfaceCachedState>()
            .current()
    })
}

/// Translate a surface's anchor + exclusive zone into a screen-edge
/// reservation, or `None` when it should not reserve (neutral / don't-care /
/// multi-edge). A zone is only meaningful anchored to a single edge, optionally
/// plus its two perpendicular edges.
fn reservation(cs: &LayerSurfaceCachedState) -> Option<Reservation> {
    let ExclusiveZone::Exclusive(size) = cs.exclusive_zone else {
        return None;
    };
    let a = cs.anchor;
    let edge = single_edge(a)?;
    Some(Reservation {
        edge,
        size: size as f32,
    })
}

/// The one edge a surface reserves against, if its anchors permit reservation.
fn single_edge(a: Anchor) -> Option<Edge> {
    let (t, b, l, r) = (
        a.contains(Anchor::TOP),
        a.contains(Anchor::BOTTOM),
        a.contains(Anchor::LEFT),
        a.contains(Anchor::RIGHT),
    );
    match (t, b, l, r) {
        // Vertical edge, spanning or not the horizontal axis.
        (true, false, _, _) if l == r => Some(Edge::Top),
        (false, true, _, _) if l == r => Some(Edge::Bottom),
        // Horizontal edge, spanning or not the vertical axis.
        (_, _, true, false) if t == b => Some(Edge::Left),
        (_, _, false, true) if t == b => Some(Edge::Right),
        _ => None,
    }
}

fn to_anchor(a: Anchor) -> LAnchor {
    LAnchor {
        top: a.contains(Anchor::TOP),
        bottom: a.contains(Anchor::BOTTOM),
        left: a.contains(Anchor::LEFT),
        right: a.contains(Anchor::RIGHT),
    }
}

fn to_margins(m: smithay::wayland::shell::wlr_layer::Margins) -> LMargins {
    LMargins {
        top: m.top as f32,
        bottom: m.bottom as f32,
        left: m.left as f32,
        right: m.right as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::wayland::shell::wlr_layer::Anchor;

    #[test]
    fn bottom_full_width_reserves_the_bottom_edge() {
        // wvkbd anchors bottom+left+right — a valid single-edge reservation.
        let a = Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT;
        assert_eq!(single_edge(a), Some(Edge::Bottom));
    }

    #[test]
    fn top_bar_reserves_the_top_edge() {
        let a = Anchor::TOP | Anchor::LEFT | Anchor::RIGHT;
        assert_eq!(single_edge(a), Some(Edge::Top));
    }

    #[test]
    fn left_bar_reserves_the_left_edge() {
        let a = Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM;
        assert_eq!(single_edge(a), Some(Edge::Left));
    }

    #[test]
    fn opposite_edges_do_not_reserve() {
        // Anchored top+bottom (a full-height strip) has no single edge.
        let a = Anchor::TOP | Anchor::BOTTOM;
        assert_eq!(single_edge(a), None);
    }

    #[test]
    fn all_edges_do_not_reserve() {
        let a = Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT;
        assert_eq!(single_edge(a), None);
    }

    #[test]
    fn corner_anchor_does_not_reserve() {
        // Only two perpendicular edges (a corner) is ambiguous — no reservation.
        let a = Anchor::BOTTOM | Anchor::RIGHT;
        assert_eq!(single_edge(a), None);
    }
}
