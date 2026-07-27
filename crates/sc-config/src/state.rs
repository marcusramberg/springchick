use sc_shell_model::ShellModel;
use std::path::Path;

pub fn save(model: &ShellModel, path: &Path) -> std::io::Result<()> {
    let s = toml::to_string_pretty(model).expect("serialize model");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state.toml")
    ));
    std::fs::write(&tmp, s)?;
    std::fs::rename(&tmp, path)
}

pub fn load(path: &Path) -> std::io::Result<ShellModel> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(toml::from_str(&s).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ShellModel::default()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_shell_model::ShellModel;

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("springchick/state.toml");
        let mut m = ShellModel::default();
        m.place("org.gnome.Maps".into());
        m.dock.push("org.gnome.Console".into());
        save(&m, &path).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(m.dock, back.dock);
        assert!(back.pages.is_empty()); // pages are not persisted
    }

    #[test]
    fn missing_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let m = load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(m, ShellModel::default());
    }

    #[test]
    fn round_trips_frecency() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("springchick/state.toml");
        let mut m = ShellModel::default();
        m.dock.push("org.gnome.Console".into());
        m.frecency.record_launch("org.gnome.Maps", 12345);
        save(&m, &path).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(m.dock, back.dock);
        assert_eq!(m.frecency, back.frecency);
        assert!(back.pages.is_empty()); // pages are not persisted
    }

    #[test]
    fn save_is_atomic_and_leaves_final_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("springchick/state.toml");
        let mut m = ShellModel::default();
        m.dock.push("org.gnome.Console".into());
        save(&m, &path).unwrap();
        assert!(path.exists());
        assert!(!path.with_file_name("state.toml.tmp").exists()); // tmp cleaned by rename
        let back = load(&path).unwrap();
        assert_eq!(m.dock, back.dock);
    }

    #[test]
    fn round_trips_hidden_and_dock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let mut m = ShellModel::default();
        m.pin("org.gnome.Console");
        m.hide("org.gnome.Maps");
        save(&m, &path).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(m.dock, back.dock);
        assert_eq!(m.hidden, back.hidden);
    }

    #[test]
    fn legacy_file_loads_dock_defaults_frecency() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        std::fs::write(
            &path,
            "dock = [\"org.gnome.Console\"]\npages = [[\"org.gnome.Maps\"]]\n",
        )
        .unwrap();
        let m = load(&path).unwrap();
        assert_eq!(m.dock, vec!["org.gnome.Console".to_string()]);
        assert!(m.frecency.apps.is_empty());
        assert!(m.pages.is_empty()); // legacy pages ignored (skipped field)
    }
}
