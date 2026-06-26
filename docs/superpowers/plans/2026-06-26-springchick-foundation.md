# springchick Foundation Implementation Plan (Milestone 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the springchick Rust workspace, prove the Skia-on-Smithay-GLES rendering path in a nested desktop window, and deliver fully unit-tested pure-logic cores (spring engine, gesture state machine, shell model, config/catalog) ready to wire into the shell.

**Architecture:** A single compositor binary (`sc-compositor`) backed by Smithay, with the Springboard drawn in-process via Skia's Ganesh GL backend on Smithay's own GLES context. All feel/logic (`sc-anim`, `sc-input`, `sc-shell-model`, `sc-config`) lives in pure library crates with zero rendering dependencies so they are unit-testable headless. A runtime backend switch (`SPRINGCHICK_BACKEND=winit|drm`) runs the same binary nested on the NixOS desktop or on the Fairphone 5.

**Tech Stack:** Rust (Cargo workspace) · Smithay (DRM/KMS, libinput, XDG-shell, EGL/GLES2) · `skia-safe` (Ganesh GL backend) · Nix flake (pinned toolchain + Skia) · `winit` (dev backend).

**Reference:** Design spec at `docs/superpowers/specs/2026-06-26-springchick-design.md`.

---

## File / Crate Structure

Cargo workspace. Pure-logic crates are libraries with `#![forbid(unsafe_code)]` and no GPU/Smithay deps. The compositor is the only binary.

```
springchick/
  flake.nix                       # pinned Rust + Skia + Smithay system deps
  rust-toolchain.toml             # pinned rustc
  Cargo.toml                      # [workspace]
  crates/
    sc-anim/                      # spring-physics engine (pure)
      src/lib.rs
    sc-input/                     # gesture recognizer + nav state machine (pure)
      src/lib.rs                  #   re-exports
      src/gesture.rs              #   touch tracking + velocity
      src/nav.rs                  #   NavStateMachine + release-target classifier
      src/thresholds.rs           #   ALL tunable feel constants in one place
    sc-shell-model/               # home grid / pages / dock model (pure)
      src/lib.rs
    sc-config/                    # TOML persistence + .desktop catalog (pure)
      src/lib.rs                  #   re-exports
      src/state.rs                #   GridState load/save (TOML)
      src/catalog.rs              #   .desktop parsing + icon-theme path resolution
    sc-compositor/                # the binary: Smithay + Skia bridge + render loop
      src/main.rs                 #   backend select, event loop
      src/backend.rs              #   winit | drm abstraction
      src/skia_gl.rs              #   Skia DirectContext bound to Smithay GLES context
      src/render.rs               #   per-frame draw orchestration
```

**Dependency direction:** `sc-compositor` depends on all four pure crates. Pure crates depend on nothing in this workspace (and no GPU libs). This keeps the feel logic testable and the render path thin.

---

## Task 1: Workspace skeleton + Nix flake

**Files:**
- Create: `flake.nix`, `rust-toolchain.toml`, `Cargo.toml`, `.gitignore` (append)

- [ ] **Step 1: Pin the Rust toolchain**

Create `rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.83.0"
components = ["rustfmt", "clippy"]
targets = ["aarch64-unknown-linux-gnu"]
```

- [ ] **Step 2: Create the workspace manifest**

Create `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = [
  "crates/sc-anim",
  "crates/sc-input",
  "crates/sc-shell-model",
  "crates/sc-config",
  "crates/sc-compositor",
]

[workspace.package]
edition = "2021"
license = "MIT"

[profile.dev]
opt-level = 1          # animations are unbearable in unoptimized debug; keep some opt

[profile.release]
lto = "thin"
codegen-units = 1
```

- [ ] **Step 3: Create the Nix flake**

Create `flake.nix` (dev shell with Rust + the system libs Smithay/Skia need). Keep it minimal; the executor will iterate on exact deps when the compositor crate is added.

```nix
{
  description = "springchick — iOS Springboard-style Wayland compositor";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };
  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
        rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rust pkgs.pkg-config
            # Smithay / Wayland / input / drm
            pkgs.wayland pkgs.libinput pkgs.libxkbcommon pkgs.libGL
            pkgs.mesa pkgs.udev pkgs.seatd pkgs.libgbm
            # winit (dev backend) X11/Wayland
            pkgs.xorg.libX11 pkgs.xorg.libXcursor pkgs.xorg.libXi
            # Skia build deps
            pkgs.fontconfig pkgs.freetype pkgs.clang pkgs.python3
          ];
          shellHook = ''export RUST_BACKTRACE=1'';
        };
      });
}
```

