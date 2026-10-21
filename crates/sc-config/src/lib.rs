//! `config.toml`: `[main]`, `[resources]` and `[keybinds]`. Lenient: a bad
//! entry is dropped with a warning and the rest applies, because a compositor
//! that won't start over a typo on a phone means a recovery session.
#![forbid(unsafe_code)]

use serde::Deserialize;
use std::collections::BTreeMap;
use tracing::warn;

/// No lock modifiers: a stuck Caps Lock must not disable every binding.
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Run through `sh -c`.
    Command(String),
    CloseApp,
    Home,
    /// DRM backend only.
    ToggleDisplay,
    VolumeUp,
    VolumeDown,
    VolumeMute,
    /// Fullscreen (immersive, rotates) vs. maximized.
    ToggleFullscreen,
    Search,
    /// Toward older apps, opening the deck if needed. Hold it on a modifier:
    /// releasing the modifier commits the focused card.
    SwitcherNext,
    SwitcherPrev,
    /// PNG to the clipboard.
    Screenshot,
}

impl Action {
    /// `Command` has no name; it is spelled `command = "..."`.
    pub fn from_name(name: &str) -> Option<Action> {
        Some(match name {
            "close-app" => Action::CloseApp,
            "home" => Action::Home,
            "toggle-display" => Action::ToggleDisplay,
            "volume-up" => Action::VolumeUp,
            "volume-down" => Action::VolumeDown,
            "volume-mute" => Action::VolumeMute,
            "toggle-fullscreen" => Action::ToggleFullscreen,
            "search" => Action::Search,
            "switcher-next" => Action::SwitcherNext,
            "switcher-prev" => Action::SwitcherPrev,
            "screenshot" => Action::Screenshot,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// xkb keysym name.
    pub key: String,
    /// Chord partners: fires as soon as `key` and all of these are down, in any
    /// order. `mods` and `press` don't apply.
    pub with: Vec<String>,
    pub mods: ModMask,
    pub press: PressKind,
    pub action: Action,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub long_press_ms: u64,
    /// Advertised via `wp_fractional_scale`; fractional values are fine.
    pub dpi: f64,
    /// `0` disables idle blanking.
    pub idle_blank_secs: u64,
    /// Physical px, for the switcher deck and drag-lift card; other radii scale
    /// from it. `None` (`"auto"`): the panel's corner radius, else
    /// [`DEFAULT_CARD_RADIUS`].
    pub card_radius: Option<f32>,
    /// For demo recordings.
    pub show_touches: bool,
    /// Also write each screenshot to `$XDG_PICTURES_DIR/Screenshots/`.
    pub save_screenshots: bool,
    /// Touchpads only, applied as they are added.
    pub natural_scroll: bool,
    /// Tell top-level windows to drop client-side titlebars. Dialogs keep CSD
    /// so GTK file choosers still get their Open/Cancel header bar.
    pub prefer_no_csd: bool,
    pub uclamp_min: UclampMin,
    pub gpu_min_mhz: GpuMinMhz,
    /// Only where the connector reports `vrr_capable`.
    pub vrr: bool,
    /// How long an accelerometer reading must hold before turning. `0` turns
    /// immediately.
    pub rotation_settle_ms: u64,
    /// Each half of the rotation dip-to-black. `0` swaps instantly.
    pub rotation_fade_ms: u64,
    /// Turn Home and ordinary windows too, not just fullscreen apps. The
    /// `rotation` ipc verb overrides it for the session.
    pub rotate_ui: bool,
    /// Run via `sh -c` once the Wayland socket exists, for setups without
    /// systemd user units. Ignored on reload.
    pub startup: Vec<String>,
    pub resources: Resources,
    pub xkb: Xkb,
    pub predict: Predict,
    pub bindings: Vec<Binding>,
}

/// The uinput device name of springchick's on-screen keyboard. It types
/// letters like hardware, but must not count as one and hide itself.
pub const OSK_DEVICE_NAME: &str = "springchick osk";

/// Word completion on the on-screen keyboard. It always completes words you
/// have typed before; `dictionaries` adds cold-start suggestions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Predict {
    /// Hunspell dictionary names, as the files are called without `.dic`
    /// (`"nb_NO"`, `"en_US"`). Several make the keyboard bilingual. Looked up
    /// in the XDG data dirs under `hunspell/` and `myspell/dicts/`.
    pub dictionaries: Vec<String>,
}

/// The seat keymap, as xkbcommon names it. Empty fields fall back to
/// `XKB_DEFAULT_*`, then xkbcommon's own defaults (`us`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Xkb {
    pub model: String,
    /// Comma-separated; switch between them with a `grp:` option.
    pub layout: String,
    pub variant: String,
    pub options: String,
}

/// Focused app vs. background resource limits, applied as properties on each
/// app's systemd scope cgroup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resources {
    /// Off leaves every app at the systemd defaults.
    pub enable: bool,
    /// Proportional: only decides who yields under contention, never caps.
    pub fg_cpu_weight: u32,
    pub bg_cpu_weight: u32,
    /// Systemd spelling (`"512M"`, `"infinity"`). Throttle-and-reclaim, not a
    /// kill. Set below real usage it reclaims continuously and costs power,
    /// hence `"infinity"` until measured per device.
    pub bg_memory_high: String,
    /// Percentage of one core (`"30%"`, `"infinity"`), shared by the whole
    /// cgroup. This is what saves power: a weight does nothing for an app
    /// spinning alone on an idle phone. Too low slows legitimate background work.
    pub bg_cpu_quota: String,
    /// `"auto"` (the efficiency cluster, derived from `cpu_capacity`), `"off"`,
    /// or a list (`"0-3"`). Auto disables itself on symmetric CPUs.
    pub bg_allowed_cpus: String,
    /// Freeze background apps (`cgroup.freeze`) after `freeze_delay_secs`.
    pub freeze: bool,
    pub freeze_delay_secs: u64,
    /// Messengers thaw for `freeze_wake_for_secs` every `freeze_wake_secs` so
    /// notifications get through.
    pub freeze_wake_secs: u64,
    pub freeze_wake_for_secs: u64,
    /// Per-app policy by catalog id, overriding the category default.
    pub apps: BTreeMap<String, AppPolicy>,
}

