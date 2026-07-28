#![forbid(unsafe_code)]

//! Pure geometry and hit-testing for the springchick home screen.
//!
//! Given output dimensions, a page index, and a `ShellModel`, produces screen-space
//! rectangles for every icon, dock slot, page-indicator dots, and the bottom bar zone.
//! Also provides the inverse: `point → Hit`.

pub mod layer;

use sc_shell_model::{ShellModel, COLS, DOCK_CAP, PAGE_CAP, ROWS};

/// A rectangle in logical pixels (origin top-left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    pub fn center_x(&self) -> f32 {
        self.x + self.w / 2.0
    }

    pub fn center_y(&self) -> f32 {
        self.y + self.h / 2.0
    }
}

/// A positioned icon (grid or dock).
#[derive(Clone, Debug, PartialEq)]
pub struct IconSlot {
    /// The app id occupying this slot.
    pub app_id: String,
    /// Bounding rect for the icon image.
    pub icon_rect: Rect,
    /// Bounding rect for the label below the icon.
    pub label_rect: Rect,
    /// Remove-badge rect (arrange mode), centered on the icon's top-left corner.
    pub badge_rect: Rect,
}

/// Full layout for one frame of the home screen.
#[derive(Clone, Debug)]
pub struct Layout {
    /// Grid icons on the current page.
    pub grid: Vec<IconSlot>,
    /// Dock icons (always visible).
    pub dock: Vec<IconSlot>,
    /// Page indicator dots area.
    pub dots_rect: Rect,
    /// Bottom bar zone (return-home tap target).
    pub bar_rect: Rect,
    /// Total page count.
    pub page_count: usize,
    /// Dock band zone (full-width strip behind the dock icons).
    pub dock_zone: Rect,
    /// Arrange-mode "Done" button tap target.
    pub done_button: Rect,
}

/// Result of hit-testing a point against the layout.
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    /// Tapped a grid icon.
    GridIcon { app_id: String, index: usize },
    /// Tapped a dock icon.
    DockIcon { app_id: String, index: usize },
    /// Tapped the bottom bar zone.
    Bar,
    /// Tapped a remove-badge on an icon (arrange mode).
    RemoveBadge { app_id: String },
    /// Tapped the arrange-mode "Done" button.
    DoneButton,
    /// Missed everything.
    Miss,
}

// --- Layout constants as fractions of output dimensions ---

/// Top padding fraction (status bar area).
const TOP_PAD: f32 = 0.04;
/// Bottom bar height fraction.
const BAR_HEIGHT: f32 = 0.03;
/// Home-pill height (logical px), centered in the bottom bar band.
pub const PILL_HEIGHT: f32 = 8.0;
/// Dock height fraction (including internal padding).
const DOCK_HEIGHT: f32 = 0.10;
/// Dots area height fraction.
const DOTS_HEIGHT: f32 = 0.02;
/// Horizontal margin fraction (each side).
const H_MARGIN: f32 = 0.04;
/// Icon size as fraction of cell width.
const ICON_SIZE_FRAC: f32 = 0.62;
/// Label height as fraction of cell height.
const LABEL_HEIGHT_FRAC: f32 = 0.18;

/// Compute the full home screen layout for the given output size, page, and model.
/// The bottom home-bar zone rectangle, standalone (no full layout needed).
pub fn bar_rect(width: f32, height: f32) -> Rect {
    Rect {
        x: 0.0,
        y: height * (1.0 - BAR_HEIGHT),
        w: width,
        h: height * BAR_HEIGHT,
    }
}

/// Bottom exclusive zone the home gesture bar reserves from apps: twice the
/// pill's offset from the screen bottom, i.e. the empty gap below + above the
/// centered pill (`bar_height - pill_height`). Same unit as `height`.
pub fn gesture_exclusive_zone(height: f32) -> f32 {
    (height * BAR_HEIGHT - PILL_HEIGHT).max(0.0)
}

/// The home-pill rectangle centered within the bottom bar band. Single source
/// for the drawn pill and the bar-fade overlap test.
pub fn pill_in_bar(bar: Rect) -> Rect {
    let pill_w = bar.w * 0.35;
    Rect {
        x: bar.x + (bar.w - pill_w) / 2.0,
        y: bar.y + (bar.h - PILL_HEIGHT) / 2.0,
        w: pill_w,
        h: PILL_HEIGHT,
    }
}

