//! Held-modifier app switching (Super+Tab).
//!
//! The touch shell reaches the switcher deck through a gesture; a keyboard
//! reaches it by holding a modifier: each Tab steps the deck one card and
//! letting the modifier go opens whatever card is focused. Nothing here jumps
//! the UI — every step goes through the same [`UiState`] springs the gestures
//! drive, so the deck slides in from the running app (or rises from Home) and
//! the chosen card zooms out to fullscreen.
//!
//! The session outlives a frame: Super+Tab from an app spends a few frames
//! animating into the deck, and the modifier may well be released before it
//! lands. So steps and the release are queued here and applied by
//! [`State::poll_kbd_switch`] once the deck actually exists.

use crate::state::State;
use crate::switcher;
use crate::ui_state::{transition, ToplevelId, UiEvent, UiState, ZoomOrigin};
use sc_input::NavTarget;
use tracing::debug;

/// An in-flight keyboard switching session: the modifier is (or was) held.
#[derive(Clone, Copy, Debug, Default)]
pub struct KbdSwitch {
    /// Steps requested before the deck existed, still to be applied.
    pending: i32,
    /// The modifier came up — open the focused card as soon as the deck is up.
    commit: bool,
}

impl State {
    /// The switcher deck as it would be entered right now: MRU order, minus any
    /// toplevel that has gone away since it was recorded.
    fn live_deck(&self) -> Vec<ToplevelId> {
        self.history
            .deck_order()
            .into_iter()
            .filter(|tid| matches!(self.toplevels.get(*tid), Some(Some(_))))
            .collect()
    }

    /// Step the switcher by `delta` cards, opening the deck first if it is not
    /// up. Positive walks toward older apps, matching the deck's own handedness.
    pub(crate) fn switcher_step(&mut self, delta: i32) {
        match &self.ui {
            // Deck already up (by gesture or an earlier step): step it now, and
            // adopt the session so releasing the modifier commits.
            UiState::Switcher { .. } => {
                transition(&mut self.ui, UiEvent::SwitcherStep { delta });
                self.kbd_switch.get_or_insert_default();
            }
            UiState::App { .. } => {
                let cards = self.live_deck();
                if cards.is_empty() {
                    return;
                }
                // Land in the front card slot, so the app shrinking out of
                // fullscreen flows straight into the fan — the same hand-off the
                // bar grab makes.
                let (cx, cy, scale) = switcher::front_slot(self.output_size_f());
                transition(
                    &mut self.ui,
                    UiEvent::OpenSwitcherFromApp {
                        cards,
                        origin: ZoomOrigin::card((cx, cy), scale),
                    },
                );
                self.kbd_switch = Some(KbdSwitch {
                    pending: delta,
                    commit: false,
                });
            }
            UiState::Home { .. } => {
                let cards = self.live_deck();
                if cards.is_empty() {
                    // Nothing to switch to: acknowledge the key the way the bar
                    // gesture acknowledges a dead-end swipe.
                    transition(&mut self.ui, UiEvent::HomeBounce);
                    return;
                }
                transition(&mut self.ui, UiEvent::OpenSwitcherFromHome { cards });
                self.kbd_switch = Some(KbdSwitch {
                    pending: delta,
                    commit: false,
                });
            }
            // Mid-animation. If that animation is this session's own settle into
            // the deck, the step belongs to it — queue it, or a quick second Tab
            // is swallowed by the frames the deck spends flying in. Anything
            // else is the shell already moving somewhere the user asked for.
            _ => {
                if let Some(session) = self.kbd_switch.as_mut() {
                    session.pending += delta;
                    self.needs_render = true;
                }
                return;
            }
        }
        self.needs_render = true;
    }

    /// The switching modifier came up: commit the focused card. A no-op unless a
    /// session is running, so a bare Super tap does nothing.
    pub(crate) fn switcher_release(&mut self) {
        let Some(session) = self.kbd_switch.as_mut() else {
            return;
        };
        session.commit = true;
        self.poll_kbd_switch();
    }

    /// Apply whatever the session still owes now that a frame has passed. Called
    /// once per frame, after the tick has advanced the state machine.
    pub(crate) fn poll_kbd_switch(&mut self) {
        let Some(mut session) = self.kbd_switch else {
            return;
        };
        match &self.ui {
            UiState::Switcher { .. } => {}
            // Still shrinking into the deck — steps and the commit wait for it.
            UiState::Settling {
                target: NavTarget::Switcher,
                ..
            } => return,
            // The deck was left some other way (a tap, a close, an app opening):
            // the session is over and must not steal the destination.
            _ => {
                self.kbd_switch = None;
                return;
            }
        }
        if session.pending != 0 {
            transition(
                &mut self.ui,
                UiEvent::SwitcherStep {
                    delta: session.pending,
                },
            );
            session.pending = 0;
        }
        if session.commit {
            self.kbd_switch = None;
            self.open_focused_card();
        } else {
            self.kbd_switch = Some(session);
        }
    }

    /// Open the card the carousel is focused on, zooming from wherever that card
    /// currently sits (it may still be flying).
    fn open_focused_card(&mut self) {
        let UiState::Switcher { cards, scroll, .. } = &self.ui else {
            return;
        };
        if cards.is_empty() {
            return;
        }
        let idx = (scroll.target.round() as i32).clamp(0, cards.len() as i32 - 1) as usize;
        let toplevel = cards[idx];
        // The laid-out rect from the last frame is where the card is on screen;
        // before the first layout, fall back to the resting front slot.
        let origin = self
            .switcher_cards
            .iter()
            .find(|c| c.toplevel == toplevel)
            .map(|c| ZoomOrigin::card((c.center_x, c.center_y), c.scale))
            .unwrap_or_else(|| {
                let (cx, cy, scale) = switcher::front_slot(self.output_size_f());
                ZoomOrigin::card((cx, cy), scale)
            });
        let app_id = self
            .toplevels
            .get(toplevel)
            .and_then(|t| t.as_ref())
            .map(|t| t.app_id.clone())
            .unwrap_or_default();
        debug!(target: "springchick::debug", "kbd switch commit toplevel={toplevel} idx={idx}");
        // Choosing a card makes it the most recent, as a tap does.
        self.history.push_foreground(toplevel);
        transition(
            &mut self.ui,
            UiEvent::SwitcherTapCard {
                toplevel,
                app_id,
                origin,
            },
        );
        self.needs_render = true;
    }
}