- [ ] **Step 4: Verify the workspace builds (empty)**

Run: `nix develop --command cargo metadata --no-deps` (will fail until member crates exist — that's expected; Task 2 creates the first member).
Expected: error naming the missing member crates. This confirms the flake's Rust toolchain works.

- [ ] **Step 5: Commit**

```bash
git add flake.nix rust-toolchain.toml Cargo.toml
git commit -m "chore: workspace skeleton + nix flake"
```

---

## Task 2: `sc-anim` — spring-physics engine (pure, TDD)

A spring drives one scalar value (position, scale, opacity, corner-radius, blur). The shell composes several. Model a standard damped harmonic oscillator integrated per-frame with real `dt`. Must be **interruptible**: retargeting mid-flight keeps current value + velocity.

**Files:**
- Create: `crates/sc-anim/Cargo.toml`, `crates/sc-anim/src/lib.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/sc-anim/Cargo.toml`:

```toml
[package]
name = "sc-anim"
version = "0.1.0"
edition.workspace = true

[dependencies]
```

- [ ] **Step 2: Write the failing test — spring converges to target without overshoot**

Create `crates/sc-anim/src/lib.rs`:

```rust
#![forbid(unsafe_code)]

/// A critically-damped-by-default spring driving one scalar.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    pub stiffness: f32, // higher = snappier
    pub damping: f32,   // critical damping ~= 2*sqrt(stiffness)
}

impl Spring {
    pub fn new(value: f32) -> Self {
        Self { value, velocity: 0.0, target: value, stiffness: 220.0, damping: 30.0 }
    }

    /// Retarget without losing current value/velocity (interruptible).
    pub fn retarget(&mut self, target: f32) { self.target = target; }

    /// Advance by dt seconds (semi-implicit Euler). Returns true while still moving.
    pub fn step(&mut self, dt: f32) -> bool {
        let force = -self.stiffness * (self.value - self.target) - self.damping * self.velocity;
        self.velocity += force * dt;
        self.value += self.velocity * dt;
        !self.is_settled()
    }

    pub fn is_settled(&self) -> bool {
        (self.value - self.target).abs() < 0.001 && self.velocity.abs() < 0.001
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_to_rest(s: &mut Spring, max_steps: usize) -> usize {
        let dt = 1.0 / 90.0;
        for i in 0..max_steps {
            if !s.step(dt) { return i; }
        }
        max_steps
    }

    #[test]
    fn converges_to_target() {
        let mut s = Spring::new(0.0);
        s.retarget(100.0);
        let steps = run_to_rest(&mut s, 1000);
        assert!(steps < 1000, "spring should settle");
        assert!((s.value - 100.0).abs() < 0.01, "value={}", s.value);
    }

    #[test]
    fn no_large_overshoot() {
        let mut s = Spring::new(0.0);
        s.retarget(100.0);
        let dt = 1.0 / 90.0;
        let mut peak = 0.0_f32;
        for _ in 0..1000 { s.step(dt); peak = peak.max(s.value); if s.is_settled() { break; } }
        assert!(peak <= 100.0 * 1.05, "overshoot too large: peak={}", peak);
    }

    #[test]
    fn retarget_preserves_velocity() {
        let mut s = Spring::new(0.0);
        s.retarget(100.0);
        let dt = 1.0 / 90.0;
        for _ in 0..5 { s.step(dt); }
        let v = s.velocity;
        s.retarget(50.0); // interrupt
        assert_eq!(s.velocity, v, "retarget must not zero velocity");
    }
}
```

- [ ] **Step 3: Run tests to verify they fail then pass**

Run: `nix develop --command cargo test -p sc-anim`
Expected: compiles and all three tests PASS (implementation is included above; if any fail, the executor tunes `stiffness`/`damping` defaults — these are starting values to be retuned on-harness in Milestone 3).

- [ ] **Step 4: Commit**

```bash
git add crates/sc-anim
git commit -m "feat(anim): interruptible spring-physics engine"
```

---

## Task 3: `sc-input` thresholds + gesture tracking (pure, TDD)

Touch tracking with low-pass velocity. All feel constants live in `thresholds.rs` so on-harness tuning (Milestone 3) touches one file and the unit tests pin the classification boundaries.

**Files:**
- Create: `crates/sc-input/Cargo.toml`, `crates/sc-input/src/lib.rs`, `crates/sc-input/src/thresholds.rs`, `crates/sc-input/src/gesture.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/sc-input/Cargo.toml`:

```toml
[package]
name = "sc-input"
version = "0.1.0"
edition.workspace = true

[dependencies]
```

- [ ] **Step 2: Define tunable thresholds**

Create `crates/sc-input/src/thresholds.rs`:

```rust
//! All navigation feel constants. Tuned on-harness in Milestone 3.
//! Distances are fractions of screen height/width (resolution-independent).

/// Release below this upward progress → return to the app.
pub const BACK_TO_APP_MAX_PROGRESS: f32 = 0.10;
/// Switcher card deck begins fanning in at/above this progress (live preview).
pub const SWITCHER_REVEAL_PROGRESS: f32 = 0.35;
/// Slow drag released at/above this progress settles into the switcher.
pub const SWITCHER_SETTLE_PROGRESS: f32 = 0.55;
/// Upward velocity (fraction of screen height per second) above which a flick
/// always flings home regardless of distance. Negative = upward.
pub const HOME_FLICK_VELOCITY: f32 = -2.2;
/// Horizontal travel fraction (of screen width) that commits a quick-switch.
pub const QUICK_SWITCH_PROGRESS: f32 = 0.15;
/// Horizontal velocity (fraction of screen width/s) that commits a quick-switch.
pub const QUICK_SWITCH_VELOCITY: f32 = 1.5;
/// Velocity low-pass smoothing factor (0..1, higher = snappier/noisier).
pub const VELOCITY_SMOOTHING: f32 = 0.6;
```

- [ ] **Step 3: Write the failing test — velocity tracker low-passes finger speed**

Create `crates/sc-input/src/gesture.rs`:

```rust
/// Normalized point: x in [0,1] of screen width, y in [0,1] of screen height
/// with y=0 at the top. Keeps the logic resolution-independent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pt { pub x: f32, pub y: f32 }

/// Tracks a single touch and produces a low-passed velocity (units: fraction/sec).
#[derive(Clone, Copy, Debug)]
pub struct Tracker {
    pub start: Pt,
    pub current: Pt,
    pub velocity: Pt,
}

impl Tracker {
    pub fn begin(p: Pt) -> Self { Self { start: p, current: p, velocity: Pt { x: 0.0, y: 0.0 } } }

    pub fn update(&mut self, p: Pt, dt: f32) {
        if dt > 0.0 {
            let inst = Pt { x: (p.x - self.current.x) / dt, y: (p.y - self.current.y) / dt };
            let a = crate::thresholds::VELOCITY_SMOOTHING;
            self.velocity.x = a * inst.x + (1.0 - a) * self.velocity.x;
            self.velocity.y = a * inst.y + (1.0 - a) * self.velocity.y;
        }
        self.current = p;
    }

    /// Upward progress: how far up from the start (0 at start, 1 = full screen up).
    pub fn up_progress(&self) -> f32 { (self.start.y - self.current.y).max(0.0) }
    /// Signed horizontal travel from start.
    pub fn dx(&self) -> f32 { self.current.x - self.start.x }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_upward_progress() {
        let mut t = Tracker::begin(Pt { x: 0.5, y: 0.95 });
        t.update(Pt { x: 0.5, y: 0.45 }, 1.0 / 90.0);
        assert!((t.up_progress() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn velocity_is_low_passed_not_instantaneous() {
        let mut t = Tracker::begin(Pt { x: 0.5, y: 0.9 });
        // one big jump; low-pass means velocity < raw instantaneous
        let dt = 1.0 / 90.0;
        let raw = (0.5 - 0.9) / dt;
        t.update(Pt { x: 0.5, y: 0.5 }, dt);
        assert!(t.velocity.y.abs() < raw.abs(), "should be smoothed");
        assert!(t.velocity.y < 0.0, "upward = negative");
    }
}
```

- [ ] **Step 4: Wire the module tree**

Create `crates/sc-input/src/lib.rs`:

```rust
#![forbid(unsafe_code)]
pub mod thresholds;
pub mod gesture;
pub mod nav;
pub use gesture::{Pt, Tracker};
pub use nav::{NavState, NavTarget, classify_release};
```

(`nav` is added in Task 4; lib.rs referencing it now means Task 3 compiles only after Task 4's file exists. To keep Task 3 self-contained, temporarily comment the `pub mod nav;` and the `pub use nav::...` line, then uncomment in Task 4.)

- [ ] **Step 5: Run tests**

Run: `nix develop --command cargo test -p sc-input`
Expected: both gesture tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/sc-input
git commit -m "feat(input): touch tracker + tunable thresholds"
```

---

## Task 4: `sc-input` nav state machine + release classifier (pure, TDD)

The defining UX, as pure logic. Given a tracker at release, classify into a `NavTarget`. This is the spec's "release targets" table, made testable.

**Files:**
- Create: `crates/sc-input/src/nav.rs`
- Modify: `crates/sc-input/src/lib.rs` (uncomment `nav` lines)

- [ ] **Step 1: Write the failing tests — release classification boundaries**

Create `crates/sc-input/src/nav.rs`:

```rust
use crate::gesture::Tracker;
use crate::thresholds as th;

/// Live navigation phase (drives what the shell renders during the drag).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NavState {
    Idle,
    Grabbing,        // window detached, tracking finger, no deck yet
    SwitcherPreview, // dragged past reveal: neighbor cards fanning in
    QuickSwitching,  // horizontal drag swapping adjacent app
}

/// Where the gesture lands on release.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NavTarget {
    BackToApp,
    Home,
    Switcher,
    QuickSwitch(i32), // -1 = previous app (swipe right), +1 = next (swipe left)
}

