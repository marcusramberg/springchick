//! Landscape rotation for fullscreen apps, following the accelerometer.
//! Wayland has no orientation request, so this is compositor policy.
//!
//! The whole view turns and stays turned between apps (grab, switcher, quick
//! switch) until the shell lands on Home or a non-fullscreen app. Layer
//! surfaces are laid out upright, so they're hidden while turned. Input is
//! mapped through the inverse once, where it enters.
//!
//! [`Settle`] debounces the sensor; [`Fade`] dips to black around the swap
//! because the client keeps drawing its wrongly-shaped buffer until it
//! resizes.

use smithay::utils::Transform;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeviceOrientation {
    /// Also the assumption with no sensor.
    #[default]
    Normal,
    BottomUp,
    /// Turned clockwise.
    LeftUp,
    /// Turned anticlockwise.
    RightUp,
    /// Flat, or no report yet.
    Undefined,
}

impl DeviceOrientation {
    /// Anything unrecognised is `Undefined`, which doesn't rotate.
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

/// Only fullscreen apps rotate. Flat/unknown and upside-down never do.
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

/// Named for the device edge that is up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rotation {
    #[default]
    None,
    /// Device turned clockwise, so the app turns anticlockwise on screen (its
    /// top along the screen's left edge). Turning it the same way as the phone
    /// lands it 180° out.
    LeftUp,
    /// The opposite quarter turn.
    RightUp,
}

impl Rotation {
    /// Composed with the output transform when rendering.
    pub fn transform(self) -> Transform {
        match self {
            Rotation::None => Transform::Normal,
            Rotation::LeftUp => Transform::_270,
            Rotation::RightUp => Transform::_90,
        }
    }

    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::LeftUp | Rotation::RightUp)
    }

    pub fn app_size(self, output: (i32, i32)) -> (i32, i32) {
        if self.swaps_axes() {
            (output.1, output.0)
        } else {
            output
        }
    }

    /// Physical screen point to rotated app space. Each arm is the inverse of
    /// [`Self::transform`]; change them as a pair.
    pub fn map_input(self, x: f32, y: f32, output: (i32, i32)) -> (f32, f32) {
        match self {
            Rotation::None => (x, y),
            // Origin at the screen's bottom-left.
            Rotation::LeftUp => (output.1 as f32 - y, x),
            // Origin at the screen's top-right.
            Rotation::RightUp => (y, output.0 as f32 - x),
        }
    }
}

/// Accelerometer debounce: an orientation must hold for `hold` before it is
/// acted on. The sensor flips right at the diagonal, so wobbles would
/// otherwise reconfigure the app twice.
#[derive(Clone, Copy, Debug)]
pub struct Settle {
    hold: Duration,
    committed: DeviceOrientation,
    pending: Option<(DeviceOrientation, Instant)>,
}

impl Settle {
    /// `hold_ms == 0` disables the debounce.
    pub fn new(hold_ms: u64, initial: DeviceOrientation) -> Self {
        Settle {
            hold: Duration::from_millis(hold_ms),
            committed: initial,
            pending: None,
        }
    }

    /// A pending change keeps its start and is judged by the new hold.
    pub fn set_hold(&mut self, hold_ms: u64) {
        self.hold = Duration::from_millis(hold_ms);
    }

    /// Reporting the committed value cancels a pending change (wobble and
    /// return).
    pub fn observe(&mut self, o: DeviceOrientation, now: Instant) {
        if o == self.committed {
            self.pending = None;
            return;
        }
        match self.pending {
            // Same candidate: keep its clock running.
            Some((p, _)) if p == o => {}
            _ => self.pending = Some((o, now)),
        }
    }

    /// Yields each change exactly once.
    pub fn poll(&mut self, now: Instant) -> Option<DeviceOrientation> {
        let (o, since) = self.pending?;
        if now.duration_since(since) < self.hold {
            return None;
        }
        self.pending = None;
        self.committed = o;
        Some(o)
    }

