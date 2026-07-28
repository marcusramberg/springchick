#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Stable identifier for an app (its .desktop file id, e.g. "org.gnome.Maps").
pub type AppId = String;

pub const COLS: usize = 4;
pub const ROWS: usize = 6;
pub const PAGE_CAP: usize = COLS * ROWS; // 24 icons per page
pub const DOCK_CAP: usize = 4;

/// Frecency half-life: a launch's contribution halves every 30 days.
pub const HALF_LIFE_SECS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppStat {
    pub score: f64,
    pub last_launch: u64, // unix seconds; 0 = never launched
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FrecencyStore {
    #[serde(default)]
    pub apps: HashMap<AppId, AppStat>,
}

/// Decayed score of `stat` evaluated at `now` (unix secs). Compare all apps at
/// the same `now` to get a consistent ordering.
pub fn eff(stat: &AppStat, now: u64) -> f64 {
    let elapsed = now.saturating_sub(stat.last_launch) as f64;
    stat.score * 0.5_f64.powf(elapsed / HALF_LIFE_SECS)
}

impl FrecencyStore {
    /// Record an app launch: decay the stored score to `now`, then add 1.
    pub fn record_launch(&mut self, app: &str, now: u64) {
        let s = self.apps.entry(app.to_owned()).or_default();
        let elapsed = now.saturating_sub(s.last_launch) as f64;
        s.score = s.score * 0.5_f64.powf(elapsed / HALF_LIFE_SECS) + 1.0;
        s.last_launch = now;
    }

    /// Insert an app not yet in the store. `first_run` true (store was empty at
    /// bootstrap) -> score 0. Later install -> seed 1.0 at `now` so it surfaces.
    pub fn seed(&mut self, app: &str, now: u64, first_run: bool) {
        if self.apps.contains_key(app) {
            return;
        }
        let stat = if first_run {
            AppStat { score: 0.0, last_launch: 0 }
        } else {
            AppStat { score: 1.0, last_launch: now }
        };
        self.apps.insert(app.to_owned(), stat);
    }

    /// Drop stats for apps no longer in the catalog.
    pub fn prune(&mut self, catalog_ids: &[AppId]) {
        self.apps.retain(|id, _| catalog_ids.contains(id));
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ShellModel {
    /// Grid view of apps, in manual (persisted) order. This is the source of
    /// truth for on-screen grid order. Seeded alphabetically from the catalog
    /// by `reconcile` on first run; reordered only by manual drag.
    #[serde(default)]
    pub pages: Vec<Vec<AppId>>,
    pub dock: Vec<AppId>, // len <= DOCK_CAP
    #[serde(default)]
    pub frecency: FrecencyStore,
    #[serde(default)]
    pub hidden: Vec<AppId>,
}

impl ShellModel {
    /// Keep `pages` in sync with the installed catalog without reordering
    /// existing slots. Appends catalog ids not yet in pages/dock/hidden (via
    /// `place`), seeds their frecency, and prunes ids no longer installed.
    /// `now`/`first_run` mirror the old startup seed loop (score 0 on a
    /// first-run empty store, 1.0 for a later install).
    pub fn reconcile(&mut self, catalog_ids: &[AppId], now: u64, first_run: bool) {
        self.pages.iter_mut().for_each(|p| p.retain(|a| catalog_ids.contains(a)));
        self.pages.retain(|p| !p.is_empty());
        self.dock.retain(|a| catalog_ids.contains(a));
        self.hidden.retain(|a| catalog_ids.contains(a));
        self.frecency.prune(catalog_ids);
        for id in catalog_ids {
            let known = self.pages.iter().any(|p| p.contains(id))
                || self.dock.contains(id)
                || self.hidden.contains(id);
            if !known {
                self.place(id.clone());
            }
            self.frecency.seed(id, now, first_run);
        }
    }

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
        for page in &mut self.pages {
            page.retain(|a| a != app);
        }
        self.dock.retain(|a| a != app);
        self.pages.retain(|p| !p.is_empty());
    }

    /// Move an app to (page, index), shifting others. Used by drag-rearrange.
    pub fn move_to(&mut self, app: &str, page: usize, index: usize) {
        self.delete_keep_pages(app);
        while self.pages.len() <= page {
            self.pages.push(Vec::new());
        }
        let p = &mut self.pages[page];
        let idx = index.min(p.len());
        p.insert(idx, app.to_string());
    }

    // delete without collapsing empty pages (internal helper for moves)
    fn delete_keep_pages(&mut self, app: &str) {
        for page in &mut self.pages {
            page.retain(|a| a != app);
        }
        self.dock.retain(|a| a != app);
    }

    /// Pin `app` to the dock. Returns false if already docked or dock is full.
    pub fn pin(&mut self, app: &str) -> bool {
        if self.dock.iter().any(|a| a == app) || self.dock.len() >= DOCK_CAP {
            return false;
        }
        self.remove_from_pages(app);
        self.dock.push(app.to_owned());
        true
    }

    /// Remove `app` from the dock, if present, restoring it to the home grid.
    pub fn unpin(&mut self, app: &str) {
        if self.dock.iter().any(|a| a == app) {
            self.dock.retain(|a| a != app);
            self.place(app.to_owned());
        }
    }

    /// Hide `app` from the home grid.
    pub fn hide(&mut self, app: &str) {
        if !self.hidden.iter().any(|a| a == app) {
            self.remove_from_pages(app);
            self.hidden.push(app.to_owned());
        }
    }

    /// Unhide `app`, restoring it to the home grid.
    pub fn unhide(&mut self, app: &str) {
        if self.hidden.iter().any(|a| a == app) {
            self.hidden.retain(|a| a != app);
            self.place(app.to_owned());
        }
    }

    /// Remove `app` from all pages, dropping any pages left empty.
    fn remove_from_pages(&mut self, app: &str) {
        for page in &mut self.pages {
            page.retain(|a| a != app);
        }
        self.pages.retain(|p| !p.is_empty());
    }

    /// Keep every page within PAGE_CAP by cascading overflow onto the next page
    /// (creating one if needed), and drop empty pages. Called after any reorder.
    pub fn normalize_pages(&mut self) {
        let mut i = 0;
        while i < self.pages.len() {
            if self.pages[i].len() > PAGE_CAP {
                let overflow: Vec<AppId> = self.pages[i].split_off(PAGE_CAP);
                if i + 1 == self.pages.len() {
                    self.pages.push(Vec::new());
                }
                let next = &mut self.pages[i + 1];
                for (k, app) in overflow.into_iter().enumerate() {
                    next.insert(k, app);
                }
            }
            i += 1;
        }
        self.pages.retain(|p| !p.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_fills_pages_then_overflows() {
        let mut m = ShellModel::default();
        for i in 0..(PAGE_CAP + 1) {
            m.place(format!("app{i}"));
        }
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
        for n in ["a", "b", "c"] {
            m.place(n.into());
        }
        m.move_to("c", 0, 0);
        assert_eq!(m.pages[0], vec!["c", "a", "b"]);
    }

    #[test]
    fn eff_halves_after_one_half_life() {
        let stat = AppStat { score: 8.0, last_launch: 0 };
        let now = HALF_LIFE_SECS as u64;
        assert!((eff(&stat, now) - 4.0).abs() < 1e-6);
    }

    #[test]
    fn record_launch_on_fresh_app_scores_one() {
        let mut s = FrecencyStore::default();
        s.record_launch("a", 1000);
        let stat = &s.apps["a"];
        assert!((stat.score - 1.0).abs() < 1e-9);
        assert_eq!(stat.last_launch, 1000);
    }

    #[test]
    fn record_launch_decays_before_incrementing() {
        let mut s = FrecencyStore::default();
        s.record_launch("a", 0);
        s.record_launch("a", HALF_LIFE_SECS as u64);
        assert!((s.apps["a"].score - 1.5).abs() < 1e-6);
    }

    #[test]
    fn seed_first_run_is_zero_later_install_is_one() {
        let mut empty = FrecencyStore::default();
        empty.seed("a", 5000, true);
        assert_eq!(empty.apps["a"], AppStat { score: 0.0, last_launch: 0 });

        let mut populated = FrecencyStore::default();
        populated.record_launch("x", 100);
        populated.seed("b", 5000, false);
        assert_eq!(populated.apps["b"], AppStat { score: 1.0, last_launch: 5000 });
    }

    #[test]
    fn seed_whole_catalog_first_run_all_zero() {
        let mut s = FrecencyStore::default();
        let first_run = s.apps.is_empty();
        for id in ["a", "b", "c"] {
            s.seed(id, 5000, first_run);
        }
        for id in ["a", "b", "c"] {
            assert_eq!(s.apps[id], AppStat { score: 0.0, last_launch: 0 });
        }
    }

    #[test]
    fn pages_round_trip_through_serde() {
        let mut m = ShellModel::default();
        m.place("a".into());
        m.place("b".into());
        let s = toml::to_string(&m).unwrap();
        let back: ShellModel = toml::from_str(&s).unwrap();
        assert_eq!(back.pages, m.pages);
    }

    #[test]
    fn old_file_without_pages_loads_empty() {
        // A config written before pages were persisted: only dock + frecency.
        let s = "dock = []\n[frecency]\n";
        let m: ShellModel = toml::from_str(s).unwrap();
        assert!(m.pages.is_empty());
    }

    #[test]
    fn prune_drops_apps_missing_from_catalog() {
        let mut s = FrecencyStore::default();
        s.record_launch("a", 0);
        s.seed("b", 0, false);
        s.prune(&["a".to_string()]);
        assert!(s.apps.contains_key("a"));
        assert!(!s.apps.contains_key("b"));
    }

    #[test]
    fn pin_adds_to_dock_under_cap() {
        let mut m = ShellModel::default();
        assert!(m.pin("a"));
        assert_eq!(m.dock, vec!["a"]);
    }

    #[test]
    fn pin_fails_when_full_or_duplicate() {
        let mut m = ShellModel::default();
        for i in 0..DOCK_CAP { assert!(m.pin(&format!("d{i}"))); }
        assert!(!m.pin("overflow"));
        assert_eq!(m.dock.len(), DOCK_CAP);
        let mut m2 = ShellModel::default();
        assert!(m2.pin("a"));
        assert!(!m2.pin("a"));
        assert_eq!(m2.dock, vec!["a"]);
    }

    #[test]
    fn unpin_removes_from_dock() {
        let mut m = ShellModel::default();
        m.pin("a");
        m.unpin("a");
        assert!(m.dock.is_empty());
    }

    #[test]
    fn pin_removes_from_pages_unpin_restores() {
        let mut m = ShellModel::default();
        m.place("a".into());
        assert!(m.pin("a"));
        assert!(!m.pages.iter().any(|p| p.contains(&"a".to_string())));
        assert!(m.dock.contains(&"a".to_string()));
        m.unpin("a");
        assert!(!m.dock.contains(&"a".to_string()));
        assert!(m.pages.iter().any(|p| p.contains(&"a".to_string())));
    }

    #[test]
    fn hide_removes_from_pages_unhide_restores() {
        let mut m = ShellModel::default();
        m.place("a".into());
        m.hide("a");
        assert!(!m.pages.iter().any(|p| p.contains(&"a".to_string())));
        assert!(m.hidden.contains(&"a".to_string()));
        m.unhide("a");
        assert!(!m.hidden.contains(&"a".to_string()));
        assert!(m.pages.iter().any(|p| p.contains(&"a".to_string())));
    }

    #[test]
    fn hide_unhide_toggle_hidden_set() {
        let mut m = ShellModel::default();
        m.hide("a");
        assert_eq!(m.hidden, vec!["a"]);
        m.hide("a");
        assert_eq!(m.hidden, vec!["a"]);
        m.unhide("a");
        assert!(m.hidden.is_empty());
    }

    #[test]
    fn hidden_serialized_with_default() {
        let mut m = ShellModel::default();
        m.hide("a");
        let s = toml::to_string_pretty(&m).unwrap();
        let back: ShellModel = toml::from_str(&s).unwrap();
        assert_eq!(back.hidden, vec!["a"]);
        let legacy: ShellModel = toml::from_str("dock = []\n").unwrap();
        assert!(legacy.hidden.is_empty());
    }

    #[test]
    fn reconcile_appends_new_catalog_ids_in_order() {
        let mut m = ShellModel::default();
        m.place("b".into());
        m.reconcile(&["a".into(), "b".into(), "c".into()], 0, false);
        assert_eq!(m.pages[0], vec!["b", "a", "c"]);
    }

    #[test]
    fn reconcile_prunes_uninstalled_from_pages_dock_hidden() {
        let mut m = ShellModel::default();
        m.place("gone".into());
        m.place("keep".into());
        m.dock.push("dgone".into());
        m.hidden.push("hgone".into());
        m.reconcile(&["keep".into()], 0, false);
        assert_eq!(m.pages, vec![vec!["keep".to_string()]]);
        assert!(m.dock.is_empty());
        assert!(m.hidden.is_empty());
    }

    #[test]
    fn reconcile_seeds_frecency_for_new_apps() {
        let mut m = ShellModel::default();
        m.frecency.apps.insert("existing".into(), AppStat { score: 5.0, last_launch: 0 });
        m.reconcile(&["existing".into(), "new".into()], 100, false);
        assert_eq!(m.frecency.apps["new"].score, 1.0);
        assert_eq!(m.frecency.apps["new"].last_launch, 100);
    }

    #[test]
    fn reconcile_does_not_move_already_placed() {
        let mut m = ShellModel::default();
        for n in ["a", "b", "c"] { m.place(n.into()); }
        m.move_to("c", 0, 0); // user order: c, a, b
        m.reconcile(&["a".into(), "b".into(), "c".into()], 0, false);
        assert_eq!(m.pages[0], vec!["c", "a", "b"]);
    }

    #[test]
    fn normalize_cascades_overflow_to_next_page() {
        let mut m = ShellModel::default();
        m.pages = vec![(0..=PAGE_CAP).map(|i| format!("a{i}")).collect()]; // PAGE_CAP+1 on one page
        m.normalize_pages();
        assert_eq!(m.pages[0].len(), PAGE_CAP);
        assert_eq!(m.pages[1], vec![format!("a{PAGE_CAP}")]);
    }

    #[test]
    fn normalize_drops_empty_trailing_pages() {
        let mut m = ShellModel::default();
        m.pages = vec![vec!["a".into()], vec![], vec![]];
        m.normalize_pages();
        assert_eq!(m.pages, vec![vec!["a".to_string()]]);
    }
}
