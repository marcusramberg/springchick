# /etc default config + NixOS module support

## Problem

Keybindings config today (`crates/sc-keys/src/config.rs`, `crates/sc-compositor/src/keybinds.rs`) loads from `SPRINGCHICK_KEYBINDS` env var or `$XDG_CONFIG_HOME/springchick/keybindings.toml`, falling back to a compiled-in `DEFAULT_TOML`. There is no system-wide config tier, and the NixOS module (`nix/module.nix`) only exposes `enable`/`package` — no way for a system config to ship or override keybindings declaratively.

## Design

### Rust: add `/etc` as a lookup tier

`crates/sc-compositor/src/keybinds.rs` — `load_config()` search order becomes:

1. `SPRINGCHICK_KEYBINDS` env var (explicit override, unchanged)
2. `$XDG_CONFIG_HOME/springchick/keybindings.toml` (or `~/.config/...` via `$HOME`)
3. `/etc/springchick/keybindings.toml`
4. `Config::defaults()` (built-in `DEFAULT_TOML`)

First path that exists and is readable wins. A file that exists but fails to parse still falls back to `Config::defaults()` via `Config::parse_or_defaults`, same behavior as today — a config typo must never block compositor startup.

Refactor `config_path() -> Option<PathBuf>` into an ordered candidate list (e.g. `candidate_paths() -> Vec<PathBuf>`) so `load_config()` can iterate tiers, and so the new `/etc` tier is unit-testable without touching the real filesystem paths (inject search roots, or test at the `load_config`-equivalent level with tempdir-based candidates).

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
- Add a test in `keybinds.rs` exercising the new `/etc`-tier fallback in the search order (falls through to it when env var and XDG path are both absent/missing).
- `nix flake check` continues to build the module; no new Nix-level test (module is a thin `environment.etc` passthrough).

## Out of scope

- Structured/typed Nix option mirroring the binding schema.
- `/etc` support for any config file other than `keybindings.toml` (none currently exist).
- Validating `cfg.keybindings` TOML at Nix eval time.