/// What a background app gets. The default comes from its `.desktop`
/// categories (`sc_compositor::resources`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppPolicy {
    /// Never touched, not even throttled.
    Ignore,
    /// CPU weight/quota/pinning only.
    Throttle,
    /// Throttle, then freeze after the delay.
    Freeze,
    /// Freeze with periodic wake-ups.
    Messenger,
}

impl std::str::FromStr for AppPolicy {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "ignore" => AppPolicy::Ignore,
            "throttle" => AppPolicy::Throttle,
            "freeze" => AppPolicy::Freeze,
            "messenger" => AppPolicy::Messenger,
            _ => return Err(()),
        })
    }
}

/// Without a floor the render thread decays onto the little cluster at a low
/// OPP and the first frames of a touch are slow (FP5: 11.78ms vs 11.11ms
/// budget, 6/6 over; with a floor 8.88ms, 0/6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UclampMin {
    /// Just above the little cluster's `cpu_capacity`, the migration knee.
    /// Disables itself on symmetric CPUs.
    Auto,
    Off,
    /// Kernel 0..=1024 scale.
    Fixed(u32),
}

/// GPU devfreq `min_freq` held while drawing, for the same ramp-up reason as
/// [`UclampMin`]. Needs `min_freq` writable (the NixOS module's udev rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuMinMhz {
    /// The lowest OPP at or above half the GPU's top clock.
    Auto,
    Off,
    Fixed(u32),
}

/// 800ms so a volume nudge doesn't cross into the long press.
pub const DEFAULT_LONG_PRESS_MS: u64 = 800;

/// The FP5 panel is illegible at 1:1.
pub const DEFAULT_DPI: f64 = 3.0;

/// 10 minutes.
pub const DEFAULT_IDLE_BLANK_SECS: u64 = 600;

/// Without a known panel shape.
pub const DEFAULT_CARD_RADIUS: f32 = 120.0;

pub const DEFAULT_SHOW_TOUCHES: bool = false;

pub const DEFAULT_SAVE_SCREENSHOTS: bool = true;

pub const DEFAULT_NATURAL_SCROLL: bool = true;

pub const DEFAULT_PREFER_NO_CSD: bool = true;

pub const DEFAULT_UCLAMP_MIN: UclampMin = UclampMin::Auto;

pub const DEFAULT_GPU_MIN_MHZ: GpuMinMhz = GpuMinMhz::Auto;

/// Still a no-op unless the connector reports `vrr_capable`.
pub const DEFAULT_VRR: bool = true;

pub const DEFAULT_ROTATION_SETTLE_MS: u64 = 400;

pub const DEFAULT_ROTATION_FADE_MS: u64 = 130;
/// Off: only fullscreen apps turn.
pub const DEFAULT_ROTATE_UI: bool = false;

impl Default for Resources {
    /// 10:1 CPU weights; the compositor sits above both at 200 (`nix/module.nix`).
    fn default() -> Resources {
        Resources {
            enable: true,
            fg_cpu_weight: 100,
            bg_cpu_weight: 20,
            bg_memory_high: "infinity".to_string(),
            bg_cpu_quota: "30%".to_string(),
            bg_allowed_cpus: "auto".to_string(),
            freeze: true,
            freeze_delay_secs: 60,
            freeze_wake_secs: 30,
            freeze_wake_for_secs: 3,
            apps: BTreeMap::new(),
        }
    }
}

