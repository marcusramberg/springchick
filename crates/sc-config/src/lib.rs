//! `config.toml`: springchick's user configuration — the `[main]` settings
//! (dpi, idle-blank, card radius, touch indicator) and the `[keybinds]` table.
//!
//! Validation is deliberately lenient. A bad entry is dropped with a warning and
//! the rest of the config still applies: on a phone, a compositor that refuses to
//! start over a config typo is a recovery session, while a skipped binding is a
//! button that does nothing.
//!
//! Persisted *state* (dock, pages, frecency) is separate — see
//! `sc_shell_model::persist` and `state.toml`.
#![forbid(unsafe_code)]

use serde::Deserialize;
use tracing::warn;

/// Modifiers a binding requires. Lock modifiers are deliberately absent — a
/// stuck Caps Lock must not disable every binding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ModMask {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl ModMask {
    pub const NONE: ModMask = ModMask {
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PressKind {
    Short,
    Long,
}

/// What a binding does when it fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Shell command, run through `sh -c`.
    Command(String),
    /// Close the front toplevel.
    CloseApp,
    /// Return to the home screen.
    Home,
    /// Blank / unblank the panel (DRM backend only).
    ToggleDisplay,
    /// Raise the volume and show the OSD.
    VolumeUp,
    /// Lower the volume and show the OSD.
    VolumeDown,
    /// Toggle mute and show the OSD.
    VolumeMute,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// xkb keysym name, resolved to a keysym by the compositor.
    pub key: String,
    pub mods: ModMask,
    pub press: PressKind,
    pub action: Action,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub long_press_ms: u64,
    /// Output scale advertised to clients. Fractional is allowed (e.g. `2.5`):
    /// the compositor advertises it via `wp_fractional_scale`, and its geometry
    /// math is `f64` throughout.
    pub dpi: f64,
    /// Seconds of no input before the panel idle-blanks. `0` disables idle
    /// blanking (the power button still blanks on demand).
    pub idle_blank_secs: u64,
    /// Base corner radius (logical px) for shrunken app cards: the switcher deck
    /// and the drag-lift card. Other card radii (drag growth, zoom transitions)
    /// scale proportionally from this.
    pub card_radius: f32,
    /// Draw a visual indicator under each touch/pointer contact. Off by default;
    /// meant for demo recordings, not daily use.
    pub show_touches: bool,
    /// Prefer server-side (compositor-owned = no) decorations. When `true`,
    /// top-level app windows are told to skip their own client-side titlebars
    /// for a borderless phone look. Child windows (dialogs) always keep CSD
    /// regardless, so toolkits like GTK still draw the header bar that holds a
    /// file chooser's Open/Cancel buttons.
    pub prefer_no_csd: bool,
    /// Scheduler utilization floor (`util_min`) applied to the render thread
    /// while it is drawing. See [`UclampMin`].
    pub uclamp_min: UclampMin,
    pub bindings: Vec<Binding>,
}

/// How to pick the `util_min` floor for the render thread.
///
/// Without a floor the render thread's utilization decays while the screen is
/// idle, so schedutil parks it on the little cluster at a low OPP and the first
/// frames of a touch are slow. Measured on the FP5: first frame after 12s idle
/// 11.78ms vs an 11.11ms budget (6/6 over), follow-up frames 10.21ms. With a
/// floor above the little cluster's capacity, 8.88ms (0/6 over) and 4.01ms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UclampMin {
    /// Derive from CPU topology at startup: just above the little cluster's
    /// `cpu_capacity`, which is where the migration knee sits. Portable across
    /// devices with different capacity splits; disables itself on machines whose
    /// CPUs are all the same capacity, where there is no bigger core to move to.
    Auto,
    /// No floor. The kernel default.
    Off,
    /// An explicit floor in the kernel's 0..=1024 utilization scale.
    Fixed(u32),
}

/// Long-press threshold when the config does not say otherwise. 800ms so a
/// volume nudge does not accidentally cross into the long-press action.
pub const DEFAULT_LONG_PRESS_MS: u64 = 800;

/// Output scale when `[main]` does not say otherwise: the FP5 panel is dense
/// enough that 1:1 client rendering (the old M4 behavior) is illegibly small.
pub const DEFAULT_DPI: f64 = 3.0;

/// Idle-blank timeout when `[main]` does not say otherwise: 10 minutes. `0` in
/// the config disables idle blanking entirely.
pub const DEFAULT_IDLE_BLANK_SECS: u64 = 600;

