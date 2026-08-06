//! App history stack for quick-switch ordering.

use crate::ui_state::ToplevelId;

/// Most-recently-used app order.
#[derive(Clone, Debug, Default)]
pub struct AppHistory {
    /// MRU stack. Front = most recent (current foreground).
    pub stack: Vec<ToplevelId>,
    /// Position the quick-switch walk currently sits at. `push_foreground`
    /// resets it to the front; a horizontal bar swipe walks it through the
    /// stack *without* reordering, so swiping repeatedly cycles the whole list
    /// instead of bouncing between the last two apps.
    cursor: usize,
}

impl AppHistory {
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            cursor: 0,
        }
    }

    /// Record an app coming to foreground. Moves it to front and resets the
    /// quick-switch cursor. Only deliberate activations (launcher, switcher tap,
    /// a newly mapped app) call this — quick-switch does not.
    pub fn push_foreground(&mut self, id: ToplevelId) {
        self.stack.retain(|&x| x != id);
        self.stack.insert(0, id);
        self.cursor = 0;
    }

    /// Remove a closed toplevel.
    pub fn remove(&mut self, id: ToplevelId) {
        self.stack.retain(|&x| x != id);
        if self.cursor >= self.stack.len() {
            self.cursor = 0;
        }
    }

    /// Peek the app one step from the cursor without moving it (`dir`: -1 =
    /// previous, +1 = next). Returns `None` at the stack ends — the live
    /// quick-switch uses this to know when to rubber-band. `quick_switch(dir)`
    /// commits the same step.
    pub fn peek(&self, dir: i32) -> Option<ToplevelId> {
        let idx = match dir {
            d if d > 0 => self.cursor + 1,
            d if d < 0 => self.cursor.checked_sub(1)?,
            _ => return None,
        };
        self.stack.get(idx).copied()
    }

    /// Walk the quick-switch cursor one step (`dir`: -1 = previous / swipe
    /// right, +1 = next / swipe left) and return the app now under it. Clamps at
    /// the stack ends — a swipe past either end returns `None` (reject, cursor
    /// unmoved) rather than wrapping — and does *not* reorder, so the MRU order
    /// is preserved.
    pub fn quick_switch(&mut self, dir: i32) -> Option<ToplevelId> {
        let len = self.stack.len();
        if len <= 1 || dir == 0 {
            return None;
        }
        let next = match dir {
            d if d > 0 => self.cursor + 1,
            _ => self.cursor.checked_sub(1)?,
        };
        if next >= len {
            return None; // reject: already at the end of the stack
        }
        self.cursor = next;
        self.stack.get(next).copied()
    }

    /// Deck order for the fan / switcher: the **current** app (the one under the
    /// quick-switch cursor, i.e. what is actually on screen) first, then the rest
    /// in MRU order. After a quick-switch *browse* the cursor has moved without
    /// reordering the stack, so `stack[0]` is no longer the current app — using
    /// the raw stack here would put the wrong card at the front and duplicate the
    /// current app (front card + a neighbour). Front-first keeps every card unique.
    pub fn deck_order(&self) -> Vec<ToplevelId> {
        if self.stack.is_empty() {
            return Vec::new();
        }
        let cur = self.stack[self.cursor.min(self.stack.len() - 1)];
        let mut v = Vec::with_capacity(self.stack.len());
        v.push(cur);
        v.extend(self.stack.iter().copied().filter(|&x| x != cur));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_foreground_moves_to_front() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3);
        assert_eq!(h.stack, vec![3, 2, 1]);
    }

    #[test]
    fn push_existing_moves_to_front() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(1);
        assert_eq!(h.stack, vec![1, 2]);
    }

    #[test]
    fn deck_order_of_a_single_app_is_that_app() {
        // Regression: the Home bar used to reach for `stack[1]`, so with exactly
        // one app running every bar gesture did nothing until a second app was
        // launched. The deck the bar reaches for is front-first.
        let mut h = AppHistory::new();
        h.push_foreground(1);
        assert_eq!(h.deck_order(), vec![1]);
    }

    #[test]
    fn quick_switch_walks_without_reordering() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3); // stack: [3, 2, 1], cursor 0

        // Swipe next walks deeper into the stack, no reorder.
        assert_eq!(h.quick_switch(1), Some(2));
        assert_eq!(h.quick_switch(1), Some(1));
        // At the end: reject, no wrap, cursor stays put.
        assert_eq!(h.quick_switch(1), None);
        assert_eq!(h.quick_switch(1), None);
        // Reversing still works from the held position.
        assert_eq!(h.quick_switch(-1), Some(2));
        // Order is untouched.
        assert_eq!(h.stack, vec![3, 2, 1]);
    }

    #[test]
    fn quick_switch_rejects_at_front() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3); // stack: [3, 2, 1], cursor 0
                              // Already at the front — swiping back rejects.
        assert_eq!(h.quick_switch(-1), None);
        assert_eq!(h.quick_switch(1), Some(2));
        assert_eq!(h.quick_switch(-1), Some(3));
        assert_eq!(h.quick_switch(-1), None);
    }

    #[test]
    fn push_foreground_resets_cursor() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3);
        h.quick_switch(1); // cursor -> 1
        h.push_foreground(9); // deliberate activation resets walk
        assert_eq!(h.quick_switch(1), Some(3)); // stack [9,3,2,1], cursor 0 -> 1
    }

    #[test]
    fn deck_order_puts_current_first_after_browse() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3); // stack [3,2,1], cursor 0
                              // No browse: current is the front already.
        assert_eq!(h.deck_order(), vec![3, 2, 1]);
        // Browse to the second app (cursor -> 1, current = 2). The deck must
        // lead with 2 and list each app exactly once — no duplicate front card.
        h.quick_switch(1);
        assert_eq!(h.deck_order(), vec![2, 3, 1]);
    }

    #[test]
    fn remove_cleans_up() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.remove(2);
        assert_eq!(h.stack, vec![1]);
    }
}
