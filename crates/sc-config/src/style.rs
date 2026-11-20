//! The shell's fixed look: what every program draws alike and no config sets.

/// `foreground` at an alpha, by job.
pub mod wash {
    /// Separators and outlines.
    pub const HAIRLINE: f32 = 0.16;
    /// Resting glass: cards, tiles, fields, an off toggle.
    pub const FILL: f32 = 0.18;
    /// Under a finger.
    pub const PRESSED: f32 = 0.25;
    /// A glass key or disc under a finger, over `FILL`.
    pub const HELD: f32 = 0.45;
    /// The home pill, on Home and on the lock screen.
    pub const PILL: f32 = 0.7;
    /// `background` over the wallpaper, under the lock screen.
    pub const SCRIM: f32 = 0.3;
    /// `background` as the hard shadow of text straight on the wallpaper.
    pub const SHADOW: f32 = 0.43;
}

/// Corner radii, logical px.
pub mod radius {
    /// Keyboard keys.
    pub const KEY: f32 = 6.0;
    /// Menus, popups, small icons and highlights.
    pub const PANEL: f32 = 10.0;
    /// Notification cards.
    pub const CARD: f32 = 22.0;
    /// Folders, the dock and control modules: of their own size.
    pub const CONTAINER: f32 = 0.24;
}
