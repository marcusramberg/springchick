//! Home-screen icon layout: the reflow springs that slide icons to their slots,
//! and arrange mode (long-press to wiggle, drag to reorder / pin / unpin).

use sc_shell_model::ShellModel;

use tracing::{debug, warn};

use crate::input_dispatch;
use crate::state::State;
use crate::ui_state::UiState;

/// Hold duration (milliseconds) an icon press must survive before arrange
/// mode engages.
pub(crate) const HOLD_MS: u128 = 500;

/// A finger held on an icon on Home, waiting to see if it becomes a
/// long-press (arrange mode). Cancelled if the finger moves past the tap
/// slop (becomes a swipe) or releases before `HOLD_MS`.
pub(crate) struct IconPress {
    pub app_id: String,
    pub source: input_dispatch::IconSource,
    pub start: (f32, f32),
    pub at: std::time::Instant,
}

/// An icon currently being dragged in arrange mode.
pub(crate) struct DragItem {
    pub app_id: String,
    pub source: input_dispatch::IconSource,
    pub cur: (f32, f32),
    /// (page, index) hole the grid opens under the finger; None until first
    /// motion or when the finger is over the dock zone.
    pub hover: Option<(usize, usize)>,
    /// When the finger entered the current edge zone, for dwell-to-flip.
    /// None when not in an edge zone.
    pub edge_since: Option<(std::time::Instant, EdgeSide)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum EdgeSide {
    Left,
    Right,
}

/// Arrange-mode state: icons wiggle, badges/Done button are live, and an
/// icon may be mid-drag toward the dock (pin) or grid (unpin).
#[derive(Default)]
pub(crate) struct ArrangeState {
    pub drag: Option<DragItem>,
}

/// Sentinel occupying the gap slot in the drag working order. Never a real app
/// id (NUL-prefixed), so it can't collide; it is laid out for spacing but never
/// drawn (it is not in `model.pages`, and `reflow_grid` drops it from targets).
pub(crate) const HOLE: &str = "\u{0}hole";

/// Global-space target (x, y) for every app currently on the grid (dock and
/// hidden apps excluded — they aren't in `model.pages`), keyed by app id.
/// Pure so it's cheap to unit-test independent of `State`.
fn reflow_targets(
    model: &ShellModel,
    width: f32,
    height: f32,
) -> std::collections::HashMap<String, (f32, f32)> {
    reflow_targets_for(&model.pages, width, height)
}

/// The drag "working order": the flattened grid with `dragged` removed and, when
/// `hover` is Some, a HOLE sentinel inserted at the hovered global index so the
/// real icons part to show the drop target. Re-chunked into pages.
fn working_order(
    pages: &[Vec<String>],
    dragged: &str,
    hover: Option<(usize, usize)>,
) -> Vec<Vec<String>> {
    let mut flat: Vec<String> = pages
        .iter()
        .flatten()
        .filter(|a| *a != dragged)
        .cloned()
        .collect();
    if let Some((page, index)) = hover {
        let gi = (page
            .saturating_mul(sc_shell_model::PAGE_CAP)
            .saturating_add(index))
        .min(flat.len());
        flat.insert(gi, HOLE.to_string());
    }
    flat.chunks(sc_shell_model::PAGE_CAP)
        .map(|c| c.to_vec())
        .collect()
}

/// Reflow targets over an explicit page list (used for the live drag "working
/// order": dragged app removed so remaining icons compact and open a gap).
fn reflow_targets_for(
    pages: &[Vec<String>],
    width: f32,
    height: f32,
) -> std::collections::HashMap<String, (f32, f32)> {
    let mut out = std::collections::HashMap::new();
    for (page, apps) in pages.iter().enumerate() {
        for (index, app) in apps.iter().enumerate() {
            out.insert(
                app.clone(),
                sc_layout::global_slot_pos(page, index, width, height),
            );
        }
    }
    out
}

impl State {
    /// Persist + reflow after a manual grid/dock edit (pin/unpin/hide/reorder).
    /// No frecency recompute — grid order is now manual.
    pub(crate) fn after_arrange_edit(&mut self) {
        self.model.repack();
        if let Err(e) =
            sc_shell_model::persist::save(&self.model, &sc_shell_model::persist::state_path())
        {
            warn!(%e, "failed to save shell model after arrange edit");
        }
        self.reflow_grid();
        self.reflow_dock();
    }

