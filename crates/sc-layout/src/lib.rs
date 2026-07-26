#![forbid(unsafe_code)]

//! Pure geometry and hit-testing for the springchick home screen.
//!
//! Given output dimensions, a page index, and a `ShellModel`, produces screen-space
//! rectangles for every icon, dock slot, page-indicator dots, and the bottom bar zone.
//! Also provides the inverse: `point → Hit`.

pub mod layer;

use sc_shell_model::{ShellModel, COLS, DOCK_CAP, ROWS};

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

    let grid_top = height * TOP_PAD;
    let grid_bottom = dots_top;
    let grid_height = grid_bottom - grid_top;

    let usable_width = width * (1.0 - 2.0 * H_MARGIN);
    let grid_left = width * H_MARGIN;

    let cell_w = usable_width / COLS as f32;
    let cell_h = grid_height / ROWS as f32;
    let icon_size = cell_w * ICON_SIZE_FRAC;
    let label_h = cell_h * LABEL_HEIGHT_FRAC;

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
                IconSlot {
                    app_id: app_id.clone(),
                    icon_rect: Rect {
                        x: icon_x,
                        y: icon_y,
                        w: icon_size,
                        h: icon_size,
                    },
                    label_rect: Rect {
                        x: cell_x,
                        y: icon_y + icon_size,
                        w: cell_w,
                        h: label_h,
                    },
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
            IconSlot {
                app_id: app_id.clone(),
                icon_rect: Rect {
                    x: icon_x,
                    y: icon_y,
                    w: dock_icon_size,
                    h: dock_icon_size,
                },
                label_rect: Rect {
                    x: cell_x,
                    y: icon_y + dock_icon_size,
                    w: dock_cell_w,
                    h: dock_label_h,
                },
            }
        })
        .collect();

    Layout {
        grid,
        dock,
        dots_rect,
        bar_rect,
        page_count,
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
}
