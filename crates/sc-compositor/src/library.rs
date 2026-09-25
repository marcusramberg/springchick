//! The app library: the last home page, holding one tile per category folder,
//! and the panel a tile opens.
//!
//! The library is derived, never stored — `State::folders` is rebuilt from each
//! catalog scan, and the page itself is synthesised past the end of
//! `model.pages` (see `sc_layout::compute`, which counts it). Home pages hold
//! only what the user put there; everything installed is always reachable here.

use sc_layout::library::{PanelHit, PanelLayout};

use crate::arrange::{ArrangeState, DragItem, IconPress};
use crate::input_dispatch::IconSource;
use crate::state::State;
use crate::ui_state::{UiState, ZoomOrigin};

/// A folder the user has opened on the library page.
pub(crate) struct OpenFolder {
    /// Index into `State::folders`.
    pub index: usize,
    /// Scroll offset in physical pixels; clamped by the layout.
    pub scroll: f32,
    /// Member index under the finger, drawn pressed and launched on release.
    pub pressed: Option<usize>,
    /// Finger position and scroll offset when the current drag began:
    /// `(x, y, scroll)`.
    pub drag: Option<(f32, f32, f32)>,
    /// Tile center the panel zooms out of, in screen coordinates.
    pub anchor: (f32, f32),
    /// 0→1 open animation, retargeted to 0 on close.
    pub open: sc_anim::Spring,
    /// Closing: the folder is inert and is dropped once `open` settles.
    pub closing: bool,
}

impl OpenFolder {
    pub fn new(index: usize, anchor: (f32, f32)) -> Self {
        Self {
            index,
            scroll: 0.0,
            pressed: None,
            drag: None,
            anchor,
            open: sc_anim::Spring::zoom(0.0, 1.0),
            closing: false,
        }
    }
}

impl State {
    /// Total home pages including the library, which is always the last one.
    /// Single source for every page-count assignment — the model alone is one
    /// short.
    pub(crate) fn home_page_count(&self) -> usize {
        self.model.pages.len().max(1) + 1
    }

    /// Index of the library page.
    pub(crate) fn library_page(&self) -> usize {
        self.home_page_count() - 1
    }

    /// Whether the shell is on (or animating onto) the library page.
    pub(crate) fn on_library_page(&self) -> bool {
        matches!(&self.ui, UiState::Home { page, .. } if *page == self.library_page())
    }

    /// Horizontal offset the library page is drawn at: zero when it fills the
    /// screen, a full width away when the last user page does. Tiles are laid
    /// out in page-local coordinates and shifted by this, the same way grid
    /// icons ride their page-scrolled springs.
    pub(crate) fn library_x_offset(&self) -> f32 {
        let (w, _) = self.output_size_f();
        (self.library_page() as f32 - self.home_page_scroll()) * w
    }

    /// Folder tiles for the library page, in page-local coordinates.
    pub(crate) fn library_tiles(&self) -> Vec<sc_layout::library::FolderSlot> {
        let (w, h) = self.output_size_f();
        let names: Vec<(String, usize)> = self
            .folders
            .iter()
            .map(|f| (f.name.to_string(), f.apps.len()))
            .collect();
        sc_layout::library::folders(w, h, &names)
    }

    /// Start the folder's zoom-out. It stays in `State::folder`, inert, until
    /// the spring settles and `advance_frame` drops it.
    pub(crate) fn close_folder(&mut self) {
        if let Some(f) = &mut self.folder {
            f.closing = true;
            f.pressed = None;
            f.drag = None;
            f.open.retarget(0.0);
        }
        self.needs_render = true;
    }

    /// Whether a folder is open and still taking input (a closing one is not).
    fn folder_live(&self) -> bool {
        self.folder.as_ref().is_some_and(|f| !f.closing)
    }

    /// Screen-space center of a library tile, the point its panel zooms out of.
    fn folder_anchor(&self, index: usize) -> (f32, f32) {
        let (w, h) = self.output_size_f();
        let dx = self.library_x_offset();
        self.library_tiles()
            .get(index)
            .map(|t| (t.tile_rect.center_x() + dx, t.tile_rect.center_y()))
            .unwrap_or((w / 2.0, h / 2.0))
    }

    /// Open the folder at `index`, zooming out of its tile.
    pub(crate) fn open_folder(&mut self, index: usize) {
        let anchor = self.folder_anchor(index);
        self.folder = Some(OpenFolder::new(index, anchor));
    }

    /// Layout for the open folder's panel, if one is open.
    pub(crate) fn folder_panel(&self) -> Option<PanelLayout> {
        let open = self.folder.as_ref()?;
        let folder = self.folders.get(open.index)?;
        let (w, h) = self.output_size_f();
        Some(sc_layout::library::panel(w, h, &folder.apps, open.scroll))
    }