    /// The current pages with `dragged` removed (its slot becomes a gap the
    /// remaining icons compact into). The dragged app renders as a ghost, so it
    /// is intentionally absent from the reflow targets. `hover` is accepted for
    /// future explicit-hole placement but the compacted layout already yields a
    /// gap at/after the removed slot.
    fn working_pages(&self, dragged: &str, hover: Option<(usize, usize)>) -> Vec<Vec<String>> {
        working_order(&self.model.pages, dragged, hover)
    }

    /// Re-target the grid-reflow springs to each app's current slot position,
    /// seeding new entries and dropping ones no longer on the grid (docked,
    /// hidden). Called after any change to `model.pages`.
    pub(crate) fn reflow_grid(&mut self) {
        let (w, h) = self.output_size_f();
        let drag_app = self
            .arrange
            .as_ref()
            .and_then(|a| a.drag.as_ref())
            .map(|d| (d.app_id.clone(), d.hover));
        let targets = match drag_app {
            Some((app_id, hover)) => {
                let working = self.working_pages(&app_id, hover);
                let mut t = reflow_targets_for(&working, w, h);
                t.remove(HOLE);
                t
            }
            None => reflow_targets(&self.model, w, h),
        };
        for (app, (tx, ty)) in &targets {
            match self.grid_anim.get_mut(app) {
                Some((sx, sy)) => {
                    sx.retarget(*tx);
                    sy.retarget(*ty);
                }
                None => {
                    self.grid_anim.insert(
                        app.clone(),
                        (sc_anim::Spring::new(*tx), sc_anim::Spring::new(*ty)),
                    );
                }
            }
        }
        self.grid_anim.retain(|app, _| targets.contains_key(app));
    }

    /// Retarget dock springs to the current dock layout, dropping a dock icon
    /// that is being dragged (it rides as the ghost). Mirror of `reflow_grid`.
    pub(crate) fn reflow_dock(&mut self) {
        let (w, h) = self.output_size_f();
        let dragged = self
            .arrange
            .as_ref()
            .and_then(|a| a.drag.as_ref())
            .filter(|d| d.source == input_dispatch::IconSource::Dock)
            .map(|d| d.app_id.clone());
        // Lay out with the dragged dock app removed so the surviving icons
        // re-center over the N-1 cells (the dock is anchored to fixed per-index
        // cells, so omitting the app from `targets` alone would not move them).
        let layout = if let Some(app) = &dragged {
            let mut m = self.model.clone();
            m.dock.retain(|a| a != app);
            sc_layout::compute(w, h, 0, &m)
        } else {
            sc_layout::compute(w, h, 0, &self.model)
        };
        let mut targets: std::collections::HashMap<String, (f32, f32)> =
            std::collections::HashMap::new();
        for slot in &layout.dock {
            targets.insert(
                slot.app_id.clone(),
                (slot.icon_rect.center_x(), slot.icon_rect.center_y()),
            );
        }
        for (app, (tx, ty)) in &targets {
            match self.dock_anim.get_mut(app) {
                Some((sx, sy)) => {
                    sx.retarget(*tx);
                    sy.retarget(*ty);
                }
                None => {
                    self.dock_anim.insert(
                        app.clone(),
                        (sc_anim::Spring::new(*tx), sc_anim::Spring::new(*ty)),
                    );
                }
            }
        }
        self.dock_anim.retain(|app, _| targets.contains_key(app));
    }

    /// Long-press hold: an icon held past HOLD_MS without moving into a swipe or
    /// launch engages arrange mode, picking up the same icon as the initial drag
    /// item so the finger doesn't need to move first.
    pub(crate) fn maybe_engage_arrange_hold(&mut self) {
        if self.arrange.is_some() || !self.pointer_down {
            return;
        }
        if let Some(p) = &self.icon_press {
            if p.at.elapsed().as_millis() >= HOLD_MS {
                let drag = DragItem {
                    app_id: p.app_id.clone(),
                    source: p.source,
                    cur: p.start,
                    hover: None,
                    edge_since: None,
                };
                // Logged so the VM test can assert arrange mode from the journal:
                // engaging it changes no `UiState` discriminant (Home stays
                // Home), so the `state changed to ...` line never fires for it.
                debug!(
                    target: "springchick::debug",
                    "arrange engaged app_id={} source={:?}", drag.app_id, drag.source
                );
                self.arrange = Some(ArrangeState { drag: Some(drag) });
                self.pending_launch = None;
                self.page_drag_start = None;
                self.icon_press = None;
            }
        }
    }

