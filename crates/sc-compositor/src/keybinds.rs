//! Keybinding glue: resolves keysym names, owns the press tracker, runs actions.
//!
//! This is the only module that knows both `sc-keys` types and smithay types.
//! The timing rules live in `sc-keys`; the I/O (xkb, config file, spawning)
//! lives here.

use crate::State;
use sc_keys::{Action, Config, KeyBindings, ModMask, PressOutcome, PressTracker};
use smithay::backend::input::{KeyState, Keycode};
use smithay::input::keyboard::{xkb, FilterResult, ModifiersState};
use smithay::utils::SERIAL_COUNTER;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;
use std::time::Instant;
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

/// Config lookup: `SPRINGCHICK_CONFIG` is a strict override — if set, it is the
/// only path tried, with no fallthrough to XDG or `/etc` if that file is
/// missing. Otherwise, in order: `$XDG_CONFIG_HOME/springchick/config.toml`
/// (or `~/.config/...`), then `/etc/springchick/config.toml`.
fn env_override(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("SPRINGCHICK_CONFIG").map(PathBuf::from)
}

/// XDG-then-`/etc` candidates, in lookup order. Takes an injectable env lookup
/// so tests don't mutate real process env vars (multithreaded test binary).
fn candidate_paths(env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let xdg = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("springchick/config.toml"));

    xdg.into_iter()
        .chain(std::iter::once(PathBuf::from(
            "/etc/springchick/config.toml",
        )))
        .collect()
}

