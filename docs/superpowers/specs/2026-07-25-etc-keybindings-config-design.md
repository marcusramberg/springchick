# /etc default config + NixOS module support

## Problem

Keybindings config today (`crates/sc-keys/src/config.rs`, `crates/sc-compositor/src/keybinds.rs`) loads from `SPRINGCHICK_KEYBINDS` env var or `$XDG_CONFIG_HOME/springchick/keybindings.toml`, falling back to a compiled-in `DEFAULT_TOML`. There is no system-wide config tier, and the NixOS module (`nix/module.nix`) only exposes `enable`/`package` — no way for a system config to ship or override keybindings declaratively.

## Design

### Rust: add `/etc` as a lookup tier

`SPRINGCHICK_KEYBINDS` keeps its current semantics unchanged: if set, it is the *only* path tried — no fallthrough to XDG or `/etc` if that file is missing or unreadable, straight to `Config::defaults()`, exactly like today. This preserves an explicit override's "this is the file, full stop" contract.

When `SPRINGCHICK_KEYBINDS` is unset, `load_config()` now tries, in order:

1. `$XDG_CONFIG_HOME/springchick/keybindings.toml` (or `~/.config/...` via `$HOME`)
2. `/etc/springchick/keybindings.toml`
3. `Config::defaults()` (built-in `DEFAULT_TOML`)

First of these that exists and is readable wins. A file that exists but fails to parse still falls back to `Config::defaults()` via `Config::parse_or_defaults`, same behavior as today — a config typo must never block compositor startup.

Refactor `config_path() -> Option<PathBuf>` (keybinds.rs:45) into two pieces so the new tier is unit-testable without racing on real process env vars (Rust tests run multithreaded in-process, so mutating `std::env` from a test would race other tests):

- `env_override(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf>` — resolves `SPRINGCHICK_KEYBINDS` only, taking an injectable env lookup.
- `candidate_paths(env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf>` — resolves the XDG and `/etc` tiers in order, same injectable signature.

`load_config()` calls `env_override` first (short-circuit, no fallthrough); if `None`, iterates `candidate_paths()`, tries each with `std::fs::read_to_string`, and falls back to `Config::defaults()` if none exist. Tests pass a closure over a local `HashMap` instead of touching real env vars, and can point `/etc`-tier assertions at literal `PathBuf::from("/etc/springchick/keybindings.toml")` without needing a real file — feasibility of the fallback logic is what's tested, not actual disk I/O for that tier.

If both `XDG_CONFIG_HOME` and `HOME` are unset, `candidate_paths` simply omits the XDG entry, returning only `/etc/springchick/keybindings.toml` — same absence-handling as today's `config_path`, just no longer short-circuiting to `None`.

Update the doc comment currently at keybinds.rs:43-44 (documents only the two existing tiers) to describe the new three/four-tier order.

No changes to `sc-keys/src/config.rs` parsing/validation logic, `resolve_keysym`, or `Keys::load`.

### NixOS module: `programs.springchick.keybindings`

`nix/module.nix` gains:

```nix
options.programs.springchick = {
  # ...existing enable/package...

  keybindings = lib.mkOption {
    type = lib.types.nullOr lib.types.lines;
    default = null;
    description = ''
      Raw TOML written to /etc/springchick/keybindings.toml. See
      crates/sc-keys/src/config.rs for the file schema. Null (default)
      leaves the compositor's built-in defaults in place and does not
      touch /etc.
    '';
  };
};

config = lib.mkIf cfg.enable {
  # ...existing environment.systemPackages, sessionPackages, hardware/polkit defaults...

  environment.etc."springchick/keybindings.toml" = lib.mkIf (cfg.keybindings != null) {
    text = cfg.keybindings;
  };
};
```

Raw TOML text, not a structured submodule — avoids duplicating the binding schema (key/mods/press/action) in Nix, which would drift from the Rust source as `sc-keys::config` evolves. Per-user `$XDG_CONFIG_HOME` override keeps working unmodified; `/etc` only matters when no user config is present.

`nix/package.nix` is unaffected.

## Testing

- Existing `sc-keys::config` and `sc-compositor::keybinds` unit tests are unaffected.
- Add tests in `keybinds.rs` against `candidate_paths`/`env_override` using injected closures (a local `HashMap`, no real env var or filesystem mutation): env override present → single-element result and no fallthrough; env override absent → XDG-then-`/etc` ordering.
- `nix flake check` continues to build the module; no new Nix-level test (module is a thin `environment.etc` passthrough).

## Out of scope

- Structured/typed Nix option mirroring the binding schema.
- `/etc` support for any config file other than `keybindings.toml` (none currently exist).
- Validating `cfg.keybindings` TOML at Nix eval time.