/// Card corner radius when `[main]` does not say otherwise.
pub const DEFAULT_CARD_RADIUS: f32 = 120.0;

/// Touch indicator is off unless `[main].show_touches = true`.
pub const DEFAULT_SHOW_TOUCHES: bool = false;

/// Prefer no client-side decorations by default: the phone shell wants
/// borderless app windows. Dialogs keep CSD regardless (see [`Config`]).
pub const DEFAULT_PREFER_NO_CSD: bool = true;

/// Utilization floor policy when `[main]` does not say otherwise. Auto-derived
/// from CPU topology, because the useful value is the little cluster's capacity
/// and that differs per device (382 on the FP5).
pub const DEFAULT_UCLAMP_MIN: UclampMin = UclampMin::Auto;

/// Parse the `uclamp_min` value: `"auto"`, `"off"`, or 0..=1024 (`0` = off).
/// Anything else is dropped with a warning and the default applies.
fn parse_uclamp_min(v: Option<&toml::Value>) -> UclampMin {
    let Some(v) = v else {
        return DEFAULT_UCLAMP_MIN;
    };
    match v {
        toml::Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "auto" => UclampMin::Auto,
            "off" | "false" | "none" => UclampMin::Off,
            _ => {
                warn!(value = %s, "uclamp_min must be \"auto\", \"off\", or 0..=1024");
                DEFAULT_UCLAMP_MIN
            }
        },
        toml::Value::Integer(0) => UclampMin::Off,
        toml::Value::Integer(n) if (1..=1024).contains(n) => UclampMin::Fixed(*n as u32),
        other => {
            warn!(value = %other, "uclamp_min must be \"auto\", \"off\", or 0..=1024");
            DEFAULT_UCLAMP_MIN
        }
    }
}

/// Shipped defaults, mirroring the user's niri bindings. Defined as TOML so the
/// documented example and the built-in behavior cannot drift apart.
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

/// Serde mirror of the on-disk shape, kept separate so the public types stay
/// free of `Option` soup and validation lives in one place.
#[derive(Deserialize, Default)]
struct RawConfig {
    long_press_ms: Option<u64>,
    #[serde(default)]
    binding: Vec<RawBinding>,
}

/// Top-level shape of `config.toml`. Other sections (display, gestures, ...)
/// may be added here later; unknown top-level keys are ignored by serde's
/// default behavior.
#[derive(Deserialize, Default)]
struct RawConfigFile {
    main: Option<RawMain>,
    keybinds: Option<RawConfig>,
}

#[derive(Deserialize, Default)]
struct RawMain {
    dpi: Option<f64>,
    idle_blank_secs: Option<u64>,
    card_radius: Option<f32>,
    show_touches: Option<bool>,
    prefer_no_csd: Option<bool>,
    /// `"auto"` (the default), `"off"`, or a number in 0..=1024. `0` means off.
    uclamp_min: Option<toml::Value>,
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

impl Config {
    /// The compiled-in defaults.
    pub fn defaults() -> Config {
        Config::parse(DEFAULT_TOML)
    }

    /// Parse config text, dropping invalid entries. A whole-file parse error
    /// yields an empty config; use [`Config::parse_or_defaults`] to fall back to
    /// the shipped bindings instead.
    pub fn parse(text: &str) -> Config {
        let file: RawConfigFile = match toml::from_str(text) {
            Ok(file) => file,
            Err(e) => {
                warn!(%e, "config is not valid TOML");
                return Config {
                    long_press_ms: DEFAULT_LONG_PRESS_MS,
                    dpi: DEFAULT_DPI,
                    idle_blank_secs: DEFAULT_IDLE_BLANK_SECS,
                    card_radius: DEFAULT_CARD_RADIUS,
                    show_touches: DEFAULT_SHOW_TOUCHES,
                    prefer_no_csd: DEFAULT_PREFER_NO_CSD,
                    uclamp_min: DEFAULT_UCLAMP_MIN,
                    bindings: Vec::new(),
                };
            }
        };
        let main = file.main.unwrap_or_default();
        let dpi = main.dpi.unwrap_or(DEFAULT_DPI);
        let idle_blank_secs = main.idle_blank_secs.unwrap_or(DEFAULT_IDLE_BLANK_SECS);
        let card_radius = main.card_radius.unwrap_or(DEFAULT_CARD_RADIUS).max(0.0);
        let show_touches = main.show_touches.unwrap_or(DEFAULT_SHOW_TOUCHES);
        let prefer_no_csd = main.prefer_no_csd.unwrap_or(DEFAULT_PREFER_NO_CSD);
        let uclamp_min = parse_uclamp_min(main.uclamp_min.as_ref());
        let raw = file.keybinds.unwrap_or_default();

        let bindings = raw.binding.into_iter().filter_map(convert).collect();
        Config {
            long_press_ms: raw.long_press_ms.unwrap_or(DEFAULT_LONG_PRESS_MS),
            dpi,
            idle_blank_secs,
            card_radius,
            show_touches,
            prefer_no_csd,
            uclamp_min,
            bindings,
        }
    }

