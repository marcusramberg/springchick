//! Landscape rotation for fullscreen apps.
//!
//! Wayland has no protocol for a client to request an orientation, so this is
//! compositor policy: **a fullscreen toplevel is turned to match how the device
//! is physically held**, and goes back to portrait when it leaves fullscreen
//! (or unmaps, or the device is held upright again).
//!
//! This used to key off fullscreen alone — anything fullscreen was assumed to
//! be video wanting landscape. That inference was wrong for any app that is
//! fullscreen *and* portrait (the pull-down search was configured at the
//! swapped size and drawn with a rotated ghost of itself), so the orientation is
//! now an input rather than a guess: [`DeviceOrientation`] comes from the
//! accelerometer and [`desired_rotation`] decides.
//!
//! Only the fullscreen app surface rotates. springchick's own chrome (Home, the
//! bar, the switcher) and layer surfaces stay portrait, which is what phone
//! shells do in practice and keeps the change to one render pass and one input
//! mapping.
//!
//! The rotation is expressed as a [`Transform`] applied on top of the output
//! transform when rendering the app, with input mapped through its inverse.

use smithay::utils::Transform;

/// How the device is physically held, from iio-sensor-proxy's
/// `AccelerometerOrientation` property.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeviceOrientation {
    /// Upright portrait. Also the assumption when there is no sensor at all.
    #[default]
    Normal,
    /// Portrait, upside down.
    BottomUp,
    /// The device's left edge is up — it was turned clockwise.
    LeftUp,
    /// The device's right edge is up — it was turned anticlockwise.
    RightUp,
    /// Lying flat, or the sensor has not reported yet.
    Undefined,
}

impl DeviceOrientation {
    /// Parse iio-sensor-proxy's `AccelerometerOrientation` string. Anything
    /// unrecognised (including its own `"undefined"`) reads as
    /// [`DeviceOrientation::Undefined`], which does not rotate.
    pub fn from_sensor(s: &str) -> Self {
        match s {
            "normal" => DeviceOrientation::Normal,
            "bottom-up" => DeviceOrientation::BottomUp,
            "left-up" => DeviceOrientation::LeftUp,
            "right-up" => DeviceOrientation::RightUp,
            _ => DeviceOrientation::Undefined,
        }
    }
}

/// How a fullscreen app should be turned, given how the device is held.
///
/// Only fullscreen apps rotate, so a portrait app that happens to be fullscreen
/// on an upright phone stays portrait. Flat/unknown does not rotate either: with
/// no reliable reading the safe answer is to leave things as they are.
///
/// Upside-down portrait deliberately does *not* rotate — a phone shell that
/// flips 180° because the user leaned back is worse than one that never does.
pub fn desired_rotation(device: DeviceOrientation, fullscreen: bool) -> Rotation {
    if !fullscreen {
        return Rotation::None;
    }
    match device {
        DeviceOrientation::LeftUp => Rotation::LeftUp,
        DeviceOrientation::RightUp => Rotation::RightUp,
        DeviceOrientation::Normal | DeviceOrientation::BottomUp | DeviceOrientation::Undefined => {
            Rotation::None
        }
    }
}

/// Which way a fullscreen app is turned. Named for the device edge that is up,
/// since that is what decides it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rotation {
    /// Portrait: the app follows the output, no extra transform.
    #[default]
    None,
    /// The device's left edge is up, so the app is turned a quarter turn
    /// clockwise on screen: its top edge runs down the screen's right side.
    LeftUp,
    /// The device's right edge is up — the opposite quarter turn, so the app's
    /// top edge runs up the screen's left side.
    RightUp,
}

impl Rotation {
    /// The extra transform to compose with the output transform when rendering.
    pub fn transform(self) -> Transform {
        match self {
            Rotation::None => Transform::Normal,
            Rotation::LeftUp => Transform::_90,
            Rotation::RightUp => Transform::_270,
        }
    }

