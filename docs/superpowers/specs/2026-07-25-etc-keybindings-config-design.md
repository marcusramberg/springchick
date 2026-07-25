# config.toml + /etc default config + NixOS module support

## Problem

Keybindings config today lives in a dedicated `keybindings.toml`, loaded by `crates/sc-compositor/src/keybinds.rs` from `SPRINGCHICK_KEYBINDS` env var or `$XDG_CONFIG_HOME/springchick/keybindings.toml`, falling back to a compiled-in `DEFAULT_TOML` (`crates/sc-keys/src/config.rs`). There is no system-wide config tier, and the NixOS module (`nix/module.nix`) only exposes `enable`/`package` — no way for a system config to ship or override settings declaratively.

Separately: springchick is early enough in development that the config file is still free to restructure. Keybinds will not be the only configurable thing (display, gestures, etc. are likely future sections), so the file is renamed to `config.toml` with keybinds under a `[keybinds]` table now, before any real users depend on the flat shape.

## Design

### Rust: `config.toml` with a `[keybinds]` table

`crates/sc-keys/src/config.rs` — the existing `RawConfig`/`convert`/`Config` keybinds schema is unchanged in content, just nested one level. Add a top-level wrapper:

```rust
#[derive(Deserialize, Default)]
struct RawConfigFile {
    keybinds: Option<RawConfig>,
}
```

`Config::parse`/`Config::parse_or_defaults` deserialize `RawConfigFile` first, then operate on `.keybinds.unwrap_or_default()` (`RawConfig` needs `#[derive(Default)]`, i.e. `long_press_ms: None, binding: vec![]`). A file with no `[keybinds]` table is valid TOML, so it does **not** trigger the whole-file parse-error path — it yields `Config { long_press_ms: DEFAULT_LONG_PRESS_MS, bindings: vec![] }`, i.e. zero bindings, same as an empty `keybindings.toml` does today via `Config::parse`. This is `Config::parse`'s existing behavior for "valid but empty," distinct from `Config::defaults()` (which returns the shipped volume/power/escape bindings) — `defaults()` is only reached when the file is missing or fails to parse at all, unchanged from today. No behavior change here, just calling out that "no `[keybinds]` table" and "unparseable file" are different cases with different outcomes.

`DEFAULT_TOML` gains the wrapping table:

```toml
[keybinds]
long_press_ms = 800

[[keybinds.binding]]
key = "XF86AudioRaiseVolume"
press = "short"
action = "volume-up"

# ...rest of existing bindings, each under [[keybinds.binding]]...
```

All existing unit tests in `config.rs` update their inline TOML fixtures to the nested `[keybinds]` shape (mechanical; no test logic changes).

### Rust: lookup tiers, pointed at the new filename

`crates/sc-compositor/src/keybinds.rs`:

- File renamed `keybindings.toml` → `config.toml` throughout (doc comments, path construction, `/etc` path).
- Env var renamed `SPRINGCHICK_KEYBINDS` → `SPRINGCHICK_CONFIG` (early dev, no back-compat needed).
- Lookup order (unchanged from the prior /etc-support design, just retargeted):
  - `SPRINGCHICK_CONFIG` env var: strict override via a separate `env_override` function — if set, it is the only path tried; a missing/unreadable file at that path falls straight to `Config::defaults()`, no fallthrough to XDG or `/etc`. Matches today's `SPRINGCHICK_KEYBINDS` semantics.
  - If unset, `candidate_paths(env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf>` yields, in order:
    1. `$XDG_CONFIG_HOME/springchick/config.toml` (or `~/.config/...` via `$HOME`)
    2. `/etc/springchick/config.toml`
  - First candidate that exists and is readable wins; none existing falls back to `Config::defaults()`. A file that exists but fails to parse still falls back to `Config::defaults()` via `Config::parse_or_defaults`.
  - If both `XDG_CONFIG_HOME` and `HOME` are unset, `candidate_paths` omits the XDG entry, returning only `/etc/springchick/config.toml` — same absence-handling as today, just no longer short-circuiting to `None`.
- Both `env_override` and `candidate_paths` take an injectable `env: impl Fn(&str) -> Option<String>` closure rather than reading `std::env` directly, so tests exercise the ordering via a local `HashMap` instead of mutating real process env vars (avoids races with other tests in the same multithreaded test binary).
- Doc comment above the old `config_path` (keybinds.rs:43-44) rewritten to describe the new tiers and filename.

No changes to `resolve_keysym` or `Keys::load`.

### NixOS module: `programs.springchick.config`

`nix/module.nix` gains:

```nix
options.programs.springchick = {
  # ...existing enable/package...

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
};

config = lib.mkIf cfg.enable {
  # ...existing environment.systemPackages, sessionPackages, hardware/polkit defaults...

  environment.etc."springchick/config.toml" = lib.mkIf (cfg.config != null) {
    text = cfg.config;
  };
};
```

Named `config` (not `keybindings`) since the file is now general-purpose — avoids a second rename when future sections (display, gestures, ...) are added. This does mean `cfg.config` reads as an echo of the module's own `config = lib.mkIf cfg.enable {...}` result attribute; the two are unrelated (one is the option value, the other is the standard NixOS module output) and this pattern (an option literally named `config`) is unambiguous but worth a comment in the module source to orient a reader. Raw TOML text, not a structured submodule — avoids duplicating the schema in Nix, which would drift from the Rust source as `sc-keys::config` evolves. Per-user `$XDG_CONFIG_HOME` override keeps working unmodified; `/etc` only matters when no user config is present.

`nix/package.nix` is unaffected.

## Testing

- Existing `sc-keys::config` and `sc-compositor::keybinds` unit tests are unaffected in intent, updated in fixture shape (nested `[keybinds]` table).
- Add tests in `keybinds.rs` against `candidate_paths`/`env_override` using injected closures (a local `HashMap`, no real env var or filesystem mutation): env override present → single-element result and no fallthrough; env override absent → XDG-then-`/etc` ordering.
- `nix flake check` continues to build the module; no new Nix-level test (module is a thin `environment.etc` passthrough).

## Out of scope

- Structured/typed Nix option mirroring the binding schema.
- Any config sections beyond `[keybinds]` (display, gestures, etc.) — the wrapper struct just needs to tolerate their future addition without a breaking change.
- Validating `cfg.config` TOML at Nix eval time.
- Back-compat shim for `SPRINGCHICK_KEYBINDS` or `keybindings.toml` — pre-1.0, no deployed users depend on the old names.
