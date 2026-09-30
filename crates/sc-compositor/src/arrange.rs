//! Home icon reflow springs and arrange mode.

use std::collections::HashMap;

use sc_shell_model::ShellModel;

use tracing::{debug, warn};

use crate::input_dispatch;
use crate::state::State;
use crate::ui_state::UiState;

pub(crate) const HOLD_MS: u128 = 500;

/// A held icon waiting to become a long press (the icon menu). Cancelled by
/// moving past the hold slop or releasing early.
pub(crate) struct IconPress {
    pub app_id: String,
    pub source: input_dispatch::IconSource,
    pub start: (f32, f32),
    pub at: std::time::Instant,
}

/// A held background waiting to become the long press that engages arrange.
pub(crate) struct BgPress {
    pub start: (f32, f32),
    pub at: std::time::Instant,
}

pub(crate) struct DragItem {
    pub app_id: String,
    pub source: input_dispatch::IconSource,
    pub cur: (f32, f32),
    /// Hole the grid opens under the finger; `None` over the dock.
    pub hover: Option<(usize, usize)>,
    /// Edge-zone entry time, for dwell-to-flip.
    pub edge_since: Option<(std::time::Instant, EdgeSide)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum EdgeSide {
    Left,
    Right,
}

#[derive(Default)]
pub(crate) struct ArrangeState {
    pub drag: Option<DragItem>,
    /// The engaging long press hasn't lifted yet. Its release would otherwise
    /// be the still tap that exits arrange.
    pub just_engaged: bool,
}

/// Gap slot in the drag working order. NUL-prefixed so it can't be an app id;
/// laid out, never drawn.
pub(crate) const HOLE: &str = "\u{0}hole";

/// Global-space target per grid app.
fn reflow_targets(
    model: &ShellModel,
    width: f32,
    height: f32,
) -> std::collections::HashMap<String, (f32, f32)> {
    reflow_targets_for(&model.pages, width, height)
}

/// Flattened grid minus `dragged`, with a HOLE at `hover`, re-chunked.
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
    /// Persist and reflow after a manual grid/dock edit.
    pub(crate) fn after_arrange_edit(&mut self) {
        self.model.repack();
        if let Err(e) =
            sc_shell_model::persist::save(&self.model, &sc_shell_model::persist::state_path())
        {
            warn!(%e, "failed to save shell model after arrange edit");
        }
        // The edit may have dropped a page, leaving the shell parked past the end.
        let page_count = self.home_page_count();
        if let UiState::Home {
            page,
            page_count: pc,
            page_spring,
            ..
        } = &mut self.ui
        {
            *pc = page_count;
            if *page >= page_count {
                *page = page_count - 1;
                page_spring.retarget(*page as f32);
            }
        }
        self.reflow_grid();
        self.reflow_dock();
    }

    /// `hover` is unused so far: compacting already opens the gap.
    fn working_pages(&self, dragged: &str, hover: Option<(usize, usize)>) -> Vec<Vec<String>> {
        working_order(&self.model.pages, dragged, hover)
    }

