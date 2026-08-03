use std::path::{Path, PathBuf};

/// XDG base data directories, highest precedence first: `$XDG_DATA_HOME`
/// (default `~/.local/share`), then each `$XDG_DATA_DIRS` entry (default
/// `/usr/local/share:/usr/share`) left-to-right. Callers append `applications`
/// (desktop files) or `icons` (themes) to each.
pub fn xdg_data_dirs() -> Vec<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());

    let mut dirs: Vec<PathBuf> = data_home.into_iter().collect();
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppEntry {
    pub id: String, // file stem, e.g. "org.gnome.Maps"
    pub name: String,
    pub exec: String, // raw Exec line (field codes like %U left for the launcher to strip)
    pub icon: String, // icon name or absolute path
}

/// Scan `.desktop` files from every XDG data dir's `applications/`, highest
/// precedence first (the first entry seen for a given id wins). Skips
/// NoDisplay/Hidden/non-Application entries via [`parse_desktop`]. Shared by the
/// compositor and the search app so both see the same catalog.
pub fn scan_apps() -> Vec<AppEntry> {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in xdg_data_dirs() {
        let Ok(read) = std::fs::read_dir(dir.join("applications")) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "desktop") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Some(app) = parse_desktop(&path, &contents) {
                        if seen.insert(app.id.clone()) {
                            entries.push(app);
                        }
                    }
                }
            }
        }
    }
    entries
}

/// Strip desktop-file field codes (`%U %f %F %u %i %c %k` etc.) from an Exec
/// line, collapsing the whitespace left behind. Shared by the compositor's
/// launcher and the search app.
pub fn strip_field_codes(exec: &str) -> String {
    let mut result = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            chars.next(); // skip the field-code character
        } else {
            result.push(ch);
        }
    }
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse a single .desktop file. Returns None if it should not be shown
/// (NoDisplay=true, Hidden=true, or not a launchable Application).
pub fn parse_desktop(path: &Path, contents: &str) -> Option<AppEntry> {
    let id = path.file_stem()?.to_string_lossy().to_string();
    let mut name = None;
    let mut exec = None;
    let mut icon = String::new();
    let mut in_entry = false;
    let mut typ = String::new();
    let (mut no_display, mut hidden) = (false, false);
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "Name" if name.is_none() => name = Some(v.trim().to_string()),
            "Exec" => exec = Some(v.trim().to_string()),
            "Icon" => icon = v.trim().to_string(),
            "Type" => typ = v.trim().to_string(),
            "NoDisplay" => no_display = v.trim() == "true",
            "Hidden" => hidden = v.trim() == "true",
            _ => {}
        }
    }
    if no_display || hidden || typ != "Application" {
        return None;
    }
    Some(AppEntry {
        id,
        name: name?,
        exec: exec?,
        icon,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SAMPLE: &str =
        "[Desktop Entry]\nType=Application\nName=Maps\nExec=gnome-maps %U\nIcon=org.gnome.Maps\n";

    #[test]
    fn parses_basic_entry() {
        let e = parse_desktop(Path::new("/x/org.gnome.Maps.desktop"), SAMPLE).unwrap();
        assert_eq!(e.id, "org.gnome.Maps");
        assert_eq!(e.name, "Maps");
        assert_eq!(e.exec, "gnome-maps %U");
        assert_eq!(e.icon, "org.gnome.Maps");
    }

    #[test]
    fn skips_nodisplay() {
        let hidden = format!("{SAMPLE}NoDisplay=true\n");
        assert!(parse_desktop(Path::new("/x/a.desktop"), &hidden).is_none());
    }

    #[test]
    fn skips_non_application() {
        let link = "[Desktop Entry]\nType=Link\nName=X\nURL=http://x\n";
        assert!(parse_desktop(Path::new("/x/a.desktop"), link).is_none());
    }
}
