# Keybindings and Wayland Keyboard Input Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bind the Fairphone 5's physical buttons (short and long press) to shell commands and internal compositor actions, and give the focused Wayland client real keyboard input on both backends.

**Architecture:** A new pure crate `sc-keys` holds the TOML binding config and a clock-injected short/long press state machine. A new compositor module `keybinds.rs` resolves keysym names via xkb, spawns `sh -c` commands, and translates internal actions. The seat keyboard moves into `State` so both the winit and DRM backends run one shared key path; keyboard focus is derived from `UiState` each frame. The DRM backend gains screen blanking so the power button has something to do.

**Tech Stack:** Rust 2021, smithay 0.7 (seat/keyboard/xkb, libinput, DRM/GBM), calloop 0.14, serde + toml, tracing.

**Spec:** `docs/superpowers/specs/2026-07-24-keybindings-design.md`

---

## File Structure

**Created:**
- `crates/sc-keys/Cargo.toml` — new workspace member
- `crates/sc-keys/src/lib.rs` — re-exports, crate docs
- `crates/sc-keys/src/config.rs` — `Binding`, `PressKind`, `Action`, `ModMask`, TOML parse, compiled-in defaults
- `crates/sc-keys/src/state.rs` — `KeyBindings` (resolved) + `PressTracker` (short/long machine over an injected clock)
- `crates/sc-compositor/src/keybinds.rs` — keysym-name resolution, config load, command spawn + reap, action dispatch
- `crates/sc-compositor/src/blank.rs` — display blank state shared by both backends

**Modified:**
- `Cargo.toml` — add `crates/sc-keys` to workspace members
- `crates/sc-compositor/Cargo.toml` — add `sc-keys` dependency
- `crates/sc-compositor/src/main.rs` — keyboard handle into `State`, `sync_keyboard_focus`, winit key path, per-frame poll
- `crates/sc-compositor/src/ui_state.rs` — `desired_focus`
- `crates/sc-compositor/src/drm_backend.rs` — `InputEvent::Keyboard` arm, 2ms-callback poll, CRTC blanking
- `crates/sc-compositor/src/debug_input.rs` — `key <name> [hold_ms]` verb
- `docs/RUNBOOK-device.md` — keybindings section

**Boundaries:** `sc-keys` never touches smithay, wayland, or the filesystem beyond reading one TOML string — it takes `&str` config text and an injected `Instant`, so every rule in the spec is unit-testable without a compositor. `keybinds.rs` is the only place that knows both `sc-keys` types and smithay types.

---

### Task 1: `sc-keys` crate skeleton and config types