/// Each bad value drops back to its default.
fn parse_resources(raw: Option<RawResources>) -> Resources {
    let d = Resources::default();
    let Some(raw) = raw else { return d };
    // systemd's CPUWeight range.
    let weight = |v: Option<u32>, name: &str, default: u32| match v {
        Some(w) if (1..=10_000).contains(&w) => w,
        Some(w) => {
            warn!(value = w, name, "cpu weight must be 1..=10000");
            default
        }
        None => default,
    };
    let bg_memory_high = match raw.bg_memory_high {
        Some(s) if is_memory_size(&s) => s,
        Some(s) => {
            warn!(value = %s, "bg_memory_high must be a systemd size (\"512M\") or \"infinity\"");
            d.bg_memory_high
        }
        None => d.bg_memory_high,
    };
    let bg_cpu_quota = match raw.bg_cpu_quota {
        Some(s) if is_cpu_quota(&s) => s,
        Some(s) => {
            warn!(value = %s, "bg_cpu_quota must be a percentage (\"30%\") or \"infinity\"");
            d.bg_cpu_quota
        }
        None => d.bg_cpu_quota,
    };
    let bg_allowed_cpus = match raw.bg_allowed_cpus {
        Some(s) if is_cpu_list(&s) => s,
        Some(s) => {
            warn!(value = %s, "bg_allowed_cpus must be \"auto\", \"off\", or a CPU list (\"0-3\")");
            d.bg_allowed_cpus
        }
        None => d.bg_allowed_cpus,
    };
    let apps = raw
        .apps
        .into_iter()
        .filter_map(|(id, policy)| match policy.parse() {
            Ok(p) => Some((id, p)),
            Err(()) => {
                warn!(app = %id, value = %policy, "app policy must be ignore, throttle, freeze or messenger");
                None
            }
        })
        .collect();
    Resources {
        enable: raw.enable.unwrap_or(d.enable),
        fg_cpu_weight: weight(raw.fg_cpu_weight, "fg_cpu_weight", d.fg_cpu_weight),
        bg_cpu_weight: weight(raw.bg_cpu_weight, "bg_cpu_weight", d.bg_cpu_weight),
        bg_memory_high,
        bg_cpu_quota,
        bg_allowed_cpus,
        freeze: raw.freeze.unwrap_or(d.freeze),
        freeze_delay_secs: raw.freeze_delay_secs.unwrap_or(d.freeze_delay_secs),
        freeze_wake_secs: raw.freeze_wake_secs.unwrap_or(d.freeze_wake_secs),
        freeze_wake_for_secs: raw.freeze_wake_for_secs.unwrap_or(d.freeze_wake_for_secs),
        apps,
    }
}

/// `auto`, `off`, or an `AllowedCPUs` list (`0`, `0-3`, `0-1,4`).
fn is_cpu_list(s: &str) -> bool {
    if s == "auto" || s == "off" {
        return true;
    }
    !s.is_empty()
        && s.split(',').all(|part| {
            let mut ends = part.split('-');
            let ok = |v: Option<&str>| {
                v.is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
            };
            ok(ends.next()) && ends.next().is_none_or(|e| !e.is_empty() && ok(Some(e)))
        })
}

