//! Geometry for the app library: the folder tiles on the last home page, and
//! the panel one of them opens.
//!
//! The tiles sit in the ordinary home grid cells — same columns, same icon
//! size — so an app dragged out of a folder lands in a cell the same size it
//! left. The panel reuses those cells too, scrolled and clipped to a card.

use crate::{grid_metrics, grid_slot, IconSlot, Rect};
use sc_shell_model::COLS;

/// A folder tile on the library page.
#[derive(Clone, Debug, PartialEq)]
pub struct FolderSlot {
    pub name: String,
    /// The rounded tile, occupying the cell's icon footprint.
    pub tile_rect: Rect,
    /// Label below the tile.
    pub label_rect: Rect,
    /// Up to four member-icon rects inside the tile, row-major 2x2.
    pub preview: Vec<Rect>,
}

/// Inset of the 2x2 preview inside the tile, as a fraction of the tile edge.
const PREVIEW_PAD_FRAC: f32 = 0.12;
/// Gap between the two preview columns/rows, as a fraction of the tile edge.
const PREVIEW_GAP_FRAC: f32 = 0.06;

/// Folder tiles for the library page: `(name, member count)` in display order.
/// Only the count is needed — the caller holds the app ids and this just makes
/// the preview rects to draw the first four into.
pub fn folders(width: f32, height: f32, names: &[(String, usize)]) -> Vec<FolderSlot> {
    let gm = grid_metrics(width, height);
    names
        .iter()
        .enumerate()
        .map(|(i, (name, members))| {
            let slot = grid_slot(&gm, i, 0.0, name.clone());
            FolderSlot {
                name: name.clone(),
                tile_rect: slot.icon_rect,
                label_rect: slot.label_rect,
                preview: preview_rects(slot.icon_rect, (*members).min(4)),
            }
        })
        .collect()
}

/// The 2x2 mini-icon rects inside a tile, row-major, first `n` of them.
fn preview_rects(tile: Rect, n: usize) -> Vec<Rect> {
    let pad = tile.w * PREVIEW_PAD_FRAC;
    let gap = tile.w * PREVIEW_GAP_FRAC;
    let s = ((tile.w - 2.0 * pad - gap) / 2.0).max(0.0);
    (0..n)
        .map(|i| Rect {
            x: tile.x + pad + (i % 2) as f32 * (s + gap),
            y: tile.y + pad + (i / 2) as f32 * (s + gap),
            w: s,
            h: s,
        })
        .collect()
}

/// Index of the folder tile at `(x, y)`, or `None`.
pub fn hit_test(folders: &[FolderSlot], x: f32, y: f32) -> Option<usize> {
    folders
        .iter()
        .position(|f| f.tile_rect.contains(x, y) || f.label_rect.contains(x, y))
}

/// An open folder's panel.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelLayout {
    /// The rounded card. Everything inside is clipped to it.
    pub panel: Rect,
    /// Folder name, at the top of the card.
    pub title_rect: Rect,
    /// Member icons, already shifted by the scroll offset — some may lie
    /// outside `panel` and must be clipped by the caller.
    pub apps: Vec<IconSlot>,
    /// Total height the rows occupy, unscrolled.
    pub content_h: f32,
    /// Height visible for rows (the card minus its title and padding).
    pub view_h: f32,
}

impl PanelLayout {
    /// Largest useful scroll offset; 0 when everything fits.
    pub fn max_scroll(&self) -> f32 {
        (self.content_h - self.view_h).max(0.0)
    }
}

/// Card inset from the screen edges, as a fraction of the smaller dimension.
const PANEL_MARGIN_FRAC: f32 = 0.04;
/// Title band height, as a fraction of output height.
const TITLE_H_FRAC: f32 = 0.035;

