//! The emoji picker, as on iOS: every emoji by category in a grid that
//! scrolls sideways, recently used first. Each category starts a new column.

use std::path::Path;

use crate::grid::Rect;

const DATA: &str = include_str!("../emoji.txt");
pub const ROWS: usize = 4;
const RECENT_MAX: usize = 32;
/// Travel that turns a tap into a scroll, logical px.
const TAP_SLOP: f32 = 8.0;
/// Fling decay rate, 1/s, and the speed it stops at, px/s.
const DECAY: f32 = 4.0;
const STOP: f32 = 20.0;

/// Shown in the strip: recently used, then one per data group.
pub const ICONS: [&str; 10] = ["🕘", "😀", "👋", "🐻", "🍔", "🚗", "⚽", "💡", "❤️", "🏳️"];

struct Item {
    emoji: String,
    col: usize,
    row: usize,
}

struct Drag {
    touch: i32,
    start: (f32, f32),
    start_offset: f32,
    last: (f32, f64),
    moved: bool,
}

pub struct Picker {
    pub recent: Vec<String>,
    items: Vec<Item>,
    /// First column of each category, recents first (possibly empty).
    starts: Vec<usize>,
    cols: usize,
    area: Rect,
    pub offset: f32,
    velocity: f32,
    drag: Option<Drag>,
}

impl Picker {
    pub fn new(recent: Vec<String>) -> Self {
        let mut p = Picker {
            recent,
            items: Vec::new(),
            starts: Vec::new(),
            cols: 0,
            area: Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
            offset: 0.0,
            velocity: 0.0,
            drag: None,
        };
        p.reset();
        p
    }

    /// Back to the start with the recents current. On opening the page: a
    /// grid that rearranged under the finger would type the wrong emoji.
    pub fn reset(&mut self) {
        self.items.clear();
        self.starts.clear();
        self.offset = 0.0;
        self.velocity = 0.0;
        self.drag = None;
        let groups = DATA
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_once('\t'))
            .map(|(_, list)| list.split(' ').map(str::to_string).collect::<Vec<_>>());
        let mut col = 0;
        for group in std::iter::once(self.recent.clone()).chain(groups) {
            self.starts.push(col);
            for (i, emoji) in group.into_iter().enumerate() {
                self.items.push(Item {
                    emoji,
                    col: col + i / ROWS,
                    row: i % ROWS,
                });
            }
            col = self.items.last().map_or(col, |it| it.col + 1).max(col);
        }
        self.cols = col;
    }

    pub fn set_area(&mut self, area: Rect) {
        self.area = area;
        self.offset = self.offset.clamp(0.0, self.max_offset());
    }

    fn cell(&self) -> f32 {
        self.area.h / ROWS as f32
    }

    fn max_offset(&self) -> f32 {
        (self.cols as f32 * self.cell() - self.area.w).max(0.0)
    }

    fn rect(&self, item: &Item) -> Rect {
        let c = self.cell();
        Rect {
            x: self.area.x + item.col as f32 * c - self.offset,
            y: self.area.y + item.row as f32 * c,
            w: c,
            h: c,
        }
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn visible(&self) -> impl Iterator<Item = (Rect, &str)> {
        let a = self.area;
        self.items
            .iter()
            .map(|it| (self.rect(it), it.emoji.as_str()))
            .filter(move |(r, _)| r.x + r.w > a.x && r.x < a.x + a.w)
    }

    /// The category the left edge is in.
    pub fn current(&self) -> usize {
        let col = (self.offset / self.cell() + 0.5) as usize;
        self.starts.iter().rposition(|&s| s <= col).unwrap_or(0)
    }

    pub fn jump(&mut self, category: usize) {
        if let Some(&col) = self.starts.get(category) {
            self.velocity = 0.0;
            self.offset = (col as f32 * self.cell()).min(self.max_offset());
        }
    }

    pub fn owns(&self, touch: i32) -> bool {
        self.drag.as_ref().is_some_and(|d| d.touch == touch)
    }

    pub fn down(&mut self, touch: i32, at: (f32, f32), now: f64) {
        self.velocity = 0.0;
        self.drag = Some(Drag {
            touch,
            start: at,
            start_offset: self.offset,
            last: (at.0, now),
            moved: false,
        });
    }

    pub fn motion(&mut self, touch: i32, at: (f32, f32), now: f64) {
        let max = self.max_offset();
        let Some(d) = self.drag.as_mut().filter(|d| d.touch == touch) else {
            return;
        };
        if (at.0 - d.start.0).abs() > TAP_SLOP {
            d.moved = true;
        }
        if d.moved {
            self.offset = (d.start_offset - (at.0 - d.start.0)).clamp(0.0, max);
            let dt = (now - d.last.1) as f32;
            if dt > 0.0 {
                self.velocity = -(at.0 - d.last.0) / dt;
            }
            d.last = (at.0, now);
        }
    }

    /// The emoji a tap picked, which also goes to the front of the recents.
    pub fn up(&mut self, touch: i32, now: f64) -> Option<String> {
        let d = self.drag.take_if(|d| d.touch == touch)?;
        if d.moved {
            // A finger that stopped before lifting doesn't fling.
            if now - d.last.1 > 0.1 {
                self.velocity = 0.0;
            }
            return None;
        }
        let emoji = self
            .items
            .iter()
            .find(|it| self.rect(it).contains(d.start.0, d.start.1))?
            .emoji
            .clone();
        self.recent.retain(|e| *e != emoji);
        self.recent.insert(0, emoji.clone());
        self.recent.truncate(RECENT_MAX);
        Some(emoji)
    }

    pub fn animating(&self) -> bool {
        self.drag.is_none() && self.velocity.abs() > STOP
    }

    pub fn tick(&mut self, dt: f32) {
        if !self.animating() {
            return;
        }
        let max = self.max_offset();
        self.offset += self.velocity * dt;
        self.velocity *= (-DECAY * dt).exp();
        if self.offset <= 0.0 || self.offset >= max {
            self.offset = self.offset.clamp(0.0, max);
            self.velocity = 0.0;
        }
    }
}