/// Live phase from the current tracker (called each frame during a grab).
pub fn live_state(t: &Tracker) -> NavState {
    let horizontal = t.dx().abs() > t.up_progress();
    if horizontal && t.dx().abs() >= th::QUICK_SWITCH_PROGRESS {
        return NavState::QuickSwitching;
    }
    if t.up_progress() >= th::SWITCHER_REVEAL_PROGRESS {
        return NavState::SwitcherPreview;
    }
    NavState::Grabbing
}

/// Classify the release target (spec: release-targets table).
pub fn classify_release(t: &Tracker) -> NavTarget {
    // Horizontal quick-switch wins if it dominates by travel or velocity.
    let horizontal_dominant = t.dx().abs() > t.up_progress();
    if horizontal_dominant
        && (t.dx().abs() >= th::QUICK_SWITCH_PROGRESS
            || t.velocity.x.abs() >= th::QUICK_SWITCH_VELOCITY)
    {
        return NavTarget::QuickSwitch(if t.dx() < 0.0 { 1 } else { -1 });
    }

    let progress = t.up_progress();
    if progress < th::BACK_TO_APP_MAX_PROGRESS {
        return NavTarget::BackToApp;
    }
    // Fast upward flick always flings home.
    if t.velocity.y <= th::HOME_FLICK_VELOCITY {
        return NavTarget::Home;
    }
    // Slow drag held far up → switcher; otherwise home.
    if progress >= th::SWITCHER_SETTLE_PROGRESS {
        NavTarget::Switcher
    } else {
        NavTarget::Home
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gesture::Pt;

    // Build a tracker with an explicit end position and velocity.
    fn t_with(start: Pt, end: Pt, vel: Pt) -> Tracker {
        let mut t = Tracker::begin(start);
        t.current = end;
        t.velocity = vel;
        t
    }

    #[test]
    fn tiny_rise_returns_to_app() {
        let t = t_with(Pt{x:0.5,y:0.95}, Pt{x:0.5,y:0.90}, Pt{x:0.0,y:-0.2});
        assert_eq!(classify_release(&t), NavTarget::BackToApp);
    }

    #[test]
    fn fast_upward_flick_goes_home_even_if_short() {
        // progress 0.2 (above back-to-app), strong upward velocity
        let t = t_with(Pt{x:0.5,y:0.95}, Pt{x:0.5,y:0.75}, Pt{x:0.0,y:-3.0});
        assert_eq!(classify_release(&t), NavTarget::Home);
    }

    #[test]
    fn slow_far_drag_settles_in_switcher() {
        // progress 0.6, slow velocity
        let t = t_with(Pt{x:0.5,y:0.95}, Pt{x:0.5,y:0.35}, Pt{x:0.0,y:-0.5});
        assert_eq!(classify_release(&t), NavTarget::Switcher);
    }

    #[test]
    fn moderate_slow_drag_goes_home() {
        // progress 0.3 (between back-to-app and switcher-settle), slow
        let t = t_with(Pt{x:0.5,y:0.95}, Pt{x:0.5,y:0.65}, Pt{x:0.0,y:-0.5});
        assert_eq!(classify_release(&t), NavTarget::Home);
    }

    #[test]
    fn horizontal_flick_quick_switches_next() {
        let t = t_with(Pt{x:0.5,y:0.95}, Pt{x:0.2,y:0.93}, Pt{x:-2.0,y:0.0});
        assert_eq!(classify_release(&t), NavTarget::QuickSwitch(1));
    }

    #[test]
    fn live_state_reveals_switcher_past_threshold() {
        let t = t_with(Pt{x:0.5,y:0.95}, Pt{x:0.5,y:0.55}, Pt{x:0.0,y:-0.5});
        assert_eq!(live_state(&t), NavState::SwitcherPreview);
    }
}
```

- [ ] **Step 2: Uncomment the nav exports in lib.rs**

In `crates/sc-input/src/lib.rs` ensure these lines are active:

```rust
pub mod nav;
pub use nav::{NavState, NavTarget, classify_release};
```

- [ ] **Step 3: Run tests**

Run: `nix develop --command cargo test -p sc-input`
Expected: all gesture + nav tests PASS (8 total).

- [ ] **Step 4: Commit**

```bash
git add crates/sc-input
git commit -m "feat(input): navigation state machine + release classifier"
```

---

## Task 5: `sc-shell-model` — home grid / pages / dock (pure, TDD)

The data model behind the home screen: pages of slots, a dock, and the MVP edit operations (rearrange, delete). Folders and page-reorder are deferred (Milestone 4+) — do not add them.

**Files:**
- Create: `crates/sc-shell-model/Cargo.toml`, `crates/sc-shell-model/src/lib.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/sc-shell-model/Cargo.toml`:

```toml
[package]
name = "sc-shell-model"
version = "0.1.0"
edition.workspace = true

[dependencies]
serde = { version = "1", features = ["derive"] }
```

- [ ] **Step 2: Write failing tests — grid place / move / delete; dock fixed capacity**

Create `crates/sc-shell-model/src/lib.rs`:

```rust
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};

