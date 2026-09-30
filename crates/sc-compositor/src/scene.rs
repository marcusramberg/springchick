//! Per-frame scene: window transforms computed from UiState.

use crate::switcher;
use crate::ui_state::{ToplevelId, UiState};
use sc_input::Tracker;

#[derive(Clone, Copy, Debug)]
pub struct WindowTransform {
    /// 1.0 = fullscreen.
    pub scale: f32,
    /// Logical px.
    pub center_x: f32,
    pub center_y: f32,
    /// Logical px.
    pub corner_radius: f32,
}

impl WindowTransform {
    pub fn fullscreen(width: f32, height: f32) -> Self {
        Self {
            scale: 1.0,
            center_x: width / 2.0,
            center_y: height / 2.0,
            corner_radius: 0.0,
        }
    }

    /// `progress`: 0 = at `origin`, 1 = fullscreen.
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

    /// `progress` 0 = below the screen, 1 = centered. Held just under fullscreen
    /// scale so the renderer keeps it on the card path until the state settles.
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

    /// Slides in from the left as Home is pushed right, paired with
    /// [`home_slide_out`] so they move as one sheet. Held under fullscreen scale
    /// like [`Self::slide_up`].
    pub fn slide_in_from_left(progress: f32, width: f32, height: f32, card_radius: f32) -> Self {
        let ease = sc_anim::ease_out_cubic(progress);
        Self {
            scale: 0.98,
            center_x: width * 0.5 - (1.0 - ease) * width,
            center_y: height / 2.0,
            corner_radius: card_radius * (1.0 - ease),
        }
    }

    /// The window follows the finger, which stays at its bottom edge.
    pub fn from_tracker(tracker: &Tracker, width: f32, height: f32, card_radius: f32) -> Self {
        let up = tracker.up_progress().clamp(0.0, 1.0);

        // 1.0 → 0.20, fast enough that the top edge doesn't reach the screen top.
        let scale = 1.0 - up.powf(0.6) * 0.8;

        let finger_x = tracker.current.x * width;
        let finger_y = tracker.current.y * height;

        let card_h = height * scale;
        let center_x = finger_x;
        let center_y = finger_y - card_h / 2.0;

        // Rounded from lift-off, approaching the deck's radius at full travel so
        // the hand-off is seamless.
        let corner_radius = card_radius * 0.6 + (1.0 - scale) * card_radius * 0.7;

        Self {
            scale,
            center_x,
            center_y,
            corner_radius,
        }
    }
}

/// Pixels; the same eased curve as [`WindowTransform::slide_in_from_left`].
fn home_slide_out(progress: f32, width: f32) -> f32 {
    sc_anim::ease_out_cubic(progress) * width
}

/// Full through the switcher band, easing out across the go-home band. Also
/// drives the settle to Home.
fn grab_backdrop_blur(up: f32) -> f32 {
    use sc_input::thresholds as th;
    const SHARP_AT: f32 = 2.0 / 3.0;
    ((SHARP_AT - up) / (SHARP_AT - th::HOME_MIN_PROGRESS)).clamp(0.0, 1.0)
}

#[derive(Clone, Debug)]
pub struct Scene {
    /// `None` = no app visible.
    pub window: Option<(ToplevelId, WindowTransform)>,
    pub show_home: bool,
    pub home_page: usize,
    /// Sorted ascending z.
    pub cards: Vec<switcher::CardRect>,
    /// Pixels, positive = up. Only while the Home bounce rings.
    pub home_lift: f32,
    /// Pixels, positive = right. Only during a slide onto the top card.
    pub home_shift: f32,
    /// 0..1 blur of what's behind the deck; 0 skips the pass.
    pub backdrop_blur: f32,
}

impl Scene {
    /// When true, home must not be drawn: it would paint over the window.
    /// Mirrors the renderer's `is_fullscreen` threshold.
    pub fn window_covers_screen(&self) -> bool {
        self.window.is_none_or(|(_, t)| t.scale >= 0.99)
    }
}

