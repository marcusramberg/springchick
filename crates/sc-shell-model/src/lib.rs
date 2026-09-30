#![forbid(unsafe_code)]

pub mod persist;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// The .desktop file id, e.g. "org.gnome.Maps".
pub type AppId = String;

pub const COLS: usize = 4;
pub const ROWS: usize = 6;
pub const PAGE_CAP: usize = COLS * ROWS;
pub const DOCK_CAP: usize = 4;

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

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compare apps at the same `now` for a consistent ordering.
pub fn eff(stat: &AppStat, now: u64) -> f64 {
    let elapsed = now.saturating_sub(stat.last_launch) as f64;
    stat.score * 0.5_f64.powf(elapsed / HALF_LIFE_SECS)
}

impl FrecencyStore {
    /// Decay the stored score to `now`, then add 1.
    pub fn record_launch(&mut self, app: &str, now: u64) {
        let s = self.apps.entry(app.to_owned()).or_default();
        s.score = eff(s, now) + 1.0;
        s.last_launch = now;
    }

    /// `first_run` (empty store at bootstrap) seeds 0; a later install seeds 1.0
    /// so it surfaces.
    pub fn seed(&mut self, app: &str, now: u64, first_run: bool) {
        if self.apps.contains_key(app) {
            return;
        }
        let stat = if first_run {
            AppStat {
                score: 0.0,
                last_launch: 0,
            }
        } else {
            AppStat {
                score: 1.0,
                last_launch: now,
            }
        };
        self.apps.insert(app.to_owned(), stat);
    }