pub fn load_recent(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn save_recent(path: &Path, recent: &[String]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, recent.join("\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(recent: &[&str]) -> Picker {
        let mut p = Picker::new(recent.iter().map(|s| s.to_string()).collect());
        p.set_area(Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 200.0,
        });
        p
    }

    #[test]
    fn data_has_every_group_and_no_skin_tones() {
        let p = picker(&[]);
        assert_eq!(p.starts.len(), ICONS.len());
        assert!(p.items.len() > 1500);
        assert!(!p.items.iter().any(|i| i.emoji.contains('\u{1F3FB}')));
    }

    #[test]
    fn a_tap_types_and_becomes_recent() {
        let mut p = picker(&[]);
        p.down(0, (10.0, 10.0), 0.0);
        let e = p.up(0, 0.05).expect("an emoji under the finger");
        assert_eq!(e, "😀");
        assert_eq!(p.recent, ["😀"]);
        // The grid only takes the recents in on the next opening.
        p.reset();
        assert_eq!(p.items[0].emoji, "😀");
        assert_eq!(p.current(), 0);
    }

    #[test]
    fn a_drag_scrolls_and_types_nothing() {
        let mut p = picker(&[]);
        p.down(0, (300.0, 10.0), 0.0);
        p.motion(0, (200.0, 10.0), 0.05);
        assert_eq!(p.offset, 100.0);
        assert_eq!(p.up(0, 0.06), None);
        assert!(p.animating(), "flings");
        let before = p.offset;
        p.tick(0.016);
        assert!(p.offset > before);
    }

    #[test]
    fn stopping_before_lifting_doesnt_fling() {
        let mut p = picker(&[]);
        p.down(0, (300.0, 10.0), 0.0);
        p.motion(0, (200.0, 10.0), 0.05);
        p.up(0, 0.5);
        assert!(!p.animating());
    }

    #[test]
    fn jump_lands_on_a_category_and_current_follows() {
        let mut p = picker(&["😀"]);
        p.jump(3);
        assert_eq!(p.current(), 3);
        p.jump(0);
        assert_eq!(p.offset, 0.0);
        p.offset = p.max_offset();
        assert_eq!(p.current(), ICONS.len() - 1);
    }

    #[test]
    fn recents_round_trip() {
        let dir = std::env::temp_dir().join(format!("sc-osk-test-{}", std::process::id()));
        let path = dir.join("recent");
        save_recent(&path, &["🔥".into(), "❤️".into()]);
        assert_eq!(load_recent(&path), ["🔥", "❤️"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
