//! `ext-session-lock-v1`: screen locking driven by an external lock client
//! (dms's lock screen, swaylock, ...).
//!
//! The protocol's contract is security-shaped, not cosmetic: once a client asks
//! for the lock, the compositor must stop showing *any* client content — and
//! keep it hidden even if the lock client crashes. So the lock is a hard gate in
//! front of the whole shell rather than another surface stacked on top:
//!
//! - [`crate::render::draw_scene`] short-circuits to [`LockView`]: the lock
//!   surface if the client gave us one, otherwise a black screen. The home
//!   screen, the app, the layers and the popups are not drawn at all.
//! - [`crate::touch`] routes every press to the lock surface (or nowhere) and
//!   the home gesture funnel is never fed.
//! - [`crate::toplevel::State::sync_keyboard_focus`] hands the keyboard to the
//!   lock surface, and [`crate::keybinds`] drops every shell action that could
//!   otherwise reach past the lock.
//!
//! The `locked` flag is only cleared by an explicit `unlock_and_destroy` from
//! the client. A lock client that dies without unlocking leaves us locked with
//! no surface — [`LockView::Blank`] — which is exactly the intended failure
//! mode: the session is unreachable, not wide open.
//!
//! ## Confirming the lock
//!
//! `ext_session_lock_v1.locked` promises the client that nothing of the session
//! is visible any more, so it may only be sent once a frame drawn under the lock
//! has actually been presented. [`SessionLock::tick`] therefore holds the
//! [`SessionLocker`] for one full frame — the lock engages visually on the frame
//! the request arrived, and the confirmation goes out on the next one.

use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};

use tracing::info;

use crate::state::State;

/// What the render path should put on screen for the lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockView {
    /// No lock: draw the shell as usual.
    Unlocked,
    /// Locked with a live lock surface — draw it, and nothing else.
    Surface,
    /// Locked with no usable surface (the lock client hasn't produced one yet,
    /// or died holding the lock). Draw black.
    Blank,
}

/// Resolve the lock's render view from the two facts that decide it.
///
/// Split out from [`SessionLock::view`] so the (load-bearing) rule that a locked
/// session without a surface still hides everything is unit-testable without a
/// Wayland display.
pub fn lock_view(locked: bool, has_surface: bool) -> LockView {
    match (locked, has_surface) {
        (false, _) => LockView::Unlocked,
        (true, true) => LockView::Surface,
        (true, false) => LockView::Blank,
    }
}

/// `ext-session-lock-v1` state: the manager global, the current lock, and the
/// surface the lock client gave us.
pub struct SessionLock {
    /// smithay's protocol state (holds the global + the locked-output list).
    pub manager: SessionLockManagerState,
    /// Whether the session is locked. Set the moment the request arrives (before
    /// the client is told), cleared only by `unlock_and_destroy`.
    locked: bool,
    /// Confirmation owed to the client, held until a locked frame has been
    /// presented. `None` once sent (or if locking was abandoned).
    pending: Option<SessionLocker>,
    /// Frames prepared since the lock engaged; the confirmation waits for one.
    frames: u32,
    /// The lock client's surface, if it has created one.
    surface: Option<LockSurface>,
}

impl SessionLock {
    /// Create the manager global. Any client may bind it — a filter would only
    /// be useful with a security-context sandbox to filter on.
    pub fn new(dh: &DisplayHandle) -> Self {
        SessionLock {
            manager: SessionLockManagerState::new::<State, _>(dh, |_client| true),
            locked: false,
            pending: None,
            frames: 0,
            surface: None,
        }
    }

    /// Whether the session is locked, i.e. no client content but the lock
    /// surface may be shown and no shell input may be acted on.
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// The lock surface's `wl_surface`, if the client made one and it is alive.
    /// A destroyed surface reads as absent so the screen falls back to black.
    pub fn wl_surface(&self) -> Option<&WlSurface> {
        self.surface
            .as_ref()
            .filter(|s| s.alive())
            .map(|s| s.wl_surface())
    }

    /// What to draw this frame.
    pub fn view(&self) -> LockView {
        lock_view(self.locked, self.wl_surface().is_some())
    }