/// `infinity` or a percentage; over 100% means more than one core.
fn is_cpu_quota(s: &str) -> bool {
    if s.eq_ignore_ascii_case("infinity") {
        return true;
    }
    match s.strip_suffix('%') {
        Some(n) => !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// `infinity`, a percentage, or bytes with an optional suffix. Checked here
/// because the `systemctl` call is fire-and-forget.
fn is_memory_size(s: &str) -> bool {
    if s.eq_ignore_ascii_case("infinity") {
        return true;
    }
    let (digits, suffix) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    !digits.is_empty()
        && matches!(
            suffix,
            "" | "%" | "K" | "M" | "G" | "T" | "Ki" | "Mi" | "Gi" | "Ti"
        )
}

/// `"auto"` or px; negative clamps to 0.
fn parse_card_radius(v: Option<&toml::Value>) -> Option<f32> {
    match v? {
        toml::Value::Float(f) => Some((*f as f32).max(0.0)),
        toml::Value::Integer(n) => Some((*n as f32).max(0.0)),
        toml::Value::String(s) if s.eq_ignore_ascii_case("auto") => None,
        other => {
            warn!(value = %other, "card_radius must be \"auto\" or px");
            None
        }
    }
}

/// `"auto"`, `"off"`, or MHz (`0` = off).
fn parse_gpu_min_mhz(v: Option<&toml::Value>) -> GpuMinMhz {
    let Some(v) = v else {
        return DEFAULT_GPU_MIN_MHZ;
    };
    match v {
        toml::Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "auto" => GpuMinMhz::Auto,
            "off" | "false" | "none" => GpuMinMhz::Off,
            _ => {
                warn!(value = %s, "gpu_min_mhz must be \"auto\", \"off\", or MHz");
                DEFAULT_GPU_MIN_MHZ
            }
        },
        toml::Value::Integer(0) => GpuMinMhz::Off,
        toml::Value::Integer(n) if (1..=100_000).contains(n) => GpuMinMhz::Fixed(*n as u32),
        other => {
            warn!(value = %other, "gpu_min_mhz must be \"auto\", \"off\", or MHz");
            DEFAULT_GPU_MIN_MHZ
        }
    }
}

/// `"auto"`, `"off"`, or 0..=1024 (`0` = off).
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

/// Defined as TOML so the documented example and built-in behavior can't
/// drift.
pub const DEFAULT_TOML: &str = r#"
[keybinds]
long_press_ms = 800

[[keybinds.binding]]
key = "XF86AudioRaiseVolume"
action = "volume-up"

[[keybinds.binding]]
key = "XF86AudioRaiseVolume"
press = "long"
action = "close-app"

[[keybinds.binding]]
key = "XF86AudioLowerVolume"
action = "volume-down"

[[keybinds.binding]]
key = "XF86AudioLowerVolume"
press = "long"
command = "pkill -SIGRTMIN -f wvkbd-mobintl"

[[keybinds.binding]]
key = "XF86PowerOff"
action = "toggle-display"

[[keybinds.binding]]
key = "XF86PowerOff"
press = "long"
command = "systemctl poweroff"

[[keybinds.binding]]
key = "h"
mods = ["Super"]
action = "home"

[[keybinds.binding]]
key = "f"
mods = ["Super"]
action = "toggle-fullscreen"

[[keybinds.binding]]
key = "s"
mods = ["Super"]
action = "search"

[[keybinds.binding]]
key = "Print"
action = "screenshot"

[[keybinds.binding]]
key = "Tab"
mods = ["Super"]
action = "switcher-next"

[[keybinds.binding]]
key = "ISO_Left_Tab"
mods = ["Super", "Shift"]
action = "switcher-prev"

[[keybinds.binding]]
key = "XF86PowerOff"
with = ["XF86AudioLowerVolume"]
action = "screenshot"
"#;

/// Serde mirror of the file; validation lives in one place.
#[derive(Deserialize, Default)]
struct RawConfig {
    long_press_ms: Option<u64>,
    #[serde(default)]
    binding: Vec<RawBinding>,
    #[serde(flatten)]
    unknown: toml::Table,
}

/// Unknown top-level keys are ignored.
#[derive(Deserialize, Default)]
struct RawConfigFile {
    main: Option<RawMain>,
    keybinds: Option<RawConfig>,
    resources: Option<RawResources>,
    xkb: Option<RawXkb>,
    predict: Option<RawPredict>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Deserialize, Default)]
struct RawPredict {
    dictionaries: Option<Vec<String>>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Deserialize, Default)]
struct RawXkb {
    model: Option<String>,
    layout: Option<String>,
    variant: Option<String>,
    options: Option<String>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Deserialize, Default)]
struct RawResources {
    enable: Option<bool>,
    fg_cpu_weight: Option<u32>,
    bg_cpu_weight: Option<u32>,
    bg_memory_high: Option<String>,
    bg_cpu_quota: Option<String>,
    bg_allowed_cpus: Option<String>,
    freeze: Option<bool>,
    freeze_delay_secs: Option<u64>,
    freeze_wake_secs: Option<u64>,
    freeze_wake_for_secs: Option<u64>,
    #[serde(default)]
    apps: BTreeMap<String, String>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Deserialize, Default)]
struct RawMain {
    dpi: Option<f64>,
    idle_blank_secs: Option<u64>,
    card_radius: Option<toml::Value>,
    show_touches: Option<bool>,
    save_screenshots: Option<bool>,
    natural_scroll: Option<bool>,
    prefer_no_csd: Option<bool>,
    uclamp_min: Option<toml::Value>,
    gpu_min_mhz: Option<toml::Value>,
    vrr: Option<bool>,
    rotation_settle_ms: Option<u64>,
    rotation_fade_ms: Option<u64>,
    rotate_ui: Option<bool>,
    #[serde(default)]
    startup: Vec<String>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Deserialize)]
struct RawBinding {
    key: String,
    #[serde(default)]
    with: Vec<String>,
    #[serde(default)]
    mods: Vec<String>,
    press: Option<String>,
    command: Option<String>,
    action: Option<String>,
    #[serde(flatten)]
    unknown: toml::Table,
}

/// Keys serde ignored, dotted. Returned rather than logged so the test can see
/// them; `[xkbd]` instead of `[xkb]` is silent otherwise.
fn unknown_keys(file: &RawConfigFile) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |prefix: &str, table: &toml::Table| {
        out.extend(table.keys().map(|k| format!("{prefix}{k}")));
    };
    add("", &file.unknown);
    if let Some(main) = &file.main {
        add("main.", &main.unknown);
    }
    if let Some(xkb) = &file.xkb {
        add("xkb.", &xkb.unknown);
    }
    if let Some(predict) = &file.predict {
        add("predict.", &predict.unknown);
    }
    if let Some(resources) = &file.resources {
        add("resources.", &resources.unknown);
    }
    if let Some(keybinds) = &file.keybinds {
        add("keybinds.", &keybinds.unknown);
        for (i, b) in keybinds.binding.iter().enumerate() {
            add(&format!("keybinds.binding[{i}]."), &b.unknown);
        }
    }
    out
}

