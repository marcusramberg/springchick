//! Volume on-screen display state.
//!
//! Holds the last-known volume plus a show timestamp; the render code turns that
//! into an alpha. Parsing `wpctl` output and the fade curve are pure and tested
//! here; the actual `wpctl` calls live in `keybinds`.

use std::time::{Duration, Instant};

/// How long the OSD stays fully opaque after a volume change, then how long it
/// fades out over.
const HOLD: Duration = Duration::from_millis(1500);
const FADE: Duration = Duration::from_millis(300);

/// Current volume reading plus when it was last shown.
#[derive(Clone, Copy, Debug)]
pub struct Osd {
    /// 0.0..=~1.5 as reported by wpctl (1.0 = 100%).
    pub level: f32,
    pub muted: bool,
    shown_at: Option<Instant>,
}

impl Default for Osd {
    fn default() -> Self {
        Osd {
            level: 0.0,
            muted: false,
            shown_at: None,
        }
    }
}

impl Osd {
    pub fn new() -> Self {
        Osd::default()
    }

    /// Record a fresh reading and (re)start the display timer.
    pub fn show(&mut self, level: f32, muted: bool, now: Instant) {
        self.level = level;
        self.muted = muted;
        self.shown_at = Some(now);
    }

    /// Opacity in `0.0..=1.0`; 0.0 means nothing to draw.
    pub fn alpha(&self, now: Instant) -> f32 {
        let Some(shown_at) = self.shown_at else {
            return 0.0;
        };
        let elapsed = now.saturating_duration_since(shown_at);
        if elapsed <= HOLD {
            1.0
        } else {
            let into_fade = elapsed - HOLD;
            if into_fade >= FADE {
                0.0
            } else {
                1.0 - into_fade.as_secs_f32() / FADE.as_secs_f32()
            }
        }
    }

    /// Whether the OSD still needs to be drawn (and the loop kept awake).
    pub fn is_active(&self, now: Instant) -> bool {
        self.alpha(now) > 0.0
    }
}

/// Parse `wpctl get-volume @DEFAULT_SINK@` output.
///
/// Lines look like `Volume: 0.45` or `Volume: 0.45 [MUTED]`. Returns
/// `(level, muted)`, or `None` if the line does not match.
pub fn parse_wpctl_volume(output: &str) -> Option<(f32, bool)> {
    let line = output
        .lines()
        .find(|l| l.trim_start().starts_with("Volume:"))?;
    let rest = line.trim_start().strip_prefix("Volume:")?.trim();
    let mut parts = rest.split_whitespace();
    let level: f32 = parts.next()?.parse().ok()?;
    let muted = rest.contains("[MUTED]");
    Some((level, muted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_volume_line() {
        assert_eq!(parse_wpctl_volume("Volume: 0.45\n"), Some((0.45, false)));
    }

    #[test]
    fn parses_a_muted_volume_line() {
        assert_eq!(
            parse_wpctl_volume("Volume: 0.45 [MUTED]\n"),
            Some((0.45, true))
        );
    }

    #[test]
    fn parses_amid_other_lines_and_over_100() {
        assert_eq!(
            parse_wpctl_volume("noise\n  Volume: 1.25\nmore"),
            Some((1.25, false))
        );
    }

    #[test]
    fn rejects_junk() {
        assert_eq!(parse_wpctl_volume("nope"), None);
        assert_eq!(parse_wpctl_volume("Volume: abc"), None);
    }

    #[test]
    fn alpha_is_zero_before_first_show() {
        assert_eq!(Osd::new().alpha(Instant::now()), 0.0);
    }

    #[test]
    fn alpha_holds_then_fades_then_gone() {
        let mut osd = Osd::new();
        let t0 = Instant::now();
        osd.show(0.5, false, t0);
        assert_eq!(osd.alpha(t0), 1.0);
        assert_eq!(osd.alpha(t0 + Duration::from_millis(1500)), 1.0);
        // Halfway through the fade.
        let mid = osd.alpha(t0 + Duration::from_millis(1650));
        assert!((0.4..=0.6).contains(&mid), "mid alpha was {mid}");
        assert_eq!(osd.alpha(t0 + Duration::from_millis(1800)), 0.0);
        assert!(!osd.is_active(t0 + Duration::from_millis(1800)));
        assert!(osd.is_active(t0 + Duration::from_millis(1600)));
    }

    #[test]
    fn show_restarts_the_timer() {
        let mut osd = Osd::new();
        let t0 = Instant::now();
        osd.show(0.5, false, t0);
        let late = t0 + Duration::from_millis(1700);
        assert!(osd.alpha(late) < 1.0);
        osd.show(0.6, false, late);
        assert_eq!(osd.alpha(late), 1.0);
        assert_eq!(osd.level, 0.6);
    }
}
