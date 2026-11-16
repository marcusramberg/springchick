//! Notification Center, like iOS 18's: what apps sent, newest first, an app's
//! notifications stacked until its stack is tapped. A tap on a card opens it,
//! a swipe aside clears it, and the header's button clears them all. A new
//! one drops in as a banner while the sheet is up. Pure: `notify` receives
//! them, `main` tells the apps what became of them.

use std::collections::HashMap;

use sc_anim::Spring;
use sc_layout::Rect;

/// Sizes in points on a 402 pt wide screen, measured on iOS.
const REFERENCE_WIDTH: f32 = 402.0;
/// Wider screens than this keep its layout, centred.
const MAX_WIDTH: f32 = 430.0;
const MARGIN: f32 = 14.5;
pub const HEIGHT: f32 = 64.0;
pub const RADIUS: f32 = 22.0;
const SPACING: f32 = 10.0;
/// What a card grows by for its action buttons, and how many it shows.
const ACTIONS: f32 = 42.0;
const MAX_BUTTONS: usize = 3;
/// The action apps listen for when their notification itself is tapped.
pub const DEFAULT_ACTION: &str = "default";
/// Older notifications of a stack peek out below its card, each this much
/// lower and narrower on either side.
pub const PEEK: f32 = 11.0;
pub const PEEK_INSET: f32 = 14.0;
const PEEKS: usize = 2;
/// The header's height above the first card, and its clear button's diameter.
const HEADER: f32 = 50.0;
const CLEAR_BUTTON: f32 = 30.0;
/// The list's end stays this far above the screen's bottom.
const LIST_END: f32 = 70.0;
/// Above the banner's card, and below it for its shadow.
const BANNER_TOP: f32 = 6.0;
const BANNER_BELOW: f32 = 22.0;
/// How long a banner stays, in seconds.
pub const BANNER_SHOWN: f64 = 5.0;
const TOUCH_SLOP: f32 = 8.0;
/// Seconds of release velocity a flick is worth.
const PROJECT: f32 = 0.3;
const MAX_KEPT: usize = 200;
/// The header's key among the rows' places; a card's is its first id.
const HEADER_KEY: u32 = u32::MAX;

fn scale(w: f32) -> f32 {
    w.min(MAX_WIDTH) / REFERENCE_WIDTH
}

/// A spring that takes about `response` seconds, as iOS names them.
fn spring(value: f32, response: f32, ratio: f32) -> Spring {
    let w = std::f32::consts::TAU / response;
    Spring {
        stiffness: w * w,
        damping: 2.0 * ratio * w,
        ..Spring::new(value)
    }
}

fn retune(s: &mut Spring, response: f32, ratio: f32, target: f32) {
    let tuned = spring(0.0, response, ratio);
    (s.stiffness, s.damping, s.target) = (tuned.stiffness, tuned.damping, target);
}

/// Gives less the further `x` goes past the end of a range.
fn rubber_band(x: f32, range: f32) -> f32 {
    (1.0 - 1.0 / (x * 0.55 / range + 1.0)) * range
}

/// A notification as an app sent it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Notification {
    /// A notification with the id of one shown replaces it.
    pub id: u32,
    /// The app's desktop entry id, if it said: whose icon it shows and which
    /// a tap opens.
    pub app: Option<String>,
    pub app_name: String,
    /// An icon name or path, for one without a desktop entry.
    pub icon: String,
    pub summary: String,
    pub body: String,
    /// Action keys, with their labels.
    pub actions: Vec<(String, String)>,
    /// Shown as a banner without staying in the list.
    pub transient: bool,
    /// Unix seconds.
    pub received: u64,
}

impl Notification {
    /// The actions that get a button: the default one is the card itself.
    pub fn buttons(&self) -> impl Iterator<Item = &(String, String)> {
        self.actions
            .iter()
            .filter(|(key, _)| key != DEFAULT_ACTION)
            .take(MAX_BUTTONS)
    }
}

/// A card's height in points, with `buttons` on it.
pub fn card_height(buttons: usize) -> f32 {
    HEIGHT + if buttons > 0 { ACTIONS } else { 0.0 }
}