/// `usable_origin` is the physical top-left of the usable area; full-height
/// cards shift by it to sit where the real app sits.
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
            home_lift: bounce.value * h,
            home_shift: 0.0,
            home_page: *page,
            backdrop_blur: 0.0,
            cards: Vec::new(),
        },
        UiState::App { toplevel, .. } => Scene {
            window: Some((*toplevel, WindowTransform::fullscreen(w, h))),
            show_home: false,
            home_lift: 0.0,
            home_shift: 0.0,
            home_page: 0,
            backdrop_blur: 0.0,
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
            // Only the sideways slide moves Home.
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
                backdrop_blur: 0.0,
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
            backdrop_blur: 0.0,
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
            let blur = grab_backdrop_blur(up);
            // Between reveal and mid: the live fan. Otherwise just the window.
            let preview = matches!(
                sc_input::live_state(tracker),
                sc_input::NavState::SwitcherPreview
            ) && cards.len() > 1;
            if preview {
                use sc_input::thresholds as th;
                // Neighbours sit at full spread and just fade in/out over FADE.
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
                card_rects.push(switcher::CardRect {
                    toplevel: *toplevel,
                    center_x: t.center_x,
                    center_y: t.center_y,
                    scale: t.scale,
                    corner_radius: t.corner_radius,
                    z: 1000,
                    alpha: 1.0,
                    dim: 0.0,
                });
                card_rects.sort_by_key(|r| r.z);
                Scene {
                    window: None,
                    show_home: true,
                    home_lift: 0.0,
                    home_shift: 0.0,
                    home_page: 0,
                    backdrop_blur: blur,
                    cards: card_rects,
                }
            } else {
                Scene {
                    window: Some((*toplevel, t)),
                    show_home: true,
                    home_lift: 0.0,
                    home_shift: 0.0,
                    home_page: 0,
                    backdrop_blur: blur,
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
                    // Hold the deck's radius the whole way; `from_zoom_progress` would flash
                    // the card square before the deck rounds it again.
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
            // Keep the fan while settling so it ends exactly at the switcher layout
            // (fan_around and layout share the peek gap).
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
                    alpha: 1.0,
                    dim: 0.0,
                });
                card_rects.sort_by_key(|r| r.z);
                return Scene {
                    window: None,
                    show_home: true,
                    home_lift: 0.0,
                    home_shift: 0.0,
                    home_page: 0,
                    // Hold full blur so the hand-off doesn't flash a sharp Home.
                    backdrop_blur: 1.0,
                    cards: card_rects,
                };
            }
            Scene {
                window: Some((*toplevel, transform)),
                show_home: !matches!(target, NavTarget::BackToApp),
                home_lift: 0.0,
                home_shift: 0.0,
                home_page: 0,
                backdrop_blur: match target {
                    NavTarget::Home => grab_backdrop_blur(progress.value),
                    NavTarget::Switcher => 1.0,
                    _ => 0.0,
                },
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
            // Full-height rounded cards sliding as a pair. The current card shifts by
            // `offset` widths; the neighbour sits one card plus gap away.
            const QS_SCALE: f32 = 1.0;
            let gap = w * 0.03;
            let step = w * QS_SCALE + gap;
            let off = offset.value;
            // Anchored to the usable area, where the fullscreen app sits.
            let cur_cx = usable_origin.0 + w / 2.0 + off * w;
            let cy = usable_origin.1 + h / 2.0;
            let mut cards = Vec::with_capacity(2);
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
                    alpha: 1.0,
                    dim: 0.0,
                });
            }
            cards.push(switcher::CardRect {
                toplevel: *current,
                center_x: cur_cx,
                center_y: cy,
                scale: QS_SCALE,
                corner_radius: card_radius,
                z: 1,
                alpha: 1.0,
                dim: 0.0,
            });
            Scene {
                window: None,
                show_home: true,
                home_lift: 0.0,
                home_shift: 0.0,
                home_page: 0,
                backdrop_blur: 0.0,
                cards,
            }
        }
        UiState::Switcher {
            cards,
            scroll,
            close,
            enter,
            exit_left,
        } => {
            let close_geo = close.map(|c| (c.toplevel, c.progress.value));
            let mut card_rects =
                switcher::layout(cards, scroll.value, (w, h), close_geo, card_radius);
            // From Home the deck rises from below; from a grab `enter` is already 1.
            let travel = 1.0 - enter.value.clamp(0.0, 1.0);
            if travel > 0.0 {
                for c in &mut card_rects {
                    // Far enough that the front (rightmost) card clears the edge.
                    if *exit_left {
                        c.center_x -= travel * w * 1.7;
                    } else {
                        c.center_y += travel * h;
                    }
                }
            }
            card_rects.sort_by_key(|r| r.z);
            Scene {
                window: None,
                show_home: true,
                home_lift: 0.0,
                home_shift: 0.0,
                home_page: 0,
                backdrop_blur: enter.value.clamp(0.0, 1.0),
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
        // The window reaches fullscreen scale while still AppOpening with
        // show_home true; window_covers_screen() must say home is occluded.
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
    fn switcher_backdrop_blur_ramps_with_the_entrance() {
        let mk = |enter: f32| UiState::Switcher {
            cards: vec![0, 1],
            scroll: sc_anim::Spring::new(0.0),
            close: None,
            enter: sc_anim::Spring::new(enter),
            exit_left: false,
        };
        let opening = compute_scene(&mk(0.0), TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        let settled = compute_scene(&mk(1.0), TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert_eq!(opening.backdrop_blur, 0.0);
        assert_eq!(settled.backdrop_blur, 1.0);
        assert_eq!(
            compute_scene(&UiState::home(0, 1), TEST_SIZE, (0.0, 0.0), TEST_RADIUS).backdrop_blur,
            0.0
        );
    }

    #[test]
    fn settling_into_the_switcher_stays_blurred() {
        for cards in [vec![], vec![0, 1]] {
            let state = UiState::Settling {
                toplevel: 0,
                app_id: "x".into(),
                target: sc_input::NavTarget::Switcher,
                progress: sc_anim::Spring::new(0.2),
                origin: crate::ui_state::ZoomOrigin::icon((0.0, 0.0)),
                cards,
            };
            let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
            assert_eq!(scene.backdrop_blur, 1.0);
        }
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
            exit_left: false,
        };
        let rising = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        let at_rest = compute_scene(
            &UiState::Switcher {
                cards: vec![0, 1],
                scroll: sc_anim::Spring::new(0.0),
                close: None,
                enter: sc_anim::Spring::new(1.0),
                exit_left: false,
            },
            TEST_SIZE,
            (0.0, 0.0),
            TEST_RADIUS,
        );
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
        let start = WindowTransform::slide_in_from_left(0.0, w, h, TEST_RADIUS);
        assert!(
            (start.center_x + w * 0.5).abs() < 1.0,
            "cx={}",
            start.center_x
        );
        assert_eq!(home_slide_out(0.0, w), 0.0);
        assert!(start.corner_radius > 0.0, "rounded while travelling");
        let end = WindowTransform::slide_in_from_left(1.0, w, h, TEST_RADIUS);
        assert!((end.center_x - w / 2.0).abs() < 1.0);
        assert!((end.center_y - h / 2.0).abs() < 1.0);
        assert!(end.corner_radius < 0.01);
        assert!((home_slide_out(1.0, w) - w).abs() < 1.0);
        // One sheet: always exactly one screen apart.
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
    fn grab_blur_is_full_until_the_go_home_band() {
        use sc_input::thresholds as th;
        assert_eq!(grab_backdrop_blur(0.0), 1.0);
        assert_eq!(grab_backdrop_blur(th::SWITCHER_REVEAL_PROGRESS), 1.0);
        assert_eq!(grab_backdrop_blur(0.25), 1.0);
        assert_eq!(grab_backdrop_blur(th::HOME_MIN_PROGRESS), 1.0);
        assert!(grab_backdrop_blur(0.5) > 0.4);
        assert_eq!(grab_backdrop_blur(2.0 / 3.0), 0.0);
    }

    #[test]
    fn grabbing_shows_blurred_home_immediately() {
        let mut tracker = Tracker::begin(sc_input::Pt { x: 0.5, y: 0.99 });
        tracker.current = sc_input::Pt { x: 0.5, y: 0.96 };
        let state = UiState::Grabbing {
            toplevel: 0,
            app_id: "x".into(),
            tracker,
            cards: Vec::new(),
        };
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert!(scene.show_home);
        assert!(scene.backdrop_blur > 0.0);
    }

    #[test]
    fn tracker_pivot_from_finger() {
        let (w, h) = (TEST_SIZE.0 as f32, TEST_SIZE.1 as f32);
        let mut tracker = Tracker::begin(sc_input::Pt { x: 0.5, y: 0.95 });
        tracker.current = sc_input::Pt { x: 0.5, y: 0.7 };
        let t = WindowTransform::from_tracker(&tracker, w, h, TEST_RADIUS);
        assert!(t.scale < 1.0);
        let finger_y = 0.7 * h;
        assert!(t.center_y < finger_y);
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
        let mut tracker = Tracker::begin(sc_input::Pt { x: 0.5, y: 0.95 });
        tracker.current = sc_input::Pt { x: 0.5, y: 0.75 }; // band B
        let state = UiState::Grabbing {
            toplevel: 7,
            app_id: "x".into(),
            tracker,
            cards: vec![7, 3, 1],
        };
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert!(scene.window.is_none(), "fan uses the card path, not window");
        assert_eq!(scene.cards.len(), 3, "front + two neighbours");
        let front = scene.cards.iter().max_by_key(|c| c.z).unwrap();
        assert_eq!(front.toplevel, 7);
        assert_eq!(front.alpha, 1.0);
        assert!(scene.cards.iter().all(|c| c.corner_radius > 0.0));
        let nb = scene.cards.iter().find(|c| c.toplevel == 3).unwrap();
        assert!(nb.alpha > 0.9, "neighbour visible in mid-band");
    }

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
        assert!(fan_neighbour_alpha(0.05).is_none());
        let a_in = fan_neighbour_alpha(0.15).expect("fan present");
        assert!(a_in > 0.0 && a_in < 1.0, "fade-in alpha was {a_in}");
        assert!((fan_neighbour_alpha(0.23).expect("fan present") - 1.0).abs() < 1e-6);
        let a_out = fan_neighbour_alpha(0.32).expect("fan present");
        assert!(a_out > 0.0 && a_out < 1.0, "fade-out alpha was {a_out}");
    }

    #[test]
    fn grabbing_band_a_keeps_single_window() {
        let mut tracker = Tracker::begin(sc_input::Pt { x: 0.5, y: 0.95 });
        tracker.current = sc_input::Pt { x: 0.5, y: 0.92 };
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
        offset.value = -0.2;
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
        assert!(scene
            .cards
            .iter()
            .all(|c| (c.scale - 1.0).abs() < 1e-6 && c.corner_radius > 0.0));
        let cur = scene.cards.iter().find(|c| c.toplevel == 1).unwrap();
        let nb = scene.cards.iter().find(|c| c.toplevel == 3).unwrap();
        assert!((nb.center_x - cur.center_x).abs() > TEST_SIZE.0 as f32 * 0.9);
    }

    #[test]
    fn dismissed_deck_slides_off_the_left_edge() {
        let mk = |enter: f32, exit_left: bool| UiState::Switcher {
            cards: vec![0, 1],
            scroll: sc_anim::Spring::new(0.0),
            close: None,
            enter: sc_anim::Spring::new(enter),
            exit_left,
        };
        let at_rest = compute_scene(&mk(1.0, true), TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        let gone = compute_scene(&mk(0.0, true), TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        let w = TEST_SIZE.0 as f32;
        for (a, b) in at_rest.cards.iter().zip(&gone.cards) {
            assert!(b.center_x < a.center_x, "cards travel left");
            assert!(b.center_x + w * b.scale / 2.0 < 0.0);
            assert_eq!(b.center_y, a.center_y, "no vertical travel on this exit");
        }
        assert_eq!(gone.backdrop_blur, 0.0, "home unblurs as the deck leaves");
    }

    #[test]
    fn switcher_scene_has_cards_back_to_front() {
        let state = UiState::Switcher {
            cards: vec![0, 1, 2],
            scroll: sc_anim::Spring::new(0.0),
            close: None,
            enter: sc_anim::Spring::new(1.0),
            exit_left: false,
        };
        let scene = compute_scene(&state, TEST_SIZE, (0.0, 0.0), TEST_RADIUS);
        assert_eq!(scene.cards.len(), 3);
        assert!(scene.show_home);
        assert!(scene.window.is_none());
        assert!(scene.cards[0].z < scene.cards[1].z);
        assert!(scene.cards[1].z < scene.cards[2].z);
    }
}
