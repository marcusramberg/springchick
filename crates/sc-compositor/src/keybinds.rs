//! Keybinding glue between `sc-keys` and smithay: keysym resolution, the press
//! tracker, and running actions.

use crate::State;
use sc_config::{Action, Config, ModMask};
use sc_keys::{KeyBindings, PressOutcome, PressTracker};
use smithay::backend::input::{KeyState, Keycode};
use smithay::input::keyboard::{xkb, FilterResult, ModifiersState};
use smithay::utils::SERIAL_COUNTER;
use std::process::{Child, Command};
use std::time::Duration;
use std::time::Instant;
use tracing::{debug, info, warn};

pub struct Keys {
    pub tracker: PressTracker,
    pub children: Vec<Child>,
    /// Keysyms whose press the blanking policy ate, so their release is eaten too.
    pub swallowed: std::collections::HashSet<u32>,
}

impl Keys {
    /// Takes the same `Config` `State::new` read, so startup never sees two
    /// versions of the file.
    pub fn from_config(config: Config) -> Keys {
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
            swallowed: std::collections::HashSet::new(),
        }
    }
}

/// Case-sensitive.
pub fn resolve_keysym(name: &str) -> Option<u32> {
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    (sym != xkb::Keysym::NoSymbol).then(|| sym.raw())
}

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

/// Either Super key.
fn is_switch_modifier(keysym: u32) -> bool {
    keysym == xkb::keysyms::KEY_Super_L || keysym == xkb::keysyms::KEY_Super_R
}

/// Lock modifiers are dropped so a stuck Caps Lock can't disable bindings.
pub fn mod_mask(mods: &ModifiersState) -> ModMask {
    ModMask {
        ctrl: mods.ctrl,
        alt: mods.alt,
        shift: mods.shift,
        logo: mods.logo,
    }
}

pub fn spawn_command(command: &str, children: &mut Vec<Child>) {
    info!(command, "running keybinding command");
    match Command::new("sh").arg("-c").arg(command).spawn() {
        Ok(child) => children.push(child),
        Err(e) => warn!(%e, command, "failed to spawn keybinding command"),
    }
}

pub fn reap(children: &mut Vec<Child>) {
    children.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
}

/// Runs inside the keyboard filter closure so `Intercept` really withholds
/// the key.
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
            // Only name bound keys and modifiers: anything else is the user's typing,
            // lock-screen passwords included.
            debug!(
                target: "springchick::debug",
                "key {} {} mods={mask:?}",
                if state.keys.tracker.bindings().binds_keysym(keysym) || mask != ModMask::NONE {
                    xkb::keysym_get_name(handle.modified_sym())
                } else {
                    "(unbound)".to_string()
                },
                if pressed { "down" } else { "up" },
            );
            let outcome = if pressed {
                let wake_key = state.keys.tracker.bindings().binds_action(
                    keysym,
                    mask,
                    &Action::ToggleDisplay,
                );
                match state.blank.on_key_press(wake_key, state.external_display) {
                    crate::blank::KeyWhileBlanked::Woke
                    | crate::blank::KeyWhileBlanked::Swallow => {
                        // A client that gets a release without a press repeats the key forever.
                        state.keys.swallowed.insert(keysym);
                        PressOutcome::Swallow
                    }
                    crate::blank::KeyWhileBlanked::Normal => {
                        state.keys.tracker.on_press(keysym, mask, now)
                    }
                }
            } else {
                // The modifier is never bound, so its release still reaches the client.
                if is_switch_modifier(keysym) && !state.session_lock.is_locked() {
                    state.switcher_release();
                }
                if state.keys.swallowed.remove(&keysym) {
                    PressOutcome::Swallow
                } else {
                    state.keys.tracker.on_release(keysym, now)
                }
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

/// Lets the debug socket inject keys through the real path.
pub fn keycode_for_keysym(state: &mut State, keysym: u32) -> Option<Keycode> {
    let keyboard = state.keyboard.clone();
    keyboard.with_xkb_state(state, |ctx| {
        let xkb = ctx.xkb().lock().unwrap();
        let layout = xkb.active_layout();
        // evdev keycodes are xkb keycodes minus 8.
        (8u32..=255).map(Keycode::from).find(|code| {
            xkb.raw_syms_for_key_in_layout(*code, layout)
                .iter()
                .any(|s| s.raw() == keysym)
        })
    })
}

/// Called once per frame (winit) or per loop wake (DRM).
pub fn poll(state: &mut State) {
    let now = Instant::now();
    while let Some(action) = state.keys.tracker.poll(now) {
        run_action(state, action);
    }
    let mut children = std::mem::take(&mut state.keys.children);
    reap(&mut children);
    state.keys.children = children;
}

/// Only volume and the display toggle survive the lock; nothing that reaches
/// the shell or spawns a process.
pub fn allowed_while_locked(action: &Action) -> bool {
    match action {
        Action::VolumeUp | Action::VolumeDown | Action::VolumeMute | Action::ToggleDisplay => true,
        Action::Command(_)
        | Action::CloseApp
        | Action::Home
        | Action::ToggleFullscreen
        | Action::Search
        | Action::SwitcherNext
        | Action::SwitcherPrev
        | Action::Screenshot => false,
    }
}

pub fn run_action(state: &mut State, action: Action) {
    if state.session_lock.is_locked() && !allowed_while_locked(&action) {
        info!(
            action = action_name(&action),
            "keybinding suppressed (session locked)"
        );
        return;
    }
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
        Action::ToggleFullscreen => state.toggle_fullscreen(),
        Action::Search => state.open_search(),
        // Positive walks toward older apps, so Super+Tab lands on the previous app.
        Action::SwitcherNext => state.switcher_step(1),
        Action::SwitcherPrev => state.switcher_step(-1),
        Action::Screenshot => {
            state.screenshot_pending = true;
            state.needs_render = true;
        }
    }
}

enum VolumeChange {
    Up,
    Down,
    Mute,
}

const SINK: &str = "@DEFAULT_SINK@";

/// Synchronous (a few ms) so the read-back reflects the change.
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

pub fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Command(_) => "command",
        Action::CloseApp => "close-app",
        Action::Home => "home",
        Action::ToggleDisplay => "toggle-display",
        Action::VolumeUp => "volume-up",
        Action::VolumeDown => "volume-down",
        Action::VolumeMute => "volume-mute",
        Action::ToggleFullscreen => "toggle-fullscreen",
        Action::Search => "search",
        Action::SwitcherNext => "switcher-next",
        Action::SwitcherPrev => "switcher-prev",
        Action::Screenshot => "screenshot",
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
        assert!(resolve_keysym("ISO_Left_Tab").is_some());
        assert_eq!(resolve_keysym("NotAKeysym"), None);
    }

    #[test]
    fn every_default_binding_resolves() {
        assert_eq!(resolve(Config::defaults()).len(), 9);
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
    fn session_lock_suppresses_shell_actions() {
        assert!(!allowed_while_locked(&Action::Home));
        assert!(!allowed_while_locked(&Action::CloseApp));
        assert!(!allowed_while_locked(&Action::Command("foot".into())));
        assert!(allowed_while_locked(&Action::VolumeUp));
        assert!(allowed_while_locked(&Action::VolumeDown));
        assert!(allowed_while_locked(&Action::VolumeMute));
        assert!(allowed_while_locked(&Action::ToggleDisplay));
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