    pub fn prune(&mut self, catalog_ids: &[AppId]) {
        self.apps.retain(|id, _| catalog_ids.contains(id));
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ShellModel {
    /// Only what the user placed. Everything else lives in the library, which
    /// is derived and never stored.
    #[serde(default)]
    pub pages: Vec<Vec<AppId>>,
    pub dock: Vec<AppId>,
    #[serde(default)]
    pub frecency: FrecencyStore,
}

impl ShellModel {
    /// Drop ids that are gone, seed frecency for new ones. New apps are not
    /// placed on a page; they reach Home only by drag from the library.
    pub fn reconcile(&mut self, catalog_ids: &[AppId], now: u64, first_run: bool) {
        let installed: HashSet<&AppId> = catalog_ids.iter().collect();
        self.pages
            .iter_mut()
            .for_each(|p| p.retain(|a| installed.contains(a)));
        self.dock.retain(|a| installed.contains(a));
        self.frecency.prune(catalog_ids);
        for id in catalog_ids {
            self.frecency.seed(id, now, first_run);
        }
        // A persisted state.toml may break PAGE_CAP, hiding icons past the 24th.
        self.repack();
    }

    pub fn place(&mut self, app: AppId) {
        if let Some(page) = self.pages.iter_mut().find(|p| p.len() < PAGE_CAP) {
            page.push(app);
        } else {
            self.pages.push(vec![app]);
        }
    }

    /// It stays in the library.
    pub fn delete(&mut self, app: &str) {
        self.remove_from_pages(app);
        self.dock.retain(|a| a != app);
        self.repack();
    }

    /// `(page, index)` is a global position `page*PAGE_CAP + index` in the
    /// flattened order. Repacks afterwards.
    pub fn move_to(&mut self, app: &str, page: usize, index: usize) {
        let mut flat: Vec<AppId> = self.flat().into_iter().filter(|a| a != app).collect();
        self.dock.retain(|a| a != app);
        let gi = page
            .saturating_mul(PAGE_CAP)
            .saturating_add(index)
            .min(flat.len());
        flat.insert(gi, app.to_string());
        self.pages = flat.chunks(PAGE_CAP).map(|c| c.to_vec()).collect();
    }

    /// False if already docked or the dock is full.
    pub fn pin(&mut self, app: &str) -> bool {
        if self.dock.iter().any(|a| a == app) || self.dock.len() >= DOCK_CAP {
            return false;
        }
        self.remove_from_pages(app);
        self.repack();
        self.dock.push(app.to_owned());
        true
    }

    pub fn unpin(&mut self, app: &str) {
        if self.dock.iter().any(|a| a == app) {
            self.dock.retain(|a| a != app);
            self.place(app.to_owned());
        }
    }

    fn remove_from_pages(&mut self, app: &str) {
        for page in &mut self.pages {
            page.retain(|a| a != app);
        }
        self.pages.retain(|p| !p.is_empty());
    }

    fn flat(&self) -> Vec<AppId> {
        self.pages.iter().flatten().cloned().collect()
    }

    /// Every page but the last is full. Always keeps at least one page, even
    /// empty.
    pub fn repack(&mut self) {
        let flat = self.flat();
        self.pages = flat.chunks(PAGE_CAP).map(|c| c.to_vec()).collect();
        if self.pages.is_empty() {
            self.pages.push(Vec::new());
        }
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
    fn delete_removes_and_collapses_to_one_empty_page() {
        let mut m = ShellModel::default();
        m.place("a".into());
        m.delete("a");
        assert_eq!(m.pages, vec![Vec::<AppId>::new()]);
    }

    #[test]
    fn delete_collapses_the_page_it_emptied() {
        let mut m = ShellModel {
            pages: vec![
                (0..PAGE_CAP).map(|i| format!("a{i:02}")).collect(),
                vec!["tail".into()],
            ],
            ..Default::default()
        };
        m.delete("a05");
        assert_eq!(m.pages.len(), 1);
        assert_eq!(m.pages[0].len(), PAGE_CAP);
        assert_eq!(m.pages[0][PAGE_CAP - 1], "tail");
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
    fn move_to_cross_page_lands_at_global_index() {
        let mut m = ShellModel {
            pages: vec![
                (0..PAGE_CAP - 1).map(|i| format!("a{i:02}")).collect(),
                vec!["x".into()],
            ],
            ..Default::default()
        };
        m.move_to("x", 0, 2);
        assert_eq!(m.pages[0][2], "x");
        assert_eq!(m.pages[0].len(), PAGE_CAP);
        assert_eq!(m.pages.len(), 1);
    }

    #[test]
    fn move_to_from_dock_removes_from_dock() {
        let mut m = ShellModel::default();
        m.place("a".into());
        m.dock.push("d".into());
        m.move_to("d", 0, 0);
        assert!(m.dock.is_empty());
        assert_eq!(m.pages[0], vec!["d", "a"]);
    }

    #[test]
    fn eff_halves_after_one_half_life() {
        let stat = AppStat {
            score: 8.0,
            last_launch: 0,
        };
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
        assert_eq!(
            empty.apps["a"],
            AppStat {
                score: 0.0,
                last_launch: 0
            }
        );

        let mut populated = FrecencyStore::default();
        populated.record_launch("x", 100);
        populated.seed("b", 5000, false);
        assert_eq!(
            populated.apps["b"],
            AppStat {
                score: 1.0,
                last_launch: 5000
            }
        );
    }

    #[test]
    fn seed_whole_catalog_first_run_all_zero() {
        let mut s = FrecencyStore::default();
        let first_run = s.apps.is_empty();
        for id in ["a", "b", "c"] {
            s.seed(id, 5000, first_run);
        }
        for id in ["a", "b", "c"] {
            assert_eq!(
                s.apps[id],
                AppStat {
                    score: 0.0,
                    last_launch: 0
                }
            );
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
        for i in 0..DOCK_CAP {
            assert!(m.pin(&format!("d{i}")));
        }
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
    fn legacy_hidden_key_is_ignored() {
        // `hidden` predates the library; those apps are simply not on a page.
        let m: ShellModel = toml::from_str("dock = []\nhidden = [\"a\"]\n").unwrap();
        assert!(m.pages.is_empty());
    }

    #[test]
    fn reconcile_does_not_place_new_catalog_ids() {
        let mut m = ShellModel::default();
        m.place("b".into());
        m.reconcile(&["a".into(), "b".into(), "c".into()], 0, false);
        assert_eq!(m.pages, vec![vec!["b"]]);
    }

    #[test]
    fn reconcile_on_fresh_install_leaves_one_empty_page() {
        let mut m = ShellModel::default();
        m.reconcile(&["a".into(), "b".into()], 0, true);
        assert_eq!(m.pages, vec![Vec::<AppId>::new()]);
        assert!(m.dock.is_empty());
        assert_eq!(m.frecency.apps.len(), 2);
    }

    #[test]
    fn reconcile_splits_an_overfull_persisted_page() {
        let mut m = ShellModel::default();
        let ids: Vec<AppId> = (0..PAGE_CAP + 5).map(|i| format!("app{i}")).collect();
        m.pages = vec![ids.clone()];
        m.reconcile(&ids, 0, false);
        assert_eq!(m.pages.len(), 2);
        assert_eq!(m.pages[0].len(), PAGE_CAP);
        assert_eq!(m.pages[1].len(), 5);
    }

    #[test]
    fn reconcile_prunes_uninstalled_from_pages_and_dock() {
        let mut m = ShellModel::default();
        m.place("gone".into());
        m.place("keep".into());
        m.dock.push("dgone".into());
        m.reconcile(&["keep".into()], 0, false);
        assert_eq!(m.pages, vec![vec!["keep".to_string()]]);
        assert!(m.dock.is_empty());
    }

    #[test]
    fn reconcile_seeds_frecency_for_new_apps() {
        let mut m = ShellModel::default();
        m.frecency.apps.insert(
            "existing".into(),
            AppStat {
                score: 5.0,
                last_launch: 0,
            },
        );
        m.reconcile(&["existing".into(), "new".into()], 100, false);
        assert_eq!(m.frecency.apps["new"].score, 1.0);
        assert_eq!(m.frecency.apps["new"].last_launch, 100);
    }

    #[test]
    fn reconcile_does_not_move_already_placed() {
        let mut m = ShellModel::default();
        for n in ["a", "b", "c"] {
            m.place(n.into());
        }
        m.move_to("c", 0, 0);
        m.reconcile(&["a".into(), "b".into(), "c".into()], 0, false);
        assert_eq!(m.pages[0], vec!["c", "a", "b"]);
    }

    #[test]
    fn repack_backfills_interior_hole() {
        let mut m = ShellModel {
            pages: vec![
                (0..PAGE_CAP).map(|i| format!("a{i:02}")).collect(),
                vec!["tail".into()],
            ],
            ..Default::default()
        };
        m.pages[0].remove(5);
        m.repack();
        assert_eq!(m.pages[0].len(), PAGE_CAP);
        assert_eq!(m.pages[0][23], "tail");
        assert_eq!(m.pages.len(), 1);
    }

    #[test]
    fn pin_backfills_grid_across_pages() {
        let mut m = ShellModel {
            pages: vec![
                (0..PAGE_CAP).map(|i| format!("a{i:02}")).collect(),
                vec!["tail".into()],
            ],
            ..Default::default()
        };
        assert!(m.pin("a05"));
        assert!(m.dock.contains(&"a05".to_string()));
        assert_eq!(m.pages[0].len(), PAGE_CAP);
        assert_eq!(m.pages.len(), 1);
    }

    #[test]
    fn repack_keeps_one_page_when_home_is_empty() {
        let mut m = ShellModel {
            pages: vec![vec![], vec![]],
            ..Default::default()
        };
        m.repack();
        assert_eq!(m.pages, vec![Vec::<AppId>::new()]);
    }

    #[test]
    fn repack_cascades_overflow_and_drops_empty_tail() {
        let mut m = ShellModel {
            pages: vec![
                (0..=PAGE_CAP).map(|i| format!("a{i}")).collect(),
                vec![],
                vec![],
            ],
            ..Default::default()
        };
        m.repack();
        assert_eq!(m.pages[0].len(), PAGE_CAP);
        assert_eq!(m.pages[1], vec![format!("a{PAGE_CAP}")]);
        assert_eq!(m.pages.len(), 2);
    }
}