    /// Keeps the render loop ticking, or the turn never lands.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FadeStep {
    None,
    /// Fully dark: swap the rotation and reconfigure the app now.
    Apply,
    Done,
}

/// Dip-to-black covering a rotation. Fades out, swaps while dark, and fades
/// in once the client draws at the new size (or after [`Fade::MAX_WAIT`]).
#[derive(Clone, Copy, Debug)]
pub struct Fade {
    dur: Duration,
    phase: Phase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Out(Instant),
    Wait(Instant),
    In(Instant),
}

impl Fade {
    /// A client that never redraws must not leave the screen black.
    pub const MAX_WAIT: Duration = Duration::from_millis(400);

    /// `ms == 0` disables: [`Self::begin`] asks for the swap immediately.
    pub fn new(ms: u64) -> Self {
        Fade {
            dur: Duration::from_millis(ms),
            phase: Phase::Idle,
        }
    }

    pub fn set_duration(&mut self, ms: u64) {
        self.dur = Duration::from_millis(ms);
    }

    /// `true` means apply now: fades are off, or an in-flight fade already
    /// covers this change.
    pub fn begin(&mut self, now: Instant) -> bool {
        if self.dur.is_zero() {
            return true;
        }
        match self.phase {
            // The pending swap picks up the latest rotation.
            Phase::Out(_) => false,
            // Already dark: apply now and re-dark, so an intermediate turn never shows.
            Phase::Wait(_) => true,
            Phase::In(_) => {
                self.phase = Phase::Wait(now);
                true
            }
            Phase::Idle => {
                self.phase = Phase::Out(now);
                false
            }
        }
    }

    pub fn tick(&mut self, now: Instant) -> FadeStep {
        match self.phase {
            Phase::Idle => FadeStep::None,
            Phase::Out(start) if now.duration_since(start) >= self.dur => {
                self.phase = Phase::Wait(now);
                FadeStep::Apply
            }
            Phase::Out(_) => FadeStep::None,
            Phase::Wait(since) if now.duration_since(since) >= Self::MAX_WAIT => {
                self.phase = Phase::In(now);
                FadeStep::None
            }
            Phase::Wait(_) => FadeStep::None,
            Phase::In(start) if now.duration_since(start) >= self.dur => {
                self.phase = Phase::Idle;
                FadeStep::Done
            }
            Phase::In(_) => FadeStep::None,
        }
    }

    pub fn content_ready(&mut self, now: Instant) {
        if matches!(self.phase, Phase::Wait(_)) {
            self.phase = Phase::In(now);
        }
    }

    pub fn dim(&self, now: Instant) -> f32 {
        let t = |start: Instant| {
            if self.dur.is_zero() {
                1.0
            } else {
                (now.duration_since(start).as_secs_f32() / self.dur.as_secs_f32()).clamp(0.0, 1.0)
            }
        };
        match self.phase {
            Phase::Idle => 0.0,
            Phase::Out(start) => smoothstep(t(start)),
            Phase::Wait(_) => 1.0,
            Phase::In(start) => smoothstep(1.0 - t(start)),
        }
    }

