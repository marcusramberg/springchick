//! Scene state: per-frame snapshot for rendering.
//!
//! Computes window transforms from UiState for the renderer to apply.

use crate::switcher;
use crate::ui_state::{ToplevelId, UiState};
use sc_input::Tracker;

/// Window transform applied to the composited app texture.
#[derive(Clone, Copy, Debug)]
pub struct WindowTransform {
    /// Scale factor (1.0 = fullscreen, 0.0 = invisible).
    pub scale: f32,
    /// Center position in logical pixels.
    pub center_x: f32,
    pub center_y: f32,
    /// Corner radius in logical pixels (0 = sharp/fullscreen).
    pub corner_radius: f32,
}

impl WindowTransform {
    /// Identity: fullscreen, no rounding.
    pub fn fullscreen(width: f32, height: f32) -> Self {
        Self {
            scale: 1.0,
            center_x: width / 2.0,
            center_y: height / 2.0,
            corner_radius: 0.0,
        }
    }

    /// Interpolate between start scale (at origin.center) and fullscreen.
    /// progress: 0 = origin, 1 = fullscreen.
    pub fn from_zoom_progress(
        progress: f32,
        origin: crate::ui_state::ZoomOrigin,
        width: f32,
        height: f32,
        card_radius: f32,
    ) -> Self {
        let screen_cx = width / 2.0;
        let screen_cy = height / 2.0;
        let p = progress.clamp(0.0, 1.0);

        let scale = origin.scale + p * (1.0 - origin.scale);
        let cx = origin.center.0 + (screen_cx - origin.center.0) * p;
        let cy = origin.center.1 + (screen_cy - origin.center.1) * p;
        let corner_radius = card_radius * 0.6 * (1.0 - p);

        Self {
            scale,
            center_x: cx,
            center_y: cy,
            corner_radius,
        }
    }

    /// Slide up full-size from below the bottom edge. `progress`: 0 = fully
    /// below the screen, 1 = centered/fullscreen. Held just under fullscreen
    /// scale so the shared renderer keeps it on the card path (drawn over home,
    /// honouring the offset) until the state settles to `App`; the final 2% pop
    /// to true fullscreen is imperceptible.
    pub fn slide_up(progress: f32, width: f32, height: f32, card_radius: f32) -> Self {
        let p = progress.clamp(0.0, 1.0);
        let ease = sc_anim::ease_out_cubic(p);
        Self {
            scale: 0.98,
            center_x: width / 2.0,
            center_y: height * 0.5 + (1.0 - ease) * height,
            corner_radius: card_radius * (1.0 - ease),
        }
    }

    /// Slide in full-size from the left edge, as Home is dragged off to the
    /// right: the stack sits to the *left* of Home, so pushing Home rightwards
    /// pulls its top card in behind it. `progress`: 0 = fully off the left edge,
    /// 1 = centered. Held just under fullscreen scale for the same reason as
    /// [`Self::slide_up`]: it keeps the card path (drawn over home, rounded)
    /// until the state settles to `App`.
    ///
    /// Paired with [`home_slide_out`], which moves Home the other way by the
    /// same eased amount so the two travel as one sheet.
    pub fn slide_in_from_left(progress: f32, width: f32, height: f32, card_radius: f32) -> Self {
        let ease = sc_anim::ease_out_cubic(progress);
        Self {
            scale: 0.98,
            center_x: width * 0.5 - (1.0 - ease) * width,
            center_y: height / 2.0,
            corner_radius: card_radius * (1.0 - ease),
        }
    }

    /// Freeform grab: window follows finger, pivoting from the bottom.
    /// The finger stays at the bottom edge of the scaled window.
    /// Scale shrinks aggressively so the top moves down toward the finger.
    pub fn from_tracker(tracker: &Tracker, width: f32, height: f32, card_radius: f32) -> Self {
        let up = tracker.up_progress().clamp(0.0, 1.0);

        // Aggressive scaling: 1.0 → 0.20 over full vertical travel, so the card
        // shrinks fast enough that its top edge doesn't reach the screen top
        // early in the drag. Ease-in so it accelerates as you drag further.
        let scale = 1.0 - up.powf(0.6) * 0.8;

        // Finger position in screen coords.
        let finger_x = tracker.current.x * width;
        let finger_y = tracker.current.y * height;

        // Finger is at the bottom edge of the card.
        let card_h = height * scale;
        let center_x = finger_x;
        let center_y = finger_y - card_h / 2.0;

        // Corner radius: a base so the card reads as rounded the instant it
        // lifts off the bottom (while held near full scale), growing as it
        // shrinks. Approaches the switcher deck's `CORNER` at full travel so the
        // grab→switcher hand-off is seamless.
        let corner_radius = card_radius * 0.6 + (1.0 - scale) * card_radius * 0.7;

        Self {
            scale,
            center_x,
            center_y,
            corner_radius,
        }
    }
}

