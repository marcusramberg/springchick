/// Default size of the winit dev window, in logical pixels (a phone-ish aspect;
/// the real output size comes from the backend at runtime, and the host
/// compositor may clamp this anyway). Override with `SPRINGCHICK_WINIT_SIZE=WxH`.
pub const DEV_WINDOW_WIDTH: i32 = 1224;
pub const DEV_WINDOW_HEIGHT: i32 = 2700;

/// Parse `SPRINGCHICK_WINIT_SIZE` (`WxH`) into a size, falling back to the
/// dev-window default. Kept pure for unit testing.
pub fn dev_window_size() -> (i32, i32) {
    parse_dev_window_size(std::env::var("SPRINGCHICK_WINIT_SIZE").as_deref().ok())
}

fn parse_dev_window_size(value: Option<&str>) -> (i32, i32) {
    value
        .and_then(|s| s.split_once(['x', 'X']))
        .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
        .filter(|&(w, h): &(i32, i32)| w > 0 && h > 0)
        .unwrap_or((DEV_WINDOW_WIDTH, DEV_WINDOW_HEIGHT))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum BackendKind {
    Winit,
    Drm,
}

impl BackendKind {
    /// Chosen by `SPRINGCHICK_BACKEND` env var (default: winit on desktop).
    #[allow(dead_code)]
    pub fn from_env() -> Self {
        Self::from_value(std::env::var("SPRINGCHICK_BACKEND").as_deref().ok())
    }

    /// Pure decode helper, kept separate from env access so it can be unit-tested.
    fn from_value(value: Option<&str>) -> Self {
        match value {
            Some("drm") => BackendKind::Drm,
            _ => BackendKind::Winit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_window_size_parses_or_falls_back() {
        assert_eq!(parse_dev_window_size(Some("720x1440")), (720, 1440));
        assert_eq!(parse_dev_window_size(Some("1080X2340")), (1080, 2340));
        // Bad input falls back to the default.
        assert_eq!(
            parse_dev_window_size(Some("garbage")),
            (DEV_WINDOW_WIDTH, DEV_WINDOW_HEIGHT)
        );
        assert_eq!(
            parse_dev_window_size(Some("0x100")),
            (DEV_WINDOW_WIDTH, DEV_WINDOW_HEIGHT)
        );
        assert_eq!(
            parse_dev_window_size(None),
            (DEV_WINDOW_WIDTH, DEV_WINDOW_HEIGHT)
        );
    }

    #[test]
    fn from_value_selects_drm_only_for_drm() {
        assert_eq!(BackendKind::from_value(Some("drm")), BackendKind::Drm);
        assert_eq!(BackendKind::from_value(Some("winit")), BackendKind::Winit);
        assert_eq!(
            BackendKind::from_value(Some("nonsense")),
            BackendKind::Winit
        );
        assert_eq!(BackendKind::from_value(None), BackendKind::Winit);
    }
}