/// Stable identifier for an app (its .desktop file id, e.g. "org.gnome.Maps").
pub type AppId = String;

pub const COLS: usize = 4;
pub const ROWS: usize = 6;
pub const PAGE_CAP: usize = COLS * ROWS; // 24 icons per page
pub const DOCK_CAP: usize = 4;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ShellModel {
    pub pages: Vec<Vec<AppId>>, // each page: ordered slots, len <= PAGE_CAP
    pub dock: Vec<AppId>,       // len <= DOCK_CAP
}

impl ShellModel {
    /// Append an app to the first page with room, creating a page if needed.
    pub fn place(&mut self, app: AppId) {
        if let Some(page) = self.pages.iter_mut().find(|p| p.len() < PAGE_CAP) {
            page.push(app);
        } else {
            self.pages.push(vec![app]);
        }
    }

    /// Remove an app entirely (delete from home).
    pub fn delete(&mut self, app: &str) {
        for page in &mut self.pages { page.retain(|a| a != app); }
        self.dock.retain(|a| a != app);
        self.pages.retain(|p| !p.is_empty());
    }

    /// Move an app to (page, index), shifting others. Used by drag-rearrange.
    pub fn move_to(&mut self, app: &str, page: usize, index: usize) {
        self.delete_keep_pages(app);
        while self.pages.len() <= page { self.pages.push(Vec::new()); }
        let p = &mut self.pages[page];
        let idx = index.min(p.len());
        p.insert(idx, app.to_string());
    }

