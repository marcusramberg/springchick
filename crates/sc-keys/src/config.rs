//! Binding config: the on-disk TOML shape and its validated in-memory form.
//!
//! Validation is deliberately lenient. A bad entry is dropped with a warning and
//! the rest of the config still applies: on a phone, a compositor that refuses to
//! start over a config typo is a recovery session, while a skipped binding is a
//! button that does nothing.

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
    pub dpi: u32,
    /// Seconds of no input before the panel idle-blanks. `0` disables idle
    /// blanking (the power button still blanks on demand).
    pub idle_blank_secs: u64,
    pub bindings: Vec<Binding>,
}

/// Long-press threshold when the config does not say otherwise. 800ms so a
/// volume nudge does not accidentally cross into the long-press action.
pub const DEFAULT_LONG_PRESS_MS: u64 = 800;

/// Output scale when `[main]` does not say otherwise: the FP5 panel is dense
/// enough that 1:1 client rendering (the old M4 behavior) is illegibly small.
pub const DEFAULT_DPI: u32 = 3;

/// Idle-blank timeout when `[main]` does not say otherwise: 10 minutes. `0` in
/// the config disables idle blanking entirely.
pub const DEFAULT_IDLE_BLANK_SECS: u64 = 600;

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
    dpi: Option<u32>,
    idle_blank_secs: Option<u64>,
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
                    bindings: Vec::new(),
                };
            }
        };
        let main = file.main.unwrap_or_default();
        let dpi = main.dpi.unwrap_or(DEFAULT_DPI);
        let idle_blank_secs = main.idle_blank_secs.unwrap_or(DEFAULT_IDLE_BLANK_SECS);
        let raw = file.keybinds.unwrap_or_default();

        let bindings = raw.binding.into_iter().filter_map(convert).collect();
        Config {
            long_press_ms: raw.long_press_ms.unwrap_or(DEFAULT_LONG_PRESS_MS),
            dpi,
            idle_blank_secs,
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
        assert_eq!(cfg.dpi, 3);
    }

    #[test]
    fn dpi_is_read_from_main_section() {
        let cfg = Config::parse("[main]\ndpi = 2\n");
        assert_eq!(cfg.dpi, 2);
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
        assert_eq!(cfg.dpi, 2);
        assert_eq!(cfg.idle_blank_secs, 90);
    }

    #[test]
    fn missing_keybinds_table_yields_empty_not_defaults() {
        let cfg = Config::parse("");
        assert_eq!(cfg.long_press_ms, DEFAULT_LONG_PRESS_MS);
        assert!(cfg.bindings.is_empty());
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
}