/// Lay out the panel for a folder holding `app_ids`, scrolled down by `scroll`
/// logical pixels. `scroll` is clamped to `[0, max_scroll]`, so a caller can
/// over-scroll freely and read back the clamped value from the slot positions.
pub fn panel(width: f32, height: f32, app_ids: &[String], scroll: f32) -> PanelLayout {
    let gm = grid_metrics(width, height);
    let margin = width.min(height) * PANEL_MARGIN_FRAC;
    let title_h = height * TITLE_H_FRAC;

    // The card spans the grid band: below the top padding, above the dots. The
    // dock stays visible underneath, which is what makes a drag out of the
    // panel and onto the dock possible.
    let panel = Rect {
        x: margin,
        y: gm.grid_top,
        w: width - 2.0 * margin,
        h: (gm.dots_top - gm.grid_top).max(title_h),
    };
    let title_rect = Rect {
        x: panel.x,
        y: panel.y,
        w: panel.w,
        h: title_h,
    };

    let rows = app_ids.len().div_ceil(COLS);
    let content_h = rows as f32 * gm.cell_h;
    let view_h = (panel.h - title_h).max(0.0);
    let scroll = scroll.clamp(0.0, (content_h - view_h).max(0.0));

    // Rows are laid out in the ordinary grid cells, then moved as a block to
    // start under the title and shifted by the scroll. Reusing `grid_slot`
    // keeps the icons pixel-identical to the ones on a home page, which is what
    // makes a drag out of the panel look continuous.
    let dy = panel.y + title_h - gm.grid_top - scroll;
    let apps = app_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let mut slot = grid_slot(&gm, i, 0.0, id.clone());
            shift_slot(&mut slot, dy);
            slot
        })
        .collect();

    PanelLayout {
        panel,
        title_rect,
        apps,
        content_h,
        view_h,
    }
}

fn shift_slot(slot: &mut IconSlot, dy: f32) {
    for r in [
        &mut slot.icon_rect,
        &mut slot.label_rect,
        &mut slot.badge_rect,
        &mut slot.dot_rect,
    ] {
        r.y += dy;
    }
}

/// What a press inside an open folder landed on.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelHit {
    /// A member app.
    App { app_id: String, index: usize },
    /// The card, but not an app — swallow it (a press here scrolls or does
    /// nothing; it must not fall through to the page underneath).
    Panel,
    /// Outside the card: dismiss the folder.
    Outside,
}

