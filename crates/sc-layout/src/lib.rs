#![forbid(unsafe_code)]

//! Home screen geometry and hit-testing: (output size, page, model) → rects.

pub mod layer;
pub mod library;
pub mod menu;

use sc_shell_model::{ShellModel, COLS, DOCK_CAP, ROWS};

/// Origin top-left.
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

#[derive(Clone, Debug, PartialEq)]
pub struct IconSlot {
    pub app_id: String,
    pub icon_rect: Rect,
    pub label_rect: Rect,
    /// Arrange-mode remove badge, centered on the icon's top-left corner.
    pub badge_rect: Rect,
    /// Running indicator; drawn only, never hit-tested.
    pub dot_rect: Rect,
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub grid: Vec<IconSlot>,
    pub dock: Vec<IconSlot>,
    pub dots_rect: Rect,
    pub bar_rect: Rect,
    pub page_count: usize,
    /// Full-width strip behind the dock icons.
    pub dock_zone: Rect,
    pub done_button: Rect,
}

impl Layout {
    /// Move "Done" below a top exclusive zone (physical px). Apply identically to
    /// render and hit-test.
    pub fn shift_done_below(&mut self, top_inset: f32) {
        self.done_button.y += top_inset.max(0.0);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    GridIcon { app_id: String, index: usize },
    DockIcon { app_id: String, index: usize },
    Bar,
    RemoveBadge { app_id: String },
    DoneButton,
    Miss,
}

// Fractions of output dimensions.

/// Status bar area.
const TOP_PAD: f32 = 0.04;
const BAR_HEIGHT: f32 = 0.03;
/// Logical px.
pub const PILL_HEIGHT: f32 = 8.0;
const DOCK_HEIGHT: f32 = 0.10;
const DOTS_HEIGHT: f32 = 0.02;
/// Each side.
const H_MARGIN: f32 = 0.04;
/// Of cell width.
const ICON_SIZE_FRAC: f32 = 0.62;
/// Of cell height.
const LABEL_HEIGHT_FRAC: f32 = 0.18;
/// Gap below a label, as a fraction of the icon+label band.
const CELL_V_PAD_FRAC: f32 = 0.06;

/// Fits both the cell width and the band shared with the label. Width alone
/// overflows on cells wider than tall (4x6 on a portrait phone), and the
/// label gets painted over by the next row.
fn fit_icon_size(cell_w: f32, band_h: f32, label_h: f32) -> f32 {
    let available = band_h - label_h - band_h * CELL_V_PAD_FRAC;
    (cell_w * ICON_SIZE_FRAC).min(available).max(0.0)
}

pub fn bar_rect(width: f32, height: f32) -> Rect {
    Rect {
        x: 0.0,
        y: height * (1.0 - BAR_HEIGHT),
        w: width,
        h: height * BAR_HEIGHT,
    }
}

/// Space the gesture bar reserves from apps: the gap above plus below the
/// centered pill.
pub fn gesture_exclusive_zone(height: f32) -> f32 {
    (height * BAR_HEIGHT - PILL_HEIGHT).max(0.0)
}

pub fn pill_in_bar(bar: Rect) -> Rect {
    let pill_w = bar.w * 0.35;
    Rect {
        x: bar.x + (bar.w - pill_w) / 2.0,
        y: bar.y + (bar.h - PILL_HEIGHT) / 2.0,
        w: pill_w,
        h: PILL_HEIGHT,
    }
}

pub fn pill_rect(width: f32, height: f32) -> Rect {
    pill_in_bar(bar_rect(width, height))
}

/// The pill under a card, measured from the card's drawn rect (the app's
/// pixels, which are only the usable area), scaled with the card so it
/// doesn't jump as the card returns to fullscreen.
pub fn pill_under(card: Rect, width: f32, height: f32) -> Rect {
    let scale = if width > 0.0 { card.w / width } else { 1.0 };
    let pill_w = card.w * 0.35;
    Rect {
        x: card.x + (card.w - pill_w) / 2.0,
        y: card.y + card.h + (gesture_exclusive_zone(height) / 2.0 - PILL_HEIGHT) * scale,
        w: pill_w,
        h: PILL_HEIGHT * scale,
    }
}

struct GridMetrics {
    dock_top: f32,
    dots_top: f32,
    grid_left: f32,
    grid_top: f32,
    cell_w: f32,
    cell_h: f32,
    icon_size: f32,
    label_h: f32,
}

fn grid_metrics(width: f32, height: f32) -> GridMetrics {
    let dock_top = bar_rect(width, height).y - height * DOCK_HEIGHT;
    let dots_top = dock_top - height * DOTS_HEIGHT;

    let grid_top = height * TOP_PAD;
    let grid_bottom = dots_top;
    let grid_height = grid_bottom - grid_top;

    let usable_width = width * (1.0 - 2.0 * H_MARGIN);
    let grid_left = width * H_MARGIN;

    let cell_w = usable_width / COLS as f32;
    let cell_h = grid_height / ROWS as f32;
    let label_h = cell_h * LABEL_HEIGHT_FRAC;
    let icon_size = fit_icon_size(cell_w, cell_h, label_h);

    GridMetrics {
        dock_top,
        dots_top,
        grid_left,
        grid_top,
        cell_w,
        cell_h,
        icon_size,
        label_h,
    }
}

/// Cell `index` on the page offset by `x_offset`.
fn grid_slot(gm: &GridMetrics, index: usize, x_offset: f32, app_id: String) -> IconSlot {
    let col = index % COLS;
    let row = index / COLS;
    let cell_x = gm.grid_left + col as f32 * gm.cell_w + x_offset;
    let cell_y = gm.grid_top + row as f32 * gm.cell_h;
    let icon_rect = Rect {
        x: cell_x + (gm.cell_w - gm.icon_size) / 2.0,
        y: cell_y + (gm.cell_h - gm.icon_size - gm.label_h) / 2.0,
        w: gm.icon_size,
        h: gm.icon_size,
    };
    let label_rect = Rect {
        x: cell_x,
        y: icon_rect.y + gm.icon_size,
        w: gm.cell_w,
        h: gm.label_h,
    };
    IconSlot {
        app_id,
        icon_rect,
        label_rect,
        badge_rect: badge_of(icon_rect),
        dot_rect: dot_of(icon_rect, label_rect, cell_y + gm.cell_h),
    }
}

fn badge_of(ir: Rect) -> Rect {
    let s = ir.w * 0.34;
    Rect {
        x: ir.x - s / 2.0,
        y: ir.y - s / 2.0,
        w: s,
        h: s,
    }
}

const DOT_SIZE_FRAC: f32 = 0.07;

/// Centered between the label and `bottom`; shrinks to fit (the dock band
/// leaves little room on squarish outputs).
fn dot_of(icon: Rect, label: Rect, bottom: f32) -> Rect {
    let top = label.y + label.h;
    let space = (bottom - top).max(0.0);
    let s = (icon.w * DOT_SIZE_FRAC).min(space / 2.0);
    Rect {
        x: icon.center_x() - s / 2.0,
        y: top + (space - s) / 2.0,
        w: s,
        h: s,
    }
}

/// Global space: pages sit edge to edge, one `width` apart. Subtract
/// `page_scroll * width` for screen space.
pub fn global_slot_pos(page: usize, index: usize, width: f32, height: f32) -> (f32, f32) {
    let gm = grid_metrics(width, height);
    let icon = grid_slot(&gm, index, page as f32 * width, String::new()).icon_rect;
    (icon.center_x(), icon.center_y())
}

/// `x` is screen-space. Callers still clamp to the page's fill length.
pub fn nearest_grid_index(width: f32, height: f32, x: f32, y: f32) -> usize {
    let gm = grid_metrics(width, height);
    let col =
        (((x - gm.grid_left) / gm.cell_w).floor() as isize).clamp(0, COLS as isize - 1) as usize;
    let row =
        (((y - gm.grid_top) / gm.cell_h).floor() as isize).clamp(0, ROWS as isize - 1) as usize;
    row * COLS + col
}

/// Same sizing as the grid, for icons in flight between slots.
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
        dot_rect: dot_of(
            icon_rect,
            label_rect,
            label_rect.y + label_rect.h + (gm.cell_h - gm.icon_size - gm.label_h) / 2.0,
        ),
    }
}

