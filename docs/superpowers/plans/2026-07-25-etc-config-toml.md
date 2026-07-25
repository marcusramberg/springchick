# config.toml + /etc default config + NixOS module Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure keybindings config into a general `config.toml` with a `[keybinds]` table, add `/etc/springchick/config.toml` as a system-wide lookup tier, and expose it via a new NixOS module option.

**Architecture:** `sc-keys::config` gains a top-level `RawConfigFile { keybinds: Option<RawConfig> }` wrapper around the existing keybinds schema (unchanged in content). `sc-compositor::keybinds` renames its target file/env var and splits its single `config_path()` into an injectable `env_override` (strict `SPRINGCHICK_CONFIG` override, no fallthrough) and `candidate_paths` (XDG → `/etc`, testable via a closure instead of real env vars). `nix/module.nix` gains `programs.springchick.config` (nullable raw TOML) writing to `/etc/springchick/config.toml` via `environment.etc`.

**Tech Stack:** Rust (serde, toml crate), NixOS module system.

**Spec:** `docs/superpowers/specs/2026-07-25-etc-keybindings-config-design.md`

---

### Task 1: `sc-keys` — nest keybinds schema under a `[keybinds]` table

**Files:**
- Modify: `crates/sc-keys/src/config.rs`

- [ ] **Step 1: Update existing test fixtures to the nested `[keybinds]` shape**

In `crates/sc-keys/src/config.rs`, every inline TOML fixture in `#[cfg(test)] mod tests` currently writes `[[binding]]` / `long_press_ms = ...` at the top level. Update each one to nest under `[keybinds]`. For example, `parses_a_command_binding` becomes:

```rust
#[test]
fn parses_a_command_binding() {
    let cfg = Config::parse(
        r#"
        [keybinds]
        [[keybinds.binding]]
        key = "XF86AudioRaiseVolume"
        press = "short"
        command = "wpctl set-volume @DEFAULT_SINK@ 5%+"
        "#,
    );
    assert_eq!(cfg.long_press_ms, DEFAULT_LONG_PRESS_MS);
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
```

Apply the same `[keybinds]` / `[[keybinds.binding]]` nesting to: `parses_an_internal_action_with_mods`, `skips_invalid_entries_without_failing`, `custom_long_press_ms_is_read` (becomes `[keybinds]\nlong_press_ms = 800\n`), `parses_volume_actions`. Leave `malformed_toml_yields_empty_not_panic`, `malformed_toml_falls_back_to_defaults_when_asked`, `defaults_cover_the_fp5_buttons`, and `defaults_parse_from_the_shipped_text` untouched — they don't hand-write nested TOML.