    /// Like [`Config::parse`], but an unparseable file leaves the defaults in
    /// place so the hardware buttons keep working.
    pub fn parse_or_defaults(text: &str) -> Config {
        match toml::from_str::<RawConfigFile>(text) {
            Ok(_) => Config::parse(text),
            Err(e) => {
                warn!(%e, "config is not valid TOML; using defaults");
                Config::defaults()
            }
        }
    }
}

/// Validate one raw entry. Returns `None` (with a warning) for anything the
/// compositor cannot act on.
fn convert(raw: RawBinding) -> Option<Binding> {
    let press = match raw.press.as_str() {
        "short" => PressKind::Short,
        "long" => PressKind::Long,
        other => {
            warn!(key = %raw.key, press = %other, "skipping keybinding: press must be short or long");
            return None;
        }
    };

    let action = match (raw.command, raw.action) {
        (Some(cmd), None) => Action::Command(cmd),
        (None, Some(name)) => match name.as_str() {
            "close-app" => Action::CloseApp,
            "home" => Action::Home,
            "toggle-display" => Action::ToggleDisplay,
            "volume-up" => Action::VolumeUp,
            "volume-down" => Action::VolumeDown,
            "volume-mute" => Action::VolumeMute,
            other => {
                warn!(key = %raw.key, action = %other, "skipping keybinding: unknown action");
                return None;
            }
        },
        (Some(_), Some(_)) => {
            warn!(key = %raw.key, "skipping keybinding: command and action are mutually exclusive");
            return None;
        }
        (None, None) => {
            warn!(key = %raw.key, "skipping keybinding: needs either command or action");
            return None;
        }
    };

    let mut mods = ModMask::NONE;
    for name in &raw.mods {
        match name.as_str() {
            "Ctrl" | "Control" => mods.ctrl = true,
            "Alt" => mods.alt = true,
            "Shift" => mods.shift = true,
            "Super" | "Logo" | "Mod" => mods.logo = true,
            other => {
                warn!(key = %raw.key, modifier = %other, "skipping keybinding: unknown modifier");
                return None;
            }
        }
    }

    Some(Binding {
        key: raw.key,
        mods,
        press,
        action,
    })
}

// --- config.toml discovery + loading ---
//
// The counterpart to `sc_shell_model::persist` (which owns `state.toml` I/O):
// resolving where `config.toml` lives and reading it belongs with the parser,
// not the compositor. A missing file is normal (shipped defaults apply); an
// unreadable or unparseable one warns but never aborts.

use std::path::{Path, PathBuf};
use tracing::info;

/// `SPRINGCHICK_CONFIG` override: if set, it is the only path tried, with no
/// fallthrough to XDG or `/etc` when that file is missing. Injectable env lookup
/// so tests don't mutate real process env vars (multithreaded test binary).
fn env_override(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("SPRINGCHICK_CONFIG").map(PathBuf::from)
}

/// XDG-then-`/etc` candidates, in lookup order:
/// `$XDG_CONFIG_HOME/springchick/config.toml` (or `~/.config/...`), then
/// `/etc/springchick/config.toml`.
fn candidate_paths(env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let xdg = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("springchick/config.toml"));

    let mut paths = Vec::new();
    if let Some(path) = xdg {
        paths.push(path);
    }
    paths.push(PathBuf::from("/etc/springchick/config.toml"));
    paths
}

/// Try to read and parse one candidate path. `None` means "this tier failed" —
/// callers fall back to defaults (env override) or the next candidate (lookup
/// tiers). A missing file is silent; any other read error is a warning.
fn try_read(path: &Path) -> Option<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            info!(path = %path.display(), "loading config");
            Some(Config::parse_or_defaults(&text))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!(%e, path = %path.display(), "cannot read config");
            None
        }
    }
}