/// The home-pill rectangle for an output of the given size.
pub fn pill_rect(width: f32, height: f32) -> Rect {
    pill_in_bar(bar_rect(width, height))
}

/// Shared grid-cell geometry, derived once from output dimensions.
struct GridMetrics {
    grid_left: f32,
    grid_top: f32,
    cell_w: f32,
    cell_h: f32,
    icon_size: f32,
    label_h: f32,
}

/// Compute the shared grid-cell metrics for the given output size. Single
/// source for the numbers used both by `compute` and the standalone
/// positioning helpers (`global_slot_pos`, `slot_at_center`).
fn grid_metrics(width: f32, height: f32) -> GridMetrics {
    let dock_top = height * (1.0 - BAR_HEIGHT) - height * DOCK_HEIGHT;
    let dots_top = dock_top - height * DOTS_HEIGHT;

    let grid_top = height * TOP_PAD;
    let grid_bottom = dots_top;
    let grid_height = grid_bottom - grid_top;

    let usable_width = width * (1.0 - 2.0 * H_MARGIN);
    let grid_left = width * H_MARGIN;

    let cell_w = usable_width / COLS as f32;
    let cell_h = grid_height / ROWS as f32;
    let icon_size = cell_w * ICON_SIZE_FRAC;
    let label_h = cell_h * LABEL_HEIGHT_FRAC;

    GridMetrics {
        grid_left,
        grid_top,
        cell_w,
        cell_h,
        icon_size,
        label_h,
    }
}

/// Remove-badge rect for a given icon rect, centered on its top-left corner.
/// Single source shared by `compute` and `slot_at_center`.
fn badge_of(ir: Rect) -> Rect {
    let s = ir.w * 0.34;
    Rect {
        x: ir.x - s / 2.0,
        y: ir.y - s / 2.0,
        w: s,
        h: s,
    }
}

/// The global-space center `(x, y)` of the grid slot at `index` on `page`,
/// for an output of the given size. `page` offsets the position by a full
/// `width` per page (so it is NOT screen-space — subtract `page_scroll * width`
/// to get the on-screen position), matching how pages are laid out edge-to-edge
/// for the reflow/paging animation.
pub fn global_slot_pos(page: usize, index: usize, width: f32, height: f32) -> (f32, f32) {
    let gm = grid_metrics(width, height);
    let col = index % COLS;
    let row = index / COLS;
    let cell_x = gm.grid_left + col as f32 * gm.cell_w;
    let cell_y = gm.grid_top + row as f32 * gm.cell_h;
    let icon_x = cell_x + (gm.cell_w - gm.icon_size) / 2.0;
    let icon_y = cell_y + (gm.cell_h - gm.icon_size - gm.label_h) / 2.0;
    (
        icon_x + gm.icon_size / 2.0 + page as f32 * width,
        icon_y + gm.icon_size / 2.0,
    )
}

/// Slot index (0..PAGE_CAP) whose cell is nearest the on-screen point (x, y)
/// for the currently visible page. Clamps to the grid; callers further clamp
/// to the page's fill length. `x` is screen-space (0..width), not page-global.
pub fn nearest_grid_index(width: f32, height: f32, x: f32, y: f32) -> usize {
    let gm = grid_metrics(width, height);
    let col = (((x - gm.grid_left) / gm.cell_w).floor() as isize)
        .clamp(0, COLS as isize - 1) as usize;
    let row = (((y - gm.grid_top) / gm.cell_h).floor() as isize)
        .clamp(0, ROWS as isize - 1) as usize;
    row * COLS + col
}

/// Build a standalone `IconSlot` for `app_id` whose icon is centered at
/// `(cx, cy)`, using the same icon/label sizing as the grid for an output of
/// the given size. Used by the reflow animation to place icons in-flight
/// between grid slots.
pub fn slot_at_center(app_id: String, cx: f32, cy: f32, width: f32, height: f32) -> IconSlot {
    let gm = grid_metrics(width, height);
    let icon_rect = Rect {
        x: cx - gm.icon_size / 2.0,
        y: cy - gm.icon_size / 2.0,
        w: gm.icon_size,
        h: gm.icon_size,
    };
    let label_rect = Rect {
        x: cx - gm.cell_w / 2.0,
        y: icon_rect.y + gm.icon_size,
        w: gm.cell_w,
        h: gm.label_h,
    };
    IconSlot {
        app_id,
        icon_rect,
        label_rect,
        badge_rect: badge_of(icon_rect),
    }
}