/// How far Home has been dragged off to the right, in pixels, for a slide onto
/// the top card of the stack. Rides the same eased curve as
/// [`WindowTransform::slide_in_from_left`], one screen width in the opposite
/// direction, so Home and the incoming card move as one sheet.
fn home_slide_out(progress: f32, width: f32) -> f32 {
    sc_anim::ease_out_cubic(progress) * width
}

/// Full scene state for one frame.
#[derive(Clone, Debug)]
pub struct Scene {
    /// Transform for the foreground app window (None = no app visible).
    pub window: Option<(ToplevelId, WindowTransform)>,
    /// Whether to draw the home screen behind the window.
    pub show_home: bool,
    /// Home screen page (for rendering).
    pub home_page: usize,
    /// Switcher deck cards (empty for non-switcher states), sorted ascending z.
    pub cards: Vec<switcher::CardRect>,
    /// Vertical offset applied to the whole home screen, in pixels, positive =
    /// drawn higher. Non-zero only while the Home bounce spring is ringing.
    pub home_lift: f32,
    /// Horizontal offset applied to the whole home screen, in pixels, positive =
    /// drawn further right. Non-zero only while Home is being dragged off to the
    /// right by a slide onto the top card of the stack.
    pub home_shift: f32,
}

impl Scene {
    /// Whether the app window (if any) fully covers the screen this frame.
    /// When true, home must not be drawn — it would paint over the window's
    /// content, since the window itself is drawn opaque and undamaged behind
    /// it. Mirrors the `is_fullscreen` threshold used to pick the app's draw
    /// pass in the renderer.
    pub fn window_covers_screen(&self) -> bool {
        self.window.is_none_or(|(_, t)| t.scale >= 0.99)
    }
}