    /// Whether this rotation swaps width and height.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::LeftUp | Rotation::RightUp)
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
    /// `output` is the physical output size (portrait). For `LeftUp`, screen x
    /// runs *up* the app's y axis and screen y runs along the app's x axis;
    /// `RightUp` is the same idea mirrored through both axes.
    pub fn map_input(self, x: f32, y: f32, output: (i32, i32)) -> (f32, f32) {
        match self {
            Rotation::None => (x, y),
            // App origin sits at the screen's top-right.
            Rotation::LeftUp => (y, output.0 as f32 - x),
            // App origin sits at the screen's bottom-left.
            Rotation::RightUp => (output.1 as f32 - y, x),
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
        assert_eq!(Rotation::LeftUp.app_size(OUTPUT), (2000, 1000));
    }

    #[test]
    fn landscape_input_maps_corners_to_corners() {
        let app = Rotation::LeftUp.app_size(OUTPUT);
        // Screen top-left is the app's bottom-left: the app's origin sits at the
        // screen's top-right, since the app is turned clockwise.
        assert_eq!(Rotation::LeftUp.map_input(0.0, 0.0, OUTPUT), (0.0, 1000.0));
        // Screen top-right → app origin.
        assert_eq!(
            Rotation::LeftUp.map_input(OUTPUT.0 as f32, 0.0, OUTPUT),
            (0.0, 0.0)
        );
        // Screen bottom-right → app's far x edge, y = 0.
        assert_eq!(
            Rotation::LeftUp.map_input(OUTPUT.0 as f32, OUTPUT.1 as f32, OUTPUT),
            (app.0 as f32, 0.0)
        );
    }

    #[test]
    fn landscape_input_stays_in_bounds() {
        let app = Rotation::LeftUp.app_size(OUTPUT);
        for (x, y) in [(0.0, 0.0), (999.0, 1999.0), (500.0, 1000.0)] {
            let (ax, ay) = Rotation::LeftUp.map_input(x, y, OUTPUT);
            assert!((0.0..=app.0 as f32).contains(&ax), "x {ax} out of {app:?}");
            assert!((0.0..=app.1 as f32).contains(&ay), "y {ay} out of {app:?}");
        }
    }

    #[test]
    fn right_up_is_the_opposite_quarter_turn() {
        assert_eq!(Rotation::RightUp.app_size(OUTPUT), (2000, 1000));
        assert_eq!(Rotation::RightUp.transform(), Transform::_270);
        // The app's origin sits at the screen's bottom-left — the mirror of
        // LeftUp, whose origin is the top-right.
        assert_eq!(
            Rotation::RightUp.map_input(0.0, OUTPUT.1 as f32, OUTPUT),
            (0.0, 0.0)
        );
        // Screen top-left → the app's far x edge at y = 0.
        let app = Rotation::RightUp.app_size(OUTPUT);
        assert_eq!(
            Rotation::RightUp.map_input(0.0, 0.0, OUTPUT),
            (app.0 as f32, 0.0)
        );
    }

    #[test]
    fn right_up_input_stays_in_bounds() {
        let app = Rotation::RightUp.app_size(OUTPUT);
        for (x, y) in [(0.0, 0.0), (999.0, 1999.0), (500.0, 1000.0)] {
            let (ax, ay) = Rotation::RightUp.map_input(x, y, OUTPUT);
            assert!((0.0..=app.0 as f32).contains(&ax), "x {ax} out of {app:?}");
            assert!((0.0..=app.1 as f32).contains(&ay), "y {ay} out of {app:?}");
        }
    }

    #[test]
    fn the_two_landscapes_are_not_the_same_turn() {
        // Guards against both directions being wired to one transform, which
        // would show one of them upside down.
        assert_ne!(Rotation::LeftUp.transform(), Rotation::RightUp.transform());
        assert_ne!(
            Rotation::LeftUp.map_input(10.0, 20.0, OUTPUT),
            Rotation::RightUp.map_input(10.0, 20.0, OUTPUT)
        );
    }

    // --- the policy itself ---

    #[test]
    fn only_fullscreen_apps_rotate() {
        // A phone on its side with no fullscreen app: chrome stays portrait.
        assert_eq!(
            desired_rotation(DeviceOrientation::LeftUp, false),
            Rotation::None
        );
        assert_eq!(
            desired_rotation(DeviceOrientation::RightUp, false),
            Rotation::None
        );
    }

    #[test]
    fn a_fullscreen_app_follows_the_device() {
        assert_eq!(
            desired_rotation(DeviceOrientation::LeftUp, true),
            Rotation::LeftUp
        );
        assert_eq!(
            desired_rotation(DeviceOrientation::RightUp, true),
            Rotation::RightUp
        );
    }

    #[test]
    fn an_upright_phone_never_rotates_even_fullscreen() {
        // The regression this policy exists for: a fullscreen *portrait* app
        // (the pull-down search) on an upright phone must stay portrait.
        assert_eq!(
            desired_rotation(DeviceOrientation::Normal, true),
            Rotation::None
        );
    }

    #[test]
    fn flat_or_upside_down_does_not_rotate() {
        assert_eq!(
            desired_rotation(DeviceOrientation::Undefined, true),
            Rotation::None
        );
        assert_eq!(
            desired_rotation(DeviceOrientation::BottomUp, true),
            Rotation::None
        );
    }

    #[test]
    fn sensor_strings_parse() {
        assert_eq!(
            DeviceOrientation::from_sensor("normal"),
            DeviceOrientation::Normal
        );
        assert_eq!(
            DeviceOrientation::from_sensor("left-up"),
            DeviceOrientation::LeftUp
        );
        assert_eq!(
            DeviceOrientation::from_sensor("right-up"),
            DeviceOrientation::RightUp
        );
        assert_eq!(
            DeviceOrientation::from_sensor("bottom-up"),
            DeviceOrientation::BottomUp
        );
        // iio-sensor-proxy's own "undefined", and anything we don't know.
        assert_eq!(
            DeviceOrientation::from_sensor("undefined"),
            DeviceOrientation::Undefined
        );
        assert_eq!(
            DeviceOrientation::from_sensor("sideways-ish"),
            DeviceOrientation::Undefined
        );
    }
}