    /// True while a confirmation is still owed, so the frame loops keep drawing
    /// until the locked frame that satisfies it has been presented.
    pub fn needs_frame(&self) -> bool {
        self.pending.is_some()
    }

    /// Engage the lock. The screen goes to [`LockView::Blank`] on this frame;
    /// the client is told on the next one (see [`Self::tick`]).
    fn engage(&mut self, confirmation: SessionLocker) {
        self.locked = true;
        self.pending = Some(confirmation);
        self.frames = 0;
    }

    /// Release the lock (client sent `unlock_and_destroy`).
    fn release(&mut self) {
        self.locked = false;
        self.pending = None;
        self.frames = 0;
        self.surface = None;
    }

    /// Adopt the lock client's surface.
    fn set_surface(&mut self, surface: LockSurface) {
        self.surface = Some(surface);
    }

    /// The lock surface was destroyed — the client unlocked, or died holding the
    /// lock. Either way there is nothing left to draw from it.
    fn forget_surface(&mut self) {
        self.surface = None;
    }

    /// Per-frame: once a full frame has been prepared under the lock, send the
    /// confirmation. Dropping the [`SessionLocker`] instead would tell the client
    /// locking failed, so it is only ever taken to confirm.
    pub fn tick(&mut self) {
        if self.pending.is_none() {
            return;
        }
        if self.frames >= 1 {
            if let Some(confirmation) = self.pending.take() {
                confirmation.lock();
                info!("session locked");
            }
        } else {
            self.frames += 1;
        }
    }
}

impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock.manager
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        info!("session lock requested");
        self.session_lock.engage(confirmation);
        // Anything the user was in the middle of on the shell is over: a
        // half-finished swipe or a held icon must not resume (or fire) when the
        // session unlocks.
        self.cancel_gestures();
        self.needs_render = true;
    }

    fn unlock(&mut self) {
        info!("session unlocked");
        self.session_lock.release();
        // The lock painted over the whole screen without touching the app's
        // commit cursor, so the first unlocked frame must repaint everything:
        // damage-since-last-present would otherwise report only what the app
        // itself changed and leave the lock's pixels on scanout.
        self.last_present = None;
        self.needs_render = true;
    }

    fn new_surface(&mut self, surface: LockSurface, _output: WlOutput) {
        // Enter the output so the client learns the scale factor and renders a
        // HiDPI buffer, exactly like an app toplevel.
        self.output.enter(surface.wl_surface());
        // Configure it to fill the output. The size is logical (the client
        // scales its buffer up by `dpi`), matching how app toplevels are sized.
        let (w, h) = self.output_size_f();
        let size = (
            (w as f64 / self.dpi).round() as u32,
            (h as f64 / self.dpi).round() as u32,
        );
        surface.with_pending_state(|state| {
            state.size = Some(size.into());
        });
        surface.send_configure();
        // A lock client that dies holding the lock leaves its last frame in the
        // scanout buffer, and nothing else would ask for a redraw: the session
        // is locked, so no shell animation or client commit is coming. Without
        // this hook the screen keeps showing the dead lock screen's pixels
        // instead of going black.
        smithay::wayland::compositor::add_destruction_hook(
            surface.wl_surface(),
            |state: &mut State, _surface| {
                state.session_lock.forget_surface();
                state.needs_render = true;
            },
        );
        self.session_lock.set_surface(surface);
        self.needs_render = true;
    }
}

smithay::delegate_session_lock!(State);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlocked_draws_the_shell() {
        assert_eq!(lock_view(false, false), LockView::Unlocked);
        assert_eq!(lock_view(false, true), LockView::Unlocked);
    }

    #[test]
    fn locked_with_surface_draws_it() {
        assert_eq!(lock_view(true, true), LockView::Surface);
    }

    /// The security-critical case: a lock client that died (or hasn't drawn yet)
    /// must leave the screen black, never fall back to showing the session.
    #[test]
    fn locked_without_surface_draws_black() {
        assert_eq!(lock_view(true, false), LockView::Blank);
    }
}