    /// Call after any change to `model.pages`.
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
        Self::reflow_springs(&mut self.grid_anim, &targets);
    }

    /// The dragged dock icon is dropped; it rides as the ghost.
    pub(crate) fn reflow_dock(&mut self) {
        let (w, h) = self.output_size_f();
        let dragged = self
            .arrange
            .as_ref()
            .and_then(|a| a.drag.as_ref())
            .filter(|d| d.source == input_dispatch::IconSource::Dock)
            .map(|d| d.app_id.clone());
        // The dock uses fixed per-index cells, so the app must leave the layout,
        // not just `targets`, for the rest to re-center.
        let layout = if let Some(app) = &dragged {
            let mut m = self.model.clone();
            m.dock.retain(|a| a != app);
            sc_layout::compute(w, h, 0, &m)
        } else {
            sc_layout::compute(w, h, 0, &self.model)
        };
        let mut targets: HashMap<String, (f32, f32)> = HashMap::new();
        for slot in &layout.dock {
            targets.insert(
                slot.app_id.clone(),
                (slot.icon_rect.center_x(), slot.icon_rect.center_y()),
            );
        }
        Self::reflow_springs(&mut self.dock_anim, &targets);
    }

    /// A page drag sets the spring's `value` and `target` together, so a
    /// half-dragged spring reads as settled and never returns. Retarget it.
    pub(crate) fn cancel_page_drag(&mut self) {
        self.page_drag = None;
        if let UiState::Home {
            page, page_spring, ..
        } = &mut self.ui
        {
            page_spring.retarget(*page as f32);
        }
    }

    fn reflow_springs(
        anim_map: &mut HashMap<String, (sc_anim::Spring, sc_anim::Spring)>,
        targets: &HashMap<String, (f32, f32)>,
    ) {
        for (app, (tx, ty)) in targets {
            match anim_map.get_mut(app) {
                Some((sx, sy)) => {
                    sx.retarget(*tx);
                    sy.retarget(*ty);
                }
                None => {
                    anim_map.insert(
                        app.clone(),
                        (sc_anim::Spring::new(*tx), sc_anim::Spring::new(*ty)),
                    );
                }
            }
        }
        anim_map.retain(|app, _| targets.contains_key(app));
    }

    /// Engages arrange with nothing lifted. In arrange, pressing an icon lifts it
    /// without a second hold.
    pub(crate) fn maybe_engage_arrange_hold(&mut self) {
        if self.arrange.is_some() || !self.pointer_down {
            return;
        }
        let Some(p) = &self.bg_press else {
            return;
        };
        if p.at.elapsed().as_millis() < HOLD_MS {
            return;
        }
        // The VM test asserts this line: arrange doesn't change the `UiState`
        // discriminant, so no state-change line fires.
        debug!(target: "springchick::debug", "arrange engaged from background");
        self.arrange = Some(ArrangeState {
            drag: None,
            just_engaged: true,
        });
        self.bg_press = None;
        self.page_drag = None;
        self.search_arm = None;
    }

    /// Take a drag over from the search app with the finger still down. Search
    /// owns the touch, so send `wl_touch.cancel` and repoint the slot at our own
    /// gesture funnel. `at` overrides the position; a real handoff passes `None`.
    pub(crate) fn lift_from_search(&mut self, app_id: String, at: Option<(f32, f32)>) {
        let (w, h) = self.output_size_f();
        let at = at
            .or(self.last_touch_pos)
            .or(self.last_pointer_pos)
            .unwrap_or((w * 0.5, h * 0.5));
        debug!(target: "springchick::debug", "search drag lifted app_id={app_id} at={at:?}");
        // A pointer-driven drag has no slot and already drives the funnel.
        let slot = self.touch_targets.keys().copied().next();
        let touch = self.touch.clone();
        touch.cancel(self);
        self.touch_targets.clear();
        self.gesture_slot = slot;

        // Not `cancel_gestures`: it clears `arrange.drag`.
        self.handle_return_home();
        self.icon_press = None;
        self.bg_press = None;
        self.search_arm = None;
        self.pending_launch = None;
        self.cancel_page_drag();

        // Look like a press already in flight: `on_motion` needs one and
        // `on_release` reads the last position.
        self.pointer_down = true;
        self.last_pointer_pos = Some(at);
        self.arrange = Some(ArrangeState {
            drag: Some(DragItem {
                app_id,
                source: input_dispatch::IconSource::Library,
                cur: at,
                hover: None,
                edge_since: None,
            }),
            just_engaged: false,
        });
        self.needs_render = true;
    }

    /// Drops the launch the same press armed.
    pub(crate) fn maybe_open_icon_menu(&mut self) {
        if self.icon_menu.is_some() || self.arrange.is_some() || !self.pointer_down {
            return;
        }
        let Some(p) = &self.icon_press else {
            return;
        };
        if p.at.elapsed().as_millis() < HOLD_MS {
            return;
        }
        let (app_id, source) = (p.app_id.clone(), p.source);
        let windows: Vec<(crate::ui_state::ToplevelId, String)> = self
            .instances(&app_id)
            .into_iter()
            .map(|id| (id, self.toplevel_title(id)))
            .collect();
        let running = windows.len();
        // The drawn center, so it's right mid-reflow.
        let anchor = self.icon_center(&app_id, source);
        debug!(
            target: "springchick::debug",
            "icon menu opened app_id={app_id} source={source:?} running={running}"
        );
        let in_library = source == input_dispatch::IconSource::Library;
        let items = crate::icon_menu::items_for(&windows, self.is_flatpak(&app_id), in_library);
        self.icon_menu = Some(crate::icon_menu::IconMenu::new(app_id, anchor, items));
        self.icon_press = None;
        self.pending_launch = None;
        self.cancel_page_drag();
    }

    /// Page offset of the home grid; turns global-space springs into screen space.
    /// Zero outside Home.
    pub(crate) fn home_page_scroll(&self) -> f32 {
        match &self.ui {
            UiState::Home { page_spring, .. } => page_spring.value,
            _ => 0.0,
        }
    }

    /// Live reflow-spring position, else the layout slot.
    fn icon_center(&self, app_id: &str, source: input_dispatch::IconSource) -> (f32, f32) {
        let (w, h) = self.output_size_f();
        match source {
            input_dispatch::IconSource::Dock => {
                if let Some((sx, sy)) = self.dock_anim.get(app_id) {
                    return (sx.value, sy.value);
                }
            }
            input_dispatch::IconSource::Grid => {
                if let Some((sx, sy)) = self.grid_anim.get(app_id) {
                    let page_scroll = self.home_page_scroll();
                    return (sx.value - page_scroll * w, sy.value);
                }
            }
            // Folder members have no spring; the open panel's layout places them.
            input_dispatch::IconSource::Library => {
                if let Some(slot) = self
                    .folder_panel()
                    .and_then(|p| p.apps.into_iter().find(|s| s.app_id == app_id))
                {
                    return (slot.icon_rect.center_x(), slot.icon_rect.center_y());
                }
            }
        }
        let layout = sc_layout::compute(w, h, self.current_home_page(), &self.model);
        layout
            .grid
            .iter()
            .chain(layout.dock.iter())
            .find(|s| s.app_id == app_id)
            .map(|s| (s.icon_rect.center_x(), s.icon_rect.center_y()))
            .unwrap_or((w / 2.0, h / 2.0))
    }

    /// Holding a dragged icon at an edge past EDGE_DWELL_MS flips the page,
    /// auto-repeating.
    pub(crate) fn tick_edge_page_flip(&mut self) {
        const EDGE_FRAC: f32 = 0.12;
        const EDGE_DWELL_MS: u128 = 300;
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
                        es = Some((now, s));
                    }
                }
                _ => es = Some((now, s)),
            },
        }
        if let Some(d) = self.arrange.as_mut().and_then(|a| a.drag.as_mut()) {
            d.edge_since = es;
        }
        if let Some(dir) = flip {
            let cur_page = self.current_home_page();
            // Stop at the library page, which is always last. Add at most one new page
            // before it.
            let new_page = if dir < 0 {
                cur_page.saturating_sub(1)
            } else if cur_page + 1 < self.model.pages.len() {
                cur_page + 1
            } else if cur_page < self.library_page() {
                if !self.model.pages.last().is_some_and(|p| p.is_empty()) {
                    self.model.pages.push(Vec::new());
                }
                cur_page + 1
            } else {
                cur_page
            };
            let page_count = self.home_page_count();
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
        }
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