impl Config {
    pub fn defaults() -> Config {
        Config::parse(DEFAULT_TOML)
    }

    /// A whole-file parse error yields an empty config; see
    /// [`Config::parse_or_defaults`].
    pub fn parse(text: &str) -> Config {
        let file: RawConfigFile = match toml::from_str(text) {
            Ok(file) => file,
            Err(e) => {
                warn!(%e, "config is not valid TOML");
                return Config {
                    long_press_ms: DEFAULT_LONG_PRESS_MS,
                    dpi: DEFAULT_DPI,
                    idle_blank_secs: DEFAULT_IDLE_BLANK_SECS,
                    card_radius: None,
                    show_touches: DEFAULT_SHOW_TOUCHES,
                    save_screenshots: DEFAULT_SAVE_SCREENSHOTS,
                    natural_scroll: DEFAULT_NATURAL_SCROLL,
                    prefer_no_csd: DEFAULT_PREFER_NO_CSD,
                    uclamp_min: DEFAULT_UCLAMP_MIN,
                    gpu_min_mhz: DEFAULT_GPU_MIN_MHZ,
                    vrr: DEFAULT_VRR,
                    rotation_settle_ms: DEFAULT_ROTATION_SETTLE_MS,
                    rotation_fade_ms: DEFAULT_ROTATION_FADE_MS,
                    rotate_ui: DEFAULT_ROTATE_UI,
                    startup: Vec::new(),
                    resources: Resources::default(),
                    xkb: Xkb::default(),
                    predict: Predict::default(),
                    bindings: Vec::new(),
                };
            }
        };
        for key in unknown_keys(&file) {
            warn!(%key, "unknown config key, ignored");
        }
        let main = file.main.unwrap_or_default();
        let dpi = main.dpi.unwrap_or(DEFAULT_DPI);
        let idle_blank_secs = main.idle_blank_secs.unwrap_or(DEFAULT_IDLE_BLANK_SECS);
        let card_radius = parse_card_radius(main.card_radius.as_ref());
        let show_touches = main.show_touches.unwrap_or(DEFAULT_SHOW_TOUCHES);
        let save_screenshots = main.save_screenshots.unwrap_or(DEFAULT_SAVE_SCREENSHOTS);
        let natural_scroll = main.natural_scroll.unwrap_or(DEFAULT_NATURAL_SCROLL);
        let prefer_no_csd = main.prefer_no_csd.unwrap_or(DEFAULT_PREFER_NO_CSD);
        let uclamp_min = parse_uclamp_min(main.uclamp_min.as_ref());
        let gpu_min_mhz = parse_gpu_min_mhz(main.gpu_min_mhz.as_ref());
        let vrr = main.vrr.unwrap_or(DEFAULT_VRR);
        let rotation_settle_ms = main
            .rotation_settle_ms
            .unwrap_or(DEFAULT_ROTATION_SETTLE_MS);
        let rotation_fade_ms = main.rotation_fade_ms.unwrap_or(DEFAULT_ROTATION_FADE_MS);
        let rotate_ui = main.rotate_ui.unwrap_or(DEFAULT_ROTATE_UI);
        let startup = main.startup;
        let resources = parse_resources(file.resources);
        let xkb = file.xkb.map_or_else(Xkb::default, |x| Xkb {
            model: x.model.unwrap_or_default(),
            layout: x.layout.unwrap_or_default(),
            variant: x.variant.unwrap_or_default(),
            options: x.options.unwrap_or_default(),
        });
        let predict = file.predict.map_or_else(Predict::default, |p| Predict {
            dictionaries: p.dictionaries.unwrap_or_default(),
        });
        let raw = file.keybinds.unwrap_or_default();

        let bindings = raw.binding.into_iter().filter_map(convert).collect();
        Config {
            long_press_ms: raw.long_press_ms.unwrap_or(DEFAULT_LONG_PRESS_MS),
            dpi,
            idle_blank_secs,
            card_radius,
            show_touches,
            save_screenshots,
            natural_scroll,
            prefer_no_csd,
            uclamp_min,
            gpu_min_mhz,
            vrr,
            rotation_settle_ms,
            rotation_fade_ms,
            rotate_ui,
            startup,
            resources,
            xkb,
            predict,
            bindings,
        }
    }

