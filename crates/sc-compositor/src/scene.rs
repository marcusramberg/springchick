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

    /// Freeform grab: window follows finger, pivoting from the bottom.
    /// The finger stays at the bottom edge of the scaled window.
    /// Scale shrinks aggressively so the top moves down toward the finger.
    pub fn from_tracker(tracker: &Tracker, width: f32, height: f32, card_radius: f32) -> Self {
        let up = tracker.up_progress().clamp(0.0, 1.0);

        // Aggressive scaling: 1.0 → 0.35 over full vertical travel.
        // Cubic ease-in so it accelerates as you drag further.
        let scale = 1.0 - up.powf(0.6) * 0.65;

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

/// Compute the scene from the current UiState.
pub fn compute_scene(state: &UiState, output_size: (i32, i32), card_radius: f32) -> Scene {
    let (w, h) = (output_size.0 as f32, output_size.1 as f32);
    match state {
        UiState::Home { page, .. } => Scene {
            window: None,
            show_home: true,
            home_page: *page,
            cards: Vec::new(),
        },
        UiState::App { toplevel, .. } => Scene {
            window: Some((*toplevel, WindowTransform::fullscreen(w, h))),
            show_home: false,
            home_page: 0,
            cards: Vec::new(),
        },
        UiState::AppOpening {
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
            home_page: 0,
            cards: Vec::new(),
        },
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
            home_page: 0,
            cards: Vec::new(),
        },
        UiState::Grabbing {
            toplevel, tracker, ..
        } => {
            let up = tracker.up_progress();
            Scene {
                window: Some((
                    *toplevel,
                    WindowTransform::from_tracker(tracker, w, h, card_radius),
                )),
                show_home: up > 0.05,
                home_page: 0,
                cards: Vec::new(),
            }
        }
        UiState::Settling {
            toplevel,
            target,
            progress,
            origin,
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
                    let mut t =
                        WindowTransform::from_zoom_progress(1.0 - progress.value, *origin, w, h, card_radius);
                    t.corner_radius = card_radius;
                    t
                }
                NavTarget::Home | NavTarget::QuickSwitch(_) => {
                    WindowTransform::from_zoom_progress(
                        1.0 - progress.value,
                        *origin,
                        w,
                        h,
                        card_radius,
                    )
                }
            };
            Scene {
                window: Some((*toplevel, transform)),
                show_home: !matches!(target, NavTarget::BackToApp),
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
            // Both apps ride at full scale; the pair slides horizontally as one.
            // The current app's centre shifts by `offset` screen-widths; the
            // revealed neighbour sits flush against it on the appropriate side.
            let off = offset.value;
            let cur_cx = w / 2.0 + off * w;
            let mut cards = Vec::with_capacity(2);
            // Neighbour first (z=0, drawn behind the seam).
            let neighbor = if off > 0.0 {
                prev.as_ref().map(|(t, _)| (*t, cur_cx - w))
            } else if off < 0.0 {
                next.as_ref().map(|(t, _)| (*t, cur_cx + w))
            } else {
                None
            };
            if let Some((tid, cx)) = neighbor {
                cards.push(switcher::CardRect {
                    toplevel: tid,
                    center_x: cx,
                    center_y: h / 2.0,
                    scale: 1.0,
                    corner_radius: 0.0,
                    z: 0,
                    close_progress: 0.0,
                });
            }
            cards.push(switcher::CardRect {
                toplevel: *current,
                center_x: cur_cx,
                center_y: h / 2.0,
                scale: 1.0,
                corner_radius: 0.0,
                z: 1,
                close_progress: 0.0,
            });
            Scene {
                window: None,
                show_home: false,
                home_page: 0,
                cards,
            }
        }
        UiState::Switcher {
            cards,
            scroll,
            close,
            entry,
        } => {
            let close_geo = close.map(|(t, p, _)| (t, p));
            let mut card_rects =
                switcher::layout(cards, scroll.value, (w, h), close_geo, entry.value, card_radius);
            // Sort ascending z for back-to-front draw order.
            card_rects.sort_by_key(|r| r.z);
            Scene {
                window: None,
                show_home: true,
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
        let scene = compute_scene(&state, TEST_SIZE, TEST_RADIUS);
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
            },
        );
        assert!(matches!(state, UiState::AppOpening { .. }));

        // Tick until the window transform reaches (near-)fullscreen scale.
        let mut scene = compute_scene(&state, TEST_SIZE, TEST_RADIUS);
        for _ in 0..200 {
            if scene.window_covers_screen() {
                break;
            }
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 60.0 });
            scene = compute_scene(&state, TEST_SIZE, TEST_RADIUS);
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
    fn app_state_fullscreen() {
        let state = UiState::App {
            toplevel: 0,
            app_id: "x".into(),
        };
        let scene = compute_scene(&state, TEST_SIZE, TEST_RADIUS);
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
    fn switcher_scene_has_cards_back_to_front() {
        let state = UiState::Switcher {
            cards: vec![0, 1, 2],
            scroll: sc_anim::Spring::new(0.0),
            close: None,
            entry: sc_anim::Spring::new(1.0),
        };
        let scene = compute_scene(&state, TEST_SIZE, TEST_RADIUS);
        assert_eq!(scene.cards.len(), 3);
        assert!(scene.show_home);
        assert!(scene.window.is_none());
        // Cards sorted ascending z: back card first.
        assert!(scene.cards[0].z < scene.cards[1].z);
        assert!(scene.cards[1].z < scene.cards[2].z);
    }
}