/// Compute the scene from the current UiState. `usable_origin` is the physical
/// top-left of the usable area (output minus exclusive-zone reservations, e.g. a
/// top panel); full-height cards are shifted by it so they sit where the real
/// fullscreen app sits instead of riding up over the reserved zone.
pub fn compute_scene(
    state: &UiState,
    output_size: (i32, i32),
    usable_origin: (f32, f32),
    card_radius: f32,
) -> Scene {
    let (w, h) = (output_size.0 as f32, output_size.1 as f32);
    match state {
        UiState::Home { page, bounce, .. } => Scene {
            window: None,
            show_home: true,
            // The bounce spring is in fractions of screen height.
            home_lift: bounce.value * h,
            home_shift: 0.0,
            home_page: *page,
            cards: Vec::new(),
        },
        UiState::App { toplevel, .. } => Scene {
            window: Some((*toplevel, WindowTransform::fullscreen(w, h))),
            show_home: false,
            home_lift: 0.0,
            home_shift: 0.0,
            home_page: 0,
            cards: Vec::new(),
        },
        UiState::AppOpening {
            toplevel,
            progress,
            origin,
            open_mode,
            ..
        } => {
            let p = progress.value.clamp(0.0, 1.0);
            let transform = match open_mode {
                crate::ui_state::OpenMode::SlideUp => {
                    WindowTransform::slide_up(p, w, h, card_radius)
                }
                crate::ui_state::OpenMode::SlideFromLeft => {
                    WindowTransform::slide_in_from_left(p, w, h, card_radius)
                }
                crate::ui_state::OpenMode::Zoom => {
                    WindowTransform::from_zoom_progress(p, *origin, w, h, card_radius)
                }
            };
            // Only the sideways slide takes Home with it; the zoom and the
            // pull-down search both play over a stationary Home.
            let home_shift = match open_mode {
                crate::ui_state::OpenMode::SlideFromLeft => home_slide_out(p, w),
                _ => 0.0,
            };
            Scene {
                window: Some((*toplevel, transform)),
                show_home: true,
                home_lift: 0.0,
                home_shift,
                home_page: 0,
                cards: Vec::new(),
            }
        }
        UiState::AppClosing {
            toplevel,
            progress,
            origin,
            ..
        } => Scene {
            window: Some((
                *toplevel,
                WindowTransform::from_zoom_progress(progress.value, *origin, w, h, card_radius),
            )),
            show_home: true,
            home_lift: 0.0,
            home_shift: 0.0,
            home_page: 0,
            cards: Vec::new(),
        },
        UiState::Grabbing {
            toplevel,
            tracker,
            cards,
            ..
        } => {
            let up = tracker.up_progress();
            let t = WindowTransform::from_tracker(tracker, w, h, card_radius);
            // Band B (reveal..mid): unfold the live fan behind the finger-tracked
            // front card. Bands A/C: just the single card (window path).
            let preview = matches!(
                sc_input::live_state(tracker),
                sc_input::NavState::SwitcherPreview
            ) && cards.len() > 1;
            if preview {
                use sc_input::thresholds as th;
                // Neighbours sit at full switcher spread and simply fade in after
                // the reveal point / fade out before the mid collapse (no gradual
                // unfolding). Linear ramps over FADE either side.
                const FADE: f32 = 0.06;
                let a_in = ((up - th::SWITCHER_REVEAL_PROGRESS) / FADE).clamp(0.0, 1.0);
                let a_out = ((th::HOME_MIN_PROGRESS - up) / FADE).clamp(0.0, 1.0);
                let alpha = a_in.min(a_out);
                let mut card_rects = switcher::fan_around(
                    t.center_x,
                    t.center_y,
                    t.scale,
                    cards,
                    alpha,
                    t.corner_radius,
                    (w, h),
                );
                // Front (current) card on top, drawn from the finger transform;
                // it is the live window, always fully opaque.
                card_rects.push(switcher::CardRect {
                    toplevel: *toplevel,
                    center_x: t.center_x,
                    center_y: t.center_y,
                    scale: t.scale,
                    corner_radius: t.corner_radius,
                    z: 1000,
                    close_progress: 0.0,
                    alpha: 1.0,
                });
                card_rects.sort_by_key(|r| r.z);
                Scene {
                    window: None,
                    show_home: true,
                    home_lift: 0.0,
                    home_shift: 0.0,
                    home_page: 0,
                    cards: card_rects,
                }
            } else {
                Scene {
                    window: Some((*toplevel, t)),
                    show_home: up > 0.05,
                    home_lift: 0.0,
                    home_shift: 0.0,
                    home_page: 0,
                    cards: Vec::new(),
                }
            }
        }
        UiState::Settling {
            toplevel,
            target,
            progress,
            origin,
            cards,
            ..
        } => {
            use sc_input::NavTarget;
            let transform = match target {
                NavTarget::BackToApp => {
                    // Settle back to fullscreen: interpolate from current toward fullscreen.
                    let p = progress.value.clamp(0.0, 1.0);
                    let scale = 1.0 - p * 0.5;
                    WindowTransform {
                        scale,
                        center_x: w / 2.0,
                        center_y: h / 2.0 - p * h * 0.1,
                        corner_radius: p * card_radius * 1.2,
                    }
                }
                NavTarget::Switcher => {
                    // Settling into the fan deck: the deck's cards are rounded at
                    // `card_radius`, so hold the radius there the whole way rather
                    // than letting `from_zoom_progress` collapse it toward 0 (which
                    // flashed the card square at release before the deck popped it
                    // back to rounded).
                    let mut t = WindowTransform::from_zoom_progress(
                        1.0 - progress.value,
                        *origin,
                        w,
                        h,
                        card_radius,
                    );
                    t.corner_radius = card_radius;
                    t
                }
                NavTarget::Home | NavTarget::QuickSwitch(_) => WindowTransform::from_zoom_progress(
                    1.0 - progress.value,
                    *origin,
                    w,
                    h,
                    card_radius,
                ),
            };
            // Settling into the switcher keeps the neighbour fan on screen the
            // whole way: neighbours fan around the front card as it zooms into
            // the deck's front slot, ending exactly at the switcher rest layout
            // (fan_around and switcher::layout share the same peek gap). The
            // switcher is then entered already-unfolded, so no vanish/re-fan.
            if matches!(target, NavTarget::Switcher) && cards.len() > 1 {
                let mut card_rects = switcher::fan_around(
                    transform.center_x,
                    transform.center_y,
                    transform.scale,
                    cards,
                    1.0,
                    transform.corner_radius,
                    (w, h),
                );
                card_rects.push(switcher::CardRect {
                    toplevel: *toplevel,
                    center_x: transform.center_x,
                    center_y: transform.center_y,
                    scale: transform.scale,
                    corner_radius: transform.corner_radius,
                    z: 1000,
                    close_progress: 0.0,
                    alpha: 1.0,
                });
                card_rects.sort_by_key(|r| r.z);
                return Scene {
                    window: None,
                    show_home: true,
                    home_lift: 0.0,
                    home_shift: 0.0,
                    home_page: 0,
                    cards: card_rects,
                };
            }
            Scene {
                window: Some((*toplevel, transform)),
                show_home: !matches!(target, NavTarget::BackToApp),
                home_lift: 0.0,
                home_shift: 0.0,
                home_page: 0,
                cards: Vec::new(),
            }
        }
        UiState::QuickSwitch {
            current,
            prev,
            next,
            offset,
            ..
        } => {
            // Rounded, FULL-HEIGHT cards with a margin between them slide
            // horizontally as a pair (no fullscreen pop, but not raised — a pure
            // left/right slide keeps full height). The current app's centre
            // shifts by `offset` screen-widths; the revealed neighbour sits one
            // card-width-plus-gap away on the appropriate side.
            const QS_SCALE: f32 = 1.0;
            let gap = w * 0.03;
            let step = w * QS_SCALE + gap;
            let off = offset.value;
            // Anchor to the usable area so a full-height card lands exactly where
            // the fullscreen app sits (below a top panel), not raised to y=0.
            let cur_cx = usable_origin.0 + w / 2.0 + off * w;
            let cy = usable_origin.1 + h / 2.0;
            let mut cards = Vec::with_capacity(2);
            // Neighbour first (z=0, drawn behind).
            let neighbor = if off > 0.0 {
                prev.as_ref().map(|(t, _)| (*t, cur_cx - step))
            } else if off < 0.0 {
                next.as_ref().map(|(t, _)| (*t, cur_cx + step))
            } else {
                None
            };
            if let Some((tid, cx)) = neighbor {
                cards.push(switcher::CardRect {
                    toplevel: tid,
                    center_x: cx,
                    center_y: cy,
                    scale: QS_SCALE,
                    corner_radius: card_radius,
                    z: 0,
                    close_progress: 0.0,
                    alpha: 1.0,
                });
            }
            cards.push(switcher::CardRect {
                toplevel: *current,
                center_x: cur_cx,
                center_y: cy,
                scale: QS_SCALE,
                corner_radius: card_radius,
                z: 1,
                close_progress: 0.0,
                alpha: 1.0,
            });
            Scene {
                window: None,
                show_home: true,
                home_lift: 0.0,
                home_shift: 0.0,
                home_page: 0,
                cards,
            }
        }
        UiState::Switcher {
            cards,
            scroll,
            close,
            enter,
        } => {
            let close_geo = close.map(|(t, p, _)| (t, p));
            let mut card_rects =
                switcher::layout(cards, scroll.value, (w, h), close_geo, card_radius);
            // Entrance from Home: the whole deck rises from below the bottom
            // edge into its rest layout. Entered from a grab the spring is
            // already at 1 and this is a no-op.
            let rise = (1.0 - enter.value.clamp(0.0, 1.0)) * h;
            if rise > 0.0 {
                for c in &mut card_rects {
                    c.center_y += rise;
                }
            }
            // Sort ascending z for back-to-front draw order.
            card_rects.sort_by_key(|r| r.z);
            Scene {
                window: None,
                show_home: true,
                home_lift: 0.0,
                home_shift: 0.0,
                home_page: 0,
                cards: card_rects,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_state::UiState;

    const TEST_SIZE: (i32, i32) = (1224, 2700);
    const TEST_RADIUS: f32 = 40.0;

    #[test]
    fn home_state_no_window() {
        let state = UiState::home(0, 1);
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert!(scene.window.is_none());
        assert!(scene.show_home);
    }

    #[test]
    fn app_opening_stops_covering_home_once_fullscreen() {
        // Regression: near the end of the icon-zoom-in animation the window
        // reaches fullscreen scale before the spring is formally "settled"
        // (state is still AppOpening, show_home still true). The renderer
        // must treat this as "home occluded" — window_covers_screen() is the
        // signal it uses to skip drawing home on top of the finished window.
        use crate::ui_state::{transition, UiEvent, ZoomOrigin};

        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::AppMapped {
                toplevel: 0,
                app_id: "x".into(),
                origin: ZoomOrigin::icon((100.0, 200.0)),
                open_mode: crate::ui_state::OpenMode::Zoom,
            },
        );
        assert!(matches!(state, UiState::AppOpening { .. }));

        // Tick until the window transform reaches (near-)fullscreen scale.
        let mut scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        for _ in 0..200 {
            if scene.window_covers_screen() {
                break;
            }
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 60.0 });
            scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        }

        assert!(
            scene.window_covers_screen(),
            "window never reached fullscreen scale"
        );
        // At this point show_home is still true (state hasn't settled to
        // UiState::App yet) — window_covers_screen() is what the renderer
        // must consult to avoid painting home over the finished window.
        assert!(scene.show_home);
    }

    #[test]
    fn sideways_slide_drags_home_out_to_the_right() {
        use crate::ui_state::{transition, OpenMode, UiEvent, ZoomOrigin};
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::AppMapped {
                toplevel: 1,
                app_id: "x".into(),
                origin: ZoomOrigin::icon((0.0, 0.0)),
                open_mode: OpenMode::SlideFromLeft,
            },
        );
        // Part-way through: home has moved right, the card is still left of centre.
        transition(&mut state, UiEvent::Tick { dt: 1.0 / 30.0 });
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert!(scene.show_home);
        assert!(scene.home_shift > 0.0, "shift={}", scene.home_shift);
        let (_, t) = scene.window.expect("card on screen");
        assert!(
            t.center_x < TEST_SIZE.0 as f32 / 2.0,
            "card should still be entering from the left, cx={}",
            t.center_x
        );
    }

    #[test]
    fn zoom_open_leaves_home_where_it_is() {
        use crate::ui_state::{transition, OpenMode, UiEvent, ZoomOrigin};
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::AppMapped {
                toplevel: 1,
                app_id: "x".into(),
                origin: ZoomOrigin::icon((100.0, 200.0)),
                open_mode: OpenMode::Zoom,
            },
        );
        transition(&mut state, UiEvent::Tick { dt: 1.0 / 30.0 });
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert_eq!(scene.home_shift, 0.0, "only the sideways slide moves home");
    }

    #[test]
    fn home_bounce_lifts_the_whole_home_screen() {
        use crate::ui_state::{transition, UiEvent};
        let mut state = UiState::home(0, 1);
        assert_eq!(
            compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS).home_lift,
            0.0
        );
        transition(&mut state, UiEvent::HomeBounce);
        transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 });
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert!(scene.home_lift > 0.0, "lift={}", scene.home_lift);
        assert!(scene.show_home);
    }

    #[test]
    fn switcher_deck_rises_from_below_while_entering() {
        let mut entering = sc_anim::Spring::new(0.0);
        entering.retarget(1.0);
        let state = UiState::Switcher {
            cards: vec![0, 1],
            scroll: sc_anim::Spring::new(0.0),
            close: None,
            enter: entering,
        };
        let rising = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        let at_rest = compute_scene(
            &UiState::Switcher {
                cards: vec![0, 1],
                scroll: sc_anim::Spring::new(0.0),
                close: None,
                enter: sc_anim::Spring::new(1.0),
            },
            TEST_SIZE,
            (0.0, 0.0),
            TEST_RADIUS,
        );
        // Every card starts a full screen height below its rest slot.
        for (r, rest) in rising.cards.iter().zip(at_rest.cards.iter()) {
            assert_eq!(r.toplevel, rest.toplevel);
            assert!(
                (r.center_y - rest.center_y - TEST_SIZE.1 as f32).abs() < 1.0,
                "card {} at {} vs rest {}",
                r.toplevel,
                r.center_y,
                rest.center_y
            );
        }
    }

    #[test]
    fn slide_enters_from_the_left_edge_as_home_leaves_right() {
        let (w, h) = (TEST_SIZE.0 as f32, TEST_SIZE.1 as f32);
        // Fully off the left edge at progress 0, with Home still in place …
        let start = WindowTransform::slide_in_from_left(0.0, w, h, TEST_RADIUS);
        assert!(
            (start.center_x + w * 0.5).abs() < 1.0,
            "cx={}",
            start.center_x
        );
        assert_eq!(home_slide_out(0.0, w), 0.0);
        assert!(start.corner_radius > 0.0, "rounded while travelling");
        // … centered (and square) once arrived, Home a full width to the right.
        let end = WindowTransform::slide_in_from_left(1.0, w, h, TEST_RADIUS);
        assert!((end.center_x - w / 2.0).abs() < 1.0);
        assert!((end.center_y - h / 2.0).abs() < 1.0);
        assert!(end.corner_radius < 0.01);
        assert!((home_slide_out(1.0, w) - w).abs() < 1.0);
        // Home and the card stay exactly one screen apart the whole way: they
        // travel as one sheet, no gap and no overlap.
        for p in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let card = WindowTransform::slide_in_from_left(p, w, h, TEST_RADIUS);
            let home_cx = w / 2.0 + home_slide_out(p, w);
            assert!(
                (home_cx - card.center_x - w).abs() < 0.01,
                "p={p}: home {home_cx} vs card {}",
                card.center_x
            );
        }
    }

    #[test]
    fn app_state_fullscreen() {
        let state = UiState::App {
            toplevel: 0,
            app_id: "x".into(),
        };
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        let (_, transform) = scene.window.unwrap();
        assert!((transform.scale - 1.0).abs() < 0.001);
        assert!(!scene.show_home);
    }

    #[test]
    fn tracker_pivot_from_finger() {
        let (w, h) = (TEST_SIZE.0 as f32, TEST_SIZE.1 as f32);
        let mut tracker = Tracker::begin(sc_input::Pt { x: 0.5, y: 0.95 });
        tracker.current = sc_input::Pt { x: 0.5, y: 0.7 };
        let t = WindowTransform::from_tracker(&tracker, w, h, TEST_RADIUS);
        // Window should be scaled down.
        assert!(t.scale < 1.0);
        // Finger at bottom of card: center_y should be above finger.
        let finger_y = 0.7 * h;
        assert!(t.center_y < finger_y);
        // Corner radius should be non-zero.
        assert!(t.corner_radius > 0.0);
    }

    #[test]
    fn tracker_no_movement_stays_fullscreen() {
        let (w, h) = (TEST_SIZE.0 as f32, TEST_SIZE.1 as f32);
        let tracker = Tracker::begin(sc_input::Pt { x: 0.5, y: 0.95 });
        let t = WindowTransform::from_tracker(&tracker, w, h, TEST_RADIUS);
        assert!((t.scale - 1.0).abs() < 0.001);
    }

    #[test]
    fn grabbing_band_b_unfolds_live_fan() {
        // Finger dragged up into band B (reveal..mid) with a multi-app deck:
        // the scene renders a rounded fan (no window), current card on top.
        let mut tracker = Tracker::begin(sc_input::Pt { x: 0.5, y: 0.95 });
        tracker.current = sc_input::Pt { x: 0.5, y: 0.75 }; // up_progress 0.20 (band B)
        let state = UiState::Grabbing {
            toplevel: 7,
            app_id: "x".into(),
            tracker,
            cards: vec![7, 3, 1],
        };
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert!(scene.window.is_none(), "fan uses the card path, not window");
        assert_eq!(scene.cards.len(), 3, "front + two neighbours");
        // Front card is the current app, sits on top (highest z), fully opaque.
        let front = scene.cards.iter().max_by_key(|c| c.z).unwrap();
        assert_eq!(front.toplevel, 7);
        assert_eq!(front.alpha, 1.0);
        // Neighbours are faded to full here (mid of band B) and rounded.
        assert!(scene.cards.iter().all(|c| c.corner_radius > 0.0));
        let nb = scene.cards.iter().find(|c| c.toplevel == 3).unwrap();
        assert!(nb.alpha > 0.9, "neighbour visible in mid-band");
    }

    /// Neighbour opacity at a given up_progress (0 if there is no fan).
    fn fan_neighbour_alpha(up: f32) -> Option<f32> {
        let mut tracker = Tracker::begin(sc_input::Pt { x: 0.5, y: 0.95 });
        tracker.current = sc_input::Pt {
            x: 0.5,
            y: 0.95 - up,
        };
        let state = UiState::Grabbing {
            toplevel: 7,
            app_id: "x".into(),
            tracker,
            cards: vec![7, 3, 1],
        };
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        scene
            .cards
            .iter()
            .find(|c| c.toplevel == 3)
            .map(|c| c.alpha)
    }

    #[test]
    fn grabbing_fan_fades_in_and_out_at_band_edges() {
        // Below reveal (0.12): no fan at all — the single card is on the window
        // path, no neighbour cards.
        assert!(fan_neighbour_alpha(0.05).is_none());
        // Fading in just above reveal: partially transparent, not popped to full.
        let a_in = fan_neighbour_alpha(0.15).expect("fan present");
        assert!(a_in > 0.0 && a_in < 1.0, "fade-in alpha was {a_in}");
        // Fully opaque through the middle of the band.
        assert!((fan_neighbour_alpha(0.23).expect("fan present") - 1.0).abs() < 1e-6);
        // Fading out approaching mid (0.35): partially transparent again.
        let a_out = fan_neighbour_alpha(0.32).expect("fan present");
        assert!(a_out > 0.0 && a_out < 1.0, "fade-out alpha was {a_out}");
    }

    #[test]
    fn grabbing_band_a_keeps_single_window() {
        // A tiny lift stays below the reveal threshold: single card, no fan.
        let mut tracker = Tracker::begin(sc_input::Pt { x: 0.5, y: 0.95 });
        tracker.current = sc_input::Pt { x: 0.5, y: 0.92 }; // up_progress 0.03
        let state = UiState::Grabbing {
            toplevel: 7,
            app_id: "x".into(),
            tracker,
            cards: vec![7, 3, 1],
        };
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert!(scene.window.is_some());
        assert!(scene.cards.is_empty());
    }

    #[test]
    fn quick_switch_cards_are_rounded_and_scaled() {
        use sc_anim::Spring;
        let mut offset = Spring::new(0.0);
        offset.value = -0.2; // sliding left, revealing `next`
        let state = UiState::QuickSwitch {
            current: 1,
            current_app: "a".into(),
            prev: Some((2, "b".into())),
            next: Some((3, "c".into())),
            offset,
            commit: None,
            releasing: false,
            start_x: 0.0,
            origin: sc_input::Pt { x: 0.5, y: 0.95 },
        };
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert_eq!(scene.cards.len(), 2, "current + revealed neighbour");
        // Full-height on a pure horizontal slide, but rounded (not a sharp pop).
        assert!(scene
            .cards
            .iter()
            .all(|c| (c.scale - 1.0).abs() < 1e-6 && c.corner_radius > 0.0));
        // Neighbour separated from the current card by more than a card width
        // (there is a visible margin, not a flush seam).
        let cur = scene.cards.iter().find(|c| c.toplevel == 1).unwrap();
        let nb = scene.cards.iter().find(|c| c.toplevel == 3).unwrap();
        assert!((nb.center_x - cur.center_x).abs() > TEST_SIZE.0 as f32 * 0.9);
    }

    #[test]
    fn switcher_scene_has_cards_back_to_front() {
        let state = UiState::Switcher {
            cards: vec![0, 1, 2],
            scroll: sc_anim::Spring::new(0.0),
            close: None,
            enter: sc_anim::Spring::new(1.0),
        };
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert_eq!(scene.cards.len(), 3);
        assert!(scene.show_home);
        assert!(scene.window.is_none());
        // Cards sorted ascending z: back card first.
        assert!(scene.cards[0].z < scene.cards[1].z);
        assert!(scene.cards[1].z < scene.cards[2].z);
    }
}