**Files:**
- Create: `crates/sc-keys/Cargo.toml`, `crates/sc-keys/src/lib.rs`, `crates/sc-keys/src/config.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Write the failing test**

In `crates/sc-keys/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_command_binding() {
        let cfg = Config::parse(
            r#"
            [[binding]]
            key = "XF86AudioRaiseVolume"
            press = "short"
            command = "wpctl set-volume @DEFAULT_SINK@ 5%+"
            "#,
        );
        assert_eq!(cfg.long_press_ms, 500);
        assert_eq!(cfg.bindings.len(), 1);
        let b = &cfg.bindings[0];
        assert_eq!(b.key, "XF86AudioRaiseVolume");
        assert_eq!(b.press, PressKind::Short);
        assert_eq!(b.mods, ModMask::NONE);
        assert_eq!(
            b.action,
            Action::Command("wpctl set-volume @DEFAULT_SINK@ 5%+".into())
        );
    }

    #[test]
    fn parses_an_internal_action_with_mods() {
        let cfg = Config::parse(
            r#"
            [[binding]]
            key = "Return"
            mods = ["Super", "Shift"]
            press = "long"
            action = "close-app"
            "#,
        );
        let b = &cfg.bindings[0];
        assert_eq!(b.press, PressKind::Long);
        assert!(b.mods.logo && b.mods.shift && !b.mods.ctrl && !b.mods.alt);
        assert_eq!(b.action, Action::CloseApp);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p sc-keys`
Expected: FAIL — the package does not exist yet (`error: package ID specification 'sc-keys' did not match any packages`).

- [ ] **Step 3: Write minimal implementation**

`crates/sc-keys/Cargo.toml`:

```toml
[package]
name = "sc-keys"
version = "0.1.0"
edition.workspace = true
license.workspace = true

[dependencies]
serde = { version = "1", features = ["derive"] }
toml = "0.8"
tracing = "0.1"
```

Add `"crates/sc-keys",` to `members` in the workspace `Cargo.toml`.

`crates/sc-keys/src/lib.rs`:

```rust
#![forbid(unsafe_code)]
//! Keybinding config and short/long press logic for springchick.
//!
//! Pure logic: no wayland, no xkb, no clock of its own. The compositor resolves
//! keysym names and supplies `Instant`s, so every rule here is unit-testable.

pub mod config;
pub mod state;

pub use config::{Action, Binding, Config, ModMask, PressKind};
pub use state::{KeyBindings, PressOutcome, PressTracker};
```

`crates/sc-keys/src/config.rs` — the types plus a lenient parse. Unknown/invalid entries are dropped with a warning, never fatal (a phone that will not boot is a recovery session):

```rust
use serde::Deserialize;
use tracing::warn;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModMask {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl ModMask {
    pub const NONE: ModMask = ModMask { ctrl: false, alt: false, shift: false, logo: false };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressKind {
    Short,
    Long,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Command(String),
    CloseApp,
    Home,
    ToggleDisplay,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub key: String,
    pub mods: ModMask,
    pub press: PressKind,
    pub action: Action,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub long_press_ms: u64,
    pub bindings: Vec<Binding>,
}

/// Serde mirror of the on-disk shape. Kept separate so the public types stay
/// free of `Option` soup and the validation lives in one place.
#[derive(Deserialize)]
struct RawConfig {
    long_press_ms: Option<u64>,
    #[serde(default)]
    binding: Vec<RawBinding>,
}

#[derive(Deserialize)]
struct RawBinding {
    key: String,
    #[serde(default)]
    mods: Vec<String>,
    press: String,
    command: Option<String>,
    action: Option<String>,
}
```

`Config::parse(&str) -> Config` maps each `RawBinding`, skipping with `warn!` when: `press` is neither `short` nor `long`; both or neither of `command`/`action` are set; `action` is unknown; a `mods` entry is unknown. A whole-file parse error returns `Config::defaults()` (Task 3) — for now have it return an empty config so this task compiles standalone.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sc-keys`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/sc-keys
git commit -m "feat(keys): add sc-keys crate with binding config types"
```

---

### Task 2: Lenient config validation

**Files:**
- Modify: `crates/sc-keys/src/config.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn skips_invalid_entries_without_failing() {
    let cfg = Config::parse(
        r#"
        [[binding]]
        key = "A"
        press = "sideways"
        command = "true"

        [[binding]]
        key = "B"
        press = "short"

        [[binding]]
        key = "C"
        press = "short"
        command = "true"
        action = "home"

        [[binding]]
        key = "D"
        press = "short"
        action = "not-a-real-action"

        [[binding]]
        key = "E"
        press = "short"
        command = "true"
        "#,
    );
    // Only the last entry survives.
    assert_eq!(cfg.bindings.len(), 1);
    assert_eq!(cfg.bindings[0].key, "E");
}

#[test]
fn malformed_toml_yields_empty_not_panic() {
    let cfg = Config::parse("this is not toml {{{");
    assert!(cfg.bindings.is_empty());
}

#[test]
fn custom_long_press_ms_is_read() {
    let cfg = Config::parse("long_press_ms = 800\n");
    assert_eq!(cfg.long_press_ms, 800);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sc-keys config::tests`
Expected: FAIL on the skipping cases until validation exists.

- [ ] **Step 3: Implement validation**

Complete the `RawBinding` → `Binding` mapping per Task 1's rules. Each rejection logs `warn!(key = %raw.key, reason, "skipping keybinding")`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sc-keys`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-keys/src/config.rs
git commit -m "feat(keys): drop invalid bindings instead of failing to start"
```

---

### Task 3: Compiled-in defaults

**Files:**
- Modify: `crates/sc-keys/src/config.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn defaults_cover_the_fp5_buttons() {
    let cfg = Config::defaults();
    let find = |key: &str, press: PressKind| {
        cfg.bindings.iter().find(|b| b.key == key && b.press == press).cloned()
    };
    assert!(matches!(
        find("XF86AudioRaiseVolume", PressKind::Short).unwrap().action,
        Action::Command(ref c) if c.contains("wpctl")
    ));
    assert_eq!(find("XF86AudioRaiseVolume", PressKind::Long).unwrap().action, Action::CloseApp);
    assert!(matches!(
        find("XF86AudioLowerVolume", PressKind::Long).unwrap().action,
        Action::Command(ref c) if c.contains("wvkbd-mobintl")
    ));
    assert_eq!(find("XF86PowerOff", PressKind::Short).unwrap().action, Action::ToggleDisplay);
    assert!(matches!(
        find("XF86PowerOff", PressKind::Long).unwrap().action,
        Action::Command(ref c) if c.contains("poweroff")
    ));
}

#[test]
fn defaults_parse_from_the_shipped_text() {
    // The defaults are defined as TOML so the file we document and the
    // behavior we ship cannot drift apart.
    assert_eq!(Config::parse(DEFAULT_TOML).bindings.len(), Config::defaults().bindings.len());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-keys defaults`
Expected: FAIL — `Config::defaults` not found.

- [ ] **Step 3: Implement**

```rust
/// Shipped defaults, mirroring the user's niri bindings. Defined as TOML so the
/// documented example and the built-in behavior are the same text.
pub const DEFAULT_TOML: &str = r#"
long_press_ms = 500

[[binding]]
key = "XF86AudioRaiseVolume"
press = "short"
command = "wpctl set-volume @DEFAULT_SINK@ 5%+"

[[binding]]
key = "XF86AudioRaiseVolume"
press = "long"
action = "close-app"

[[binding]]
key = "XF86AudioLowerVolume"
press = "short"
command = "wpctl set-volume @DEFAULT_SINK@ 5%-"

[[binding]]
key = "XF86AudioLowerVolume"
press = "long"
command = "pkill -SIGRTMIN -f wvkbd-mobintl"

[[binding]]
key = "XF86PowerOff"
press = "short"
action = "toggle-display"

[[binding]]
key = "XF86PowerOff"
press = "long"
command = "systemctl poweroff"
"#;

impl Config {
    pub fn defaults() -> Config {
        Config::parse(DEFAULT_TOML)
    }
}
```

Change the malformed-TOML path in `parse` to return `Config { long_press_ms: 500, bindings: vec![] }` and add a separate `Config::load_str_or_defaults(&str) -> Config` that falls back to `defaults()` on a whole-file parse error, so callers get working buttons rather than none.

- [ ] **Step 4: Run tests**

Run: `cargo test -p sc-keys`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-keys/src/config.rs
git commit -m "feat(keys): ship default fp5 button bindings"
```

---

### Task 4: `PressTracker` short/long state machine

**Files:**
- Create: `crates/sc-keys/src/state.rs`

This is the heart of the feature. Every spec rule gets a test.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Action, ModMask, PressKind};
    use std::time::{Duration, Instant};

    const VOL_UP: u32 = 100; // stand-in keysym values; resolution happens elsewhere
    const VOL_DOWN: u32 = 200;
    const UNBOUND: u32 = 999;

    fn bindings() -> KeyBindings {
        KeyBindings::new(
            vec![
                (VOL_UP, ModMask::NONE, PressKind::Short, Action::Command("short".into())),
                (VOL_UP, ModMask::NONE, PressKind::Long, Action::Command("long".into())),
                (VOL_DOWN, ModMask::NONE, PressKind::Long, Action::Command("down-long".into())),
            ],
            Duration::from_millis(500),
        )
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn short_press_fires_on_release() {
        let (mut t, t0) = (PressTracker::new(bindings()), Instant::now());
        assert_eq!(t.on_press(VOL_UP, ModMask::NONE, t0), PressOutcome::Swallow);
        assert_eq!(
            t.on_release(VOL_UP, at(t0, 100)),
            PressOutcome::Fire(Action::Command("short".into()))
        );
    }

    #[test]
    fn long_press_fires_at_the_threshold_without_release() {
        let (mut t, t0) = (PressTracker::new(bindings()), Instant::now());
        t.on_press(VOL_UP, ModMask::NONE, t0);
        assert_eq!(t.poll(at(t0, 499)), None);
        assert_eq!(t.poll(at(t0, 500)), Some(Action::Command("long".into())));
    }

    #[test]
    fn short_is_suppressed_once_long_fired() {
        let (mut t, t0) = (PressTracker::new(bindings()), Instant::now());
        t.on_press(VOL_UP, ModMask::NONE, t0);
        t.poll(at(t0, 500));
        assert_eq!(t.on_release(VOL_UP, at(t0, 900)), PressOutcome::Swallow);
    }

    #[test]
    fn long_fires_only_once_while_held() {
        let (mut t, t0) = (PressTracker::new(bindings()), Instant::now());
        t.on_press(VOL_UP, ModMask::NONE, t0);
        assert!(t.poll(at(t0, 500)).is_some());
        assert_eq!(t.poll(at(t0, 700)), None);
    }

    #[test]
    fn key_with_only_a_long_binding_still_swallows_the_short_press() {
        let (mut t, t0) = (PressTracker::new(bindings()), Instant::now());
        assert_eq!(t.on_press(VOL_DOWN, ModMask::NONE, t0), PressOutcome::Swallow);
        assert_eq!(t.on_release(VOL_DOWN, at(t0, 100)), PressOutcome::Swallow);
    }

    #[test]
    fn unbound_keys_forward_both_ways() {
        let (mut t, t0) = (PressTracker::new(bindings()), Instant::now());
        assert_eq!(t.on_press(UNBOUND, ModMask::NONE, t0), PressOutcome::Forward);
        assert_eq!(t.on_release(UNBOUND, at(t0, 10)), PressOutcome::Forward);
    }

    #[test]
    fn wrong_modifiers_do_not_match() {
        let mods = ModMask { ctrl: true, ..ModMask::NONE };
        let (mut t, t0) = (PressTracker::new(bindings()), Instant::now());
        assert_eq!(t.on_press(VOL_UP, mods, t0), PressOutcome::Forward);
    }

    #[test]
    fn repeat_press_of_a_held_key_is_ignored() {
        let (mut t, t0) = (PressTracker::new(bindings()), Instant::now());
        t.on_press(VOL_UP, ModMask::NONE, t0);
        assert_eq!(t.on_press(VOL_UP, ModMask::NONE, at(t0, 50)), PressOutcome::Swallow);
        // The original press time still governs the long threshold.
        assert!(t.poll(at(t0, 500)).is_some());
    }

    #[test]
    fn next_deadline_is_the_earliest_of_two_held_keys() {
        let (mut t, t0) = (PressTracker::new(bindings()), Instant::now());
        t.on_press(VOL_DOWN, ModMask::NONE, at(t0, 100));
        t.on_press(VOL_UP, ModMask::NONE, t0);
        assert_eq!(t.next_deadline(), Some(at(t0, 500)));
    }

    #[test]
    fn release_of_a_never_pressed_key_is_harmless() {
        let (mut t, t0) = (PressTracker::new(bindings()), Instant::now());
        assert_eq!(t.on_release(VOL_UP, t0), PressOutcome::Swallow);
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p sc-keys state`
Expected: FAIL — `state` module does not exist.

- [ ] **Step 3: Implement**

```rust
pub enum PressOutcome { Fire(Action), Swallow, Forward }
```

`KeyBindings` holds a `HashMap<(u32, ModMask), (Option<Action> /*short*/, Option<Action> /*long*/)>` plus `long_press: Duration`. `PressTracker` holds `held: HashMap<u32, Held { mods, pressed_at, long_fired }>`.

- `on_press`: if unbound → `Forward`. If already held → `Swallow` (keep original `pressed_at`). Else record and `Swallow`.
- `on_release`: remove the entry; if it existed, was not `long_fired`, elapsed `< long_press`, and a short action exists → `Fire`; otherwise `Swallow`. Unknown key → `Forward` only if unbound, else `Swallow`.
- `poll(now)`: for each held entry not yet `long_fired` whose elapsed `>= long_press` and that has a long action, mark `long_fired` and return it. (Return one action per call; the caller polls in a loop.)
- `next_deadline`: min of `pressed_at + long_press` over held entries that have a long action and have not fired.

Note the boundary the tests pin: elapsed `>= long_press` fires, so exactly 500ms is a long press.

- [ ] **Step 4: Run tests**

Run: `cargo test -p sc-keys`
Expected: PASS, 17 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-keys/src/state.rs crates/sc-keys/src/lib.rs
git commit -m "feat(keys): short/long press tracker over an injected clock"
```

---

### Task 5: Compositor-side keysym resolution and config loading

**Files:**
- Create: `crates/sc-compositor/src/keybinds.rs`
- Modify: `crates/sc-compositor/Cargo.toml`, `crates/sc-compositor/src/main.rs` (add `mod keybinds;`)

- [ ] **Step 1: Write the failing test**

In `keybinds.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_fp5_button_keysym_names() {
        // Catches a misspelled default before it reaches the phone.
        assert!(resolve_keysym("XF86AudioRaiseVolume").is_some());
        assert!(resolve_keysym("XF86AudioLowerVolume").is_some());
        assert!(resolve_keysym("XF86PowerOff").is_some());
        assert!(resolve_keysym("Return").is_some());
        assert_eq!(resolve_keysym("NotAKeysym"), None);
    }

    #[test]
    fn every_default_binding_resolves() {
        let bindings = resolve(sc_keys::Config::defaults());
        assert_eq!(bindings.len(), 6);
    }

    #[test]
    fn unresolvable_names_are_dropped_not_fatal() {
        let cfg = sc_keys::Config::parse(
            "[[binding]]\nkey = \"Nonsense\"\npress = \"short\"\ncommand = \"true\"\n",
        );
        assert_eq!(resolve(cfg).len(), 0);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-compositor keybinds`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement**

Add `sc-keys = { path = "../sc-keys" }` to `crates/sc-compositor/Cargo.toml`, `mod keybinds;` to `main.rs`.

```rust
//! Keybinding glue: resolves keysym names, owns the press tracker, runs actions.

use smithay::input::keyboard::xkb;
use sc_keys::{Action, Config, KeyBindings, ModMask, PressKind};
use tracing::{info, warn};

/// xkb keysym name → raw keysym value. Case-sensitive, as xkb defines them.
pub fn resolve_keysym(name: &str) -> Option<u32> {
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    (sym.raw() != xkb::keysyms::KEY_NoSymbol).then(|| sym.raw())
}
```

`resolve(Config) -> KeyBindings` maps each binding through `resolve_keysym`, `warn!`ing and skipping unresolvable names. `load() -> KeyBindings` reads `SPRINGCHICK_KEYBINDS`, else `$XDG_CONFIG_HOME/springchick/keybindings.toml`, else `~/.config/springchick/keybindings.toml`; a missing file uses `Config::defaults()`, an unreadable or unparseable one logs and falls back to defaults. Log the resolved binding count at startup so the device log shows what took effect.

- [ ] **Step 4: Run tests**

Run: `cargo test -p sc-compositor keybinds`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/Cargo.toml crates/sc-compositor/src/keybinds.rs crates/sc-compositor/src/main.rs Cargo.lock
git commit -m "feat(keybinds): resolve keysym names and load the binding config"
```

---

### Task 6: Running actions — `sh -c` spawn and reaping

**Files:**
- Modify: `crates/sc-compositor/src/keybinds.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn spawns_a_shell_command_and_reaps_it() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("fired");
    let mut children = Vec::new();
    spawn_command(&format!("touch {}", marker.display()), &mut children);
    assert_eq!(children.len(), 1);
    // Wait for it rather than sleeping blindly.
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
    spawn_command(&format!("echo hi | tee {} > /dev/null", marker.display()), &mut children);
    children[0].wait().unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "hi");
}
```

Add `tempfile` to `crates/sc-compositor/Cargo.toml` under `[dev-dependencies]` if not already present.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-compositor keybinds`
Expected: FAIL — `spawn_command` not found.

- [ ] **Step 3: Implement**

```rust
/// Run a binding's command through `sh -c`, detached. Mirrors launcher.rs:
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
```

The command inherits the compositor's environment, so `WAYLAND_DISPLAY` points at springchick's socket — `wpctl` and `pkill` do not care, but a command that opens a window will land on the right compositor.

- [ ] **Step 4: Run tests**

Run: `cargo test -p sc-compositor keybinds`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/keybinds.rs crates/sc-compositor/Cargo.toml
git commit -m "feat(keybinds): run binding commands through sh -c and reap them"
```

---

### Task 7: Move the seat keyboard into `State`

**Files:**
- Modify: `crates/sc-compositor/src/main.rs:213-214` (seat construction), `:275-276` (struct init), `:632-636` (winit `add_keyboard`)

This is a refactor with no behavior change; it is what lets the DRM backend have a keyboard at all.

- [ ] **Step 1: Add the field**

Add to `struct State`:

```rust
    /// Seat keyboard. Owned by State (not the winit loop) so both backends
    /// share one key path.
    keyboard: KeyboardHandle<Self>,
    /// Resolved keybindings + in-flight press state.
    keys: keybinds::Keys,
```

Where `keybinds::Keys` bundles `PressTracker` and the spawned-children `Vec<Child>`.

- [ ] **Step 2: Construct it in `State::new`**

Right after `let seat = seat_state.new_wl_seat(&dh, "springchick");`:

```rust
        // 200ms delay / 25Hz repeat: xkb defaults, forwarded to clients.
        let keyboard = seat
            .add_keyboard(XkbConfig::default(), 200, 25)
            .expect("add keyboard");
```

- [ ] **Step 3: Delete the winit-local keyboard**

Remove the `add_keyboard` block at `main.rs:632-636` and change `handle_winit_input`'s signature to drop the `keyboard: &KeyboardHandle<State>` parameter, taking it from `state.keyboard.clone()` instead (cloning the handle is cheap and avoids borrowing `state` twice).

- [ ] **Step 4: Verify nothing broke**

Run: `cargo build -p sc-compositor && cargo test --workspace`
Expected: builds; all existing tests pass. Esc-to-home still works in the winit dev window (manual check: `cargo run -p sc-compositor`, launch an app, press Esc).

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/main.rs
git commit -m "refactor(compositor): own the seat keyboard in State"
```

---

### Task 8: Keyboard focus follows the UI state

**Files:**
- Modify: `crates/sc-compositor/src/ui_state.rs`, `crates/sc-compositor/src/main.rs`

- [ ] **Step 1: Write the failing test**

In `ui_state.rs` tests:

```rust
#[test]
fn focus_only_when_an_app_is_settled() {
    let home = UiState::home(0, 1);
    assert_eq!(desired_focus(&home), None);

    let app = UiState::App { toplevel: 3, app_id: "x".into() };
    assert_eq!(desired_focus(&app), Some(3));

    let mut progress = Spring::new(0.0);
    progress.retarget(1.0);
    let opening = UiState::AppOpening {
        toplevel: 3,
        app_id: "x".into(),
        progress,
        origin: ZoomOrigin::icon((0.0, 0.0)),
    };
    assert_eq!(desired_focus(&opening), None);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-compositor ui_state::tests::focus`
Expected: FAIL — `desired_focus` not found.

- [ ] **Step 3: Implement**

```rust
/// Which toplevel should hold keyboard focus in this state.
///
/// Only the settled `App` state focuses a client: during zoom, grab, settle and
/// switcher the compositor owns the screen, and a mapped-but-hidden app must not
/// eat keys.
pub fn desired_focus(state: &UiState) -> Option<ToplevelId> {
    match state {
        UiState::App { toplevel, .. } => Some(*toplevel),
        _ => None,
    }
}
```

In `main.rs`, add to `impl State`:

```rust
    /// Push `desired_focus` into the seat keyboard when it changed.
    fn sync_keyboard_focus(&mut self) {
        let want = ui_state::desired_focus(&self.ui)
            .and_then(|tid| self.toplevels.get(tid))
            .and_then(|slot| slot.as_ref())
            .map(|tl| tl.surface.wl_surface().clone());
        if want == self.focused_surface {
            return;
        }
        self.focused_surface = want.clone();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, want, SERIAL_COUNTER.next_serial());
    }
```

Add the `focused_surface: Option<WlSurface>` field to `State` (initialized `None`) so the comparison avoids re-sending focus every frame.

- [ ] **Step 4: Call it once per frame in both backends**

In `render_frame` (winit, `main.rs`) and `App::render` (`drm_backend.rs`), right after the `Tick` transition, call `state.sync_keyboard_focus()`.

- [ ] **Step 5: Verify**

Run: `cargo test --workspace`
Expected: PASS. Manual: `cargo run -p sc-compositor`, launch a terminal app, type — characters appear; press Esc to go home, type — nothing reaches the app.

- [ ] **Step 6: Commit**

```bash
git add crates/sc-compositor/src/ui_state.rs crates/sc-compositor/src/main.rs crates/sc-compositor/src/drm_backend.rs
git commit -m "feat(compositor): give the focused app keyboard focus"
```

---

### Task 9: Shared key path in the winit backend

**Files:**
- Modify: `crates/sc-compositor/src/keybinds.rs`, `crates/sc-compositor/src/main.rs:724-750`

- [ ] **Step 1: Implement the shared entry point**

In `keybinds.rs`:

```rust
/// Decide what happens to one key event. Called from inside the seat keyboard's
/// filter closure, so returning `Swallow` keeps the key from the client.
pub fn on_key(
    state: &mut State,
    keysym: u32,
    mods: ModMask,
    pressed: bool,
    now: Instant,
) -> PressOutcome
```

It calls `on_press`/`on_release`, and on `Fire(action)` calls `run_action(state, action)`. `run_action` handles `Command` (spawn), `CloseApp` (transition `UiEvent` closing the front toplevel — reuse the path behind `Effect::CloseToplevel` at `main.rs:780`), `Home` (`state.handle_return_home()`), `ToggleDisplay` (Task 11; log-only for now).

Add a `ModMask` conversion from smithay's `ModifiersState`, deliberately ignoring `caps_lock`/`num_lock` so a stuck lock key cannot disable every binding.

- [ ] **Step 2: Wire the winit path**

Replace the Esc special-case in `handle_winit_input` with:

```rust
        InputEvent::Keyboard { event } => {
            let keyboard = state.keyboard.clone();
            let now = Instant::now();
            keyboard.input::<(), _>(state, event.key_code(), event.state(), SERIAL_COUNTER.next_serial(), event.time(), |state, mods, handle| {
                let pressed = /* KeyState::Pressed */;
                match keybinds::on_key(state, handle.modified_sym().raw(), mods.into(), pressed, now) {
                    PressOutcome::Forward => FilterResult::Forward,
                    _ => FilterResult::Intercept(()),
                }
            });
        }
```

Keep the existing Esc → home behavior by adding an `Escape` short binding to the compiled-in defaults rather than special-casing it in code; delete `input_common::on_escape` and its call sites once nothing references it.

- [ ] **Step 3: Poll long presses per frame**

In `render_frame`, before rendering:

```rust
    let now = Instant::now();
    while let Some(action) = state.keys.tracker.poll(now) {
        keybinds::run_action(state, action);
    }
    keybinds::reap(&mut state.keys.children);
```

- [ ] **Step 4: Verify**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Manual: `cargo run -p sc-compositor`; Esc still returns home; a bound test key runs its command.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src
git commit -m "feat(keybinds): route winit keys through the binding tracker"
```

---

### Task 10: DRM backend keyboard support

**Files:**
- Modify: `crates/sc-compositor/src/drm_backend.rs:206-225` (`handle_input`), `:180-186` (2ms loop callback)

- [ ] **Step 1: Add the keyboard arm**

```rust
            InputEvent::Keyboard { event } => {
                let keyboard = self.state.keyboard.clone();
                let now = Instant::now();
                keyboard.input::<(), _>(
                    &mut self.state,
                    event.key_code(),
                    event.state(),
                    SERIAL_COUNTER.next_serial(),
                    event.time(),
                    |state, mods, handle| { /* identical body to the winit filter */ },
                );
            }
```

Factor the filter body into `keybinds::filter(state, mods, handle, pressed, now)` so both backends call one function rather than duplicating it.

- [ ] **Step 2: Poll long presses in the 2ms callback**

The DRM loop already wakes every 2ms for wayland dispatch, and page-flips stop when the screen is idle, so the poll belongs there — not in `render`:

```rust
    event_loop.run(Some(Duration::from_millis(2)), &mut app, |app| {
        app.dispatch_wayland();
        app.poll_keys();   // drains PressTracker::poll + reaps children
    })?;
```

- [ ] **Step 3: Verify on the device**

Build and run per `docs/RUNBOOK-device.md`, with `SPRINGCHICK_KEYBINDS` pointed at a test config that binds every button to `logger -t springchick <name>`. Watch `journalctl -f -t springchick` and confirm: short volume press logs `short`, holding it 500ms logs `long` *while still held*, release after a long logs nothing more.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/drm_backend.rs crates/sc-compositor/src/keybinds.rs
git commit -m "feat(drm): handle keyboard events and fire keybindings"
```

---

### Task 11: Display blanking (`toggle-display`)

**Files:**
- Create: `crates/sc-compositor/src/blank.rs`
- Modify: `crates/sc-compositor/src/drm_backend.rs`

- [ ] **Step 1: Write the failing test**

`blank.rs` holds the policy, testable without a GPU:

```rust
#[test]
fn a_bound_key_wakes_the_screen_instead_of_firing() {
    let mut b = Blank::new();
    assert!(!b.is_blanked());
    b.toggle();
    assert!(b.is_blanked());
    // First key press while blanked wakes and consumes.
    assert_eq!(b.on_key_press(), KeyWhileBlanked::Woke);
    assert!(!b.is_blanked());
    assert_eq!(b.on_key_press(), KeyWhileBlanked::Normal);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-compositor blank`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement the policy, then the DRM side**

`Blank` is a two-state flag with the wake-consumes-the-press rule. In `drm_backend.rs`: when blanking, stop queueing page-flips (`render` returns early while blanked) and disable the CRTC; when unblanking, re-enable it, `reset_buffers()`, and force a full redraw — the same sequence `SessionEvent::ActivateSession` already uses at `drm_backend.rs:170-176`, which is the known-good path for restoring scanout.

Wire `Action::ToggleDisplay` in `keybinds::run_action` to it, and check `Blank::on_key_press` before the tracker so a wake never fires a binding. On winit, `ToggleDisplay` logs and does nothing.

- [ ] **Step 4: Verify**

Run: `cargo test --workspace`
Manual, on device: power short-press blanks the panel; the perf log stops ticking; a second press restores a correctly rendered screen (not black, not torn).

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/blank.rs crates/sc-compositor/src/drm_backend.rs crates/sc-compositor/src/keybinds.rs
git commit -m "feat(drm): blank and restore the panel from the power button"
```

---

### Task 12: Debug-socket key injection and end-to-end test

**Files:**
- Modify: `crates/sc-compositor/src/debug_input.rs:10-30` (`DebugCmd`, `parse_line`), `:200` (`dispatch`)
- Test: `tests/` (follow the existing harness layout)

- [ ] **Step 1: Write the failing parse tests**

```rust
#[test]
fn parses_key_with_and_without_hold() {
    assert_eq!(parse_line("key XF86PowerOff", W, H), Ok(DebugCmd::Key { name: "XF86PowerOff".into(), hold_ms: 0 }));
    assert_eq!(parse_line("key XF86PowerOff 600", W, H), Ok(DebugCmd::Key { name: "XF86PowerOff".into(), hold_ms: 600 }));
    assert!(parse_line("key", W, H).is_err());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-compositor debug_input`
Expected: FAIL — no `Key` variant.

- [ ] **Step 3: Implement**

Add `Key { name: String, hold_ms: u32 }` to `DebugCmd` (this makes the enum non-`Copy` — change the derive to `Clone, Debug, PartialEq`). `dispatch` resolves the name via `keybinds::resolve_keysym`, calls the press path, and schedules the release `hold_ms` later through the same mechanism `Swipe` uses for timed injection, so a 600ms hold exercises the real threshold rather than a mocked clock.

- [ ] **Step 4: Write the end-to-end test**

Headless winit run with `SPRINGCHICK_DEBUG_SOCK` and a `SPRINGCHICK_KEYBINDS` config binding a key's short and long presses to `touch` two different temp files. Send `key X 100` → short marker exists, long marker does not. Send `key X 600` → long marker appears. Assert focus behavior too: with an app open, an unbound key reaches the client; on Home it does not.

- [ ] **Step 5: Run and commit**

```bash
cargo test --workspace
git add crates/sc-compositor/src/debug_input.rs tests
git commit -m "test(keybinds): drive short and long presses over the debug socket"
```

---

### Task 13: Documentation

**Files:**
- Modify: `docs/RUNBOOK-device.md`

- [ ] **Step 1: Document the config**

Add a "Keybindings" section: the config path and `SPRINGCHICK_KEYBINDS` override, the full TOML example, the defaults table, the action list, and the short/long timing diagram. Note explicitly that bound keys never reach clients, and that testing `poweroff` should start with `logger`.

- [ ] **Step 2: Commit**

```bash
git add docs/RUNBOOK-device.md
git commit -m "docs: document springchick keybindings"
```

---

## Verification checklist

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `nix build .#springchick` succeeds (the packaged build runs the workspace tests)
- [ ] On device: volume short presses change volume; vol-up long closes the front app; vol-down long toggles wvkbd; power short blanks and restores; power long (bound to `logger` first) fires at 500ms while held
- [ ] On device: a terminal app receives typed keys when open, and does not when Home is showing
