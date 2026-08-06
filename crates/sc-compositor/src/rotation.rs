//! Landscape rotation for fullscreen apps.
//!
//! Wayland has no protocol for a client to request an orientation, so this is
//! compositor policy: **a toplevel that goes fullscreen gets rotated to
//! landscape**, and goes back to portrait when it leaves fullscreen (or
//! unmaps). Fullscreen on a phone shell means video, games and slideshows —
//! landscape content, near enough always.
//!
//! Only the fullscreen app surface rotates. springchick's own chrome (Home, the
//! bar, the switcher) and layer surfaces stay portrait, which is what phone
//! shells do in practice and keeps the change to one render pass and one input
//! mapping.
//!
//! The rotation is expressed as a [`Transform`] applied on top of the output
//! transform when rendering the app, with input mapped through its inverse.

use smithay::utils::Transform;

/// Which way a fullscreen app is turned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rotation {
    /// Portrait: the app follows the output, no extra transform.
    #[default]
    None,
    /// Landscape: the app is turned a quarter turn clockwise on screen, so its
    /// top edge runs down the screen's right side. A phone held in the left
    /// hand and turned clockwise reads it upright.
    Landscape,
}

impl Rotation {
    /// The extra transform to compose with the output transform when rendering.
    pub fn transform(self) -> Transform {
        match self {
            Rotation::None => Transform::Normal,
            Rotation::Landscape => Transform::_90,
        }
    }

    /// Whether this rotation swaps width and height.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::Landscape)
    }

    /// The app's drawing area, given the output's. Landscape swaps the axes: a
    /// rotated app is as wide as the output is tall.
    pub fn app_size(self, output: (i32, i32)) -> (i32, i32) {
        if self.swaps_axes() {
            (output.1, output.0)
        } else {
            output
        }
    }

    /// Map a physical screen point to the app's rotated space, so a tap lands
    /// where the user sees it. The inverse of what [`Self::transform`] does to
    /// the pixels.
    ///
    /// `output` is the physical output size (portrait). For `Landscape`, screen
    /// x runs *up* the app's y axis and screen y runs along the app's x axis.
    pub fn map_input(self, x: f32, y: f32, output: (i32, i32)) -> (f32, f32) {
        match self {
            Rotation::None => (x, y),
            Rotation::Landscape => (y, output.0 as f32 - x),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: (i32, i32) = (1000, 2000);

    #[test]
    fn portrait_is_identity() {
        assert_eq!(Rotation::None.app_size(OUTPUT), OUTPUT);
        assert_eq!(Rotation::None.map_input(10.0, 20.0, OUTPUT), (10.0, 20.0));
    }

    #[test]
    fn landscape_swaps_the_app_size() {
        assert_eq!(Rotation::Landscape.app_size(OUTPUT), (2000, 1000));
    }

    #[test]
    fn landscape_input_maps_corners_to_corners() {
        let app = Rotation::Landscape.app_size(OUTPUT);
        // Screen top-left is the app's bottom-left: the app's origin sits at the
        // screen's top-right, since the app is turned clockwise.
        assert_eq!(
            Rotation::Landscape.map_input(0.0, 0.0, OUTPUT),
            (0.0, 1000.0)
        );
        // Screen top-right → app origin.
        assert_eq!(
            Rotation::Landscape.map_input(OUTPUT.0 as f32, 0.0, OUTPUT),
            (0.0, 0.0)
        );
        // Screen bottom-right → app's far x edge, y = 0.
        assert_eq!(
            Rotation::Landscape.map_input(OUTPUT.0 as f32, OUTPUT.1 as f32, OUTPUT),
            (app.0 as f32, 0.0)
        );
    }

    #[test]
    fn landscape_input_stays_in_bounds() {
        let app = Rotation::Landscape.app_size(OUTPUT);
        for (x, y) in [(0.0, 0.0), (999.0, 1999.0), (500.0, 1000.0)] {
            let (ax, ay) = Rotation::Landscape.map_input(x, y, OUTPUT);
            assert!((0.0..=app.0 as f32).contains(&ax), "x {ax} out of {app:?}");
            assert!((0.0..=app.1 as f32).contains(&ay), "y {ay} out of {app:?}");
        }
    }
}
