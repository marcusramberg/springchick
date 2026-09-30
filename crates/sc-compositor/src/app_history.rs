use crate::ui_state::ToplevelId;

#[derive(Clone, Debug, Default)]
pub struct AppHistory {
    /// Front = most recent.
    pub stack: Vec<ToplevelId>,
    /// Quick-switch position. Bar swipes walk it without reordering, so repeated
    /// swipes cycle the whole list instead of bouncing between two apps.
    cursor: usize,
}

impl AppHistory {
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            cursor: 0,
        }
    }

    /// Deliberate activations only; quick-switch doesn't call this.
    pub fn push_foreground(&mut self, id: ToplevelId) {
        self.stack.retain(|&x| x != id);
        self.stack.insert(0, id);
        self.cursor = 0;
    }

    pub fn remove(&mut self, id: ToplevelId) {
        let removed_before_cursor = self
            .stack
            .iter()
            .position(|&x| x == id)
            .is_some_and(|i| i < self.cursor);
        self.stack.retain(|&x| x != id);
        if removed_before_cursor {
            self.cursor = self.cursor.saturating_sub(1);
        }
        if self.cursor >= self.stack.len() {
            self.cursor = 0;
        }
    }

    /// The app one step from the cursor (`dir` ±1), `None` at the ends.
    pub fn peek(&self, dir: i32) -> Option<ToplevelId> {
        let idx = match dir {
            d if d > 0 => self.cursor + 1,
            d if d < 0 => self.cursor.checked_sub(1)?,
            _ => return None,
        };
        self.stack.get(idx).copied()
    }

    /// Move the cursor one step without reordering. `None` at the ends (no wrap).
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
            return None;
        }
        self.cursor = next;
        self.stack.get(next).copied()
    }

    /// The app on screen (under the cursor) first, then the rest in MRU order.
    /// After a quick-switch browse `stack[0]` isn't current, so the raw stack
    /// would duplicate a card.
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
        // With one app running the bar must still reach it.
        let mut h = AppHistory::new();
        h.push_foreground(1);
        assert_eq!(h.deck_order(), vec![1]);
    }

    #[test]
    fn quick_switch_walks_without_reordering() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3);

        assert_eq!(h.quick_switch(1), Some(2));
        assert_eq!(h.quick_switch(1), Some(1));
        assert_eq!(h.quick_switch(1), None);
        assert_eq!(h.quick_switch(1), None);
        assert_eq!(h.quick_switch(-1), Some(2));
        assert_eq!(h.stack, vec![3, 2, 1]);
    }

    #[test]
    fn quick_switch_rejects_at_front() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3);
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
        h.quick_switch(1);
        h.push_foreground(9);
        assert_eq!(h.quick_switch(1), Some(3));
    }

    #[test]
    fn deck_order_puts_current_first_after_browse() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3);
        assert_eq!(h.deck_order(), vec![3, 2, 1]);
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

    #[test]
    fn remove_before_cursor_keeps_current_app_current() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3);
        h.quick_switch(1);
        h.remove(3);
        assert_eq!(h.deck_order(), vec![2, 1]);
    }
}