pub fn compute(width: f32, height: f32, page: usize, model: &ShellModel) -> Layout {
    let page_count = model.pages.len().max(1);
    let clamped_page = page.min(page_count.saturating_sub(1));

    let bar_rect = Rect {
        x: 0.0,
        y: height * (1.0 - BAR_HEIGHT),
        w: width,
        h: height * BAR_HEIGHT,
    };

    let dock_top = bar_rect.y - height * DOCK_HEIGHT;
    let dots_top = dock_top - height * DOTS_HEIGHT;
    let dots_rect = Rect {
        x: 0.0,
        y: dots_top,
        w: width,
        h: height * DOTS_HEIGHT,
    };

    let gm = grid_metrics(width, height);
    let grid_top = gm.grid_top;
    let usable_width = width * (1.0 - 2.0 * H_MARGIN);
    let grid_left = gm.grid_left;
    let cell_w = gm.cell_w;
    let cell_h = gm.cell_h;
    let icon_size = gm.icon_size;
    let label_h = gm.label_h;

    // Grid icons
    let grid = if let Some(apps) = model.pages.get(clamped_page) {
        apps.iter()
            .enumerate()
            .map(|(i, app_id)| {
                let col = i % COLS;
                let row = i / COLS;
                let cell_x = grid_left + col as f32 * cell_w;
                let cell_y = grid_top + row as f32 * cell_h;
                let icon_x = cell_x + (cell_w - icon_size) / 2.0;
                let icon_y = cell_y + (cell_h - icon_size - label_h) / 2.0;
                let icon_rect = Rect {
                    x: icon_x,
                    y: icon_y,
                    w: icon_size,
                    h: icon_size,
                };
                IconSlot {
                    app_id: app_id.clone(),
                    icon_rect,
                    label_rect: Rect {
                        x: cell_x,
                        y: icon_y + icon_size,
                        w: cell_w,
                        h: label_h,
                    },
                    badge_rect: badge_of(icon_rect),
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    // Dock icons
    let dock_cell_w = usable_width / DOCK_CAP as f32;
    let dock_icon_size = dock_cell_w * ICON_SIZE_FRAC;
    let dock_label_h = (height * DOCK_HEIGHT) * LABEL_HEIGHT_FRAC;
    let dock = model
        .dock
        .iter()
        .enumerate()
        .map(|(i, app_id)| {
            let cell_x = grid_left + i as f32 * dock_cell_w;
            let icon_x = cell_x + (dock_cell_w - dock_icon_size) / 2.0;
            let icon_y = dock_top + (height * DOCK_HEIGHT - dock_icon_size - dock_label_h) / 2.0;
            let icon_rect = Rect {
                x: icon_x,
                y: icon_y,
                w: dock_icon_size,
                h: dock_icon_size,
            };
            IconSlot {
                app_id: app_id.clone(),
                icon_rect,
                label_rect: Rect {
                    x: cell_x,
                    y: icon_y + dock_icon_size,
                    w: dock_cell_w,
                    h: dock_label_h,
                },
                badge_rect: badge_of(icon_rect),
            }
        })
        .collect();

    let dock_zone = Rect {
        x: 0.0,
        y: dock_top,
        w: width,
        h: height * DOCK_HEIGHT,
    };
    // Sits entirely within the top-padding band (above `grid_top`) so it never
    // overlaps grid icon hit-targets in normal (non-arrange) hit-testing.
    let done_side = width * 0.12;
    let done_button = Rect {
        x: width * (1.0 - H_MARGIN) - done_side,
        y: 0.0,
        w: done_side,
        h: height * TOP_PAD,
    };

    Layout {
        grid,
        dock,
        dots_rect,
        bar_rect,
        page_count,
        dock_zone,
        done_button,
    }
}

/// Hit-test a point (logical pixels, origin top-left) against a layout.
pub fn hit_test(layout: &Layout, x: f32, y: f32) -> Hit {
    // Bar has highest priority (always-on-top affordance).
    if layout.bar_rect.contains(x, y) {
        return Hit::Bar;
    }

    // Check dock icons.
    for (i, slot) in layout.dock.iter().enumerate() {
        if slot.icon_rect.contains(x, y) || slot.label_rect.contains(x, y) {
            return Hit::DockIcon {
                app_id: slot.app_id.clone(),
                index: i,
            };
        }
    }

    // Check grid icons.
    for (i, slot) in layout.grid.iter().enumerate() {
        if slot.icon_rect.contains(x, y) || slot.label_rect.contains(x, y) {
            return Hit::GridIcon {
                app_id: slot.app_id.clone(),
                index: i,
            };
        }
    }

    Hit::Miss
}

/// Hit-test in arrange mode. Checks Done + remove-badges (which overlap icons)
/// BEFORE falling through to normal icon/bar/miss testing.
pub fn hit_test_arrange(layout: &Layout, x: f32, y: f32) -> Hit {
    if layout.done_button.contains(x, y) {
        return Hit::DoneButton;
    }
    for s in layout.grid.iter().chain(layout.dock.iter()) {
        if s.badge_rect.contains(x, y) {
            return Hit::RemoveBadge {
                app_id: s.app_id.clone(),
            };
        }
    }
    hit_test(layout, x, y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_shell_model::ShellModel;

    fn sample_model() -> ShellModel {
        let mut m = ShellModel::default();
        for i in 0..6 {
            m.place(format!("app{i}"));
        }
        m.dock.push("dock0".into());
        m.dock.push("dock1".into());
        m
    }

    #[test]
    fn layout_produces_correct_grid_count() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        assert_eq!(l.grid.len(), 6);
        assert_eq!(l.dock.len(), 2);
        assert_eq!(l.page_count, 1);
    }

    #[test]
    fn grid_icons_within_bounds() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        for slot in &l.grid {
            assert!(slot.icon_rect.x >= 0.0);
            assert!(slot.icon_rect.y >= 0.0);
            assert!(slot.icon_rect.x + slot.icon_rect.w <= 1224.0);
            assert!(slot.icon_rect.y + slot.icon_rect.h <= 2700.0);
        }
    }

    #[test]
    fn hit_test_bar() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        // Bottom center should hit bar
        let hit = hit_test(&l, 612.0, 2690.0);
        assert_eq!(hit, Hit::Bar);
    }

    #[test]
    fn hit_test_grid_icon() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        // Center of first icon
        let slot = &l.grid[0];
        let cx = slot.icon_rect.center_x();
        let cy = slot.icon_rect.center_y();
        let hit = hit_test(&l, cx, cy);
        assert_eq!(
            hit,
            Hit::GridIcon {
                app_id: "app0".into(),
                index: 0
            }
        );
    }

    #[test]
    fn hit_test_dock_icon() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        let slot = &l.dock[0];
        let cx = slot.icon_rect.center_x();
        let cy = slot.icon_rect.center_y();
        let hit = hit_test(&l, cx, cy);
        assert_eq!(
            hit,
            Hit::DockIcon {
                app_id: "dock0".into(),
                index: 0
            }
        );
    }

    #[test]
    fn hit_test_miss() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        // Top-left corner should be a miss (top padding area)
        let hit = hit_test(&l, 5.0, 5.0);
        assert_eq!(hit, Hit::Miss);
    }

    #[test]
    fn page_count_from_model() {
        let mut m = ShellModel::default();
        for i in 0..30 {
            m.place(format!("app{i}"));
        }
        let l = compute(1224.0, 2700.0, 0, &m);
        assert_eq!(l.page_count, 2);
    }

    #[test]
    fn second_page_shows_correct_icons() {
        let mut m = ShellModel::default();
        for i in 0..30 {
            m.place(format!("app{i}"));
        }
        let l = compute(1224.0, 2700.0, 1, &m);
        assert_eq!(l.grid.len(), 6); // 30 - 24 = 6 on page 2
        assert_eq!(l.grid[0].app_id, "app24");
    }

    #[test]
    fn empty_model_produces_empty_layout() {
        let m = ShellModel::default();
        let l = compute(1224.0, 2700.0, 0, &m);
        assert!(l.grid.is_empty());
        assert!(l.dock.is_empty());
        assert_eq!(l.page_count, 1);
    }

    #[test]
    fn clamped_page_beyond_max() {
        let mut m = ShellModel::default();
        m.place("x".into());
        let l = compute(1224.0, 2700.0, 99, &m);
        // Should clamp to last page
        assert_eq!(l.grid.len(), 1);
        assert_eq!(l.grid[0].app_id, "x");
    }

    #[test]
    fn badge_rect_at_icon_top_left() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        let s = &l.grid[0];
        assert!((s.badge_rect.center_x() - s.icon_rect.x).abs() < s.icon_rect.w);
        assert!((s.badge_rect.center_y() - s.icon_rect.y).abs() < s.icon_rect.h);
        assert!(s.badge_rect.w > 0.0 && s.badge_rect.h > 0.0);
    }

    #[test]
    fn dock_zone_spans_dock_band() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        let d = &l.dock[0];
        assert!(l.dock_zone.contains(d.icon_rect.center_x(), d.icon_rect.center_y()));
        assert!(!l.dock_zone.contains(612.0, 100.0));
    }

    #[test]
    fn done_button_nonempty_outside_grid() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        assert!(l.done_button.w > 0.0 && l.done_button.h > 0.0);
        for s in &l.grid {
            assert!(!l.done_button.contains(s.icon_rect.center_x(), s.icon_rect.center_y()));
        }
    }

    #[test]
    fn arrange_hit_prefers_badge_then_done_then_icon() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        let s = &l.grid[0];
        let hit = hit_test_arrange(&l, s.badge_rect.center_x(), s.badge_rect.center_y());
        assert!(matches!(hit, Hit::RemoveBadge { .. }));
        let hit = hit_test_arrange(&l, l.done_button.center_x(), l.done_button.center_y());
        assert_eq!(hit, Hit::DoneButton);
        let far_x = s.icon_rect.x + s.icon_rect.w * 0.9;
        let far_y = s.icon_rect.y + s.icon_rect.h * 0.9;
        assert!(matches!(hit_test_arrange(&l, far_x, far_y), Hit::GridIcon { .. }));
    }

    #[test]
    fn normal_hit_test_ignores_badge_and_done() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        assert_eq!(hit_test(&l, l.done_button.center_x(), l.done_button.center_y()), Hit::Miss);
    }

    #[test]
    fn global_slot_pos_matches_compute_first_slot() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        let (gx, gy) = global_slot_pos(0, 0, 1224.0, 2700.0);
        assert!((gx - l.grid[0].icon_rect.center_x()).abs() < 0.01);
        assert!((gy - l.grid[0].icon_rect.center_y()).abs() < 0.01);
    }

    #[test]
    fn global_slot_pos_page1_offset_by_width() {
        let (x0, y0) = global_slot_pos(0, 0, 1224.0, 2700.0);
        let (x1, y1) = global_slot_pos(1, 0, 1224.0, 2700.0);
        assert!((x1 - (x0 + 1224.0)).abs() < 0.01);
        assert!((y1 - y0).abs() < 0.01);
    }

    #[test]
    fn global_slot_pos_advances_by_cell_within_page() {
        let (x0, _) = global_slot_pos(0, 0, 1224.0, 2700.0);
        let (x1, _) = global_slot_pos(0, 1, 1224.0, 2700.0);
        assert!(x1 > x0);
    }

    #[test]
    fn slot_at_center_places_icon_and_badge() {
        let s = slot_at_center("x".into(), 500.0, 600.0, 1224.0, 2700.0);
        assert_eq!(s.app_id, "x");
        assert!((s.icon_rect.center_x() - 500.0).abs() < 0.01);
        assert!((s.icon_rect.center_y() - 600.0).abs() < 0.01);
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        assert!((s.icon_rect.w - l.grid[0].icon_rect.w).abs() < 0.01);
        assert!(s.badge_rect.w > 0.0);
        assert!((s.badge_rect.center_x() - s.icon_rect.x).abs() < s.icon_rect.w);
    }

    #[test]
    fn nearest_grid_index_maps_and_clamps() {
        let (w, h) = (1224.0, 2700.0);
        let p0 = global_slot_pos(0, 0, w, h);
        assert_eq!(nearest_grid_index(w, h, p0.0, p0.1), 0);
        let p1 = global_slot_pos(0, 1, w, h);
        assert_eq!(nearest_grid_index(w, h, p1.0, p1.1), 1);
        assert!(nearest_grid_index(w, h, -9999.0, -9999.0) < PAGE_CAP);
        assert!(nearest_grid_index(w, h, 9e9, 9e9) < PAGE_CAP);
    }
}
