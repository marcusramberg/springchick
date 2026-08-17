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
//!
//! Two things stand between the sensor and that transform, both here and both
//! pure: [`Settle`] debounces the reading (the sensor flips the moment the phone
//! crosses the diagonal, so a wobble used to turn the app and turn it straight
//! back), and [`Fade`] dips the screen to black around the swap, because the
//! client keeps drawing its old, now wrongly-shaped buffer until it gets round
//! to the resize and that stretched frame is the ugliest part of a turn.

use smithay::utils::Transform;
use std::time::{Duration, Instant};

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
    /// The device's left edge is up (it was turned clockwise), so the app is
    /// turned a quarter turn *anticlockwise* on screen — its top edge runs up
    /// the screen's left side.
    ///
    /// Turning the phone clockwise swings the panel's +x axis to point at the
    /// ground, so world-up is panel −x: the image's top edge has to lie along
    /// the screen's left edge to read upright. Turning the app the same way as
    /// the phone would land it 180° out, which is exactly how this looked on
    /// device before the two transforms were swapped.
    LeftUp,
    /// The device's right edge is up — the opposite quarter turn, so the app's
    /// top edge runs down the screen's right side.
    RightUp,
}

impl Rotation {
    /// The extra transform to compose with the output transform when rendering.
    pub fn transform(self) -> Transform {
        match self {
            Rotation::None => Transform::Normal,
            Rotation::LeftUp => Transform::_270,
            Rotation::RightUp => Transform::_90,
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
    /// `output` is the physical output size (portrait). Each arm is the inverse
    /// of the matching [`Self::transform`], so the two must always be changed as
    /// a pair — a mismatch leaves taps landing somewhere other than what the
    /// user is looking at.
    pub fn map_input(self, x: f32, y: f32, output: (i32, i32)) -> (f32, f32) {
        match self {
            Rotation::None => (x, y),
            // Turned anticlockwise: the app's origin sits at the screen's
            // bottom-left.
            Rotation::LeftUp => (output.1 as f32 - y, x),
            // Turned clockwise: the app's origin sits at the screen's top-right.
            Rotation::RightUp => (y, output.0 as f32 - x),
        }
    }
}

/// Debounce for accelerometer reports.
///
/// The sensor flips the moment the phone passes the diagonal, so a wobble on the
/// way to putting it down — or a hand that overshoots and comes back — used to
/// re-configure the app twice in a few frames. Nothing is acted on until one
/// orientation has held still for `hold`.
///
/// Clock-free: the caller passes `now`, so tests drive it with synthetic time.
/// [`Self::observe`] takes what the sensor said; [`Self::poll`] is called once a
/// frame and yields an orientation only once it has settled.
#[derive(Clone, Copy, Debug)]
pub struct Settle {
    hold: Duration,
    /// The orientation the compositor is acting on right now.
    committed: DeviceOrientation,
    /// A different orientation the sensor is reporting, and since when.
    pending: Option<(DeviceOrientation, Instant)>,
}

impl Settle {
    /// `hold_ms == 0` disables the debounce: the next `poll` commits.
    pub fn new(hold_ms: u64, initial: DeviceOrientation) -> Self {
        Settle {
            hold: Duration::from_millis(hold_ms),
            committed: initial,
            pending: None,
        }
    }

    /// Change the hold time (config reload). Anything already pending keeps its
    /// start instant and is judged by the new hold.
    pub fn set_hold(&mut self, hold_ms: u64) {
        self.hold = Duration::from_millis(hold_ms);
    }

    /// The sensor reported `o`. Reporting what is already committed cancels a
    /// pending change — that is the wobble-and-return case, and it must not
    /// leave a stale candidate waiting to fire.
    pub fn observe(&mut self, o: DeviceOrientation, now: Instant) {
        if o == self.committed {
            self.pending = None;
            return;
        }
        match self.pending {
            // Same candidate as before: keep its clock running.
            Some((p, _)) if p == o => {}
            _ => self.pending = Some((o, now)),
        }
    }

    /// Commit a pending orientation once it has held for `hold`. Returns the new
    /// orientation exactly once per change; `None` otherwise.
    pub fn poll(&mut self, now: Instant) -> Option<DeviceOrientation> {
        let (o, since) = self.pending?;
        if now.duration_since(since) < self.hold {
            return None;
        }
        self.pending = None;
        self.committed = o;
        Some(o)
    }

    /// A change is waiting out its hold, so the render loop must keep ticking —
    /// otherwise nothing calls [`Self::poll`] and the turn never lands.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// What [`Fade::tick`] wants the caller to do this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FadeStep {
    /// Nothing to do.
    None,
    /// The screen is fully dark: swap the rotation and re-configure the app now.
    /// The client's resize — which it does in its own time, at whatever size it
    /// last drew — happens behind the black.
    Apply,
    /// The fade-in finished; the transition is over.
    Done,
}

/// The dip-to-black that covers an orientation change.
///
/// A rotation is not a thing that can be animated honestly: the client owns its
/// buffer, so between the configure and its next commit the old (now wrongly
/// shaped) buffer is all there is to draw, stretched across a screen whose axes
/// just swapped. Rather than show that, the screen fades out, the swap happens
/// while it is dark, and it fades back in once the client has drawn at the new
/// size (or [`Fade::MAX_WAIT`] passes — a client that never redraws must not
/// leave the screen black).
///
/// Clock-free like [`Settle`]: `now` comes from the caller.
#[derive(Clone, Copy, Debug)]
pub struct Fade {
    dur: Duration,
    phase: Phase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Fading to black, since.
    Out(Instant),
    /// Black, waiting for the client to draw at its new size, since.
    Wait(Instant),
    /// Fading back in, since.
    In(Instant),
}

impl Fade {
    /// How long the screen may stay black waiting for a client that is slow to
    /// redraw (or has decided not to). Long enough for a toolkit round trip,
    /// short enough that a stuck client is not mistaken for a dead screen.
    pub const MAX_WAIT: Duration = Duration::from_millis(400);

    /// `ms == 0` disables the transition: [`Self::begin`] asks for the swap
    /// immediately and nothing is ever dimmed.
    pub fn new(ms: u64) -> Self {
        Fade {
            dur: Duration::from_millis(ms),
            phase: Phase::Idle,
        }
    }

    pub fn set_duration(&mut self, ms: u64) {
        self.dur = Duration::from_millis(ms);
    }

    /// The rotation is about to change. Returns `true` when the caller should
    /// apply it right now — either fades are off, or one is already in flight
    /// past the point of no return, in which case the in-flight fade covers this
    /// change too and only the newest rotation is ever drawn.
    pub fn begin(&mut self, now: Instant) -> bool {
        if self.dur.is_zero() {
            return true;
        }
        match self.phase {
            // Already on the way out: the pending swap will pick up whatever the
            // rotation is by the time it lands.
            Phase::Out(_) => false,
            // Dark already (waiting, or fading back in after an earlier turn):
            // apply immediately and re-dark, so a second turn during the fade-in
            // never shows the intermediate one.
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

    /// Advance the fade. Call once a frame while [`Self::is_active`].
    pub fn tick(&mut self, now: Instant) -> FadeStep {
        match self.phase {
            Phase::Idle => FadeStep::None,
            Phase::Out(start) if now.duration_since(start) >= self.dur => {
                self.phase = Phase::Wait(now);
                FadeStep::Apply
            }
            Phase::Out(_) => FadeStep::None,
            // The client is taking too long (or will never redraw): come back
            // anyway rather than hold a black screen.
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

    /// The client has committed a buffer at the size the new rotation implies:
    /// there is something correct to show, so start coming back.
    pub fn content_ready(&mut self, now: Instant) {
        if matches!(self.phase, Phase::Wait(_)) {
            self.phase = Phase::In(now);
        }
    }

    /// How black the screen is, `0.0` (nothing) to `1.0` (fully dark). Smooth at
    /// both ends so the dip reads as a fade rather than a cut.
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

    /// Whether a transition is in flight, so the render loop keeps drawing and
    /// the DRM partial-damage fast path stays off (the dim is a Skia overlay).
    pub fn is_active(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }
}

/// Hermite ease, so the fade has no hard corner at either end.
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
        // Turned clockwise, so the app's origin sits at the screen's top-right.
        assert_eq!(Rotation::RightUp.map_input(0.0, 0.0, OUTPUT), (0.0, 1000.0));
        // Screen top-right → app origin.
        assert_eq!(
            Rotation::RightUp.map_input(OUTPUT.0 as f32, 0.0, OUTPUT),
            (0.0, 0.0)
        );
        // Screen bottom-right → app's far x edge, y = 0.
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
        // Turning the phone CLOCKWISE (left edge up) turns the app
        // ANTICLOCKWISE, or it reads upside down — confirmed on device.
        assert_eq!(Rotation::LeftUp.app_size(OUTPUT), (2000, 1000));
        assert_eq!(Rotation::LeftUp.transform(), Transform::_270);
        // The app's origin sits at the screen's bottom-left — the mirror of
        // RightUp, whose origin is the top-right.
        assert_eq!(
            Rotation::LeftUp.map_input(0.0, OUTPUT.1 as f32, OUTPUT),
            (0.0, 0.0)
        );
        // Screen top-left → the app's far x edge at y = 0.
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

    // --- the debounce ---

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
        // Reported exactly once.
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
        // 400ms after the *first* report, but only 100ms into the second.
        assert_eq!(s.poll(t0 + Duration::from_millis(400)), None);
        assert_eq!(
            s.poll(t0 + Duration::from_millis(700)),
            Some(DeviceOrientation::RightUp)
        );
    }

    #[test]
    fn repeating_the_same_candidate_does_not_restart_the_hold() {
        // iio-sensor-proxy re-emits on every property change; a repeat must not
        // push the deadline out forever.
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

    // --- the fade ---

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

        // Fade-out over: swap now, screen fully dark.
        let swap = t0 + Duration::from_millis(120);
        assert_eq!(f.tick(swap), FadeStep::Apply);
        assert_eq!(f.dim(swap), 1.0);
        assert_eq!(f.tick(swap + Duration::from_millis(50)), FadeStep::None);
        assert_eq!(f.dim(swap + Duration::from_millis(50)), 1.0);

        // The client draws at its new size; fade back in.
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
        // No `content_ready` ever arrives.
        let give_up = swap + Fade::MAX_WAIT;
        assert_eq!(f.tick(give_up), FadeStep::None);
        assert_eq!(f.tick(give_up + Duration::from_millis(120)), FadeStep::Done);
        assert!(!f.is_active());
    }

    #[test]
    fn content_ready_outside_the_dark_is_ignored() {
        let t0 = Instant::now();
        let mut f = Fade::new(FADE);
        // Nothing in flight.
        f.content_ready(t0);
        assert!(!f.is_active());
        // Mid fade-out: the swap has not happened yet, so a commit is the *old*
        // size and must not cut the fade short.
        f.begin(t0);
        f.content_ready(t0 + Duration::from_millis(40));
        assert_eq!(f.tick(t0 + Duration::from_millis(120)), FadeStep::Apply);
    }

    #[test]
    fn a_second_turn_mid_fade_is_covered_by_the_same_dip() {
        let t0 = Instant::now();
        let mut f = Fade::new(FADE);
        assert!(!f.begin(t0));
        // Turned again while still fading out: no second swap yet, the pending
        // one picks up the newer rotation.
        assert!(!f.begin(t0 + Duration::from_millis(40)));
        let swap = t0 + Duration::from_millis(120);
        assert_eq!(f.tick(swap), FadeStep::Apply);

        // Turned again while coming back: apply at once and go dark again, so
        // the intermediate orientation is never shown.
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