    /// An unparseable file keeps the defaults so hardware buttons still work.
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

fn convert(raw: RawBinding) -> Option<Binding> {
    if !raw.with.is_empty() && raw.press.as_deref() == Some("long") {
        warn!(key = %raw.key, "skipping keybinding: a chord (`with`) fires on press, it can't be long");
        return None;
    }
    let press = match raw.press.as_deref().unwrap_or("short") {
        "short" => PressKind::Short,
        "long" => PressKind::Long,
        other => {
            warn!(key = %raw.key, press = %other, "skipping keybinding: press must be short or long");
            return None;
        }
    };

    let action = match (raw.command, raw.action) {
        (Some(cmd), None) => Action::Command(cmd),
        (None, Some(name)) => match Action::from_name(&name) {
            Some(action) => action,
            None => {
                warn!(key = %raw.key, action = %name, "skipping keybinding: unknown action");
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
        with: raw.with,
        mods,
        press,
        action,
    })
}

// config.toml discovery and loading. A missing file is normal; unreadable or
// unparseable warns but never aborts.

use std::path::{Path, PathBuf};
use tracing::info;

/// If set, the only path tried. Injectable so tests don't mutate process env.
fn env_override(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("SPRINGCHICK_CONFIG").map(PathBuf::from)
}

fn config_home(env: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))
}

/// `$XDG_CONFIG_HOME/springchick/config.toml` (or `~/.config/...`), then
/// `/etc/springchick/config.toml`.
fn candidate_paths(env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let xdg = config_home(&env).map(|base| base.join("springchick/config.toml"));

    let mut paths = Vec::new();
    if let Some(path) = xdg {
        paths.push(path);
    }
    paths.push(PathBuf::from("/etc/springchick/config.toml"));
    paths
}

/// `$XDG_PICTURES_DIR`, then xdg-user-dirs' `user-dirs.dirs`, then `~/Pictures`.
fn pictures_dir(
    env: impl Fn(&str) -> Option<String>,
    read: impl Fn(&Path) -> Option<String>,
) -> Option<PathBuf> {
    if let Some(dir) = env("XDG_PICTURES_DIR") {
        return Some(dir.into());
    }
    let home = env("HOME")?;
    let from_user_dirs = config_home(&env)
        .and_then(|c| read(&c.join("user-dirs.dirs")))
        .and_then(|dirs| {
            let v = dirs
                .lines()
                .find_map(|l| l.trim().strip_prefix("XDG_PICTURES_DIR="))?
                .trim()
                .trim_matches('"')
                .to_string();
            let v = match v.strip_prefix("$HOME") {
                Some(rest) => format!("{home}{rest}"),
                None => v,
            };
            v.starts_with('/').then(|| PathBuf::from(v))
        });
    Some(from_user_dirs.unwrap_or_else(|| Path::new(&home).join("Pictures")))
}

/// Where `save_screenshots` writes. `None` without `$HOME`.
pub fn screenshot_dir() -> Option<PathBuf> {
    pictures_dir(
        |k| std::env::var(k).ok(),
        |p| std::fs::read_to_string(p).ok(),
    )
    .map(|p| p.join("Screenshots"))
}