    /// Edge-dwell page flip: while dragging a reorder icon, holding it against a
    /// screen edge past EDGE_DWELL_MS flips the home page (auto-repeating), adding
    /// a trailing page if needed.
    pub(crate) fn tick_edge_page_flip(&mut self) {
        const EDGE_FRAC: f32 = 0.06;
        const EDGE_DWELL_MS: u128 = 400;
        let Some((cur_x, mut es)) = self
            .arrange
            .as_ref()
            .and_then(|a| a.drag.as_ref())
            .map(|d| (d.cur.0, d.edge_since))
        else {
            return;
        };
        let (w, _h) = self.output_size_f();
        let side = if cur_x < w * EDGE_FRAC {
            Some(EdgeSide::Left)
        } else if cur_x > w * (1.0 - EDGE_FRAC) {
            Some(EdgeSide::Right)
        } else {
            None
        };
        let now = std::time::Instant::now();
        let mut flip: Option<i32> = None;
        match side {
            None => es = None,
            Some(s) => match es {
                Some((since, prev)) if prev == s => {
                    if now.duration_since(since).as_millis() >= EDGE_DWELL_MS {
                        flip = Some(if s == EdgeSide::Left { -1 } else { 1 });
                        es = Some((now, s)); // reset -> auto-repeat
                    }
                }
                _ => es = Some((now, s)),
            },
        }
        // Write edge_since back.
        if let Some(d) = self.arrange.as_mut().and_then(|a| a.drag.as_mut()) {
            d.edge_since = es;
        }
        // Apply a flip.
        if let Some(dir) = flip {
            let cur_page = self.current_home_page();
            let new_page = if dir < 0 {
                cur_page.saturating_sub(1)
            } else if cur_page + 1 < self.model.pages.len() {
                cur_page + 1
            } else if self.model.pages.last().is_some_and(|p| p.is_empty()) {
                // Already a trailing empty page — go to it, don't add more.
                self.model.pages.len() - 1
            } else {
                self.model.pages.push(Vec::new());
                self.model.pages.len() - 1
            };
            let page_count = self.model.pages.len().max(1);
            if let UiState::Home {
                page,
                page_spring,
                page_count: pc,
                ..
            } = &mut self.ui
            {
                *page = new_page;
                *pc = page_count;
                page_spring.retarget(new_page as f32);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflow_targets_maps_pages_and_excludes_dock() {
        let mut m = ShellModel::default();
        for i in 0..25 {
            m.place(format!("app{i:02}"));
        } // 24 on page 0, 1 on page 1
        let t = reflow_targets(&m, 1224.0, 2700.0);
        assert_eq!(t.len(), 25);
        let page1_app = &m.pages[1][0];
        assert!(t[page1_app].0 > 1224.0);
        let page0_app = &m.pages[0][0];
        assert!(t[page0_app].0 < 1224.0);
    }

    #[test]
    fn working_order_opens_hole_at_hover() {
        let pages = vec![vec!["a".to_string(), "b".into(), "c".into(), "d".into()]];
        // Drag "a", hover global index 2 -> order without "a" is [b,c,d];
        // hole at 2 -> [b, c, HOLE, d].
        let out = working_order(&pages, "a", Some((0, 2)));
        assert_eq!(
            out[0],
            vec!["b".to_string(), "c".into(), HOLE.to_string(), "d".into()]
        );
    }

    #[test]
    fn working_order_no_hole_when_hover_none() {
        let pages = vec![vec!["a".to_string(), "b".into(), "c".into()]];
        let out = working_order(&pages, "a", None);
        assert_eq!(out[0], vec!["b".to_string(), "c".into()]);
    }
}