    // delete without collapsing empty pages (internal helper for moves)
    fn delete_keep_pages(&mut self, app: &str) {
        for page in &mut self.pages { page.retain(|a| a != app); }
        self.dock.retain(|a| a != app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_fills_pages_then_overflows() {
        let mut m = ShellModel::default();
        for i in 0..(PAGE_CAP + 1) { m.place(format!("app{i}")); }
        assert_eq!(m.pages.len(), 2);
        assert_eq!(m.pages[0].len(), PAGE_CAP);
        assert_eq!(m.pages[1].len(), 1);
    }

    #[test]
    fn delete_removes_and_collapses_empty_pages() {
        let mut m = ShellModel::default();
        m.place("a".into());
        m.delete("a");
        assert!(m.pages.is_empty());
    }

    #[test]
    fn move_to_reorders_within_page() {
        let mut m = ShellModel::default();
        for n in ["a","b","c"] { m.place(n.into()); }
        m.move_to("c", 0, 0);
        assert_eq!(m.pages[0], vec!["c","a","b"]);
    }
}
```

- [ ] **Step 3: Run tests**

Run: `nix develop --command cargo test -p sc-shell-model`
Expected: all three tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-shell-model
git commit -m "feat(shell-model): grid/page/dock model with rearrange+delete"
```

---

## Task 6: `sc-config` — TOML persistence + .desktop catalog (pure, TDD)

Two responsibilities, both filesystem-backed but pure (take paths, no globals). Persistence uses **TOML** (decision locked in spec review). Catalog parses freedesktop `.desktop` entries.

**Files:**
- Create: `crates/sc-config/Cargo.toml`, `crates/sc-config/src/lib.rs`, `crates/sc-config/src/state.rs`, `crates/sc-config/src/catalog.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/sc-config/Cargo.toml`:

```toml
[package]
name = "sc-config"
version = "0.1.0"
edition.workspace = true

[dependencies]
sc-shell-model = { path = "../sc-shell-model" }
serde = { version = "1", features = ["derive"] }
toml = "0.8"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write failing test — grid state round-trips through TOML**

Create `crates/sc-config/src/state.rs`:

```rust
use sc_shell_model::ShellModel;
use std::path::Path;

pub fn save(model: &ShellModel, path: &Path) -> std::io::Result<()> {
    let s = toml::to_string_pretty(model).expect("serialize model");
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    std::fs::write(path, s)
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
        assert_eq!(m, back);
    }

    #[test]
    fn missing_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let m = load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(m, ShellModel::default());
    }
}
```

- [ ] **Step 3: Write failing test — .desktop parsing extracts Name/Exec/Icon, skips NoDisplay**

Create `crates/sc-config/src/catalog.rs`:

```rust
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct AppEntry {
    pub id: String,    // file stem, e.g. "org.gnome.Maps"
    pub name: String,
    pub exec: String,  // raw Exec line (field codes like %U left for the launcher to strip)
    pub icon: String,  // icon name or absolute path
}

