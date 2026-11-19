//! The pull-down sheet: where its controls sit and what a finger does to
//! them. Pure; `main` carries out the [`Change`]s and `system` reads the
//! [`Controls`]. A control the system doesn't report is left out.

use sc_anim::Spring;
use sc_layout::Rect;

/// Logical px.
pub const BAR_H: f32 = 28.0;
/// Kept clear above the modules, for the power glyph and the status row.
const TOP: f32 = 70.0;
const TOUCH_SLOP: f32 = 8.0;
/// Underdamped, so the sheet lands with some weight: it runs about 3% of
/// the way it fell past the bottom, and comes back.
const STIFFNESS: f32 = 450.0;
const DAMPING: f32 = 32.0;
/// Seconds of release velocity that count towards open or closed.
const PROJECT: f32 = 0.2;
/// Gap between modules, in cells.
const GAP: f32 = 0.24;
const COLUMNS: f32 = 4.0;
/// Before the last row wraps.
const ROWS: usize = 5;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Controls {
    /// By `Toggle as usize`.
    pub toggles: [Option<bool>; 10],
    /// 0..1.
    pub brightness: Option<f32>,
    /// 0..1.
    pub volume: Option<f32>,
    pub media: Option<Media>,
    /// How many app shortcuts the shell keeps for the sheet.
    pub shortcuts: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Media {
    pub title: String,
    pub artist: String,
    pub playing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toggle {
    Airplane,
    Cellular,
    Wifi,
    Bluetooth,
    /// On keeps the shell upright; off lets it turn with the device (`rotate_ui`).
    RotationLock,
    Silent,
    DarkMode,
    LowPower,
    Torch,
    /// Wi-Fi sharing the connection as an access point.
    Hotspot,
}

impl Toggle {
    /// As `[bar.long_press]` names it.
    pub fn named(name: &str) -> Option<Toggle> {
        Some(match name {
            "airplane" => Toggle::Airplane,
            "cellular" => Toggle::Cellular,
            "wifi" => Toggle::Wifi,
            "bluetooth" => Toggle::Bluetooth,
            "rotation_lock" => Toggle::RotationLock,
            "silent" => Toggle::Silent,
            "dark_mode" => Toggle::DarkMode,
            "low_power" => Toggle::LowPower,
            "torch" => Toggle::Torch,
            "hotspot" => Toggle::Hotspot,
            _ => return None,
        })
    }

    pub fn get(self, c: &Controls) -> Option<bool> {
        c.toggles[self as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slider {
    Brightness,
    Volume,
}

impl Slider {
    pub fn get(self, c: &Controls) -> Option<f32> {
        match self {
            Slider::Brightness => c.brightness,
            Slider::Volume => c.volume,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaAction {
    Previous,
    PlayPause,
    Next,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    Sleep,
    LogOut,
    Restart,
    PowerOff,
}

impl PowerAction {
    pub fn label(self) -> &'static str {
        match self {
            PowerAction::Sleep => "Sleep",
            PowerAction::LogOut => "Log out",
            PowerAction::Restart => "Restart",
            PowerAction::PowerOff => "Power off",
        }
    }
}

/// What the sheet shows: its controls, the power menu over them, or
/// Notification Centre.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Main,
    Power,
    /// Notification Centre, which `center` lays out and `main` hands touches.
    Notifications,
}

/// What the user changed, for the host to carry out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    Toggle(Toggle, bool),
    Slider(Slider, f32),
    Media(MediaAction),
    /// The shortcut at this index.
    Launch(usize),
    Power(PowerAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Toggle(Toggle),
    Slider(Slider),
    Media(MediaAction),
    Shortcut(usize),
    /// Opens the power page.
    PowerMenu,
    Power(PowerAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// A circle in the connectivity module, filled with its colour when on.
    Round,
    /// A square module that turns white when on.
    Tile,
    Slider,
    /// A glyph on the media module.
    Media,
    /// A labelled circle on the power page.
    Power,
    /// A bare glyph in the screen's corner.
    Corner,
}

#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub control: Control,
    pub rect: Rect,
    pub look: Look,
}

/// The module grid: 4 columns by 5 rows or more, sized to fit the screen either way up.
#[derive(Clone, Copy)]
pub struct Grid {
    pub x: f32,
    y: f32,
    pub cell: f32,
}

impl Grid {
    pub fn new(w: f32, h: f32, c: &Controls) -> Grid {
        let span = |n: f32| n + (n - 1.0) * GAP;
        let rows = (ROWS - 1 + last_rows(c).len().div_ceil(COLUMNS as usize)) as f32;
        let cell = (w * 0.84 / span(COLUMNS))
            .min((h - TOP - 16.0) / span(rows))
            .min(92.0);
        Grid {
            x: (w - cell * span(COLUMNS)) / 2.0,
            y: ((h - cell * span(rows)) * 0.35 - 20.0).max(TOP),
            cell,
        }
    }

    pub fn cell(&self, column: f32, row: f32, columns: f32, rows: f32) -> Rect {
        let pitch = self.cell * (1.0 + GAP);
        Rect {
            x: self.x + column * pitch,
            y: self.y + row * pitch,
            w: self.cell + (columns - 1.0) * pitch,
            h: self.cell + (rows - 1.0) * pitch,
        }
    }

    /// Where the open sheet's status row sits, down from the bar: its centre,
    /// just above the modules.
    pub fn status_y(&self) -> f32 {
        self.y - 20.0
    }

    /// The power glyph, where the bar has the battery: its ink ends at the
    /// modules' right edge.
    pub fn corner(&self, w: f32) -> Rect {
        Rect {
            x: w - self.x - 26.0,
            y: 4.0,
            w: 36.0,
            h: BAR_H,
        }
    }

    pub fn connectivity(&self) -> Rect {
        self.cell(0.0, 0.0, 2.0, 2.0)
    }

    pub fn media(&self) -> Rect {
        self.cell(2.0, 0.0, 2.0, 2.0)
    }

    /// The power page's one module.
    pub fn power(&self) -> Rect {
        self.cell(0.0, 0.5, 4.0, 3.0)
    }
}

/// The tiles under the toggles and sliders: the torch, the hotspot, then the
/// shortcuts.
fn last_rows(c: &Controls) -> Vec<Control> {
    let tile = |t: Toggle| t.get(c).map(|_| Control::Toggle(t));
    tile(Toggle::Torch)
        .into_iter()
        .chain(tile(Toggle::Hotspot))
        .chain((0..c.shortcuts).map(Control::Shortcut))
        .collect()
}

/// Where the controls there are go: connectivity and the player on top, then
/// toggles left of the sliders, then rows of the rest.
pub fn items(w: f32, h: f32, c: &Controls, page: Page) -> Vec<Item> {
    let grid = Grid::new(w, h, c);
    let mut items = Vec::new();
    if page == Page::Notifications {
        return items;
    }
    if page == Page::Power {
        let module = grid.power();
        let d = grid.cell * 0.95;
        let actions = [
            PowerAction::Sleep,
            PowerAction::LogOut,
            PowerAction::Restart,
            PowerAction::PowerOff,
        ];
        for (i, action) in actions.into_iter().enumerate() {
            items.push(Item {
                control: Control::Power(action),
                rect: Rect {
                    x: module.x + module.w * (0.28 + 0.44 * (i % 2) as f32) - d / 2.0,
                    y: module.y + module.h * (0.25 + 0.46 * (i / 2) as f32) - d / 2.0,
                    w: d,
                    h: d,
                },
                look: Look::Power,
            });
        }
        return items;
    }
    items.push(Item {
        control: Control::PowerMenu,
        rect: grid.corner(w),
        look: Look::Corner,
    });
    let round = [
        Toggle::Airplane,
        Toggle::Cellular,
        Toggle::Wifi,
        Toggle::Bluetooth,
    ];
    let module = grid.connectivity();
    let d = module.w * 0.36;
    for (i, toggle) in round.into_iter().filter(|t| t.get(c).is_some()).enumerate() {
        let slot = |origin: f32, i: usize| origin + module.w * (0.27 + 0.46 * i as f32) - d / 2.0;
        items.push(Item {
            control: Control::Toggle(toggle),
            rect: Rect {
                x: slot(module.x, i % 2),
                y: slot(module.y, i / 2),
                w: d,
                h: d,
            },
            look: Look::Round,
        });
    }
    let media = grid.media();
    let actions = [
        (-1.0, MediaAction::Previous),
        (0.0, MediaAction::PlayPause),
        (1.0, MediaAction::Next),
    ];
    for (i, action) in actions {
        let s = media.w * 0.26;
        let rect = Rect {
            x: media.center_x() + i * media.w * 0.3 - s / 2.0,
            y: media.y + media.h * 0.72 - s / 2.0,
            w: s,
            h: s,
        };
        items.push(Item {
            control: Control::Media(action),
            rect,
            look: Look::Media,
        });
    }
    let tile = |t: Toggle| t.get(c).map(|_| Control::Toggle(t));
    let tiles = [
        tile(Toggle::RotationLock),
        tile(Toggle::Silent),
        tile(Toggle::LowPower),
        tile(Toggle::DarkMode),
    ];
    for (i, control) in tiles.into_iter().flatten().enumerate() {
        items.push(Item {
            control,
            rect: grid.cell((i % 2) as f32, 2.0 + (i / 2) as f32, 1.0, 1.0),
            look: Look::Tile,
        });
    }
    let sliders = [Slider::Brightness, Slider::Volume];
    for (i, slider) in sliders
        .into_iter()
        .filter(|s| s.get(c).is_some())
        .enumerate()
    {
        items.push(Item {
            control: Control::Slider(slider),
            rect: grid.cell(2.0 + i as f32, 2.0, 1.0, 2.0),
            look: Look::Slider,
        });
    }
    for (i, control) in last_rows(c).into_iter().enumerate() {
        items.push(Item {
            control,
            rect: grid.cell((i % 4) as f32, (4 + i / 4) as f32, 1.0, 1.0),
            look: Look::Tile,
        });
    }
    items
}

enum Drag {
    /// On the bar, with the sheet still up: a tap opens it, a pull drags it.
    Bar {
        y: f32,
    },
    /// Not yet decided between a tap and a drag.
    Pending {
        at: (f32, f32),
        control: Option<Control>,
    },
    Sheet {
        y: f32,
        shown: f32,
    },
    Slider {
        slider: Slider,
        y: f32,
        value: f32,
        height: f32,
    },
}

pub struct Panel {
    /// 0 up out of sight, 1 covering the screen.
    shown: Spring,
    drag: Option<Drag>,
    pub pressed: Option<Control>,
    pub page: Page,
}

impl Panel {
    /// Up out of sight, with a finger on the bar at `y`.
    pub fn pulled(y: f32, page: Page) -> Panel {
        Panel {
            shown: Spring {
                stiffness: STIFFNESS,
                damping: DAMPING,
                ..Spring::new(0.0)
            },
            drag: Some(Drag::Bar { y }),
            pressed: None,
            page,
        }
    }

    pub fn shown(&self) -> f32 {
        self.shown.value.clamp(0.0, 1.0)
    }

    pub fn is_animating(&self) -> bool {
        !self.shown.is_settled()
    }

    /// How far the status row has come down from the bar, 0..1: it is there
    /// well before the sheet is.
    pub fn lowered(&self) -> f32 {
        (self.shown() / 0.6).min(1.0)
    }

    /// How far past the bottom the sheet has swung, as a share of the screen.
    pub fn overshoot(&self) -> f32 {
        (self.shown.value - 1.0).max(0.0)
    }

    /// Up out of sight: the bounce past the top is never seen, so not waited for.
    pub fn is_dismissed(&self) -> bool {
        self.drag.is_none() && self.shown.target <= 0.0 && self.shown.value <= 0.0
    }

    /// The slider under the finger, if it's dragging one.
    pub fn dragged_slider(&self) -> Option<Slider> {
        match self.drag {
            Some(Drag::Slider { slider, .. }) => Some(slider),
            _ => None,
        }
    }

    pub fn step(&mut self, dt: f32) {
        if matches!(self.drag, Some(Drag::Sheet { .. })) {
            return;
        }
        self.shown.step(dt);
        // The top has no bounce to show: it stops there.
        if self.shown.target <= 0.0 && self.shown.value <= 0.0 {
            self.shown.value = 0.0;
            self.shown.velocity = 0.0;
        }
    }

    /// The control a finger is resting on, not yet a tap or a drag.
    pub fn held(&self) -> Option<Control> {
        match self.drag {
            Some(Drag::Pending { control, .. }) => control,
            _ => None,
        }
    }

    pub fn close(&mut self) {
        self.drag = None;
        self.pressed = None;
        self.shown.retarget(0.0);
    }

    fn hold(&mut self, shown: f32) {
        self.shown.value = shown.clamp(0.0, 1.0);
        self.shown.target = self.shown.value;
        self.shown.velocity = 0.0;
    }

    pub fn touch_down(&mut self, at: (f32, f32), w: f32, h: f32, c: &Controls) {
        let grow = 4.0;
        let control = items(w, h, c, self.page)
            .into_iter()
            .find(|i| {
                let r = i.rect;
                at.0 >= r.x - grow
                    && at.0 < r.x + r.w + grow
                    && at.1 >= r.y - grow
                    && at.1 < r.y + r.h + grow
            })
            .map(|i| i.control);
        self.pressed = control.filter(|c| !matches!(c, Control::Slider(_)));
        self.drag = Some(Drag::Pending { at, control });
    }

    /// Returns a slider's new value while it is dragged.
    pub fn touch_move(&mut self, at: (f32, f32), w: f32, h: f32, c: &Controls) -> Option<Change> {
        match self.drag {
            Some(Drag::Bar { y }) if at.1 - y > TOUCH_SLOP => {
                self.drag = Some(Drag::Sheet { y: 0.0, shown: 0.0 });
                self.hold(at.1 / h);
                None
            }
            Some(Drag::Pending { at: start, control })
                if (at.0 - start.0).hypot(at.1 - start.1) > TOUCH_SLOP =>
            {
                self.pressed = None;
                self.drag = Some(match control {
                    Some(Control::Slider(slider)) => {
                        let height = items(w, h, c, self.page)
                            .into_iter()
                            .find(|i| i.control == Control::Slider(slider))?
                            .rect
                            .h;
                        let value = slider.get(c).unwrap_or(0.0);
                        Drag::Slider {
                            slider,
                            y: at.1,
                            value,
                            height,
                        }
                    }
                    _ => Drag::Sheet {
                        y: at.1,
                        shown: self.shown(),
                    },
                });
                None
            }
            Some(Drag::Sheet { y, shown }) => {
                self.hold(shown + (at.1 - y) / h);
                None
            }
            Some(Drag::Slider {
                slider,
                y,
                value,
                height,
            }) => {
                let value = (value + (y - at.1) / height).clamp(0.0, 1.0);
                Some(Change::Slider(slider, value))
            }
            _ => None,
        }
    }

    /// `velocity`: downward, px/s. Returns what a tap changed.
    pub fn touch_up(&mut self, velocity: f32, h: f32, c: &Controls) -> Option<Change> {
        self.pressed = None;
        match self.drag.take()? {
            Drag::Bar { .. } => {
                self.shown.retarget(1.0);
                None
            }
            Drag::Pending {
                control: Some(Control::Toggle(t)),
                ..
            } => t.get(c).map(|on| Change::Toggle(t, !on)),
            Drag::Pending {
                control: Some(Control::Media(a)),
                ..
            } => Some(Change::Media(a)),
            Drag::Pending {
                control: Some(Control::Shortcut(i)),
                ..
            } => Some(Change::Launch(i)),
            Drag::Pending {
                control: Some(Control::Power(a)),
                ..
            } => Some(Change::Power(a)),
            Drag::Pending {
                control: Some(Control::PowerMenu),
                ..
            } => {
                self.page = Page::Power;
                None
            }
            // A tap on nothing closes it, or leaves the power page.
            Drag::Pending { control: None, .. } => {
                match self.page {
                    Page::Main => self.shown.retarget(0.0),
                    Page::Power => self.page = Page::Main,
                    // Its cards are too easy to miss: only a swipe up closes it.
                    Page::Notifications => {}
                }
                None
            }
            Drag::Sheet { .. } => {
                let open = self.shown.value + velocity * PROJECT / h > 0.5;
                self.shown.velocity = velocity / h;
                self.shown.retarget(if open { 1.0 } else { 0.0 });
                None
            }
            _ => None,
        }
    }
}

/// Applies a change the user made, until the system reports what it made of it.
pub fn apply(c: &mut Controls, change: Change) {
    match change {
        Change::Toggle(toggle, on) => c.toggles[toggle as usize] = Some(on),
        Change::Slider(Slider::Brightness, v) => c.brightness = Some(v),
        Change::Slider(Slider::Volume, v) => c.volume = Some(v),
        Change::Media(MediaAction::PlayPause) => {
            if let Some(media) = &mut c.media {
                media.playing = !media.playing;
            }
        }
        Change::Media(_) | Change::Launch(_) | Change::Power(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 390.0;
    const H: f32 = 844.0;

    fn all() -> Controls {
        Controls {
            toggles: [Some(false); 10],
            brightness: Some(0.5),
            volume: Some(0.4),
            media: None,
            shortcuts: 0,
        }
    }

    fn settle(p: &mut Panel) {
        for _ in 0..240 {
            p.step(1.0 / 120.0);
        }
    }

    fn open() -> Panel {
        let mut p = Panel::pulled(5.0, Page::Main);
        p.touch_move((350.0, 500.0), W, H, &all());
        p.touch_up(0.0, H, &all());
        settle(&mut p);
        p
    }

    fn rect(c: &Controls, control: Control) -> Rect {
        items(W, H, c, Page::Main)
            .into_iter()
            .find(|i| i.control == control)
            .unwrap()
            .rect
    }

    fn tap(p: &mut Panel, c: &Controls, control: Control) -> Option<Change> {
        let r = rect(c, control);
        p.touch_down((r.center_x(), r.center_y()), W, H, c);
        p.touch_up(0.0, H, c)
    }

    #[test]
    fn the_sheet_follows_the_finger() {
        let mut p = Panel::pulled(5.0, Page::Main);
        p.touch_move((350.0, 211.0), W, H, &all());
        assert_eq!(p.shown(), 0.25);
    }

    #[test]
    fn pulls_open_and_a_tap_on_nothing_closes_it() {
        let mut p = open();
        assert!((p.shown() - 1.0).abs() < 0.01);
        p.touch_down((380.0, 830.0), W, H, &all());
        p.touch_up(0.0, H, &all());
        settle(&mut p);
        assert!(p.is_dismissed());
    }

    #[test]
    fn it_swings_past_the_bottom_and_comes_back() {
        let mut p = Panel::pulled(5.0, Page::Main);
        p.touch_up(0.0, H, &all());
        let mut furthest = 0.0f32;
        for _ in 0..240 {
            p.step(1.0 / 120.0);
            furthest = furthest.max(p.overshoot());
        }
        assert!(furthest > 0.01 && furthest < 0.05, "{furthest}");
        assert!(p.overshoot() < 0.001 && p.shown() > 0.999);
    }

    #[test]
    fn a_tap_on_the_bar_opens_it() {
        let mut p = Panel::pulled(5.0, Page::Main);
        p.touch_up(0.0, H, &all());
        settle(&mut p);
        assert!(!p.is_dismissed() && p.shown() > 0.99);
    }

    #[test]
    fn a_short_pull_falls_back_unless_flicked() {
        let mut p = Panel::pulled(5.0, Page::Main);
        p.touch_move((350.0, 100.0), W, H, &all());
        p.touch_up(0.0, H, &all());
        settle(&mut p);
        assert!(p.is_dismissed());

        let mut p = Panel::pulled(5.0, Page::Main);
        p.touch_move((350.0, 100.0), W, H, &all());
        p.touch_up(3000.0, H, &all());
        settle(&mut p);
        assert!(p.shown() > 0.99);
    }

    #[test]
    fn taps_toggle_and_drags_set_sliders() {
        let mut p = open();
        let c = all();
        assert_eq!(
            tap(&mut p, &c, Control::Toggle(Toggle::Wifi)),
            Some(Change::Toggle(Toggle::Wifi, true))
        );
        assert_eq!(
            tap(&mut p, &c, Control::Media(MediaAction::PlayPause)),
            Some(Change::Media(MediaAction::PlayPause))
        );

        let r = rect(&c, Control::Slider(Slider::Volume));
        let (x, y) = (r.center_x(), r.center_y());
        p.touch_down((x, y), W, H, &c);
        p.touch_move((x, y - 10.0), W, H, &c);
        let change = p.touch_move((x, y - 10.0 - r.h / 4.0), W, H, &c);
        p.touch_up(0.0, H, &c);
        let Some(Change::Slider(Slider::Volume, value)) = change else {
            panic!("{change:?}")
        };
        assert!((value - 0.65).abs() < 0.001, "{value}");
        assert!(p.shown() > 0.99, "dragging a slider doesn't move the sheet");
    }

    #[test]
    fn leaves_out_what_the_system_doesnt_report() {
        let mut c = Controls {
            volume: Some(0.5),
            ..Controls::default()
        };
        c.toggles[Toggle::Wifi as usize] = Some(true);
        let shown: Vec<Control> = items(W, H, &c, Page::Main)
            .into_iter()
            .filter(|i| i.look != Look::Media)
            .map(|i| i.control)
            .collect();
        assert_eq!(
            shown,
            [
                Control::PowerMenu,
                Control::Toggle(Toggle::Wifi),
                Control::Slider(Slider::Volume)
            ]
        );
    }

    #[test]
    fn a_resting_finger_holds_its_control_until_it_moves() {
        let mut p = open();
        let c = all();
        let r = rect(&c, Control::Toggle(Toggle::Wifi));
        p.touch_down((r.center_x(), r.center_y()), W, H, &c);
        assert_eq!(p.held(), Some(Control::Toggle(Toggle::Wifi)));
        p.touch_move((r.center_x(), r.center_y() + 30.0), W, H, &c);
        assert_eq!(p.held(), None);
        // Closed from under the finger, as a long press does: the lift is no tap.
        p.touch_down((r.center_x(), r.center_y()), W, H, &c);
        p.close();
        assert_eq!(p.touch_up(0.0, H, &c), None);
    }

    #[test]
    fn the_power_glyph_opens_a_page_that_a_tap_outside_leaves() {
        let mut p = open();
        let c = all();
        assert_eq!(tap(&mut p, &c, Control::PowerMenu), None);
        assert_eq!(p.page, Page::Power);
        let r = items(W, H, &c, Page::Power)
            .into_iter()
            .find(|i| i.control == Control::Power(PowerAction::Restart))
            .unwrap()
            .rect;
        p.touch_down((r.center_x(), r.center_y()), W, H, &c);
        assert_eq!(
            p.touch_up(0.0, H, &c),
            Some(Change::Power(PowerAction::Restart))
        );
        p.touch_down((5.0, H - 5.0), W, H, &c);
        p.touch_up(0.0, H, &c);
        assert_eq!(p.page, Page::Main);
        assert!(!p.is_animating(), "back to the controls, not closed");
    }

    #[test]
    fn the_status_row_is_down_before_the_sheet_is() {
        let mut p = Panel::pulled(5.0, Page::Main);
        assert_eq!(p.lowered(), 0.0);
        p.touch_move((350.0, H * 0.3), W, H, &all());
        assert_eq!(p.lowered(), 0.5);
        p.touch_move((350.0, H * 0.7), W, H, &all());
        assert_eq!(p.lowered(), 1.0);
    }

    #[test]
    fn notification_center_has_no_controls_and_only_a_swipe_closes_it() {
        let mut p = Panel::pulled(5.0, Page::Notifications);
        let c = all();
        p.touch_up(0.0, H, &c);
        settle(&mut p);
        assert!(items(W, H, &c, p.page).is_empty());
        p.touch_down((200.0, 400.0), W, H, &c);
        p.touch_up(0.0, H, &c);
        settle(&mut p);
        assert!(!p.is_dismissed(), "a tap on nothing leaves it open");
        p.touch_down((200.0, 800.0), W, H, &c);
        p.touch_move((200.0, 700.0), W, H, &c);
        p.touch_move((200.0, 500.0), W, H, &c);
        p.touch_up(-2000.0, H, &c);
        settle(&mut p);
        assert!(p.is_dismissed());
    }

    #[test]
    fn shortcuts_follow_the_torch_and_wrap() {
        let c = Controls {
            shortcuts: 5,
            ..all()
        };
        let last = rect(&c, Control::Shortcut(4));
        let first = rect(&c, Control::Toggle(Toggle::Torch));
        assert!(
            last.y > first.y && last.x > first.x,
            "{last:?} wraps under {first:?}"
        );
        let mut p = open();
        assert_eq!(
            tap(&mut p, &c, Control::Shortcut(4)),
            Some(Change::Launch(4))
        );
    }

    #[test]
    fn modules_fit_the_screen_either_way_up() {
        let all = Controls {
            shortcuts: 7,
            ..all()
        };
        for (w, h) in [(W, H), (H, W), (281.0, 456.0)] {
            for i in [Page::Main, Page::Power]
                .into_iter()
                .flat_map(|page| items(w, h, &all, page))
                .filter(|i| i.look != Look::Corner)
            {
                let r = i.rect;
                assert!(
                    r.x >= 0.0 && r.x + r.w <= w && r.y >= TOP && r.y + r.h <= h,
                    "{:?} at {w}x{h}",
                    i.control
                );
            }
        }
    }
}
