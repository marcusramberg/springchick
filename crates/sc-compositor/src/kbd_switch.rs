//! Held-modifier app switching (Super+Tab). Steps drive the same [`UiState`]
//! springs as the gestures. The modifier may come up before the deck has
//! animated in, so steps and the release are queued and applied by
//! [`State::poll_kbd_switch`].

use crate::state::State;
use crate::switcher;
use crate::ui_state::{transition, ToplevelId, UiEvent, UiState, ZoomOrigin};
use sc_input::NavTarget;
use tracing::debug;

#[derive(Clone, Copy, Debug, Default)]
pub struct KbdSwitch {
    /// Steps requested before the deck existed.
    pending: i32,
    commit: bool,
}

impl State {
    /// MRU order, minus toplevels that have gone away.
    fn live_deck(&self) -> Vec<ToplevelId> {
        self.history
            .deck_order()
            .into_iter()
            .filter(|tid| matches!(self.toplevels.get(*tid), Some(Some(_))))
            .collect()
    }

    /// Positive walks toward older apps.
    pub(crate) fn switcher_step(&mut self, delta: i32) {
        match &self.ui {
            // Adopt the session so releasing the modifier commits.
            UiState::Switcher { .. } => {
                transition(&mut self.ui, UiEvent::SwitcherStep { delta });
                self.kbd_switch.get_or_insert_default();
            }
            UiState::App { .. } => {
                let cards = self.live_deck();
                if cards.is_empty() {
                    return;
                }
                // Land in the front slot so the shrinking app flows into the fan.
                let (cx, cy, scale) = switcher::front_slot(self.output_size_f());
                transition(
                    &mut self.ui,
                    UiEvent::OpenSwitcherFromApp {
                        cards,
                        origin: ZoomOrigin::card((cx, cy), scale),
                    },
                );
                // EnterSwitcher already focuses cards[1]; that is this Tab's step.
                self.kbd_switch = Some(KbdSwitch {
                    pending: delta - 1,
                    commit: false,
                });
            }
            UiState::Home { .. } => {
                let cards = self.live_deck();
                if cards.is_empty() {
                    transition(&mut self.ui, UiEvent::HomeBounce);
                    return;
                }
                transition(&mut self.ui, UiEvent::OpenSwitcherFromHome { cards });
                // The deck opens on the most recent app; Tab lands there, Shift+Tab
                // steps back to the oldest.
                self.kbd_switch = Some(KbdSwitch {
                    pending: if delta > 0 { delta - 1 } else { delta },
                    commit: false,
                });
            }
            // Mid-animation. If it's this session's own settle into the deck, queue the
            // step, or a quick second Tab is swallowed.
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

    /// A no-op without a session, so a bare Super tap does nothing.
    pub(crate) fn switcher_release(&mut self) {
        let Some(session) = self.kbd_switch.as_mut() else {
            return;
        };
        session.commit = true;
        self.poll_kbd_switch();
    }

    /// Called once per frame, after the tick.
    pub(crate) fn poll_kbd_switch(&mut self) {
        let Some(mut session) = self.kbd_switch else {
            return;
        };
        match &self.ui {
            UiState::Switcher { .. } => {}
            UiState::Settling {
                target: NavTarget::Switcher,
                ..
            } => return,
            // The deck was left some other way; the session must not steal the
            // destination.
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

    /// Zooms from wherever the card is now; it may still be flying.
    fn open_focused_card(&mut self) {
        let UiState::Switcher { cards, scroll, .. } = &self.ui else {
            return;
        };
        if cards.is_empty() {
            return;
        }
        let idx = (scroll.target.round() as i32).clamp(0, cards.len() as i32 - 1) as usize;
        let toplevel = cards[idx];
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
