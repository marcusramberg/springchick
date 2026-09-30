//! `ext-session-lock-v1`. Once locked, no client content but the lock surface
//! may be shown, even if the lock client crashes: render, touch, keyboard and
//! keybinds all gate on [`SessionLock::is_locked`], and a dead lock client
//! leaves [`LockView::Blank`]. Only `unlock_and_destroy` from the owning lock
//! clears it.
//!
//! smithay reports every `lock` request and treats whichever `SessionLocker`
//! we confirm as the owner, so a second `lock` is refused and lock surfaces
//! are only adopted from the owner. Otherwise any client could take over a
//! locked session.
//!
//! `locked` may only be sent after a frame drawn under the lock is presented,
//! so [`SessionLock::tick`] holds the confirmation one frame.

use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};

use tracing::{info, warn};

use crate::state::State;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockView {
    Unlocked,
    Surface,
    /// No surface yet, or the lock client died. Draw black.
    Blank,
}

/// Split out so the locked-without-surface rule is testable.
pub fn lock_view(locked: bool, has_surface: bool) -> LockView {
    match (locked, has_surface) {
        (false, _) => LockView::Unlocked,
        (true, true) => LockView::Surface,
        (true, false) => LockView::Blank,
    }
}

pub struct SessionLock {
    pub manager: SessionLockManagerState,
    /// Set when the request arrives; cleared only by `unlock_and_destroy`.
    locked: bool,
    /// Held until a locked frame has been presented.
    pending: Option<SessionLocker>,
    frames: u32,
    surface: Option<LockSurface>,
    /// The `ext_session_lock_v1` that owns the lock; see [`SessionLock::owns`].
    owner: Option<ExtSessionLockV1>,
}

impl SessionLock {
    pub fn new(dh: &DisplayHandle) -> Self {
        SessionLock {
            manager: SessionLockManagerState::new::<State, _>(dh, |_client| true),
            locked: false,
            pending: None,
            frames: 0,
            surface: None,
            owner: None,
        }
    }

    /// False for every other instance, including one from the same client.
    fn owns(&self, lock: &ExtSessionLockV1) -> bool {
        self.owner.as_ref() == Some(lock)
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// A destroyed surface reads as absent.
    pub fn wl_surface(&self) -> Option<&WlSurface> {
        self.surface
            .as_ref()
            .filter(|s| s.alive())
            .map(|s| s.wl_surface())
    }

    pub fn view(&self) -> LockView {
        lock_view(self.locked, self.wl_surface().is_some())
    }

    /// Keeps the frame loops drawing until the confirmation is sent.
    pub fn needs_frame(&self) -> bool {
        self.pending.is_some()
    }

    fn engage(&mut self, confirmation: SessionLocker) {
        self.locked = true;
        self.owner = Some(confirmation.ext_session_lock().clone());
        self.pending = Some(confirmation);
        self.frames = 0;
    }

    fn release(&mut self) {
        self.locked = false;
        self.pending = None;
        self.frames = 0;
        self.surface = None;
        self.owner = None;
    }

    fn set_surface(&mut self, surface: LockSurface) {
        self.surface = Some(surface);
    }

    fn forget_surface(&mut self) {
        self.surface = None;
    }

    /// Dropping the [`SessionLocker`] would tell the client locking failed, so
    /// it is only taken to confirm.
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

    /// Confirm without a frame. Only valid with the panel dark: `tick` doesn't
    /// run while blanked, and the lock would stay pending until wake (measured
    /// 63s with the loop pinned awake).
    pub fn confirm_dark(&mut self) {
        if let Some(confirmation) = self.pending.take() {
            confirmation.lock();
            info!("session locked (panel dark)");
        }
    }
}

impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock.manager
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // Confirming a second locker would hand the session to whoever asked last.
        // Dropping it sends `finished`.
        if self.session_lock.is_locked() {
            warn!("refusing session lock: already locked by another client");
            drop(confirmation);
            return;
        }
        info!("session lock requested");
        self.session_lock.engage(confirmation);
        // A half-finished gesture must not resume after unlock.
        self.cancel_gestures();
        self.needs_render = true;
    }

    fn unlock(&mut self) {
        info!("session unlocked");
        self.session_lock.release();
        // The lock never touched the app's commit cursor; force a full repaint or
        // the lock's pixels stay on scanout.
        self.last_present = None;
        self.needs_render = true;
    }

    fn new_surface(&mut self, surface: LockSurface, _output: WlOutput) {
        // A refused lock's object stays alive (`finished` isn't a destructor) and
        // can still make lock surfaces: a fake password prompt.
        if !self.session_lock.owns(surface.ext_session_lock()) {
            warn!("ignoring lock surface from a lock we did not grant");
            return;
        }
        // So the client learns the scale and renders HiDPI.
        self.output.enter(surface.wl_surface());
        // Logical size; the client scales by `dpi`.
        let (w, h) = (self.panel_size.0 as f64, self.panel_size.1 as f64);
        let size = ((w / self.dpi).round() as u32, (h / self.dpi).round() as u32);
        surface.with_pending_state(|state| {
            state.size = Some(size.into());
        });
        surface.send_configure();
        // Nothing else will redraw after a lock client dies; without this its last
        // frame stays on screen.
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

    #[test]
    fn locked_without_surface_draws_black() {
        assert_eq!(lock_view(true, false), LockView::Blank);
    }
}