Add one new test documenting the "valid TOML, no `[keybinds]` table" case is empty-bindings, not `Config::defaults()` (this test already passes against both old and new code — it's a regression guard, not a red step; see Step 2):

```rust
#[test]
fn missing_keybinds_table_yields_empty_not_defaults() {
    let cfg = Config::parse("");
    assert_eq!(cfg.long_press_ms, DEFAULT_LONG_PRESS_MS);
    assert!(cfg.bindings.is_empty());
}
```

- [ ] **Step 2: Run tests to verify the updated fixtures fail**

Run: `cargo test -p sc-keys`
Expected: FAIL on the tests updated to nested `[keybinds]`/`[[keybinds.binding]]` TOML (e.g. `parses_a_command_binding`, `parses_an_internal_action_with_mods`, `custom_long_press_ms_is_read`, `parses_volume_actions`, `skips_invalid_entries_without_failing`). The pre-change `RawConfig` has no `deny_unknown_fields`, so it parses these fixtures without a TOML error — it just silently ignores the unrecognized `[keybinds]` table and top-level `binding` stays empty, so these tests fail on assertion mismatches (e.g. `assert_eq!(cfg.bindings.len(), 1)` seeing `0`), not on a parse error. `missing_keybinds_table_yields_empty_not_defaults` is expected to PASS already at this step — empty TOML behaves the same under old and new code — it's included as a regression guard for Step 3, not as a red-step test.

- [ ] **Step 3: Add the `RawConfigFile` wrapper and update `Config::parse`/`parse_or_defaults`**

In `crates/sc-keys/src/config.rs`:

1. Add `#[derive(Default)]` to `RawConfig`'s derive list (currently just `#[derive(Deserialize)]` at line 117-122):

```rust
#[derive(Deserialize, Default)]
struct RawConfig {
    long_press_ms: Option<u64>,
    #[serde(default)]
    binding: Vec<RawBinding>,
}
```

2. Add the wrapper struct right after `RawConfig`:

```rust
/// Top-level shape of `config.toml`. Other sections (display, gestures, ...)
/// may be added here later; unknown top-level keys are ignored by serde's
/// default behavior.
#[derive(Deserialize, Default)]
struct RawConfigFile {
    keybinds: Option<RawConfig>,
}
```

3. Update `Config::parse` (currently deserializes `RawConfig` directly) to deserialize `RawConfigFile` and unwrap:

```rust
pub fn parse(text: &str) -> Config {
    let file: RawConfigFile = match toml::from_str(text) {
        Ok(file) => file,
        Err(e) => {
            warn!(%e, "config is not valid TOML");
            return Config {
                long_press_ms: DEFAULT_LONG_PRESS_MS,
                bindings: Vec::new(),
            };
        }
    };
    let raw = file.keybinds.unwrap_or_default();

    let bindings = raw.binding.into_iter().filter_map(convert).collect();
    Config {
        long_press_ms: raw.long_press_ms.unwrap_or(DEFAULT_LONG_PRESS_MS),
        bindings,
    }
}
```

4. Update `Config::parse_or_defaults` the same way — it currently does `toml::from_str::<RawConfig>(text)` to check validity before calling `parse`; change the type annotation to `RawConfigFile`:

```rust
pub fn parse_or_defaults(text: &str) -> Config {
    match toml::from_str::<RawConfigFile>(text) {
        Ok(_) => Config::parse(text),
        Err(e) => {
            warn!(%e, "config is not valid TOML; using defaults");
            Config::defaults()
        }
    }
}
```

5. Update `DEFAULT_TOML` to nest under `[keybinds]`:

```rust
pub const DEFAULT_TOML: &str = r#"
[keybinds]
long_press_ms = 800

[[keybinds.binding]]
key = "XF86AudioRaiseVolume"
press = "short"
action = "volume-up"

[[keybinds.binding]]
key = "XF86AudioRaiseVolume"
press = "long"
action = "close-app"

[[keybinds.binding]]
key = "XF86AudioLowerVolume"
press = "short"
action = "volume-down"

[[keybinds.binding]]
key = "XF86AudioLowerVolume"
press = "long"
command = "pkill -SIGRTMIN -f wvkbd-mobintl"

[[keybinds.binding]]
key = "XF86PowerOff"
press = "short"
action = "toggle-display"

[[keybinds.binding]]
key = "XF86PowerOff"
press = "long"
command = "systemctl poweroff"

[[keybinds.binding]]
key = "Escape"
press = "short"
action = "home"
"#;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sc-keys`
Expected: PASS, all tests including the new `missing_keybinds_table_yields_empty_not_defaults`.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-keys/src/config.rs
git commit -m "feat(config): nest keybinds schema under a [keybinds] table

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 2: `sc-compositor` — rename to `config.toml`, add `/etc` lookup tier

**Files:**
- Modify: `crates/sc-compositor/src/keybinds.rs`

- [ ] **Step 1: Update the existing top-level-TOML test to the nested shape**

`unresolvable_names_are_dropped_not_fatal` (keybinds.rs, in `mod tests`) currently parses top-level `[[binding]]` TOML:

```rust
#[test]
fn unresolvable_names_are_dropped_not_fatal() {
    let cfg = Config::parse(
        "[[binding]]\nkey = \"Nonsense\"\npress = \"short\"\ncommand = \"true\"\n",
    );
    assert!(resolve(cfg).is_empty());
}
```

Change the TOML literal to nest under `[keybinds]`:

```rust
#[test]
fn unresolvable_names_are_dropped_not_fatal() {
    let cfg = Config::parse(
        "[keybinds]\n[[keybinds.binding]]\nkey = \"Nonsense\"\npress = \"short\"\ncommand = \"true\"\n",
    );
    assert!(resolve(cfg).is_empty());
}
```

- [ ] **Step 2: Write failing tests for `env_override` and `candidate_paths`**

Add to `mod tests` in `crates/sc-compositor/src/keybinds.rs`:

```rust
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
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p sc-compositor keybinds::tests`
Expected: FAIL with "cannot find function `env_override`" / "cannot find function `candidate_paths`" (compile error).

- [ ] **Step 4: Implement `env_override`, `candidate_paths`, and update `load_config`**

Replace the existing `config_path`/`load_config` block (keybinds.rs:43-73) with:

```rust
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p sc-compositor keybinds::tests`
Expected: PASS, all tests including the five new lookup-tier tests.

- [ ] **Step 6: Commit**

```bash
git add crates/sc-compositor/src/keybinds.rs
git commit -m "feat(config): rename keybindings.toml to config.toml, add /etc lookup tier

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 3: NixOS module — `programs.springchick.config`

**Files:**
- Modify: `nix/module.nix`

- [ ] **Step 1: Add the `config` option and `environment.etc` wiring**

In `nix/module.nix`, add to `options.programs.springchick` (after the existing `package` option, before the closing `};` at line 22):

```nix
    # Named `config`, not `keybindings`: config.toml is a general file (a
    # [keybinds] table today, more sections later), and `cfg.config` here is
    # unrelated to this module's own `config = lib.mkIf cfg.enable { ... }`
    # output attribute below — same name, two different things.
    config = lib.mkOption {
      type = lib.types.nullOr lib.types.lines;
      default = null;
      description = ''
        Raw TOML written to /etc/springchick/config.toml. See
        crates/sc-keys/src/config.rs for the [keybinds] table schema.
        Null (default) leaves the compositor's built-in defaults in
        place and does not touch /etc.
      '';
    };
