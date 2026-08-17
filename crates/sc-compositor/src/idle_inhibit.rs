//! zwp_idle_inhibit_manager_v1: clients holding off idle.
//!
//! A client (video player, navigation, ebook reader) creates an inhibitor for
//! one of its surfaces. While that surface is *visible*, the shell must not
//! treat the user as idle: idle-blanking is held off and inhibitor-honouring
//! ext-idle-notify clients never get `idled` (see [`crate::idle_notify`]).
//!
//! Visibility matters — an inhibitor from a backgrounded app must not keep the
//! phone awake. springchick shows one app at a time, so for apps "visible" is
//! exactly "the surface of the foreground app" ([`State::app_focus_surface`]).
//!
//! Layer surfaces count too, as long as they are mapped. A shell (dms) relays
//! inhibits it collects over D-Bus — `org.freedesktop.ScreenSaver`, which is how
//! Electron apps publish a video wake lock — onto one of its own layer
//! surfaces. Ignoring those meant a playing video let the screen blank and the
//! session lock, because the inhibit never reached the compositor's own
//! idle-blank timer or the `ext-idle-notify` clients waiting on it.
//!
//! The protocol dispatch itself is smithay's; this module holds the surface set
//! and answers the visibility question.

use std::collections::HashSet;
use std::hash::Hash;

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

    /// Whether anything visible holds an inhibitor: either `visible` (the
    /// foreground app's surface, if any) or a surface `mapped_layer` accepts.
    /// Nothing visible, or nothing inhibiting, means not inhibited.
    ///
    /// Also drops dead surfaces: the protocol only calls `uninhibit` when a
    /// client explicitly destroys its inhibitor, so a crashed client would
    /// otherwise leave its surface in the set forever.
    pub fn is_inhibited(
        &mut self,
        visible: Option<&WlSurface>,
        mapped_layer: impl FnMut(&WlSurface) -> bool,
    ) -> bool {
        self.surfaces.retain(|s| s.alive());
        inhibited_by(&self.surfaces, visible, mapped_layer)
    }
}

/// The visibility decision, over surface *keys* rather than `WlSurface` so it can
/// be tested without a Wayland client.
fn inhibited_by<S: Eq + Hash>(
    inhibiting: &HashSet<S>,
    visible: Option<&S>,
    mapped_layer: impl FnMut(&S) -> bool,
) -> bool {
    if inhibiting.is_empty() {
        return false;
    }
    if visible.is_some_and(|s| inhibiting.contains(s)) {
        return true;
    }
    inhibiting.iter().any(mapped_layer)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-ins for `WlSurface`: app surfaces ("app", "bg-app") and shell layer
    /// surfaces ("bar", "unmapped-bar").
    fn set(items: &[&'static str]) -> HashSet<&'static str> {
        items.iter().copied().collect()
    }

    /// Only "bar" is a mapped layer surface.
    fn mapped_layer(s: &&str) -> bool {
        *s == "bar"
    }

    #[test]
    fn nothing_inhibiting_is_not_inhibited() {
        assert!(!inhibited_by(&set(&[]), Some(&"app"), mapped_layer));
    }

    #[test]
    fn foreground_app_inhibitor_counts() {
        assert!(inhibited_by(&set(&["app"]), Some(&"app"), mapped_layer));
    }

    #[test]
    fn backgrounded_app_inhibitor_does_not_count() {
        assert!(!inhibited_by(&set(&["bg-app"]), Some(&"app"), mapped_layer));
    }

    /// The dms case: a shell relays a D-Bus screensaver inhibit (freetube's
    /// video wake lock) onto its bar layer surface, which is not the foreground
    /// app's surface. It must still hold off idle.
    #[test]
    fn mapped_layer_inhibitor_counts() {
        assert!(inhibited_by(&set(&["bar"]), Some(&"app"), mapped_layer));
    }

    /// …and works on home, where there is no foreground app at all.
    #[test]
    fn mapped_layer_inhibitor_counts_with_no_app() {
        assert!(inhibited_by(&set(&["bar"]), None, mapped_layer));
    }

    #[test]
    fn unmapped_layer_inhibitor_does_not_count() {
        assert!(!inhibited_by(
            &set(&["unmapped-bar"]),
            Some(&"app"),
            mapped_layer
        ));
    }

    #[test]
    fn no_foreground_app_ignores_app_inhibitors() {
        assert!(!inhibited_by(&set(&["app", "bg-app"]), None, mapped_layer));
    }
}
