//! Category folders for the app library: `.desktop` `Categories=` → one folder
//! per app, in a fixed order. Derived on every scan — no folder state is ever
//! persisted, so an install/uninstall needs no reconciliation.

use crate::AppEntry;

/// A category folder in the library, in display order.
#[derive(Clone, Debug, PartialEq)]
pub struct Folder {
    pub name: &'static str,
    /// App ids, sorted by display name.
    pub apps: Vec<String>,
}

/// Folder order on the library page. `Other` is last and catches everything
/// unmatched.
const FOLDERS: &[&str] = &[
    "Media",
    "Network",
    "Office",
    "Graphics",
    "Development",
    "Game",
    "Science",
    "Utility",
    "System",
    "Settings",
    "Other",
];

/// Registered main categories mapped to our folders, best match first: an entry
/// declaring both `AudioVideo` and `Utility` lands in Media. Only main
/// categories are consulted; additional ones (`Player`, `TextEditor`, …) are
/// too numerous to be worth it and rarely appear alone.
const MAIN: &[(&str, &str)] = &[
    ("AudioVideo", "Media"),
    ("Audio", "Media"),
    ("Video", "Media"),
    ("Game", "Game"),
    // Office above Graphics: a document viewer declares both (Papers is
    // `Office;Viewer;Graphics`), and it is an Office app to a user.
    ("Office", "Office"),
    ("Graphics", "Graphics"),
    ("Development", "Development"),
    ("Science", "Science"),
    ("Education", "Science"),
    ("Network", "Network"),
    ("Settings", "Settings"),
    ("System", "System"),
    ("Utility", "Utility"),
];

/// App ids whose declared categories put them somewhere useless (or that
/// declare none at all). Checked before `Categories=`; the name must be one of
/// [`FOLDERS`] (asserted by `overrides_name_a_real_folder`).
///
/// Filled by hand from `dump_catalog` output; a Flathub-derived generator is
/// the eventual plan.
const OVERRIDES: &[(&str, &str)] = &[
    ("Music Assistant", "Media"), // declares no Categories at all
];

/// Group `entries` into library folders. Empty folders are dropped; apps within
/// a folder are sorted by display name (ties broken by id, so the order is
/// stable across scans).
pub fn folders(entries: &[AppEntry]) -> Vec<Folder> {
    let mut out: Vec<(&'static str, Vec<&AppEntry>)> =
        FOLDERS.iter().map(|n| (*n, Vec::new())).collect();
    for e in entries {
        let name = folder_for(e);
        let slot = out
            .iter_mut()
            .find(|(n, _)| *n == name)
            .expect("folder_for returns a FOLDERS name");
        slot.1.push(e);
    }
    out.into_iter()
        .filter(|(_, apps)| !apps.is_empty())
        .map(|(name, mut apps)| {
            apps.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
            Folder {
                name,
                apps: apps.into_iter().map(|e| e.id.clone()).collect(),
            }
        })
        .collect()
}

/// The folder a single entry belongs in. Always one of [`FOLDERS`].
pub fn folder_for(entry: &AppEntry) -> &'static str {
    if let Some((_, name)) = OVERRIDES.iter().find(|(id, _)| *id == entry.id) {
        return FOLDERS
            .iter()
            .find(|f| *f == name)
            .copied()
            .unwrap_or("Other");
    }
    MAIN.iter()
        .find(|(cat, _)| entry.categories.iter().any(|c| c == cat))
        .map(|(_, folder)| *folder)
        .unwrap_or("Other")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, cats: &[&str]) -> AppEntry {
        AppEntry {
            id: id.into(),
            name: name.into(),
            categories: cats.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn overrides_name_a_real_folder() {
        for (id, name) in OVERRIDES {
            assert!(
                FOLDERS.contains(name),
                "override for {id} names unknown folder {name}"
            );
        }
    }

    #[test]
    fn main_category_wins_in_listed_order() {
        // AudioVideo outranks Utility regardless of declaration order.
        assert_eq!(
            folder_for(&app("a", "A", &["Utility", "AudioVideo"])),
            "Media"
        );
        assert_eq!(
            folder_for(&app("b", "B", &["Network", "Development"])),
            "Development"
        );
        // A document viewer is Office, not Graphics.
        assert_eq!(
            folder_for(&app("c", "C", &["Office", "Viewer", "Graphics"])),
            "Office"
        );
    }

    #[test]
    fn override_beats_declared_categories() {
        let mut e = app("Music Assistant", "Music Assistant", &["Development"]);
        assert_eq!(folder_for(&e), "Media");
        e.id = "not-overridden".into();
        assert_eq!(folder_for(&e), "Development");
    }

    #[test]
    fn unknown_or_missing_categories_fall_to_other() {
        assert_eq!(folder_for(&app("a", "A", &[])), "Other");
        assert_eq!(folder_for(&app("b", "B", &["TextEditor", "Qt"])), "Other");
    }

    #[test]
    fn folders_drop_empties_and_sort_by_name() {
        let f = folders(&[
            app("z", "Zebra", &["Utility"]),
            app("a", "Apple", &["Utility"]),
            app("n", "Net", &["Network"]),
        ]);
        assert_eq!(
            f,
            vec![
                Folder {
                    name: "Network",
                    apps: vec!["n".into()]
                },
                Folder {
                    name: "Utility",
                    apps: vec!["a".into(), "z".into()]
                },
            ]
        );
    }

    #[test]
    fn folders_are_in_fixed_display_order() {
        let f = folders(&[
            app("s", "S", &["Settings"]),
            app("m", "M", &["Audio"]),
            app("o", "O", &[]),
        ]);
        let names: Vec<_> = f.iter().map(|f| f.name).collect();
        assert_eq!(names, ["Media", "Settings", "Other"]);
    }

    /// Not a test: dumps the *installed* catalog grouped by folder, so the
    /// OVERRIDES table can be written from real data rather than guesses.
    /// Run it on the target device after installing packages:
    ///
    ///   cargo test -p sc-catalog dump_catalog -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_catalog() {
        let entries = crate::scan_apps();
        println!("{} apps installed", entries.len());
        for folder in folders(&entries) {
            println!("\n== {} ({})", folder.name, folder.apps.len());
            for id in &folder.apps {
                let e = entries.iter().find(|e| &e.id == id).unwrap();
                println!("  {:<40} {}", e.id, e.categories.join(";"));
            }
        }
    }
}