/// Where a card's `count` buttons are, left to right.
pub fn button_rects(card: Rect, count: usize) -> Vec<Rect> {
    let k = card.h / card_height(count);
    let (inset, gap) = (13.5 * k, 8.0 * k);
    let w = (card.w - 2.0 * inset - gap * count.saturating_sub(1) as f32) / count.max(1) as f32;
    (0..count)
        .map(|i| Rect {
            x: card.x + inset + (w + gap) * i as f32,
            y: card.y + (HEIGHT - 2.0) * k,
            w,
            h: 34.0 * k,
        })
        .collect()
}

/// What the user did to a notification, for the host to tell its app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Opened,
    Cleared,
}

pub struct Entry {
    pub notification: Notification,
    /// Sideways, while a finger swipes it or it slides out.
    offset: Spring,
    /// Swiped out: removed once it's off screen.
    leaving: bool,
    /// Grows in from 0.
    appear: Spring,
}

struct Banner {
    id: u32,
    /// 0 above the screen, 1 in place.
    shown: Spring,
    /// How far a finger moved it up; it springs back from there.
    lift: Spring,
}

/// Something in the list, at its place on the sheet.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// A notification, or a stack of one app's (newest first) if `stacked`.
    Card {
        ids: Vec<u32>,
        stacked: bool,
        /// How many action buttons it shows; none while stacked.
        buttons: usize,
        rect: Rect,
    },
    /// The title, with the button that clears the list.
    Header { rect: Rect },
}

impl Row {
    pub fn rect(&self) -> Rect {
        match self {
            Row::Card { rect, .. } | Row::Header { rect } => *rect,
        }
    }

