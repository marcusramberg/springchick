//! Keybinding glue: resolves keysym names, owns the press tracker, runs actions.
//!
//! This is the only module that knows both `sc-keys` types and smithay types.
//! The timing rules live in `sc-keys`; the I/O (xkb, config file, spawning)
//! lives here.

use sc_keys::{Action, Config, KeyBindings, ModMask, PressTracker};
use smithay::input::keyboard::{xkb, ModifiersState};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;
use tracing::{info, warn};

/// Keybinding runtime state owned by `State`.
pub struct Keys {
    pub tracker: PressTracker,
    /// Spawned binding commands, reaped as they exit.
    pub children: Vec<Child>,
}

impl Keys {
    /// Load the config from disk (or defaults) and resolve it.
    pub fn load() -> Keys {
        let config = load_config();
        let long_press = Duration::from_millis(config.long_press_ms);
        let bindings = resolve(config);
        info!(
            bindings = bindings.len(),
            long_press_ms = long_press.as_millis(),
            "keybindings loaded"
        );
        Keys {
            tracker: PressTracker::new(bindings),
            children: Vec::new(),
        }
    }
}

/// Config path: `SPRINGCHICK_KEYBINDS`, else `$XDG_CONFIG_HOME/springchick/`,
/// else `~/.config/springchick/`.
fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SPRINGCHICK_KEYBINDS") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok()?;
    Some(base.join("springchick/keybindings.toml"))
}

/// Read the config file, falling back to the shipped defaults. A missing file is
/// normal; an unreadable or unparseable one is a warning, never fatal.
fn load_config() -> Config {
    let Some(path) = config_path() else {
        return Config::defaults();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            info!(path = %path.display(), "loading keybindings");
            Config::parse_or_defaults(&text)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::defaults(),
        Err(e) => {
            warn!(%e, path = %path.display(), "cannot read keybindings; using defaults");
            Config::defaults()
        }
    }
}

/// xkb keysym name → raw keysym value. Case-sensitive, as xkb defines them.
pub fn resolve_keysym(name: &str) -> Option<u32> {
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    (sym != xkb::Keysym::NoSymbol).then(|| sym.raw())
}

/// Resolve a parsed config into keysym-keyed bindings, dropping names xkb does
/// not know.
pub fn resolve(config: Config) -> KeyBindings {
    let long_press = Duration::from_millis(config.long_press_ms);
    let entries = config.bindings.into_iter().filter_map(|b| {
        match resolve_keysym(&b.key) {
            Some(keysym) => Some((keysym, b.mods, b.press, b.action)),
            None => {
                warn!(key = %b.key, "skipping keybinding: unknown keysym name");
                None
            }
        }
    });
    KeyBindings::new(entries, long_press)
}

/// smithay modifiers → binding modifiers. Lock modifiers are dropped on
/// purpose: a stuck Caps Lock must not disable every binding.
pub fn mod_mask(mods: &ModifiersState) -> ModMask {
    ModMask {
        ctrl: mods.ctrl,
        alt: mods.alt,
        shift: mods.shift,
        logo: mods.logo,
    }
}

/// Run a binding's command through `sh -c`, detached. Mirrors `launcher.rs`:
/// log the failure, never block the compositor on a user command.
pub fn spawn_command(command: &str, children: &mut Vec<Child>) {
    info!(command, "running keybinding command");
    match Command::new("sh").arg("-c").arg(command).spawn() {
        Ok(child) => children.push(child),
        Err(e) => warn!(%e, command, "failed to spawn keybinding command"),
    }
}

/// Drop finished children so they do not linger as zombies.
pub fn reap(children: &mut Vec<Child>) {
    children.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
}

/// Names an action for logs.
pub fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Command(_) => "command",
        Action::CloseApp => "close-app",
        Action::Home => "home",
        Action::ToggleDisplay => "toggle-display",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_fp5_button_keysym_names() {
        assert!(resolve_keysym("XF86AudioRaiseVolume").is_some());
        assert!(resolve_keysym("XF86AudioLowerVolume").is_some());
        assert!(resolve_keysym("XF86PowerOff").is_some());
        assert!(resolve_keysym("Return").is_some());
        assert!(resolve_keysym("Escape").is_some());
        assert_eq!(resolve_keysym("NotAKeysym"), None);
    }

    #[test]
    fn every_default_binding_resolves() {
        // Distinct (keysym, mods) pairs: vol up, vol down, power, escape.
        assert_eq!(resolve(Config::defaults()).len(), 4);
    }

    #[test]
    fn unresolvable_names_are_dropped_not_fatal() {
        let cfg =
            Config::parse("[[binding]]\nkey = \"Nonsense\"\npress = \"short\"\ncommand = \"true\"\n");
        assert!(resolve(cfg).is_empty());
    }

    #[test]
    fn spawns_a_shell_command_and_reaps_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("fired");
        let mut children = Vec::new();
        spawn_command(&format!("touch {}", marker.display()), &mut children);
        assert_eq!(children.len(), 1);
        children[0].wait().unwrap();
        assert!(marker.exists());
        reap(&mut children);
        assert!(children.is_empty());
    }

    #[test]
    fn shell_metacharacters_work() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("piped");
        let mut children = Vec::new();
        spawn_command(
            &format!("echo hi | tee {} > /dev/null", marker.display()),
            &mut children,
        );
        children[0].wait().unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "hi");
    }

    #[test]
    fn lock_modifiers_are_ignored() {
        let mods = ModifiersState {
            caps_lock: true,
            num_lock: true,
            ..Default::default()
        };
        assert_eq!(mod_mask(&mods), ModMask::NONE);
    }
}
