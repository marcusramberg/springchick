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
use std::collections::HashSet;

/// `(surface, physical origin)` pairs to composite, in bottom-to-top order.
pub type RenderList = Vec<(WlSurface, (i32, i32))>;

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
    /// Physical output height, for the home-bar bottom exclusive zone.
    output_h: f32,
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
            output_h,
        }
    }

    /// Recompute the usable area after an arrange. Returns `Some(new)` if it
    /// changed since the last call (so the caller resizes app toplevels).
    pub fn usable_changed(&mut self, dpi: f64) -> Option<Rect> {
        let now = self.usable(dpi);
        (now != self.last_usable).then(|| {
            self.last_usable = now;
            now
        })
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

        if is_mapped(surface) {
            self.unmapped.remove(surface);
        } else if !self.unmapped.contains(surface) {
            // Was mapped, now unmapped via a null commit: it must redo the
            // initial configure sequence before mapping again.
            self.unmapped.insert(surface.clone());
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
        let docked = r.y + r.h >= self.output_h - 1.0 && r.y > 1.0;
        if docked {
            r.y -= self.gesture_zone();
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
                        let r = self.shift_docked(to_physical(geo, dpi));
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
                .is_some_and(|g| rects_overlap(self.shift_docked(to_physical(g, dpi)), rect))
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
                    let rect = self.shift_docked(to_physical(geo, dpi));
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
