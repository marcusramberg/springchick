//! App search ranking, used by the pull-down search app.
//!
//! Pure ordering of catalog apps: the default view (empty query) surfaces the
//! highest-frecency apps; typing filters the full catalog by a case-insensitive
//! name substring. Ordering is always frecency (decayed to `now`) descending,
//! tie-broken by name so the result is stable.

use std::collections::HashMap;

use crate::AppEntry;
use sc_shell_model::{eff, AppStat, FrecencyStore};

/// Order catalog apps for the search view.
///
/// `query` empty → the top `limit` apps by decayed frecency (the default view).
/// `query` non-empty → catalog entries whose name contains `query`
/// (case-insensitive), same ordering, capped at `limit`. Apps never launched
/// (frecency score 0, or absent from the store) still appear, ranked last.
pub fn rank(
    catalog: &HashMap<String, AppEntry>,
    frecency: &FrecencyStore,
    now: u64,
    query: &str,
    limit: usize,
) -> Vec<String> {
    let q = query.trim().to_lowercase();
    let zero = AppStat::default();
    let mut matches: Vec<(&String, &AppEntry, f64)> = catalog
        .iter()
        .filter(|(_, e)| q.is_empty() || e.name.to_lowercase().contains(&q))
        .map(|(id, e)| {
            let stat = frecency.apps.get(id).unwrap_or(&zero);
            (id, e, eff(stat, now))
        })
        .collect();

    matches.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
            .then_with(|| a.0.cmp(b.0))
    });

    matches
        .into_iter()
        .take(limit)
        .map(|(id, _, _)| id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str) -> (String, AppEntry) {
        (
            id.to_string(),
            AppEntry {
                id: id.to_string(),
                name: name.to_string(),
                exec: "x".into(),
                ..Default::default()
            },
        )
    }

    fn catalog(items: &[(&str, &str)]) -> HashMap<String, AppEntry> {
        items.iter().map(|(id, name)| entry(id, name)).collect()
    }

    #[test]
    fn empty_query_orders_by_frecency_and_caps() {
        let cat = catalog(&[("a", "Alpha"), ("b", "Bravo"), ("c", "Charlie")]);
        let mut fr = FrecencyStore::default();
        fr.record_launch("b", 1000);
        fr.record_launch("b", 1000);
        fr.record_launch("c", 1000);
        assert_eq!(rank(&cat, &fr, 1000, "", 5), vec!["b", "c", "a"]);
        assert_eq!(rank(&cat, &fr, 1000, "", 2), vec!["b", "c"]);
    }

    #[test]
    fn query_filters_by_name_substring_case_insensitive() {
        let cat = catalog(&[("fx", "Firefox"), ("ff", "Foot"), ("gm", "GNOME Maps")]);
        let fr = FrecencyStore::default();
        let out = rank(&cat, &fr, 0, "fo", 8);
        assert_eq!(out.len(), 2);
        assert!(out.contains(&"fx".to_string()));
        assert!(out.contains(&"ff".to_string()));
    }

    #[test]
    fn tie_break_is_stable_by_name() {
        let cat = catalog(&[("z", "Zed"), ("a", "Able"), ("m", "Mid")]);
        let fr = FrecencyStore::default();
        assert_eq!(rank(&cat, &fr, 0, "", 5), vec!["a", "m", "z"]);
    }

    #[test]
    fn never_launched_apps_are_included() {
        let cat = catalog(&[("a", "Alpha")]);
        let fr = FrecencyStore::default();
        assert_eq!(rank(&cat, &fr, 0, "", 5), vec!["a"]);
        assert_eq!(rank(&cat, &fr, 0, "alp", 5), vec!["a"]);
    }
}