/// Read `config.toml` from the first tier that exists, falling back to the
/// shipped defaults. A missing file is normal; unreadable/unparseable warns,
/// never fatal.
pub fn load() -> Config {
    let real_env = |k: &str| std::env::var(k).ok();

    if let Some(path) = env_override(real_env) {
        return try_read(&path).unwrap_or_else(Config::defaults);
    }

    for path in candidate_paths(real_env) {
        if let Some(config) = try_read(&path) {
            return config;
        }
    }
    Config::defaults()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn parses_an_internal_action_with_mods() {
        let cfg = Config::parse(
            r#"
            [keybinds]
            [[keybinds.binding]]
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

    #[test]
    fn skips_invalid_entries_without_failing() {
        let cfg = Config::parse(
            r#"
            [keybinds]
            [[keybinds.binding]]
            key = "A"
            press = "sideways"
            command = "true"

            [[keybinds.binding]]
            key = "B"
            press = "short"

            [[keybinds.binding]]
            key = "C"
            press = "short"
            command = "true"
            action = "home"

            [[keybinds.binding]]
            key = "D"
            press = "short"
            action = "not-a-real-action"

            [[keybinds.binding]]
            key = "E"
            press = "short"
            command = "true"
            "#,
        );
        assert_eq!(cfg.bindings.len(), 1);
        assert_eq!(cfg.bindings[0].key, "E");
    }

    #[test]
    fn malformed_toml_yields_empty_not_panic() {
        let cfg = Config::parse("this is not toml {{{");
        assert!(cfg.bindings.is_empty());
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults_when_asked() {
        let cfg = Config::parse_or_defaults("this is not toml {{{");
        assert_eq!(cfg.bindings.len(), Config::defaults().bindings.len());
    }

    #[test]
    fn custom_long_press_ms_is_read() {
        let cfg = Config::parse("[keybinds]\nlong_press_ms = 800\n");
        assert_eq!(cfg.long_press_ms, 800);
    }

    #[test]
    fn dpi_defaults_to_3_when_main_section_absent() {
        let cfg = Config::parse("[keybinds]\nlong_press_ms = 800\n");
        assert_eq!(cfg.dpi, 3.0);
    }

    #[test]
    fn dpi_is_read_from_main_section() {
        let cfg = Config::parse("[main]\ndpi = 2\n");
        assert_eq!(cfg.dpi, 2.0);
    }

    #[test]
    fn fractional_dpi_is_read_from_main_section() {
        let cfg = Config::parse("[main]\ndpi = 2.5\n");
        assert_eq!(cfg.dpi, 2.5);
    }

    #[test]
    fn idle_blank_defaults_to_600_when_main_section_absent() {
        let cfg = Config::parse("[keybinds]\nlong_press_ms = 800\n");
        assert_eq!(cfg.idle_blank_secs, 600);
    }

    #[test]
    fn idle_blank_is_read_from_main_section() {
        let cfg = Config::parse("[main]\nidle_blank_secs = 120\n");
        assert_eq!(cfg.idle_blank_secs, 120);
    }

    #[test]
    fn idle_blank_zero_is_kept_as_disabled() {
        let cfg = Config::parse("[main]\nidle_blank_secs = 0\n");
        assert_eq!(cfg.idle_blank_secs, 0);
    }

    #[test]
    fn dpi_and_idle_blank_read_together_from_main() {
        let cfg = Config::parse("[main]\ndpi = 2\nidle_blank_secs = 90\n");
        assert_eq!(cfg.dpi, 2.0);
        assert_eq!(cfg.idle_blank_secs, 90);
    }

    #[test]
    fn card_radius_defaults_when_main_section_absent() {
        let cfg = Config::parse("[keybinds]\nlong_press_ms = 800\n");
        assert_eq!(cfg.card_radius, DEFAULT_CARD_RADIUS);
    }

    #[test]
    fn card_radius_is_read_from_main_section() {
        let cfg = Config::parse("[main]\ncard_radius = 12.5\n");
        assert_eq!(cfg.card_radius, 12.5);
    }

    #[test]
    fn negative_card_radius_clamped_to_zero() {
        let cfg = Config::parse("[main]\ncard_radius = -4.0\n");
        assert_eq!(cfg.card_radius, 0.0);
    }

    #[test]
    fn missing_keybinds_table_yields_empty_not_defaults() {
        let cfg = Config::parse("");
        assert_eq!(cfg.long_press_ms, DEFAULT_LONG_PRESS_MS);
        assert!(cfg.bindings.is_empty());
    }

    #[test]
    fn uclamp_min_defaults_to_auto() {
        assert_eq!(Config::parse("[main]\n").uclamp_min, UclampMin::Auto);
        assert_eq!(Config::defaults().uclamp_min, UclampMin::Auto);
    }

    #[test]
    fn uclamp_min_accepts_auto_off_and_numbers() {
        let p = |s: &str| Config::parse(s).uclamp_min;
        assert_eq!(p("[main]\nuclamp_min = \"auto\"\n"), UclampMin::Auto);
        assert_eq!(p("[main]\nuclamp_min = \"AUTO\"\n"), UclampMin::Auto);
        assert_eq!(p("[main]\nuclamp_min = \"off\"\n"), UclampMin::Off);
        // 0 is the natural way to spell "no floor" for a numeric setting.
        assert_eq!(p("[main]\nuclamp_min = 0\n"), UclampMin::Off);
        assert_eq!(p("[main]\nuclamp_min = 450\n"), UclampMin::Fixed(450));
        assert_eq!(p("[main]\nuclamp_min = 1024\n"), UclampMin::Fixed(1024));
    }

    #[test]
    fn uclamp_min_rejects_out_of_range_and_nonsense() {
        // Lenient parsing: a bad value falls back to the default, it does not
        // abort the whole config.
        let p = |s: &str| Config::parse(s).uclamp_min;
        assert_eq!(p("[main]\nuclamp_min = 2000\n"), UclampMin::Auto);
        assert_eq!(p("[main]\nuclamp_min = -5\n"), UclampMin::Auto);
        assert_eq!(p("[main]\nuclamp_min = \"fast\"\n"), UclampMin::Auto);
        assert_eq!(p("[main]\nuclamp_min = 1.5\n"), UclampMin::Auto);
    }

    #[test]
    fn a_bad_uclamp_min_does_not_break_the_rest_of_main() {
        let cfg = Config::parse("[main]\nuclamp_min = \"nope\"\ncard_radius = 42.0\n");
        assert_eq!(cfg.uclamp_min, UclampMin::Auto);
        assert_eq!(cfg.card_radius, 42.0);
    }

    #[test]
    fn defaults_cover_the_fp5_buttons() {
        let cfg = Config::defaults();
        let find = |key: &str, press: PressKind| {
            cfg.bindings
                .iter()
                .find(|b| b.key == key && b.press == press)
                .cloned()
        };
        assert_eq!(
            find("XF86AudioRaiseVolume", PressKind::Short)
                .unwrap()
                .action,
            Action::VolumeUp
        );
        assert_eq!(
            find("XF86AudioLowerVolume", PressKind::Short)
                .unwrap()
                .action,
            Action::VolumeDown
        );
        assert_eq!(
            find("XF86AudioRaiseVolume", PressKind::Long)
                .unwrap()
                .action,
            Action::CloseApp
        );
        assert!(matches!(
            find("XF86AudioLowerVolume", PressKind::Long).unwrap().action,
            Action::Command(ref c) if c.contains("wvkbd-mobintl")
        ));
        assert_eq!(
            find("XF86PowerOff", PressKind::Short).unwrap().action,
            Action::ToggleDisplay
        );
        assert!(matches!(
            find("XF86PowerOff", PressKind::Long).unwrap().action,
            Action::Command(ref c) if c.contains("poweroff")
        ));
        assert_eq!(
            find("Escape", PressKind::Short).unwrap().action,
            Action::Home
        );
    }

    #[test]
    fn parses_volume_actions() {
        let cfg = Config::parse(
            r#"
            [keybinds]
            [[keybinds.binding]]
            key = "XF86AudioRaiseVolume"
            press = "short"
            action = "volume-up"

            [[keybinds.binding]]
            key = "XF86AudioLowerVolume"
            press = "short"
            action = "volume-down"

            [[keybinds.binding]]
            key = "XF86AudioMute"
            press = "short"
            action = "volume-mute"
            "#,
        );
        assert_eq!(cfg.bindings.len(), 3);
        assert_eq!(cfg.bindings[0].action, Action::VolumeUp);
        assert_eq!(cfg.bindings[1].action, Action::VolumeDown);
        assert_eq!(cfg.bindings[2].action, Action::VolumeMute);
    }

    #[test]
    fn defaults_parse_from_the_shipped_text() {
        assert_eq!(
            Config::parse(DEFAULT_TOML).bindings.len(),
            Config::defaults().bindings.len()
        );
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
