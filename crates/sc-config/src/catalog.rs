use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct AppEntry {
    pub id: String, // file stem, e.g. "org.gnome.Maps"
    pub name: String,
    pub exec: String, // raw Exec line (field codes like %U left for the launcher to strip)
    pub icon: String, // icon name or absolute path
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
