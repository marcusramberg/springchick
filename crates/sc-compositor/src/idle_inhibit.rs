//! zwp_idle_inhibit_manager_v1: clients holding off idle.
//!
//! A client (video player, navigation, ebook reader) creates an inhibitor for
//! one of its surfaces. While that surface is *visible*, the shell must not
//! treat the user as idle: idle-blanking is held off and inhibitor-honouring
//! ext-idle-notify clients never get `idled` (see [`crate::idle_notify`]).
//!
//! Visibility matters — an inhibitor from a backgrounded app must not keep the
//! phone awake. springchick shows one app at a time, so "visible" is exactly
//! "the surface of the foreground app" ([`State::app_focus_surface`]). The
//! protocol dispatch itself is smithay's; this module holds the surface set and
//! answers the visibility question.

use std::collections::HashSet;

use smithay::delegate_idle_inhibit;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::IsAlive;
use smithay::wayland::idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState};

use crate::State;

/// Surfaces currently holding an idle inhibitor.
pub struct IdleInhibit {
    surfaces: HashSet<WlSurface>,
    /// Held to keep the manager global advertised for the compositor's lifetime.
    #[allow(dead_code)]
    manager: IdleInhibitManagerState,
}

impl IdleInhibit {
    /// Create the manager global.
    pub fn new(dh: &DisplayHandle) -> Self {
        IdleInhibit {
            surfaces: HashSet::new(),
            manager: IdleInhibitManagerState::new::<State>(dh),
        }
    }

    /// Whether `visible` (the foreground app's surface, if any) holds an
    /// inhibitor. Nothing visible, or nothing inhibiting, means not inhibited.
    ///
    /// Also drops dead surfaces: the protocol only calls `uninhibit` when a
    /// client explicitly destroys its inhibitor, so a crashed client would
    /// otherwise leave its surface in the set forever.
    pub fn is_inhibited(&mut self, visible: Option<&WlSurface>) -> bool {
        self.surfaces.retain(|s| s.alive());
        visible.is_some_and(|s| self.surfaces.contains(s))
    }
}

impl IdleInhibitHandler for State {
    fn inhibit(&mut self, surface: WlSurface) {
        self.idle_inhibit.surfaces.insert(surface);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibit.surfaces.remove(&surface);
    }
}

delegate_idle_inhibit!(State);