/// Read the config file, falling back to the shipped defaults. A missing file is
/// normal; an unreadable or unparseable one is a warning, never fatal.
fn load_config() -> Config {
    let real_env = |k: &str| std::env::var(k).ok();

    if let Some(path) = env_override(real_env) {
        return match std::fs::read_to_string(&path) {
            Ok(text) => {
                info!(path = %path.display(), "loading config");
                Config::parse_or_defaults(&text)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::defaults(),
            Err(e) => {
                warn!(%e, path = %path.display(), "cannot read config; using defaults");
                Config::defaults()
            }
        };
    }

    for path in candidate_paths(real_env) {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                info!(path = %path.display(), "loading config");
                return Config::parse_or_defaults(&text);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warn!(%e, path = %path.display(), "cannot read config; trying next");
                continue;
            }
        }
    }
    Config::defaults()
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
    let entries = config
        .bindings
        .into_iter()
        .filter_map(|b| match resolve_keysym(&b.key) {
            Some(keysym) => Some((keysym, b.mods, b.press, b.action)),
            None => {
                warn!(key = %b.key, "skipping keybinding: unknown keysym name");
                None
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

/// Feed one key event through the bindings, forwarding it to the focused client
/// only when nothing is bound to it.
///
/// Runs inside the seat keyboard's filter closure so `Intercept` genuinely keeps
/// the key from the client.
pub fn on_key_event(state: &mut State, key_code: Keycode, key_state: KeyState, time: u32) {
    let keyboard = state.keyboard.clone();
    let now = Instant::now();
    let pressed = key_state == KeyState::Pressed;
    keyboard.input::<(), _>(
        state,
        key_code,
        key_state,
        SERIAL_COUNTER.next_serial(),
        time,
        |state, mods, handle| {
            let keysym = handle.modified_sym().raw();
            let mask = mod_mask(mods);
            let outcome = if pressed {
                // A press while the panel is blanked wakes it and fires nothing.
                if state.blank.on_key_press() == crate::blank::KeyWhileBlanked::Woke {
                    PressOutcome::Swallow
                } else {
                    state.keys.tracker.on_press(keysym, mask, now)
                }
            } else {
                state.keys.tracker.on_release(keysym, now)
            };
            match outcome {
                PressOutcome::Forward => FilterResult::Forward,
                PressOutcome::Swallow => FilterResult::Intercept(()),
                PressOutcome::Fire(action) => {
                    run_action(state, action);
                    FilterResult::Intercept(())
                }
            }
        },
    );
}

/// Find the keycode that produces `keysym` in the active layout.
///
/// Used by the debug socket so injected keys travel the same path as real ones
/// (xkb mapping, filter closure, client forwarding) rather than poking the
/// tracker directly.
pub fn keycode_for_keysym(state: &mut State, keysym: u32) -> Option<Keycode> {
    let keyboard = state.keyboard.clone();
    keyboard.with_xkb_state(state, |ctx| {
        let xkb = ctx.xkb().lock().unwrap();
        let layout = xkb.active_layout();
        // evdev keycodes are xkb keycodes minus 8; 8..=255 covers the keyboard.
        (8u32..=255).map(Keycode::from).find(|code| {
            xkb.raw_syms_for_key_in_layout(*code, layout)
                .iter()
                .any(|s| s.raw() == keysym)
        })
    })
}

/// Fire any long presses whose threshold has passed, and reap finished
/// commands. Called once per frame (winit) or per event-loop wake (DRM).
pub fn poll(state: &mut State) {
    let now = Instant::now();
    while let Some(action) = state.keys.tracker.poll(now) {
        run_action(state, action);
    }
    let mut children = std::mem::take(&mut state.keys.children);
    reap(&mut children);
    state.keys.children = children;
}

/// Perform a fired action.
pub fn run_action(state: &mut State, action: Action) {
    info!(action = action_name(&action), "keybinding fired");
    match action {
        Action::Command(cmd) => {
            let mut children = std::mem::take(&mut state.keys.children);
            spawn_command(&cmd, &mut children);
            state.keys.children = children;
        }
        Action::CloseApp => state.close_front_app(),
        Action::Home => state.handle_return_home(),
        Action::ToggleDisplay => state.blank.toggle(),
        Action::VolumeUp => adjust_volume(state, VolumeChange::Up),
        Action::VolumeDown => adjust_volume(state, VolumeChange::Down),
        Action::VolumeMute => adjust_volume(state, VolumeChange::Mute),
    }
}

/// One volume step.
enum VolumeChange {
    Up,
    Down,
    Mute,
}

/// The audio sink the volume actions target.
const SINK: &str = "@DEFAULT_SINK@";

/// Change the volume via `wpctl`, read it back, and refresh the OSD. Runs
/// `wpctl` synchronously (it returns in a few ms) so the read reflects the
/// change we just made.
fn adjust_volume(state: &mut State, change: VolumeChange) {
    let set_args: [&str; 3] = match change {
        VolumeChange::Up => ["set-volume", SINK, "5%+"],
        VolumeChange::Down => ["set-volume", SINK, "5%-"],
        VolumeChange::Mute => ["set-mute", SINK, "toggle"],
    };
    if let Err(e) = Command::new("wpctl").args(set_args).status() {
        warn!(%e, "wpctl set failed");
        return;
    }
    match Command::new("wpctl").args(["get-volume", SINK]).output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some((level, muted)) = crate::osd::parse_wpctl_volume(&text) {
                state.osd.show(level, muted, Instant::now());
            } else {
                warn!(output = %text.trim(), "could not parse wpctl volume");
            }
        }
        Err(e) => warn!(%e, "wpctl get-volume failed"),
    }
}

/// Names an action for logs.
pub fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Command(_) => "command",
        Action::CloseApp => "close-app",
        Action::Home => "home",
        Action::ToggleDisplay => "toggle-display",
        Action::VolumeUp => "volume-up",
        Action::VolumeDown => "volume-down",
        Action::VolumeMute => "volume-mute",
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
        let cfg = Config::parse(
            "[keybinds]\n[[keybinds.binding]]\nkey = \"Nonsense\"\npress = \"short\"\ncommand = \"true\"\n",
        );
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

    #[test]
    fn env_override_short_circuits_on_missing_var() {
        let env = |_: &str| None;
        assert_eq!(env_override(env), None);
    }

    #[test]
    fn env_override_uses_springchick_config_only() {
        let env = |k: &str| match k {
            "SPRINGCHICK_CONFIG" => Some("/tmp/x.toml".to_string()),
            _ => None,
        };
        assert_eq!(env_override(env), Some(PathBuf::from("/tmp/x.toml")));
    }

    #[test]
    fn candidate_paths_orders_xdg_then_etc() {
        let env = |k: &str| match k {
            "XDG_CONFIG_HOME" => Some("/home/u/.config".to_string()),
            _ => None,
        };
        assert_eq!(
            candidate_paths(env),
            vec![
                PathBuf::from("/home/u/.config/springchick/config.toml"),
                PathBuf::from("/etc/springchick/config.toml"),
            ]
        );
    }

    #[test]
    fn candidate_paths_falls_back_to_home_for_xdg() {
        let env = |k: &str| match k {
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        };
        assert_eq!(
            candidate_paths(env),
            vec![
                PathBuf::from("/home/u/.config/springchick/config.toml"),
                PathBuf::from("/etc/springchick/config.toml"),
            ]
        );
    }

    #[test]
    fn candidate_paths_omits_xdg_when_neither_var_set() {
        let env = |_: &str| None;
        assert_eq!(
            candidate_paths(env),
            vec![PathBuf::from("/etc/springchick/config.toml")],
        );
    }
}