    /// Press while a folder is open. The panel owns every press: a member arms
    /// its launch, the card swallows and starts a scroll drag, and anywhere
    /// outside closes the folder without falling through to the page beneath.
    pub(crate) fn folder_press(&mut self, x: f32, y: f32) -> bool {
        if !self.folder_live() {
            return false;
        }
        let Some(panel) = self.folder_panel() else {
            return false;
        };
        match sc_layout::library::panel_hit_test(&panel, x, y) {
            PanelHit::App { index, app_id } => {
                if let Some(f) = &mut self.folder {
                    f.pressed = Some(index);
                    f.drag = Some((x, y, f.scroll));
                }
                // Arm the hold that opens the member's context menu ("Add to
                // Home" and friends), the same way a home icon does.
                self.icon_press = Some(IconPress {
                    app_id,
                    source: IconSource::Library,
                    start: (x, y),
                    at: std::time::Instant::now(),
                });
            }
            PanelHit::Panel => {
                if let Some(f) = &mut self.folder {
                    f.pressed = None;
                    f.drag = Some((x, y, f.scroll));
                }
            }
            PanelHit::Outside => self.close_folder(),
        }
        self.needs_render = true;
        true
    }

    /// Drag inside an open folder. The axis decides: vertical scrolls the
    /// member list, horizontal lifts the pressed app out of the folder and
    /// hands it to the ordinary arrange drag.
    ///
    /// Axis rather than a long press because the long press is spoken for by
    /// the member's context menu — and sideways is the direction the home pages
    /// are in anyway.
    pub(crate) fn folder_motion(&mut self, x: f32, y: f32) -> bool {
        if !self.folder_live() {
            return false;
        }
        let Some(panel) = self.folder_panel() else {
            return false;
        };
        let max = panel.max_scroll();
        let Some(f) = &self.folder else {
            return false;
        };
        let Some((start_x, start_y, start_scroll)) = f.drag else {
            return true;
        };
        let (dx, dy) = (x - start_x, y - start_y);

        if let Some(app_id) = f
            .pressed
            .filter(|_| dx.abs() > dy.abs() && dx.abs() > SCROLL_SLOP)
            .and_then(|i| panel.apps.get(i))
            .map(|s| s.app_id.clone())
        {
            self.lift_from_library(app_id, (x, y));
            return true;
        }

        let Some(f) = &mut self.folder else {
            return false;
        };
        if dy.abs() > SCROLL_SLOP {
            f.pressed = None;
            self.icon_press = None;
        }
        f.scroll = (start_scroll - dy).clamp(0.0, max);
        self.needs_render = true;
        true
    }

    /// Pick an app up out of the open folder: close the folder, engage arrange
    /// mode, and start an ordinary drag carrying it. From here the existing
    /// arrange machinery owns the gesture — edge-dwell flips to a home page and
    /// the drop places or pins it.
    fn lift_from_library(&mut self, app_id: String, at: (f32, f32)) {
        tracing::debug!(
            target: "springchick::debug",
            "library drag lifted app_id={app_id}"
        );
        self.close_folder();
        self.icon_press = None;
        self.pending_folder = None;
        self.cancel_page_drag();
        self.arrange = Some(ArrangeState {
            drag: Some(DragItem {
                app_id,
                source: IconSource::Library,
                cur: at,
                hover: None,
                edge_since: None,
            }),
            just_engaged: false,
        });
        self.needs_render = true;
    }

    /// Release inside an open folder: launch the armed member and close.
    pub(crate) fn folder_release(&mut self) -> bool {
        if !self.folder_live() {
            return false;
        }
        let Some(panel) = self.folder_panel() else {
            return false;
        };
        let Some(f) = &mut self.folder else {
            return false;
        };
        f.drag = None;
        self.icon_press = None;
        let Some(f) = &mut self.folder else {
            return false;
        };
        let Some(index) = f.pressed.take() else {
            return true;
        };
        let Some(slot) = panel.apps.get(index) else {
            return true;
        };
        let (app_id, origin) = (
            slot.app_id.clone(),
            ZoomOrigin::icon((slot.icon_rect.center_x(), slot.icon_rect.center_y())),
        );
        self.close_folder();
        self.launch_or_raise(&app_id, origin);
        self.needs_render = true;
        true
    }

    /// Press on the library page with no folder open: arm the tile under the
    /// finger. Never consumes the press — the page drag still has to be armed
    /// from the same point, so a swipe that starts on a tile pages instead.
    pub(crate) fn library_press(&mut self, x: f32, y: f32) {
        if !self.on_library_page() || self.arrange.is_some() {
            return;
        }
        let tiles = self.library_tiles();
        let dx = self.library_x_offset();
        if let Some(i) = sc_layout::library::hit_test(&tiles, x - dx, y) {
            self.pending_folder = Some((i, (x, y)));
        }
    }
}

/// Finger travel (physical px) past which a press inside a folder is a scroll,
/// not a tap.
const SCROLL_SLOP: f32 = 12.0;