/// A missing file is silent; other read errors warn.
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
    fn natural_scroll_defaults_on_and_can_be_disabled() {
        assert!(Config::parse("").natural_scroll);
        assert!(!Config::parse("[main]\nnatural_scroll = false").natural_scroll);
    }

    #[test]
    fn parses_a_command_binding() {
        let cfg = Config::parse(
            r#"
            [keybinds]
            [[keybinds.binding]]
            key = "XF86AudioRaiseVolume"
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
    fn parses_a_chord_and_rejects_a_long_one() {
        let cfg = Config::parse(
            r#"
            [keybinds]
            [[keybinds.binding]]
            key = "XF86PowerOff"
            with = ["XF86AudioLowerVolume"]
            action = "screenshot"

            [[keybinds.binding]]
            key = "XF86PowerOff"
            with = ["XF86AudioRaiseVolume"]
            press = "long"
            action = "home"
            "#,
        );
        assert_eq!(cfg.bindings.len(), 1);
        assert_eq!(cfg.bindings[0].with, vec!["XF86AudioLowerVolume"]);
        assert_eq!(cfg.bindings[0].action, Action::Screenshot);
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
    fn rotation_timings_default_when_main_section_absent() {
        let cfg = Config::parse("[keybinds]\nlong_press_ms = 800\n");
        assert_eq!(cfg.rotation_settle_ms, DEFAULT_ROTATION_SETTLE_MS);
        assert_eq!(cfg.rotation_fade_ms, DEFAULT_ROTATION_FADE_MS);
        assert_eq!(cfg.rotate_ui, DEFAULT_ROTATE_UI);
    }

    #[test]
    fn rotation_timings_are_read_from_main_section() {
        let cfg = Config::parse("[main]\nrotation_settle_ms = 250\nrotation_fade_ms = 0\n");
        assert_eq!(cfg.rotation_settle_ms, 250);
        assert_eq!(cfg.rotation_fade_ms, 0);
        assert!(Config::parse("[main]\nrotate_ui = true\n").rotate_ui);
    }

    #[test]
    fn predict_section_is_optional() {
        assert!(Config::parse("").predict.dictionaries.is_empty());
        assert_eq!(
            Config::parse("[predict]\ndictionaries = [\"nb_NO\", \"en_US\"]\n")
                .predict
                .dictionaries,
            ["nb_NO", "en_US"]
        );
    }

    #[test]
    fn unknown_keys_are_collected_for_the_warning() {
        let keys = |t: &str| unknown_keys(&toml::from_str(t).unwrap());
        assert!(keys("[xkb]\nlayout = \"no\"\n").is_empty());
        assert_eq!(keys("[xkbd]\nlayout = \"no\"\n"), ["xkbd"]);
        assert_eq!(keys("[main]\ndpy = 3\n"), ["main.dpy"]);
        assert_eq!(
            keys("[keybinds]\n[[keybinds.binding]]\nkey = \"a\"\nmod = [\"Super\"]\n"),
            ["keybinds.binding[0].mod"]
        );
    }

    #[test]
    fn xkb_section_is_optional_and_partial() {
        assert_eq!(Config::parse("").xkb, Xkb::default());
        let x =
            Config::parse("[xkb]\nlayout = \"us,de\"\noptions = \"grp:win_space_toggle\"\n").xkb;
        assert_eq!(x.layout, "us,de");
        assert_eq!(x.options, "grp:win_space_toggle");
        assert_eq!(x.variant, "");
    }

    #[test]
    fn card_radius_defaults_when_main_section_absent() {
        let cfg = Config::parse("[keybinds]\nlong_press_ms = 800\n");
        assert_eq!(cfg.card_radius, None);
    }

    #[test]
    fn card_radius_is_read_from_main_section() {
        let p = |s: &str| Config::parse(s).card_radius;
        assert_eq!(p("[main]\ncard_radius = 12.5\n"), Some(12.5));
        assert_eq!(p("[main]\ncard_radius = 90\n"), Some(90.0));
        assert_eq!(p("[main]\ncard_radius = \"auto\"\n"), None);
        assert_eq!(p("[main]\ncard_radius = \"round\"\n"), None);
    }

    #[test]
    fn negative_card_radius_clamped_to_zero() {
        let cfg = Config::parse("[main]\ncard_radius = -4.0\n");
        assert_eq!(cfg.card_radius, Some(0.0));
    }

    #[test]
    fn missing_keybinds_table_yields_empty_not_defaults() {
        let cfg = Config::parse("");
        assert_eq!(cfg.long_press_ms, DEFAULT_LONG_PRESS_MS);
        assert!(cfg.bindings.is_empty());
    }

    #[test]
    fn vrr_defaults_on_and_parses() {
        assert!(Config::parse("[main]\n").vrr);
        assert!(Config::defaults().vrr);
        assert!(!Config::parse("[main]\nvrr = false\n").vrr);
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
        assert_eq!(p("[main]\nuclamp_min = 0\n"), UclampMin::Off);
        assert_eq!(p("[main]\nuclamp_min = 450\n"), UclampMin::Fixed(450));
        assert_eq!(p("[main]\nuclamp_min = 1024\n"), UclampMin::Fixed(1024));
    }

    #[test]
    fn uclamp_min_rejects_out_of_range_and_nonsense() {
        let p = |s: &str| Config::parse(s).uclamp_min;
        assert_eq!(p("[main]\nuclamp_min = 2000\n"), UclampMin::Auto);
        assert_eq!(p("[main]\nuclamp_min = -5\n"), UclampMin::Auto);
        assert_eq!(p("[main]\nuclamp_min = \"fast\"\n"), UclampMin::Auto);
        assert_eq!(p("[main]\nuclamp_min = 1.5\n"), UclampMin::Auto);
    }

    #[test]
    fn gpu_min_mhz_parses_auto_off_and_numbers() {
        let p = |s: &str| Config::parse(s).gpu_min_mhz;
        assert_eq!(p("[main]\n"), GpuMinMhz::Auto);
        assert_eq!(p("[main]\ngpu_min_mhz = \"off\"\n"), GpuMinMhz::Off);
        assert_eq!(p("[main]\ngpu_min_mhz = 0\n"), GpuMinMhz::Off);
        assert_eq!(p("[main]\ngpu_min_mhz = 450\n"), GpuMinMhz::Fixed(450));
        assert_eq!(p("[main]\ngpu_min_mhz = -1\n"), GpuMinMhz::Auto);
        assert_eq!(p("[main]\ngpu_min_mhz = \"fast\"\n"), GpuMinMhz::Auto);
    }

    #[test]
    fn a_bad_uclamp_min_does_not_break_the_rest_of_main() {
        let cfg = Config::parse("[main]\nuclamp_min = \"nope\"\ncard_radius = 42.0\n");
        assert_eq!(cfg.uclamp_min, UclampMin::Auto);
        assert_eq!(cfg.card_radius, Some(42.0));
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
        let home = find("h", PressKind::Short).unwrap();
        assert_eq!(home.action, Action::Home);
        assert!(home.mods.logo, "Home is Super+h, not a bare h");
        assert_eq!(
            find("f", PressKind::Short).unwrap().action,
            Action::ToggleFullscreen
        );
        assert_eq!(find("s", PressKind::Short).unwrap().action, Action::Search);
        assert_eq!(
            find("Tab", PressKind::Short).unwrap().action,
            Action::SwitcherNext
        );
        let prev = find("ISO_Left_Tab", PressKind::Short).unwrap();
        assert_eq!(prev.action, Action::SwitcherPrev);
        assert!(prev.mods.logo && prev.mods.shift);
    }

    #[test]
    fn parses_startup_commands() {
        let cfg = Config::parse("[main]\nstartup = [\"foo\", \"bar --baz\"]\n");
        assert_eq!(cfg.startup, vec!["foo", "bar --baz"]);
        assert!(Config::defaults().startup.is_empty());
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

    #[test]
    fn pictures_dir_prefers_env_then_user_dirs_then_home() {
        let home = |k: &str| (k == "HOME").then(|| "/home/u".to_string());
        let none = |_: &Path| None;
        assert_eq!(
            pictures_dir(home, none),
            Some(PathBuf::from("/home/u/Pictures"))
        );
        let user_dirs = |p: &Path| {
            assert_eq!(p, Path::new("/home/u/.config/user-dirs.dirs"));
            Some("# c\nXDG_DESKTOP_DIR=\"$HOME/D\"\nXDG_PICTURES_DIR=\"$HOME/Bilder\"\n".into())
        };
        assert_eq!(
            pictures_dir(home, user_dirs),
            Some(PathBuf::from("/home/u/Bilder"))
        );
        let env = |k: &str| match k {
            "XDG_PICTURES_DIR" => Some("/srv/pics".to_string()),
            _ => home(k),
        };
        assert_eq!(
            pictures_dir(env, user_dirs),
            Some(PathBuf::from("/srv/pics"))
        );
        assert_eq!(pictures_dir(|_| None, none), None);
    }

    #[test]
    fn resources_default_when_section_absent() {
        assert_eq!(Config::parse("").resources, Resources::default());
    }

    #[test]
    fn resources_section_parses() {
        let c = Config::parse(
            "[resources]\nenable = false\nfg_cpu_weight = 500\nbg_cpu_weight = 5\nbg_memory_high = \"512M\"\nbg_cpu_quota = \"10%\"\nbg_allowed_cpus = \"0-3\"\n",
        );
        assert_eq!(
            c.resources,
            Resources {
                enable: false,
                fg_cpu_weight: 500,
                bg_cpu_weight: 5,
                bg_memory_high: "512M".to_string(),
                bg_cpu_quota: "10%".to_string(),
                bg_allowed_cpus: "0-3".to_string(),
                ..Resources::default()
            }
        );
    }

    #[test]
    fn resources_freeze_and_app_policies() {
        let c = Config::parse(
            "[resources]\nfreeze = false\nfreeze_delay_secs = 5\n[resources.apps]\n\"org.gnome.Maps\" = \"ignore\"\n\"x\" = \"bogus\"\n",
        );
        assert!(!c.resources.freeze);
        assert_eq!(c.resources.freeze_delay_secs, 5);
        assert_eq!(
            c.resources.apps.get("org.gnome.Maps"),
            Some(&AppPolicy::Ignore)
        );
        assert!(!c.resources.apps.contains_key("x"));
    }

    #[test]
    fn resources_bad_values_drop_to_defaults() {
        let c = Config::parse(
            "[resources]\nfg_cpu_weight = 99999\nbg_cpu_weight = 7\nbg_memory_high = \"lots\"\n",
        );
        let d = Resources::default();
        assert_eq!(c.resources.fg_cpu_weight, d.fg_cpu_weight);
        assert_eq!(c.resources.bg_memory_high, d.bg_memory_high);
        assert_eq!(c.resources.bg_cpu_weight, 7);
    }

    #[test]
    fn memory_sizes_systemd_accepts() {
        for ok in [
            "infinity", "Infinity", "512M", "1G", "2Gi", "80%", "1048576",
        ] {
            assert!(is_memory_size(ok), "{ok} should be accepted");
        }
        for bad in ["", "lots", "M", "512MB", "-1", "1.5G"] {
            assert!(!is_memory_size(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn cpu_lists_systemd_accepts() {
        for ok in ["auto", "off", "0", "0-3", "0-1,4", "2,5,7"] {
            assert!(is_cpu_list(ok), "{ok} should be accepted");
        }
        for bad in ["", "0-", "-3", "0,,1", "big", "0-3%"] {
            assert!(!is_cpu_list(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn cpu_quotas_systemd_accepts() {
        for ok in ["infinity", "30%", "5%", "200%"] {
            assert!(is_cpu_quota(ok), "{ok} should be accepted");
        }
        for bad in ["", "30", "%", "30.5%", "-30%", "lots"] {
            assert!(!is_cpu_quota(bad), "{bad} should be rejected");
        }
    }
}