/// `page == model.pages.len()` is the library page: no grid, but the same
/// dock, dots and bar.
pub fn compute(width: f32, height: f32, page: usize, model: &ShellModel) -> Layout {
    // +1 for the library page, which the model never stores.
    let page_count = model.pages.len().max(1) + 1;

    let bar_rect = bar_rect(width, height);

    let gm = grid_metrics(width, height);
    let dock_top = gm.dock_top;
    let dots_top = gm.dots_top;
    let dots_rect = Rect {
        x: 0.0,
        y: dots_top,
        w: width,
        h: height * DOTS_HEIGHT,
    };

    let usable_width = width * (1.0 - 2.0 * H_MARGIN);
    let grid_left = gm.grid_left;

    let grid = model
        .pages
        .get(page)
        .map(|apps| {
            apps.iter()
                .enumerate()
                .map(|(i, app_id)| grid_slot(&gm, i, 0.0, app_id.clone()))
                .collect()
        })
        .unwrap_or_default();

    let dock_cell_w = usable_width / DOCK_CAP as f32;
    let dock_band_h = height * DOCK_HEIGHT;
    let dock_label_h = dock_band_h * LABEL_HEIGHT_FRAC;
    let dock_icon_size = fit_icon_size(dock_cell_w, dock_band_h, dock_label_h);
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
            let label_rect = Rect {
                x: cell_x,
                y: icon_y + dock_icon_size,
                w: dock_cell_w,
                h: dock_label_h,
            };
            IconSlot {
                app_id: app_id.clone(),
                icon_rect,
                label_rect,
                badge_rect: badge_of(icon_rect),
                dot_rect: dot_of(icon_rect, label_rect, dock_top + dock_band_h),
            }
        })
        .collect();

    let dock_zone = Rect {
        x: 0.0,
        y: dock_top,
        w: width,
        h: height * DOCK_HEIGHT,
    };
    // Inside the top padding, so it never overlaps grid icons.
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

