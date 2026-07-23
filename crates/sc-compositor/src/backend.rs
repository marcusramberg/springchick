/// FP5 logical output geometry. The winit dev window is forced to match so layout
/// and animation are pixel-identical to the device.
pub const FP5_WIDTH: i32 = 1224;
pub const FP5_HEIGHT: i32 = 2700;

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