pub fn panel_hit_test(p: &PanelLayout, x: f32, y: f32) -> PanelHit {
    if !p.panel.contains(x, y) {
        return PanelHit::Outside;
    }
    // Rows are clipped to the card, so a slot scrolled out from under the title
    // must not stay tappable where it is no longer drawn.
    let rows = Rect {
        x: p.panel.x,
        y: p.title_rect.y + p.title_rect.h,
        w: p.panel.w,
        h: p.view_h,
    };
    if rows.contains(x, y) {
        for (i, slot) in p.apps.iter().enumerate() {
            if slot.icon_rect.contains(x, y) || slot.label_rect.contains(x, y) {
                return PanelHit::App {
                    app_id: slot.app_id.clone(),
                    index: i,
                };
            }
        }
    }
    PanelHit::Panel
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: &[(f32, f32)] = &[
        (1224.0, 2700.0), // Fairphone 5, portrait
        (1901.0, 2088.0), // nested winit window
        (2700.0, 1224.0), // rotated
        (720.0, 1440.0),
    ];

    fn names(n: usize) -> Vec<(String, usize)> {
        (0..n).map(|i| (format!("F{i}"), 4)).collect()
    }

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("app{i:02}")).collect()
    }

    #[test]
    fn tiles_land_in_grid_cells_and_hit_test_back() {
        for &(w, h) in SIZES {
            let f = folders(w, h, &names(11));
            assert_eq!(f.len(), 11);
            for (i, slot) in f.iter().enumerate() {
                assert_eq!(
                    hit_test(&f, slot.tile_rect.center_x(), slot.tile_rect.center_y()),
                    Some(i),
                    "{w}x{h} tile {i}"
                );
            }
            // Row-major: the first COLS tiles share a row, the next starts lower.
            assert!((f[0].tile_rect.y - f[COLS - 1].tile_rect.y).abs() < 0.01);
            assert!(f[COLS].tile_rect.y > f[0].tile_rect.y);
        }
    }

    #[test]
    fn preview_is_a_2x2_inside_the_tile() {
        let f = folders(1224.0, 2700.0, &names(1));
        let t = f[0].tile_rect;
        assert_eq!(f[0].preview.len(), 4);
        for r in &f[0].preview {
            assert!(
                r.x >= t.x && r.y >= t.y && r.x + r.w <= t.x + t.w && r.y + r.h <= t.y + t.h,
                "preview {r:?} escapes tile {t:?}"
            );
        }
        // Row-major 2x2: second is right of first, third below it.
        assert!(f[0].preview[1].x > f[0].preview[0].x);
        assert!(f[0].preview[2].y > f[0].preview[0].y);
    }

    #[test]
    fn preview_shows_only_what_the_folder_holds() {
        let f = folders(1224.0, 2700.0, &[("One".into(), 1), ("Many".into(), 9)]);
        assert_eq!(f[0].preview.len(), 1);
        assert_eq!(f[1].preview.len(), 4); // capped
    }

    #[test]
    fn panel_fits_between_top_pad_and_dots_on_every_size() {
        for &(w, h) in SIZES {
            let p = panel(w, h, &ids(25), 0.0);
            assert!(p.panel.x > 0.0 && p.panel.x + p.panel.w < w, "{w}x{h}");
            assert!(p.panel.y > 0.0 && p.panel.y + p.panel.h < h, "{w}x{h}");
            assert!(p.view_h > 0.0, "{w}x{h}");
        }
    }

    #[test]
    fn short_folder_does_not_scroll() {
        let p = panel(1224.0, 2700.0, &ids(3), 0.0);
        assert_eq!(p.max_scroll(), 0.0);
        // First row sits under the title, inside the card.
        let first = p.apps[0].icon_rect;
        assert!(first.y >= p.title_rect.y + p.title_rect.h);
    }

    #[test]
    fn long_folder_scrolls_and_clamps() {
        let (w, h) = (1224.0, 2700.0);
        let unscrolled = panel(w, h, &ids(40), 0.0);
        assert!(unscrolled.max_scroll() > 0.0);

        let max = unscrolled.max_scroll();
        let scrolled = panel(w, h, &ids(40), max);
        assert!(scrolled.apps[0].icon_rect.y < unscrolled.apps[0].icon_rect.y);

        // Over-scroll is clamped, not compounded.
        let over = panel(w, h, &ids(40), max * 4.0);
        assert_eq!(over.apps[0].icon_rect.y, scrolled.apps[0].icon_rect.y);

        // Negative scroll is clamped too.
        let under = panel(w, h, &ids(40), -500.0);
        assert_eq!(under.apps[0].icon_rect.y, unscrolled.apps[0].icon_rect.y);
    }

    #[test]
    fn panel_hit_test_finds_apps_and_distinguishes_dismiss() {
        let (w, h) = (1224.0, 2700.0);
        let p = panel(w, h, &ids(8), 0.0);
        for (i, slot) in p.apps.iter().enumerate() {
            assert_eq!(
                panel_hit_test(&p, slot.icon_rect.center_x(), slot.icon_rect.center_y()),
                PanelHit::App {
                    app_id: slot.app_id.clone(),
                    index: i
                }
            );
        }
        // The title band is inside the card: swallowed, not a dismiss.
        assert_eq!(
            panel_hit_test(&p, p.title_rect.center_x(), p.title_rect.center_y()),
            PanelHit::Panel
        );
        assert_eq!(panel_hit_test(&p, 1.0, 1.0), PanelHit::Outside);
    }

    /// A row half-scrolled under the title is drawn clipped, so the hidden part
    /// must not stay tappable — otherwise a press on the title launches
    /// whatever happens to be sliding beneath it.
    #[test]
    fn the_part_of_a_row_hidden_under_the_title_is_not_tappable() {
        let (w, h) = (1224.0, 2700.0);
        let unscrolled = panel(w, h, &ids(40), 0.0);
        let rows_top = unscrolled.title_rect.y + unscrolled.title_rect.h;
        // Scroll the first row's icon centre just above the title edge, so its
        // lower half is still inside the rows viewport and it straddles.
        let p = panel(
            w,
            h,
            &ids(40),
            unscrolled.apps[0].icon_rect.center_y() - rows_top + 5.0,
        );
        let straddler = p
            .apps
            .iter()
            .find(|s| s.icon_rect.y < rows_top && s.icon_rect.y + s.icon_rect.h > rows_top)
            .expect("a slot should straddle the title edge");
        let probe_y = rows_top - 1.0;
        assert!(straddler
            .icon_rect
            .contains(straddler.icon_rect.center_x(), probe_y));
        assert_eq!(
            panel_hit_test(&p, straddler.icon_rect.center_x(), probe_y),
            PanelHit::Panel
        );
    }
}
