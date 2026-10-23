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
    /// Unix seconds first seen in the catalog; 0 = present at bootstrap.
    #[serde(default)]
    pub added: u64,
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
                added: 0,
            }
        } else {
            AppStat {
                score: 1.0,
                last_launch: now,
                added: now,
            }
        };
        self.apps.insert(app.to_owned(), stat);
    }

    /// Newest first; apps present at bootstrap never count.
    pub fn recent(&self, n: usize) -> Vec<AppId> {
        let mut v: Vec<(&AppId, u64)> = self
            .apps
            .iter()
            .filter(|(_, s)| s.added > 0)
            .map(|(id, s)| (id, s.added))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        v.into_iter().take(n).map(|(id, _)| id.clone()).collect()
    }

    pub fn prune(&mut self, catalog_ids: &[AppId]) {
        self.apps.retain(|id, _| catalog_ids.contains(id));
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ShellModel {
    /// Only what the user placed. Everything else lives in the library, which
    /// is derived and never stored. Positional cells like the dock; a page
    /// with no apps survives only while arranging.
    #[serde(default)]
    pub pages: Vec<Vec<AppId>>,
    /// Fixed cells; `""` is an empty one. Trailing empties are trimmed.
    pub dock: Vec<AppId>,
    #[serde(default)]
    pub frecency: FrecencyStore,
}

/// Puts `app` in `cells[cell]`, shifting occupants toward `from` (the cell
/// it left) or else the nearest empty cell. With none free, the last cell's
/// app falls off and is returned.
fn put(
    cells: &mut Vec<AppId>,
    cap: usize,
    app: AppId,
    cell: usize,
    from: Option<usize>,
) -> Option<AppId> {
    let cell = cell.min(cap - 1);
    if cells.len() < cap {
        cells.resize(cap, AppId::new());
    }
    let mut spill = None;
    if !cells[cell].is_empty() {
        let gap = from
            .filter(|&f| f < cap && cells[f].is_empty())
            .or_else(|| {
                (0..cap)
                    .filter(|&i| cells[i].is_empty())
                    .min_by_key(|&i| i.abs_diff(cell))
            });
        match gap {
            Some(gap) if gap > cell => cells[cell..=gap].rotate_right(1),
            Some(gap) => cells[gap..=cell].rotate_left(1),
            None => {
                cells[cell..].rotate_right(1);
                spill = Some(std::mem::take(&mut cells[cell]));
            }
        }
    }
    cells[cell] = app;
    trim(cells);
    spill
}

fn trim(cells: &mut Vec<AppId>) {
    while cells.last().is_some_and(|a| a.is_empty()) {
        cells.pop();
    }
}

/// Returns the cell it held.
fn vacate(cells: &mut Vec<AppId>, app: &str) -> Option<usize> {
    let i = cells.iter().position(|a| a == app)?;
    cells[i].clear();
    trim(cells);
    Some(i)
}

impl ShellModel {
    /// Drop ids that are gone, seed frecency for new ones. New apps are not
    /// placed on a page; they reach Home only by drag from the library.
    pub fn reconcile(&mut self, catalog_ids: &[AppId], now: u64, first_run: bool) {
        let installed: HashSet<&AppId> = catalog_ids.iter().collect();
        for cells in self.pages.iter_mut().chain([&mut self.dock]) {
            for cell in cells.iter_mut() {
                if !installed.contains(cell) {
                    cell.clear();
                }
            }
            trim(cells);
        }
        self.frecency.prune(catalog_ids);
        for id in catalog_ids {
            self.frecency.seed(id, now, first_run);
        }
        // A persisted state.toml may break PAGE_CAP, hiding icons past the 24th.
        self.repack();
    }

    /// The first free cell on any page.
    pub fn place(&mut self, app: AppId) {
        for page in &mut self.pages {
            if let Some(cell) = page.iter_mut().find(|a| a.is_empty()) {
                *cell = app;
                return;
            }
            if page.len() < PAGE_CAP {
                page.push(app);
                return;
            }
        }
        self.pages.push(vec![app]);
    }

    /// It stays in the library. Leaves an emptied page for [`Self::repack`].
    pub fn delete(&mut self, app: &str) {
        for page in &mut self.pages {
            vacate(page, app);
        }
        vacate(&mut self.dock, app);
    }

    /// Into `cell` of `page`. A full page pushes its last icon onto the next.
    pub fn move_to(&mut self, app: &str, page: usize, cell: usize) {
        let from = self
            .pages
            .get_mut(page)
            .and_then(|cells| vacate(cells, app));
        self.delete(app);
        let mut item = app.to_string();
        let (mut page, mut cell, mut from) = (page, cell, from);
        loop {
            if self.pages.len() <= page {
                self.pages.resize(page + 1, Vec::new());
            }
            match put(&mut self.pages[page], PAGE_CAP, item, cell, from.take()) {
                Some(spill) => (item, page, cell) = (spill, page + 1, 0),
                None => break,
            }
        }
    }

    /// Returns its index. Reuses a trailing empty page rather than stacking them.
    pub fn add_page(&mut self) -> usize {
        if !self.pages.last().is_some_and(|p| p.is_empty()) {
            self.pages.push(Vec::new());
        }
        self.pages.len() - 1
    }

    /// Into dock `cell`, shifting occupants toward the nearest gap. False if
    /// the dock is full of other apps.
    pub fn pin(&mut self, app: &str, cell: usize) -> bool {
        let from = vacate(&mut self.dock, app);
        if self.dock.iter().filter(|a| !a.is_empty()).count() >= DOCK_CAP {
            return false;
        }
        put(&mut self.dock, DOCK_CAP, app.to_owned(), cell, from);
        if from.is_none() {
            for page in &mut self.pages {
                vacate(page, app);
            }
        }
        true
    }

    pub fn unpin(&mut self, app: &str) {
        if vacate(&mut self.dock, app).is_some() {
            self.place(app.to_owned());
        }
    }

    /// Caps every page and drops empty ones. Always keeps at least one page.
    pub fn repack(&mut self) {
        let mut i = 0;
        while i < self.pages.len() {
            if self.pages[i].len() > PAGE_CAP {
                let spill = self.pages[i].split_off(PAGE_CAP);
                if i + 1 == self.pages.len() {
                    self.pages.push(Vec::new());
                }
                self.pages[i + 1].splice(0..0, spill);
            }
            trim(&mut self.pages[i]);
            i += 1;
        }
        self.pages.retain(|p| !p.is_empty());
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
    fn delete_then_repack_keeps_one_empty_page() {
        let mut m = ShellModel::default();
        m.place("a".into());
        m.delete("a");
        m.repack();
        assert_eq!(m.pages, vec![Vec::<AppId>::new()]);
    }

    #[test]
    fn move_to_any_cell_leaves_gaps_before_it() {
        let mut m = ShellModel::default();
        m.place("a".into());
        m.place("b".into());
        m.move_to("b", 0, 6);
        assert_eq!(m.pages[0], vec!["a", "", "", "", "", "", "b"]);
        m.move_to("a", 0, 5);
        assert_eq!(m.pages[0], vec!["", "", "", "", "", "a", "b"]);
        m.place("c".into());
        assert_eq!(m.pages[0][0], "c");
    }

    #[test]
    fn move_within_a_page_shifts_toward_the_vacated_cell() {
        let mut m = ShellModel::default();
        for n in ["a", "b", "c"] {
            m.place(n.into());
        }
        m.move_to("a", 0, 2);
        assert_eq!(m.pages[0], vec!["b", "c", "a"]);
        m.move_to("a", 0, 0);
        assert_eq!(m.pages[0], vec!["a", "b", "c"]);
    }

    #[test]
    fn move_to_occupied_cell_shifts_toward_nearest_gap() {
        let mut m = ShellModel {
            pages: vec![vec!["a".into(), "b".into(), "".into(), "c".into()]],
            ..Default::default()
        };
        m.move_to("x", 0, 1);
        assert_eq!(m.pages[0], vec!["a", "x", "b", "c"]);
    }

    #[test]
    fn delete_leaves_a_gap() {
        let mut m = ShellModel {
            pages: vec![
                (0..PAGE_CAP).map(|i| format!("a{i:02}")).collect(),
                vec!["tail".into()],
            ],
            ..Default::default()
        };
        m.delete("a05");
        assert_eq!(m.pages[0][5], "");
        assert_eq!(m.pages[1], vec!["tail"]);
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
    fn move_to_cross_page_leaves_source_page_empty_until_repack() {
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
        assert!(m.pages[1].is_empty());
        m.repack();
        assert_eq!(m.pages.len(), 1);
    }

    #[test]
    fn move_to_empty_page_lands_there() {
        let mut m = ShellModel::default();
        for n in ["a", "b"] {
            m.place(n.into());
        }
        let page = m.add_page();
        assert_eq!(m.add_page(), page);
        m.move_to("b", page, 2);
        assert_eq!(m.pages, vec![vec!["a"], vec!["", "", "b"]]);
    }

    #[test]
    fn move_to_full_page_spills_its_last_icon() {
        let mut m = ShellModel {
            pages: vec![
                (0..PAGE_CAP).map(|i| format!("a{i:02}")).collect(),
                vec!["b".into(), "x".into()],
            ],
            ..Default::default()
        };
        m.move_to("x", 0, 0);
        assert_eq!(m.pages[0][0], "x");
        assert_eq!(m.pages[0].len(), PAGE_CAP);
        assert_eq!(
            m.pages[1],
            vec![format!("a{:02}", PAGE_CAP - 1), "b".into()]
        );
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
            added: 0,
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
                last_launch: 0,
                added: 0
            }
        );

        let mut populated = FrecencyStore::default();
        populated.record_launch("x", 100);
        populated.seed("b", 5000, false);
        assert_eq!(
            populated.apps["b"],
            AppStat {
                score: 1.0,
                last_launch: 5000,
                added: 5000
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
                    last_launch: 0,
                    added: 0
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
    fn pin_adds_to_dock_cell_leaving_gaps() {
        let mut m = ShellModel::default();
        assert!(m.pin("a", 2));
        assert_eq!(m.dock, vec!["", "", "a"]);
    }

    #[test]
    fn pin_fails_when_full() {
        let mut m = ShellModel::default();
        for i in 0..DOCK_CAP {
            assert!(m.pin(&format!("d{i}"), i));
        }
        assert!(!m.pin("overflow", 0));
        assert_eq!(m.dock, vec!["d0", "d1", "d2", "d3"]);
    }

    #[test]
    fn pin_reorders_within_a_full_dock() {
        let mut m = ShellModel::default();
        for i in 0..DOCK_CAP {
            m.pin(&format!("d{i}"), i);
        }
        assert!(m.pin("d3", 0));
        assert_eq!(m.dock, vec!["d3", "d0", "d1", "d2"]);
        assert!(m.pin("d3", 3));
        assert_eq!(m.dock, vec!["d0", "d1", "d2", "d3"]);
    }

    #[test]
    fn pin_onto_occupied_cell_shifts_toward_nearest_gap() {
        let mut m = ShellModel {
            dock: vec!["a".into(), "".into(), "b".into(), "c".into()],
            ..Default::default()
        };
        assert!(m.pin("x", 3));
        assert_eq!(m.dock, vec!["a", "b", "c", "x"]);
        let mut m = ShellModel {
            dock: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        assert!(m.pin("x", 0));
        assert_eq!(m.dock, vec!["x", "a", "b"]);
    }

    #[test]
    fn unpin_removes_from_dock() {
        let mut m = ShellModel::default();
        m.pin("a", 0);
        m.unpin("a");
        assert!(m.dock.is_empty());
    }

    #[test]
    fn pin_removes_from_pages_unpin_restores() {
        let mut m = ShellModel::default();
        m.place("a".into());
        assert!(m.pin("a", 0));
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
        assert_eq!(m.pages, vec![vec!["", "keep"]]);
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
                added: 0,
            },
        );
        m.reconcile(&["existing".into(), "new".into()], 100, false);
        assert_eq!(m.frecency.apps["new"].score, 1.0);
        assert_eq!(m.frecency.apps["new"].last_launch, 100);
        assert_eq!(m.frecency.apps["new"].added, 100);
        assert_eq!(m.frecency.recent(20), vec!["new".to_string()]);
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
    fn repack_keeps_gaps_and_drops_empty_pages() {
        let mut m = ShellModel {
            pages: vec![vec!["a".into()], vec![], vec!["b".into()]],
            ..Default::default()
        };
        m.repack();
        assert_eq!(m.pages, vec![vec!["a"], vec!["b"]]);
    }

    #[test]
    fn pin_leaves_a_gap_on_the_page() {
        let mut m = ShellModel {
            pages: vec![
                (0..PAGE_CAP).map(|i| format!("a{i:02}")).collect(),
                vec!["tail".into()],
            ],
            ..Default::default()
        };
        assert!(m.pin("a05", 0));
        assert_eq!(m.dock, vec!["a05"]);
        assert_eq!(m.pages[0][5], "");
        assert_eq!(m.pages[1], vec!["tail"]);
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