/// Parse a single .desktop file. Returns None if it should not be shown
/// (NoDisplay=true, Hidden=true, or not a launchable Application).
pub fn parse_desktop(path: &Path, contents: &str) -> Option<AppEntry> {
    let id = path.file_stem()?.to_string_lossy().to_string();
    let mut name = None; let mut exec = None; let mut icon = String::new();
    let mut in_entry = false;
    let mut typ = String::new();
    let (mut no_display, mut hidden) = (false, false);
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') { in_entry = line == "[Desktop Entry]"; continue; }
        if !in_entry { continue; }
        let Some((k, v)) = line.split_once('=') else { continue; };
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
    if no_display || hidden || typ != "Application" { return None; }
    Some(AppEntry { id, name: name?, exec: exec?, icon })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SAMPLE: &str = "[Desktop Entry]\nType=Application\nName=Maps\nExec=gnome-maps %U\nIcon=org.gnome.Maps\n";

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
```

- [ ] **Step 4: Wire the module tree**

Create `crates/sc-config/src/lib.rs`:

```rust
#![forbid(unsafe_code)]
pub mod state;
pub mod catalog;
pub use catalog::{AppEntry, parse_desktop};
```

- [ ] **Step 5: Run tests**

Run: `nix develop --command cargo test -p sc-config`
Expected: all five tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/sc-config
git commit -m "feat(config): TOML grid persistence + .desktop catalog parsing"
```

---

## Task 7: `sc-compositor` skeleton + winit dev backend

Now the binary. This task gets a window open on the desktop at FP5 logical geometry via Smithay's winit backend, clearing to a solid color each frame. **No Skia yet** — prove the event loop, output, and frame clock first. This is integration/discovery work: exact Smithay API calls shift with the crate version, so steps specify the acceptance behavior and let the executor wire the current API.

**Files:**
- Create: `crates/sc-compositor/Cargo.toml`, `crates/sc-compositor/src/main.rs`, `crates/sc-compositor/src/backend.rs`

- [ ] **Step 1: Create the crate manifest**

Create `crates/sc-compositor/Cargo.toml`:

```toml
[package]
name = "sc-compositor"
version = "0.1.0"
edition.workspace = true

[[bin]]
name = "springchick"
path = "src/main.rs"

[dependencies]
sc-anim = { path = "../sc-anim" }
sc-input = { path = "../sc-input" }
sc-shell-model = { path = "../sc-shell-model" }
sc-config = { path = "../sc-config" }
smithay = { version = "0.4", default-features = false, features = [
  "backend_winit", "backend_drm", "backend_gbm", "backend_egl",
  "backend_libinput", "backend_session_libseat", "renderer_gl",
  "wayland_frontend", "desktop",
] }
tracing = "0.1"
tracing-subscriber = "0.3"
```

> Pin `smithay`'s exact version/rev during this task; the feature set above is the target. If a feature name changed upstream, adjust and note it.

- [ ] **Step 2: Define the backend abstraction**

Create `crates/sc-compositor/src/backend.rs`:

```rust
/// FP5 logical output geometry. The winit dev window is forced to match so layout
/// and animation are pixel-identical to the device.
pub const FP5_WIDTH: i32 = 1224;
pub const FP5_HEIGHT: i32 = 2700;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BackendKind { Winit, Drm }

impl BackendKind {
    /// Chosen by SPRINGCHICK_BACKEND env var (default: winit on desktop).
    pub fn from_env() -> Self {
        match std::env::var("SPRINGCHICK_BACKEND").as_deref() {
            Ok("drm") => BackendKind::Drm,
            _ => BackendKind::Winit,
        }
    }
}
```

- [ ] **Step 3: Implement the winit event loop (solid-color clear)**

Create `crates/sc-compositor/src/main.rs` that:
- reads `BackendKind::from_env()`,
- for `Winit`: initializes the Smithay winit backend with a window sized `FP5_WIDTH x FP5_HEIGHT` (scaled down to fit the desktop is fine — keep the logical size at FP5),
- runs the event loop, clearing the framebuffer to a solid color each frame and submitting,
- logs frame timing via `tracing`,
- exits cleanly on window close.

Leave `Drm` as `todo!("device backend — Milestone 5")`.

- [ ] **Step 4: Run it — see a window**

Run: `nix develop --command cargo run -p sc-compositor`
Expected: a window opens on the NixOS desktop showing the solid clear color; closing it exits cleanly. Confirm `tracing` logs ~90 frames/sec (or display rate).

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor
git commit -m "feat(compositor): winit dev backend skeleton with frame loop"
```

---

## Task 8: Skia-on-Smithay-GLES spike (the named first risk)

Prove the load-bearing integration: bind a Skia `DirectContext` to Smithay's GLES/EGL context and draw **one blurred rounded rectangle** into the same framebuffer the compositor presents. Success here de-risks the entire project. This is a spike — optimize for proving feasibility, not clean abstraction (that comes in Milestone 2).

**Files:**
- Create: `crates/sc-compositor/src/skia_gl.rs`
- Modify: `crates/sc-compositor/Cargo.toml` (add skia), `crates/sc-compositor/src/main.rs` (call the draw)

- [ ] **Step 1: Add skia-safe with the GL backend**

Add to `crates/sc-compositor/Cargo.toml` dependencies:

```toml
skia-safe = { version = "0.78", features = ["gl"] }
```

And ensure `flake.nix` exposes the libs skia-safe needs at build time (fontconfig, freetype, clang already added in Task 1; add others if the build errors).

- [ ] **Step 2: Bind Skia to the live GLES context**

Create `crates/sc-compositor/src/skia_gl.rs` exposing:

```rust
// Pseudocode contract — fill in against skia-safe 0.78 + the GL loader Smithay uses.
// 1. After Smithay has made its EGL/GLES context current, build a Skia GL interface
//    from the same GL proc loader (skia_safe::gpu::gl::Interface::new_load_with(...)).
// 2. Create a DirectContext: skia_safe::gpu::direct_contexts::make_gl(interface, None).
// 3. Wrap the current framebuffer as a Skia render target
//    (BackendRenderTarget::new_gl((w, h), samples, stencil, FramebufferInfo{fboid, format})).
// 4. Surface::from_backend_render_target(&mut ctx, &target, BottomLeft, RGBA8888, ...).
// Return a handle the render code can draw into each frame.
pub struct SkiaGl { /* ctx + reusable surface */ }

impl SkiaGl {
    /// Draw one blurred rounded rect to prove the path. Returns after flush.
    pub fn draw_spike(&mut self, w: i32, h: i32) {
        // canvas.clear(...); Paint with MaskFilter::blur(Normal, sigma);
        // canvas.draw_rrect(RRect::new_rect_xy(rect, 48.0, 48.0), &paint);
        // ctx.flush_and_submit();
    }
}
```

- [ ] **Step 3: Call the spike from the winit frame loop**

In `main.rs`, after the GLES context is current each frame (winit backend), call `skia.draw_spike(FP5_WIDTH, FP5_HEIGHT)` instead of the plain color clear, then let Smithay present.

- [ ] **Step 4: Run it — see a blurred rounded rect**

Run: `nix develop --command cargo run -p sc-compositor`
Expected: the nested window shows a crisp **blurred, rounded rectangle** on the clear color — Skia drawing through Smithay's own GLES context, no separate context, no dmabuf juggling. If colors/orientation are wrong, fix the `SurfaceOrigin`/color type. **This is the go/no-go gate for the architecture.**

- [ ] **Step 5: Document the integration in the spike file**

Add a top-of-file doc comment in `skia_gl.rs` recording exactly how the context handoff was wired (proc loader source, FBO id retrieval, color type, origin). Milestone 2 builds the real renderer on this.

- [ ] **Step 6: Commit**

```bash
git add crates/sc-compositor
git commit -m "feat(compositor): Skia-on-Smithay-GLES spike — blurred rounded rect"
```

---

## Task 9: Milestone wrap-up — CI check + roadmap note

**Files:**
- Create: `docs/superpowers/plans/README.md` (milestone roadmap)

- [ ] **Step 1: Verify the whole workspace is green**

Run: `nix develop --command cargo test --workspace && nix develop --command cargo clippy --workspace -- -D warnings`
Expected: all tests PASS, no clippy warnings.

- [ ] **Step 2: Record the roadmap for the next plans**

Create `docs/superpowers/plans/README.md`:

```markdown
# springchick milestone roadmap

- **M1 — Foundation** (this plan): workspace, pure-logic cores, winit harness, Skia spike. ✅ when Task 9 is green.
- **M2 — Home screen render:** wire sc-shell-model + sc-config + Skia into a real paginated grid + dock; launch apps as XDG toplevels.
- **M3 — Navigation:** bottom-bar grab/shrink/switcher/quick-switch driven by sc-input + sc-anim; on-harness tuning of `thresholds.rs` and spring constants.
- **M4 — Edit mode:** jiggle + drag-rearrange + delete (folders/page-reorder deferred).
- **M5 — Device bring-up:** drm backend, libinput touch, power button + idle blank on the Fairphone 5.

Each milestone gets its own plan via superpowers:writing-plans, building on M1.
```

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/plans/README.md
git commit -m "docs: milestone roadmap"
```

---

## Notes for the executor

- **Pure crates use strict TDD** with the exact code above. The compositor tasks (7, 8) are **integration spikes** — the steps fix the *acceptance behavior* (a window; a blurred rounded rect) because exact Smithay/skia-safe API calls depend on the resolved crate versions. Wire against the current API and record what you did.
- **Feel constants are starting values.** `sc-anim` spring defaults and everything in `sc-input/thresholds.rs` get retuned on-harness in M3. The unit tests pin *relative* behavior (ordering of targets), not the final numbers.
- Run `nix develop` once and work inside the shell, or prefix commands with `nix develop --command` as shown.
- Frequent commits, one per task minimum.