    fn key(&self) -> u32 {
        match self {
            Row::Card { ids, .. } => ids[0],
            Row::Header { .. } => HEADER_KEY,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Gesture {
    Pending,
    Scroll,
    Swipe,
}

enum Touch {
    Banner {
        start: (f32, f32),
        moved: bool,
    },
    List {
        start: (f32, f32),
        start_scroll: f32,
        row: Option<Row>,
        gesture: Gesture,
    },
}

/// Where the list is on the sheet: its top, and the sheet's size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub top: f32,
    pub w: f32,
    pub h: f32,
}

impl View {
    /// Points to px: the reference phone's, and no bigger on a wider screen,
    /// where the cards keep a phone's width in the middle.
    pub fn k(&self) -> f32 {
        scale(self.w)
    }

    fn card(&self, y: f32) -> Rect {
        let k = self.k();
        let w = self.w.min(MAX_WIDTH) - 2.0 * MARGIN * k;
        Rect {
            x: (self.w - w) / 2.0,
            y,
            w,
            h: HEIGHT * k,
        }
    }
}

#[derive(Default)]
pub struct Center {
    /// Newest first.
    entries: Vec<Entry>,
    banner: Option<Banner>,
    touch: Option<Touch>,
    /// The apps whose stacks are spread out.
    expanded: Vec<String>,
    /// How far the list is scrolled up.
    scroll: Option<Spring>,
    outcomes: Vec<(u32, Outcome)>,
    /// Rows on the way to their places, unscrolled, by key.
    placed: HashMap<u32, Spring>,
}

impl Center {
    /// Adds a notification, or replaces the one with its id; `banner` also
    /// drops it in as one.
    pub fn add(&mut self, notification: Notification, banner: bool) {
        let id = notification.id;
        // A transient one lives only as a banner.
        if notification.transient && !banner {
            return self.remove(id);
        }
        if banner {
            let covered = self.banner.as_ref().map(|b| b.id).filter(|&b| b != id);
            self.entries
                .retain(|e| !(Some(e.notification.id) == covered && e.notification.transient));
        }
        let mut appear = spring(0.0, 0.5, 0.8);
        appear.target = 1.0;
        let mut entry = Entry {
            notification,
            offset: Spring::new(0.0),
            leaving: false,
            appear,
        };
        match self.entries.iter().position(|e| e.notification.id == id) {
            Some(i) => {
                entry.appear = self.entries[i].appear;
                self.entries[i] = entry;
            }
            None => self.entries.insert(0, entry),
        }
        // An app in a loop must not fill the memory: the oldest go unseen.
        self.entries.truncate(MAX_KEPT);
        if banner {
            let mut shown = self
                .banner
                .as_ref()
                .map_or(spring(0.0, 0.45, 0.82), |b| b.shown);
            shown.target = 1.0;
            let lift = self.banner.as_ref().map_or(Spring::new(0.0), |b| b.lift);
            self.banner = Some(Banner { id, shown, lift });
        }
    }

    /// The app took one back.
    pub fn remove(&mut self, id: u32) {
        self.entries.retain(|e| e.notification.id != id);
        if self.banner.as_ref().is_some_and(|b| b.id == id) {
            self.dismiss_banner();
        }
    }

    /// The sheet went away: next time it starts over, everything in its place.
    pub fn rest(&mut self) {
        self.expanded.clear();
        self.scroll = None;
        self.placed.clear();
        self.touch = None;
        for e in &mut self.entries {
            e.appear.value = 1.0;
            e.appear.velocity = 0.0;
        }
    }

    pub fn take_outcomes(&mut self) -> Vec<(u32, Outcome)> {
        std::mem::take(&mut self.outcomes)
    }

    pub fn entry(&self, id: u32) -> Option<&Entry> {
        self.entries.iter().find(|e| e.notification.id == id)
    }

    pub fn notifications(&self) -> impl Iterator<Item = &Notification> {
        self.entries.iter().map(|e| &e.notification)
    }

    fn listed(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| !e.notification.transient)
    }

    /// Whether the list has anything in it.
    pub fn any(&self) -> bool {
        self.listed().next().is_some()
    }

    fn scroll(&self) -> f32 {
        self.scroll.map_or(0.0, |s| s.value)
    }

    /// The list is moving, or under a finger.
    pub fn is_animating(&self) -> bool {
        matches!(self.touch, Some(Touch::List { .. }))
            || self.scroll.is_some_and(|s| !s.is_settled())
            || self
                .entries
                .iter()
                .any(|e| !e.offset.is_settled() || !e.appear.is_settled())
            || self.placed.values().any(|p| !p.is_settled())
    }

    /// Animates the list, whose rows move to their places while it shows.
    pub fn step(&mut self, dt: f32, view: Option<View>) {
        for entry in &mut self.entries {
            entry.offset.step(dt);
            entry.appear.step(dt);
        }
        self.entries
            .retain(|e| !(e.leaving && e.offset.is_settled()));
        if let Some(scroll) = &mut self.scroll {
            scroll.step(dt);
        }
        let Some(view) = view else {
            self.placed.clear();
            return;
        };
        let rows = self.layout(view);
        for row in &rows {
            let y = row.rect().y;
            let placed = self
                .placed
                .entry(row.key())
                .or_insert_with(|| spring(y, 0.45, 0.88));
            placed.target = y;
            placed.step(dt);
        }
        self.placed
            .retain(|key, _| rows.iter().any(|r| r.key() == *key));
    }

    /// The list from the view's top, unscrolled: the header, then the cards
    /// by app, each app's newest first, stacked unless spread out.
    fn layout(&self, view: View) -> Vec<Row> {
        let k = view.k();
        let mut rows = Vec::new();
        if !self.any() {
            return rows;
        }
        let mut y = view.top;
        rows.push(Row::Header {
            rect: Rect {
                h: HEADER * k,
                ..view.card(y)
            },
        });
        y += HEADER * k;
        let mut groups: Vec<Vec<&Entry>> = Vec::new();
        for entry in self.listed() {
            let app = &entry.notification.app;
            match groups
                .iter_mut()
                .find(|g| app.is_some() && g[0].notification.app == *app)
            {
                Some(group) => group.push(entry),
                None => groups.push(vec![entry]),
            }
        }
        for group in groups {
            let app = &group[0].notification.app;
            let spread = app.as_ref().is_some_and(|a| self.expanded.contains(a));
            let stacked = group.len() > 1 && !spread;
            let members = if stacked { &group[..1] } else { &group[..] };
            for entry in members {
                let ids = if stacked {
                    group.iter().map(|e| e.notification.id).collect()
                } else {
                    vec![entry.notification.id]
                };
                let buttons = if stacked {
                    0
                } else {
                    entry.notification.buttons().count()
                };
                let mut rect = view.card(y);
                rect.x += entry.offset.value;
                rect.h = card_height(buttons) * k;
                y += rect.h + SPACING * k;
                if stacked {
                    y += PEEK * k * (group.len() - 1).min(PEEKS) as f32;
                }
                rows.push(Row::Card {
                    ids,
                    stacked,
                    buttons,
                    rect,
                });
            }
        }
        rows
    }

    /// The rows as drawn: scrolled, on their way to their places, new ones
    /// growing in.
    pub fn rows(&self, view: View) -> Vec<Row> {
        let scroll = self.scroll();
        self.layout(view)
            .into_iter()
            .map(|mut row| {
                let y = self
                    .placed
                    .get(&row.key())
                    .map_or(row.rect().y, |p| p.value)
                    - scroll;
                let appear = match &row {
                    Row::Card { ids, .. } => self.entry(ids[0]).map_or(1.0, |e| e.appear.value),
                    Row::Header { .. } => 1.0,
                };
                let (Row::Card { rect, .. } | Row::Header { rect }) = &mut row;
                rect.y = y;
                let s = 0.9 + 0.1 * appear.clamp(0.0, 1.0);
                *rect = Rect {
                    x: rect.x + rect.w * (1.0 - s) / 2.0,
                    y: rect.y + rect.h * (1.0 - s) / 2.0,
                    w: rect.w * s,
                    h: rect.h * s,
                };
                row
            })
            .collect()
    }

    /// How see-through a card is: it fades in as it arrives, and out as it's
    /// swiped away.
    pub fn fade(&self, id: u32, view: View) -> f32 {
        let Some(e) = self.entry(id) else { return 1.0 };
        let gone = ((e.offset.value.abs() - view.w * 0.3) / (view.w * 0.6)).clamp(0.0, 1.0);
        e.appear.value.clamp(0.0, 1.0) * (1.0 - gone)
    }

    /// How far the list scrolls: until its end comes above the sheet's bottom.
    fn max_scroll(&self, view: View) -> f32 {
        let end = self.layout(view).last().map_or(0.0, |row| {
            let r = row.rect();
            r.y + r.h
        });
        (end - (view.h - LIST_END * view.k())).max(0.0)
    }

    /// A finger going down on the list. Whether the list takes it: it is on a
    /// row, or there is something to scroll.
    pub fn touch_list(&mut self, p: (f32, f32), view: View) -> bool {
        let k = view.k();
        let row = self.rows(view).into_iter().find(|row| match row {
            Row::Card {
                rect, stacked, ids, ..
            } => {
                let peeks = if *stacked {
                    (ids.len() - 1).min(PEEKS)
                } else {
                    0
                };
                let r = Rect {
                    h: rect.h + PEEK * k * peeks as f32,
                    ..*rect
                };
                r.contains(p.0, p.1)
            }
            Row::Header { rect } => {
                let b = clear_button(*rect, view);
                Rect {
                    x: b.x - 8.0,
                    y: b.y - 8.0,
                    w: b.w + 16.0,
                    h: b.h + 16.0,
                }
                .contains(p.0, p.1)
            }
        });
        if row.is_none() && self.max_scroll(view) <= 0.0 {
            return false;
        }
        let scroll = self.scroll();
        self.scroll = Some(Spring::new(scroll));
        self.touch = Some(Touch::List {
            start: p,
            start_scroll: scroll,
            row,
            gesture: Gesture::Pending,
        });
        true
    }

    /// The compositor took the finger away.
    pub fn cancel_touch(&mut self) {
        self.touch = None;
    }

    pub fn is_touched(&self) -> bool {
        self.touch.is_some()
    }

    pub fn touch_move(&mut self, p: (f32, f32), view: View) {
        let max_scroll = self.max_scroll(view);
        match &mut self.touch {
            Some(Touch::Banner { start, moved }) => {
                *moved |= (p.0 - start.0).hypot(p.1 - start.1) > TOUCH_SLOP;
                if let Some(banner) = &mut self.banner {
                    // Up follows the finger; down only gives a little.
                    let dy = start.1 - p.1;
                    let lift = if dy > 0.0 {
                        dy
                    } else {
                        -rubber_band(-dy, 200.0)
                    };
                    banner.lift = Spring::new(lift);
                }
            }
            Some(Touch::List {
                start,
                start_scroll,
                row,
                gesture,
            }) => {
                let (dx, dy) = (p.0 - start.0, p.1 - start.1);
                if *gesture == Gesture::Pending && dx.hypot(dy) > TOUCH_SLOP {
                    let card = matches!(row, Some(Row::Card { .. }));
                    *gesture = if card && dx.abs() > dy.abs() {
                        Gesture::Swipe
                    } else {
                        Gesture::Scroll
                    };
                }
                match gesture {
                    Gesture::Scroll => {
                        let scroll = *start_scroll - dy;
                        let scroll = if scroll < 0.0 {
                            -rubber_band(-scroll, view.h)
                        } else if scroll > max_scroll {
                            max_scroll + rubber_band(scroll - max_scroll, view.h)
                        } else {
                            scroll
                        };
                        self.scroll = Some(Spring::new(scroll));
                    }
                    Gesture::Swipe => {
                        let Some(Row::Card { ids, .. }) = row else {
                            return;
                        };
                        for entry in self
                            .entries
                            .iter_mut()
                            .filter(|e| ids.contains(&e.notification.id))
                        {
                            entry.offset = Spring::new(dx);
                        }
                    }
                    Gesture::Pending => {}
                }
            }
            None => {}
        }
    }

    /// The finger lifted with `velocity` (px/s); returns the notification it
    /// tapped, to [`Self::open`], and the key of the action button if it was
    /// on one.
    pub fn touch_up(&mut self, velocity: (f32, f32), view: View) -> Option<(u32, Option<String>)> {
        match self.touch.take()? {
            Touch::Banner { moved, .. } => {
                let banner = self.banner.as_mut()?;
                let id = banner.id;
                if !moved {
                    self.banner = None;
                    return Some((id, None));
                }
                if banner.lift.value - velocity.1 * PROJECT > 30.0 {
                    self.dismiss_banner();
                } else {
                    // Back into place, carrying the finger's speed.
                    banner.lift.velocity = -velocity.1;
                    retune(&mut banner.lift, 0.4, 0.7, 0.0);
                }
                None
            }
            Touch::List {
                row,
                gesture,
                start,
                ..
            } => match (gesture, row) {
                (Gesture::Scroll, _) => {
                    let max = self.max_scroll(view);
                    let scroll = self.scroll.get_or_insert_with(|| Spring::new(0.0));
                    let target = (scroll.value - velocity.1 * PROJECT).clamp(0.0, max);
                    scroll.velocity = -velocity.1;
                    retune(scroll, 0.45, 1.0, target);
                    None
                }
                (Gesture::Swipe, Some(Row::Card { ids, .. })) => {
                    let offset = self.entry(ids[0]).map_or(0.0, |e| e.offset.value);
                    let travel = offset + velocity.0 * PROJECT;
                    let clear = travel.abs() > view.w / 2.0;
                    for entry in self
                        .entries
                        .iter_mut()
                        .filter(|e| ids.contains(&e.notification.id))
                    {
                        entry.offset.velocity = velocity.0;
                        let to = if clear { view.w * travel.signum() } else { 0.0 };
                        retune(&mut entry.offset, 0.3, 1.0, to);
                        entry.leaving = clear;
                    }
                    if clear {
                        self.outcomes
                            .extend(ids.iter().map(|&id| (id, Outcome::Cleared)));
                    }
                    None
                }
                // A tap on a stack spreads it out.
                (
                    Gesture::Pending,
                    Some(Row::Card {
                        ids, stacked: true, ..
                    }),
                ) => {
                    if let Some(app) = self.entry(ids[0]).and_then(|e| e.notification.app.clone()) {
                        self.expanded.push(app);
                    }
                    None
                }
                (
                    Gesture::Pending,
                    Some(Row::Card {
                        ids, buttons, rect, ..
                    }),
                ) => {
                    let button = button_rects(rect, buttons)
                        .iter()
                        .position(|b| b.contains(start.0, start.1));
                    let key = button.and_then(|i| {
                        let (key, _) = self.entry(ids[0])?.notification.buttons().nth(i)?;
                        Some(key.clone())
                    });
                    Some((ids[0], key))
                }
                (Gesture::Pending, Some(Row::Header { .. })) => {
                    self.clear_all(view.w);
                    None
                }
                _ => None,
            },
        }
    }

    fn clear_all(&mut self, w: f32) {
        for entry in self
            .entries
            .iter_mut()
            .filter(|e| !e.notification.transient && !e.leaving)
        {
            retune(&mut entry.offset, 0.3, 1.0, -w);
            entry.leaving = true;
            self.outcomes
                .push((entry.notification.id, Outcome::Cleared));
        }
        if let Some(scroll) = &mut self.scroll {
            retune(scroll, 0.45, 1.0, 0.0);
        }
    }

    /// Takes a tapped notification out of the list, for its app to open.
    pub fn open(&mut self, id: u32) -> Option<Notification> {
        let i = self.entries.iter().position(|e| e.notification.id == id)?;
        self.outcomes.push((id, Outcome::Opened));
        if self.banner.as_ref().is_some_and(|b| b.id == id) {
            self.banner = None;
        }
        Some(self.entries.remove(i).notification)
    }

    /// The banner surface's height for a screen `w` wide.
    pub fn banner_height(w: f32) -> f32 {
        (BANNER_TOP + HEIGHT + BANNER_BELOW) * scale(w)
    }

    /// The notification shown as a banner, and its card on the banner's
    /// surface.
    pub fn banner(&self, w: f32) -> Option<(&Notification, Rect)> {
        let banner = self.banner.as_ref()?;
        let k = scale(w);
        let view = View {
            top: 0.0,
            w,
            h: 0.0,
        };
        let mut rect = view.card(BANNER_TOP * k);
        rect.y -= (rect.y + rect.h + 10.0 * k) * (1.0 - banner.shown.value) + banner.lift.value;
        Some((&self.entry(banner.id)?.notification, rect))
    }

    /// The banner has gone back up: its surface can go.
    pub fn banner_gone(&self) -> bool {
        self.banner.is_none()
    }

    pub fn banner_animating(&self) -> bool {
        self.banner
            .as_ref()
            .is_some_and(|b| !b.shown.is_settled() || !b.lift.is_settled() || b.shown.target <= 0.0)
    }

    /// Sends the banner back up.
    pub fn dismiss_banner(&mut self) {
        if let Some(banner) = &mut self.banner {
            banner.shown.target = 0.0;
        }
    }

    pub fn step_banner(&mut self, dt: f32) {
        let Some(banner) = &mut self.banner else {
            return;
        };
        banner.shown.step(dt);
        banner.lift.step(dt);
        if banner.shown.target <= 0.0 && banner.shown.value <= 0.01 {
            let id = banner.id;
            self.banner = None;
            // A transient one is gone with its banner.
            self.entries
                .retain(|e| !(e.notification.id == id && e.notification.transient));
        }
    }

    /// Whether a finger going down at `p` is on the banner; it takes the
    /// touch then.
    pub fn touch_banner(&mut self, p: (f32, f32), w: f32) -> bool {
        let on = self.banner.as_ref().is_some_and(|b| b.shown.target > 0.0)
            && self.banner(w).is_some_and(|(_, r)| r.contains(p.0, p.1));
        if on {
            self.touch = Some(Touch::Banner {
                start: p,
                moved: false,
            });
        }
        on
    }
}

/// The ✕ at the header's right.
pub fn clear_button(header: Rect, view: View) -> Rect {
    let d = CLEAR_BUTTON * view.k();
    Rect {
        x: header.x + header.w - d - 4.0 * view.k(),
        y: header.y + (header.h - d) / 2.0,
        w: d,
        h: d,
    }
}

/// When it came, like iOS says it.
pub fn ago(received: u64, now: u64) -> String {
    match now.saturating_sub(received) {
        0..60 => "now".into(),
        s @ 60..3600 => format!("{}m ago", s / 60),
        s @ 3600..86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: View = View {
        top: 250.0,
        w: 402.0,
        h: 874.0,
    };

    fn notification(id: u32, app: &str) -> Notification {
        Notification {
            id,
            app: Some(app.into()),
            app_name: "Chats".into(),
            summary: format!("Message {id}"),
            body: "Hi".into(),
            ..Notification::default()
        }
    }

    fn settle(c: &mut Center) {
        for _ in 0..240 {
            c.step(1.0 / 120.0, Some(VIEW));
            c.step_banner(1.0 / 120.0);
        }
    }

    fn cards(c: &Center) -> Vec<(Vec<u32>, bool)> {
        c.rows(VIEW)
            .into_iter()
            .filter_map(|row| match row {
                Row::Card { ids, stacked, .. } => Some((ids, stacked)),
                Row::Header { .. } => None,
            })
            .collect()
    }

    fn first_card(c: &Center) -> Rect {
        c.rows(VIEW)
            .into_iter()
            .find(|r| matches!(r, Row::Card { .. }))
            .unwrap()
            .rect()
    }

    fn tap(c: &mut Center, r: Rect) -> Option<(u32, Option<String>)> {
        assert!(c.touch_list((r.center_x(), r.center_y()), VIEW));
        c.touch_up((0.0, 0.0), VIEW)
    }

    #[test]
    fn an_apps_notifications_stack_until_tapped() {
        let mut c = Center::default();
        c.add(notification(1, "a"), false);
        c.add(notification(2, "b"), false);
        c.add(notification(3, "a"), false);
        c.add(
            Notification {
                summary: "Edited".into(),
                ..notification(2, "b")
            },
            false,
        );
        settle(&mut c);
        assert_eq!(cards(&c), [(vec![3, 1], true), (vec![2], false)]);
        assert_eq!(c.entry(2).unwrap().notification.summary, "Edited");

        let stack = first_card(&c);
        assert_eq!(tap(&mut c, stack), None, "tapping a stack spreads it out");
        settle(&mut c);
        assert_eq!(
            cards(&c),
            [(vec![3], false), (vec![1], false), (vec![2], false)]
        );
        let card = first_card(&c);
        assert_eq!(tap(&mut c, card), Some((3, None)));
        assert_eq!(c.open(3).unwrap().id, 3);
        assert_eq!(c.take_outcomes(), [(3, Outcome::Opened)]);
    }

    #[test]
    fn swiping_a_card_clears_it_and_the_header_clears_the_rest() {
        let mut c = Center::default();
        c.add(notification(1, "a"), false);
        c.add(notification(2, "b"), false);
        settle(&mut c);
        let card = first_card(&c);
        let at = (card.center_x(), card.center_y());
        assert!(c.touch_list(at, VIEW));
        c.touch_move((at.0 - 150.0, at.1), VIEW);
        c.touch_up((-800.0, 0.0), VIEW);
        assert_eq!(c.take_outcomes(), [(2, Outcome::Cleared)]);
        settle(&mut c);
        assert_eq!(cards(&c), [(vec![1], false)]);

        let header = c.rows(VIEW)[0].rect();
        tap(&mut c, clear_button(header, VIEW));
        assert_eq!(c.take_outcomes(), [(1, Outcome::Cleared)]);
        settle(&mut c);
        assert!(!c.any() && !c.is_animating());
    }

    #[test]
    fn a_long_list_scrolls_and_a_short_one_leaves_the_touch_alone() {
        let mut c = Center::default();
        c.add(notification(1, "a"), false);
        settle(&mut c);
        assert!(
            !c.touch_list((200.0, 800.0), VIEW),
            "nothing there, nothing to scroll"
        );

        for id in 2..14 {
            c.add(notification(id, &format!("app{id}")), false);
        }
        settle(&mut c);
        let before = first_card(&c).y;
        assert!(c.touch_list((200.0, 800.0), VIEW));
        c.touch_move((200.0, 500.0), VIEW);
        c.touch_up((0.0, 0.0), VIEW);
        settle(&mut c);
        assert!(first_card(&c).y < before - 100.0);

        // Flung past the end, it comes back to it.
        assert!(c.touch_list((200.0, 800.0), VIEW));
        c.touch_move((200.0, -3000.0), VIEW);
        c.touch_up((0.0, 0.0), VIEW);
        settle(&mut c);
        let last = c.rows(VIEW).last().unwrap().rect();
        assert!(
            (last.y + last.h - (VIEW.h - LIST_END)).abs() < 1.0,
            "the end rests above the bottom: {last:?}"
        );
    }

    #[test]
    fn a_banner_drops_in_goes_when_dismissed_and_takes_a_transient_one_along() {
        let mut c = Center::default();
        c.add(
            Notification {
                transient: true,
                ..notification(1, "a")
            },
            true,
        );
        assert!(!c.any(), "a transient one is not listed");
        settle(&mut c);
        let (_, rect) = c.banner(402.0).unwrap();
        assert!(rect.y > 0.0 && !c.banner_animating());
        c.dismiss_banner();
        settle(&mut c);
        assert!(c.banner_gone() && c.entry(1).is_none());
    }

    #[test]
    fn a_transient_one_is_kept_only_while_it_is_the_banner() {
        let transient = |id| Notification {
            transient: true,
            ..notification(id, "a")
        };
        let mut c = Center::default();
        c.add(transient(1), false);
        assert!(c.entry(1).is_none(), "no banner to live in");
        c.add(transient(2), true);
        c.add(notification(3, "b"), true);
        assert!(c.entry(2).is_none(), "its banner was taken over");
        assert!(c.entry(3).is_some());
    }

    #[test]
    fn a_card_with_actions_is_taller_and_its_buttons_answer_with_their_keys() {
        let mut c = Center::default();
        c.add(notification(1, "a"), false);
        let plain = first_card(&c);
        let mut c = Center::default();
        c.add(
            Notification {
                actions: [
                    ("default", "Open"),
                    ("reply", "Reply"),
                    ("read", "Mark read"),
                ]
                .map(|(k, l)| (k.to_string(), l.to_string()))
                .to_vec(),
                ..notification(1, "a")
            },
            false,
        );
        settle(&mut c);
        let card = first_card(&c);
        assert!(card.h > plain.h + 30.0);
        let buttons = button_rects(card, 2);
        assert!(buttons[1].x > buttons[0].x + buttons[0].w);
        assert!(buttons[1].y + buttons[1].h < card.y + card.h);
        assert_eq!(tap(&mut c, buttons[1]), Some((1, Some("read".to_string()))));
        // Above the buttons it is the card: the default action.
        assert!(c.touch_list((card.center_x(), card.y + 20.0), VIEW));
        assert_eq!(c.touch_up((0.0, 0.0), VIEW), Some((1, None)));
    }

    #[test]
    fn a_tap_on_the_banner_opens_it_and_a_flick_up_puts_it_away() {
        let mut c = Center::default();
        c.add(notification(1, "a"), true);
        settle(&mut c);
        let (_, r) = c.banner(402.0).unwrap();
        let at = (r.center_x(), r.center_y());
        assert!(c.touch_banner(at, 402.0));
        assert_eq!(c.touch_up((0.0, 0.0), VIEW), Some((1, None)));

        c.add(notification(2, "a"), true);
        settle(&mut c);
        assert!(c.touch_banner(at, 402.0));
        c.touch_move((at.0, at.1 - 40.0), VIEW);
        assert_eq!(c.touch_up((0.0, -500.0), VIEW), None);
        settle(&mut c);
        assert!(
            c.banner_gone() && c.entry(2).is_some(),
            "put away, still listed"
        );
    }

    #[test]
    fn times_read_like_ios() {
        assert_eq!(ago(100, 130), "now");
        assert_eq!(ago(100, 100 + 45 * 60), "45m ago");
        assert_eq!(ago(100, 100 + 2 * 3600 + 5), "2h ago");
        assert_eq!(ago(100, 100 + 3 * 86400), "3d ago");
    }
}
