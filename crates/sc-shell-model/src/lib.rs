#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};

/// Stable identifier for an app (its .desktop file id, e.g. "org.gnome.Maps").
pub type AppId = String;

pub const COLS: usize = 4;
pub const ROWS: usize = 6;
pub const PAGE_CAP: usize = COLS * ROWS; // 24 icons per page
pub const DOCK_CAP: usize = 4;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ShellModel {
    pub pages: Vec<Vec<AppId>>, // each page: ordered slots, len <= PAGE_CAP
    pub dock: Vec<AppId>,       // len <= DOCK_CAP
}

impl ShellModel {
    /// Append an app to the first page with room, creating a page if needed.
    pub fn place(&mut self, app: AppId) {
        if let Some(page) = self.pages.iter_mut().find(|p| p.len() < PAGE_CAP) {
            page.push(app);
        } else {
            self.pages.push(vec![app]);
        }
    }

    /// Remove an app entirely (delete from home).
    pub fn delete(&mut self, app: &str) {
        for page in &mut self.pages { page.retain(|a| a != app); }
        self.dock.retain(|a| a != app);
        self.pages.retain(|p| !p.is_empty());
    }

    /// Move an app to (page, index), shifting others. Used by drag-rearrange.
    pub fn move_to(&mut self, app: &str, page: usize, index: usize) {
        self.delete_keep_pages(app);
        while self.pages.len() <= page { self.pages.push(Vec::new()); }
        let p = &mut self.pages[page];
        let idx = index.min(p.len());
        p.insert(idx, app.to_string());
    }

    // delete without collapsing empty pages (internal helper for moves)
    fn delete_keep_pages(&mut self, app: &str) {
        for page in &mut self.pages { page.retain(|a| a != app); }
        self.dock.retain(|a| a != app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_fills_pages_then_overflows() {
        let mut m = ShellModel::default();
        for i in 0..(PAGE_CAP + 1) { m.place(format!("app{i}")); }
        assert_eq!(m.pages.len(), 2);
        assert_eq!(m.pages[0].len(), PAGE_CAP);
        assert_eq!(m.pages[1].len(), 1);
    }

    #[test]
    fn delete_removes_and_collapses_empty_pages() {
        let mut m = ShellModel::default();
        m.place("a".into());
        m.delete("a");
        assert!(m.pages.is_empty());
    }

    #[test]
    fn move_to_reorders_within_page() {
        let mut m = ShellModel::default();
        for n in ["a","b","c"] { m.place(n.into()); }
        m.move_to("c", 0, 0);
        assert_eq!(m.pages[0], vec!["c","a","b"]);
    }
}