```

Add to the `config = lib.mkIf cfg.enable { ... }` block, directly after the `services.displayManager.sessionPackages = [ cfg.package ];` line and its preceding comment, and before the `# DRM master + libinput come from the logind seat...` comment that precedes `hardware.graphics.enable` — i.e. as a new paragraph between the two existing comment/line pairs, not interleaved with either:

```nix
    environment.etc."springchick/config.toml" = lib.mkIf (cfg.config != null) {
      text = cfg.config;
    };
```

- [ ] **Step 2: Verify the module evaluates**

Run: `nix flake check` (from repo root)
Expected: succeeds (no eval errors). This is a Nix-level sanity check, not a new automated test — the module has no existing test harness beyond flake evaluation.

- [ ] **Step 3: Commit**

```bash
git add nix/module.nix
git commit -m "feat(nix): add programs.springchick.config option for /etc/springchick/config.toml

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 4: Update RUNBOOK docs

**Files:**
- Modify: `docs/RUNBOOK-device.md`

- [ ] **Step 1: Update the Keybindings section**

In `docs/RUNBOOK-device.md`, replace lines 35-69 (the whole `## Keybindings` section, header through the closing `| volume-mute | ... |` table row) with:

```markdown
## Config

Config file: `$XDG_CONFIG_HOME/springchick/config.toml` (→ `~/.config/springchick/…`),
then `/etc/springchick/config.toml`, overridable with `SPRINGCHICK_CONFIG=<path>` (strict
override — no fallthrough to the other tiers if that file is missing). A missing/unreadable/
unparseable file at any tier falls through to the next; if none apply, the compiled-in
keybinding defaults apply. Nothing is written to disk. Loaded once at startup — edits need a
restart.

On NixOS, `programs.springchick.config` writes `/etc/springchick/config.toml` from a raw TOML
string (`null` by default — leaves it unmanaged).

### Keybinds

```toml
[keybinds]
long_press_ms = 800          # optional, global

[[keybinds.binding]]
key = "XF86AudioRaiseVolume" # xkb keysym name
press = "short"              # "short" | "long"
action = "volume-up"         # internal action; mutually exclusive with `command`

[[keybinds.binding]]
key = "XF86AudioRaiseVolume"
press = "long"
action = "close-app"

[[keybinds.binding]]
key = "Return"
mods = ["Super"]             # optional; exact match on Ctrl/Alt/Shift/Super
press = "short"
command = "foot"
```

`command` runs through `sh -c`, so pipes and quoting work as written. Actions:

| action | effect |
|---|---|
| `close-app` | close the front toplevel |
| `home` | return to the home screen |
| `toggle-display` | blank/unblank the panel via DPMS — DRM only, no-op under winit |
| `volume-up` / `volume-down` | `wpctl` step the default sink ±5% and show the OSD |
| `volume-mute` | `wpctl` toggle mute and show the OSD |
```

- [ ] **Step 2: Commit**

```bash
git add docs/RUNBOOK-device.md
git commit -m "docs: update RUNBOOK for config.toml [keybinds] table and /etc tier

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 5: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: PASS, no regressions in any crate.

- [ ] **Step 2: Run `nix flake check`**

Run: `nix flake check`
Expected: succeeds — package and module both still evaluate/build.

- [ ] **Step 3: Grep for stale references**

Run: `grep -rn "keybindings\.toml\|SPRINGCHICK_KEYBINDS" --include="*.rs" --include="*.nix" --include="*.md" . | grep -v docs/superpowers`
Expected: no output (all renamed).
