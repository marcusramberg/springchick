//! App history stack for quick-switch ordering.

use crate::ui_state::ToplevelId;

/// Most-recently-used app order.
#[derive(Clone, Debug, Default)]
pub struct AppHistory {
    /// MRU stack. Front = most recent (current foreground).
    pub stack: Vec<ToplevelId>,
}

impl AppHistory {
    pub fn new() -> Self {
        Self { stack: Vec::new() }
    }

    /// Record an app coming to foreground. Moves it to front.
    pub fn push_foreground(&mut self, id: ToplevelId) {
        self.stack.retain(|&x| x != id);
        self.stack.insert(0, id);
    }

    /// Remove a closed toplevel.
    #[allow(dead_code)]
    pub fn remove(&mut self, id: ToplevelId) {
        self.stack.retain(|&x| x != id);
    }

    /// Get the previous app (for quick-switch -1).
    pub fn previous(&self) -> Option<ToplevelId> {
        self.stack.get(1).copied()
    }

    /// Get the next app (for quick-switch +1). Wraps around.
    pub fn next(&self) -> Option<ToplevelId> {
        if self.stack.len() <= 1 {
            return None;
        }
        self.stack.get(2).or(self.stack.get(1)).copied()
    }

    /// Get app in the given direction (-1 = previous, +1 = next).
    pub fn quick_switch(&self, dir: i32) -> Option<ToplevelId> {
        match dir {
            d if d < 0 => self.previous(),
            d if d > 0 => self.next(),
            _ => None,
        }
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
    fn previous_is_second() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3);
        assert_eq!(h.previous(), Some(2));
    }

    #[test]
    fn previous_none_when_single() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        assert_eq!(h.previous(), None);
    }

    #[test]
    fn quick_switch_directions() {
        let mut h = AppHistory::new();
        h.push_foreground(1);
        h.push_foreground(2);
        h.push_foreground(3);
        assert_eq!(h.quick_switch(-1), Some(2));
        assert_eq!(h.quick_switch(1), Some(1));
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