    /// Also keeps the DRM partial-damage fast path off (the dim is Skia).
    pub fn is_active(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
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
    fn right_up_input_maps_corners_to_corners() {
        let app = Rotation::RightUp.app_size(OUTPUT);
        assert_eq!(Rotation::RightUp.map_input(0.0, 0.0, OUTPUT), (0.0, 1000.0));
        assert_eq!(
            Rotation::RightUp.map_input(OUTPUT.0 as f32, 0.0, OUTPUT),
            (0.0, 0.0)
        );
        assert_eq!(
            Rotation::RightUp.map_input(OUTPUT.0 as f32, OUTPUT.1 as f32, OUTPUT),
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
    fn left_up_is_the_anticlockwise_quarter_turn() {
        // Turning the phone clockwise turns the app anticlockwise; confirmed on
        // device.
        assert_eq!(Rotation::LeftUp.app_size(OUTPUT), (2000, 1000));
        assert_eq!(Rotation::LeftUp.transform(), Transform::_270);
        assert_eq!(
            Rotation::LeftUp.map_input(0.0, OUTPUT.1 as f32, OUTPUT),
            (0.0, 0.0)
        );
        let app = Rotation::LeftUp.app_size(OUTPUT);
        assert_eq!(
            Rotation::LeftUp.map_input(0.0, 0.0, OUTPUT),
            (app.0 as f32, 0.0)
        );
    }

    #[test]
    fn left_up_input_stays_in_bounds_too() {
        let app = Rotation::RightUp.app_size(OUTPUT);
        for (x, y) in [(0.0, 0.0), (999.0, 1999.0), (500.0, 1000.0)] {
            let (ax, ay) = Rotation::RightUp.map_input(x, y, OUTPUT);
            assert!((0.0..=app.0 as f32).contains(&ax), "x {ax} out of {app:?}");
            assert!((0.0..=app.1 as f32).contains(&ay), "y {ay} out of {app:?}");
        }
    }

    #[test]
    fn the_two_landscapes_are_not_the_same_turn() {
        assert_ne!(Rotation::LeftUp.transform(), Rotation::RightUp.transform());
        assert_ne!(
            Rotation::LeftUp.map_input(10.0, 20.0, OUTPUT),
            Rotation::RightUp.map_input(10.0, 20.0, OUTPUT)
        );
    }

    #[test]
    fn only_fullscreen_apps_rotate() {
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
        // A fullscreen portrait app on an upright phone stays portrait.
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
        assert_eq!(
            DeviceOrientation::from_sensor("undefined"),
            DeviceOrientation::Undefined
        );
        assert_eq!(
            DeviceOrientation::from_sensor("sideways-ish"),
            DeviceOrientation::Undefined
        );
    }

    const HOLD: u64 = 400;

    fn settle() -> Settle {
        Settle::new(HOLD, DeviceOrientation::Normal)
    }

    #[test]
    fn an_orientation_commits_only_after_it_has_held() {
        let t0 = Instant::now();
        let mut s = settle();
        s.observe(DeviceOrientation::LeftUp, t0);
        assert!(s.is_pending());
        assert_eq!(s.poll(t0 + Duration::from_millis(399)), None);
        assert_eq!(
            s.poll(t0 + Duration::from_millis(400)),
            Some(DeviceOrientation::LeftUp)
        );
        assert_eq!(s.poll(t0 + Duration::from_millis(500)), None);
        assert!(!s.is_pending());
    }

    #[test]
    fn a_wobble_back_to_where_it_was_never_fires() {
        let t0 = Instant::now();
        let mut s = settle();
        s.observe(DeviceOrientation::LeftUp, t0);
        s.observe(DeviceOrientation::Normal, t0 + Duration::from_millis(100));
        assert!(!s.is_pending());
        assert_eq!(s.poll(t0 + Duration::from_secs(10)), None);
    }

    #[test]
    fn switching_candidate_restarts_the_hold() {
        let t0 = Instant::now();
        let mut s = settle();
        s.observe(DeviceOrientation::LeftUp, t0);
        s.observe(DeviceOrientation::RightUp, t0 + Duration::from_millis(300));
        assert_eq!(s.poll(t0 + Duration::from_millis(400)), None);
        assert_eq!(
            s.poll(t0 + Duration::from_millis(700)),
            Some(DeviceOrientation::RightUp)
        );
    }

    #[test]
    fn repeating_the_same_candidate_does_not_restart_the_hold() {
        // iio-sensor-proxy re-emits on every property change; a repeat must not
        // push the deadline out.
        let t0 = Instant::now();
        let mut s = settle();
        s.observe(DeviceOrientation::LeftUp, t0);
        s.observe(DeviceOrientation::LeftUp, t0 + Duration::from_millis(300));
        assert_eq!(
            s.poll(t0 + Duration::from_millis(400)),
            Some(DeviceOrientation::LeftUp)
        );
    }

    #[test]
    fn a_zero_hold_commits_on_the_next_poll() {
        let t0 = Instant::now();
        let mut s = Settle::new(0, DeviceOrientation::Normal);
        s.observe(DeviceOrientation::LeftUp, t0);
        assert_eq!(s.poll(t0), Some(DeviceOrientation::LeftUp));
    }

    const FADE: u64 = 120;

    #[test]
    fn the_swap_happens_while_the_screen_is_black() {
        let t0 = Instant::now();
        let mut f = Fade::new(FADE);
        assert!(!f.begin(t0), "a fade defers the swap");
        assert!(f.is_active());
        assert!(f.dim(t0) < 0.01);
        assert!(f.dim(t0 + Duration::from_millis(60)) > 0.4);
        assert_eq!(f.tick(t0 + Duration::from_millis(60)), FadeStep::None);

        let swap = t0 + Duration::from_millis(120);
        assert_eq!(f.tick(swap), FadeStep::Apply);
        assert_eq!(f.dim(swap), 1.0);
        assert_eq!(f.tick(swap + Duration::from_millis(50)), FadeStep::None);
        assert_eq!(f.dim(swap + Duration::from_millis(50)), 1.0);

        let drew = swap + Duration::from_millis(80);
        f.content_ready(drew);
        assert!(f.dim(drew + Duration::from_millis(60)) < 0.6);
        assert_eq!(f.tick(drew + Duration::from_millis(120)), FadeStep::Done);
        assert!(!f.is_active());
        assert_eq!(f.dim(drew + Duration::from_millis(120)), 0.0);
    }

    #[test]
    fn a_client_that_never_redraws_does_not_hold_a_black_screen() {
        let t0 = Instant::now();
        let mut f = Fade::new(FADE);
        f.begin(t0);
        let swap = t0 + Duration::from_millis(120);
        assert_eq!(f.tick(swap), FadeStep::Apply);
        let give_up = swap + Fade::MAX_WAIT;
        assert_eq!(f.tick(give_up), FadeStep::None);
        assert_eq!(f.tick(give_up + Duration::from_millis(120)), FadeStep::Done);
        assert!(!f.is_active());
    }

    #[test]
    fn content_ready_outside_the_dark_is_ignored() {
        let t0 = Instant::now();
        let mut f = Fade::new(FADE);
        f.content_ready(t0);
        assert!(!f.is_active());
        // Mid fade-out a commit is still the old size and must not end the fade.
        f.begin(t0);
        f.content_ready(t0 + Duration::from_millis(40));
        assert_eq!(f.tick(t0 + Duration::from_millis(120)), FadeStep::Apply);
    }

    #[test]
    fn a_second_turn_mid_fade_is_covered_by_the_same_dip() {
        let t0 = Instant::now();
        let mut f = Fade::new(FADE);
        assert!(!f.begin(t0));
        assert!(!f.begin(t0 + Duration::from_millis(40)));
        let swap = t0 + Duration::from_millis(120);
        assert_eq!(f.tick(swap), FadeStep::Apply);

        f.content_ready(swap);
        assert!(f.begin(swap + Duration::from_millis(40)));
        assert_eq!(f.dim(swap + Duration::from_millis(40)), 1.0);
        assert!(f.is_active());
    }

    #[test]
    fn a_zero_duration_fade_is_the_old_instant_behaviour() {
        let t0 = Instant::now();
        let mut f = Fade::new(0);
        assert!(f.begin(t0), "swap immediately");
        assert!(!f.is_active());
        assert_eq!(f.dim(t0), 0.0);
    }
}