pub fn hit_test(layout: &Layout, x: f32, y: f32) -> Hit {
    if layout.bar_rect.contains(x, y) {
        return Hit::Bar;
    }

    for (i, slot) in layout.dock.iter().enumerate() {
        if slot.icon_rect.contains(x, y) || slot.label_rect.contains(x, y) {
            return Hit::DockIcon {
                app_id: slot.app_id.clone(),
                index: i,
            };
        }
    }

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

/// Done and badges overlap icons, so they're checked first.
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
    #[test]
    fn the_pill_meets_a_fullscreen_card_exactly_where_the_screen_pill_sits() {
        let (w, h) = (1224.0, 2700.0);
        let app = Rect {
            x: 0.0,
            y: 0.0,
            w,
            h: h - gesture_exclusive_zone(h),
        };
        let under = pill_under(app, w, h);
        let screen = pill_rect(w, h);
        // Must agree with the screen-edge pill at scale 1.
        assert!((under.x - screen.x).abs() < 0.01, "{under:?} vs {screen:?}");
        assert!((under.y - screen.y).abs() < 0.01, "{under:?} vs {screen:?}");
        assert!((under.w - screen.w).abs() < 0.01);
        assert!((under.h - screen.h).abs() < 0.01);

        let half = Rect {
            x: 100.0,
            y: 200.0,
            w: w / 2.0,
            h: app.h / 2.0,
        };
        let small = pill_under(half, w, h);
        assert!((small.w - under.w / 2.0).abs() < 0.01);
        assert!((small.h - under.h / 2.0).abs() < 0.01);
        let gap = |p: Rect, card: Rect| p.y - (card.y + card.h);
        assert!((gap(small, half) - gap(under, app) / 2.0).abs() < 0.01);
    }

    use super::*;
    use sc_shell_model::{ShellModel, PAGE_CAP};

    fn sample_model() -> ShellModel {
        let mut m = ShellModel::default();
        for i in 0..6 {
            m.place(format!("app{i}"));
        }
        m.dock.push("dock0".into());
        m.dock.push("dock1".into());
        m
    }

    const SIZES: &[(f32, f32)] = &[
        (1224.0, 2700.0), // Fairphone 5, portrait
        (1901.0, 2088.0),
        (2700.0, 1224.0),
        (720.0, 1440.0),
        (1080.0, 1080.0),
        (400.0, 800.0),
    ];

    #[test]
    fn label_never_overlaps_the_next_row() {
        let mut m = ShellModel::default();
        for i in 0..(COLS * ROWS) {
            m.place(format!("app{i}"));
        }

        for &(w, h) in SIZES {
            let l = compute(w, h, 0, &m);
            for (i, slot) in l.grid.iter().enumerate() {
                // The label must end before the next row's icon.
                if let Some(below) = l.grid.get(i + COLS) {
                    let label_bottom = slot.label_rect.y + slot.label_rect.h;
                    assert!(
                        label_bottom <= below.icon_rect.y,
                        "{w}x{h}: slot {i} label ends at {label_bottom} but the icon below \
                         starts at {}",
                        below.icon_rect.y
                    );
                }
            }
        }
    }

    #[test]
    fn running_dot_sits_under_its_icon_and_clears_the_row_below() {
        let mut m = ShellModel::default();
        for i in 0..(COLS * ROWS) {
            m.place(format!("app{i}"));
        }
        m.dock.push("dock0".into());

        for &(w, h) in SIZES {
            let l = compute(w, h, 0, &m);
            for (i, slot) in l.grid.iter().chain(l.dock.iter()).enumerate() {
                let dot = slot.dot_rect;
                assert!(
                    (dot.center_x() - slot.icon_rect.center_x()).abs() < 0.01,
                    "{w}x{h}: slot {i} dot is not centered under its icon"
                );
                assert!(
                    dot.y >= slot.label_rect.y + slot.label_rect.h,
                    "{w}x{h}: slot {i} dot overlaps its label"
                );
            }
            for slot in &l.dock {
                let dot_bottom = slot.dot_rect.y + slot.dot_rect.h;
                assert!(
                    dot_bottom <= l.dock_zone.y + l.dock_zone.h,
                    "{w}x{h}: dock dot ends at {dot_bottom}, past the dock band"
                );
            }
            for (i, slot) in l.grid.iter().enumerate() {
                if let Some(below) = l.grid.get(i + COLS) {
                    let dot_bottom = slot.dot_rect.y + slot.dot_rect.h;
                    assert!(
                        dot_bottom <= below.icon_rect.y,
                        "{w}x{h}: slot {i} dot ends at {dot_bottom} but the icon below \
                         starts at {}",
                        below.icon_rect.y
                    );
                }
            }
        }
    }

    #[test]
    fn icon_and_label_fit_inside_their_cell() {
        let mut m = ShellModel::default();
        for i in 0..(COLS * ROWS) {
            m.place(format!("app{i}"));
        }

        for &(w, h) in SIZES {
            let gm = grid_metrics(w, h);
            assert!(
                gm.icon_size + gm.label_h <= gm.cell_h,
                "{w}x{h}: icon {} + label {} exceeds cell height {}",
                gm.icon_size,
                gm.label_h,
                gm.cell_h
            );
            assert!(gm.icon_size > 0.0, "{w}x{h}: icon collapsed to nothing");

            let l = compute(w, h, 0, &m);
            for slot in &l.grid {
                assert!(
                    slot.icon_rect.y >= gm.grid_top - 0.5,
                    "{w}x{h}: icon at {} is above grid_top {}",
                    slot.icon_rect.y,
                    gm.grid_top
                );
            }
        }
    }

    #[test]
    fn dock_icon_and_label_fit_their_band() {
        let m = sample_model();
        for &(w, h) in SIZES {
            let l = compute(w, h, 0, &m);
            let dock_top = h * (1.0 - BAR_HEIGHT) - h * DOCK_HEIGHT;
            let dock_bottom = dock_top + h * DOCK_HEIGHT;
            for slot in &l.dock {
                assert!(
                    slot.icon_rect.y >= dock_top - 0.5,
                    "{w}x{h}: dock icon starts above its band"
                );
                let label_bottom = slot.label_rect.y + slot.label_rect.h;
                assert!(
                    label_bottom <= dock_bottom + 0.5,
                    "{w}x{h}: dock label ends at {label_bottom}, past band bottom {dock_bottom}"
                );
            }
        }
    }

    #[test]
    fn layout_produces_correct_grid_count() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        assert_eq!(l.grid.len(), 6);
        assert_eq!(l.dock.len(), 2);
        assert_eq!(l.page_count, 2);
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
        let hit = hit_test(&l, 612.0, 2690.0);
        assert_eq!(hit, Hit::Bar);
    }

    #[test]
    fn hit_test_grid_icon() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
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
        assert_eq!(l.page_count, 3);
    }

    #[test]
    fn second_page_shows_correct_icons() {
        let mut m = ShellModel::default();
        for i in 0..30 {
            m.place(format!("app{i}"));
        }
        let l = compute(1224.0, 2700.0, 1, &m);
        assert_eq!(l.grid.len(), 6);
        assert_eq!(l.grid[0].app_id, "app24");
    }

    #[test]
    fn empty_model_produces_empty_layout() {
        let m = ShellModel::default();
        let l = compute(1224.0, 2700.0, 0, &m);
        assert!(l.grid.is_empty());
        assert!(l.dock.is_empty());
        assert_eq!(l.page_count, 2);
    }

    #[test]
    fn library_page_has_no_grid_of_its_own() {
        let mut m = ShellModel::default();
        m.place("x".into());
        m.dock.push("d".into());
        let l = compute(1224.0, 2700.0, 1, &m); // page 1 == the library
        assert!(l.grid.is_empty());
        assert_eq!(l.dock.len(), 1);
        assert_eq!(l.page_count, 2);
        assert!(compute(1224.0, 2700.0, 99, &m).grid.is_empty());
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
        assert!(l
            .dock_zone
            .contains(d.icon_rect.center_x(), d.icon_rect.center_y()));
        assert!(!l.dock_zone.contains(612.0, 100.0));
    }

    #[test]
    fn done_button_nonempty_outside_grid() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        assert!(l.done_button.w > 0.0 && l.done_button.h > 0.0);
        for s in &l.grid {
            assert!(!l
                .done_button
                .contains(s.icon_rect.center_x(), s.icon_rect.center_y()));
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
        assert!(matches!(
            hit_test_arrange(&l, far_x, far_y),
            Hit::GridIcon { .. }
        ));
    }

    #[test]
    fn normal_hit_test_ignores_badge_and_done() {
        let m = sample_model();
        let l = compute(1224.0, 2700.0, 0, &m);
        assert_eq!(
            hit_test(&l, l.done_button.center_x(), l.done_button.center_y()),
            Hit::Miss
        );
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
