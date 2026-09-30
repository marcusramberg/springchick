//! zwp_idle_inhibit_manager_v1. An inhibitor counts only while its surface is
//! visible: the foreground app's surface, or any mapped layer surface. The
//! layer case matters: dms relays D-Bus screensaver inhibits (Electron video
//! wake locks) onto its own layer surface.

use std::collections::HashSet;
use std::hash::Hash;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::IsAlive;
use smithay::wayland::idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState};

use crate::State;

pub struct IdleInhibit {
    surfaces: HashSet<WlSurface>,
    #[allow(dead_code)]
    manager: IdleInhibitManagerState,
}

impl IdleInhibit {
    pub fn new(dh: &DisplayHandle) -> Self {
        IdleInhibit {
            surfaces: HashSet::new(),
            manager: IdleInhibitManagerState::new::<State>(dh),
        }
    }

    /// Also prunes dead surfaces: `uninhibit` only fires on explicit destroy, so
    /// a crashed client would stay in the set forever.
    pub fn is_inhibited(
        &mut self,
        visible: Option<&WlSurface>,
        mapped_layer: impl FnMut(&WlSurface) -> bool,
    ) -> bool {
        self.surfaces.retain(|s| s.alive());
        inhibited_by(&self.surfaces, visible, mapped_layer)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&'static str]) -> HashSet<&'static str> {
        items.iter().copied().collect()
    }

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

    /// dms relays a D-Bus screensaver inhibit onto its bar layer surface.
    #[test]
    fn mapped_layer_inhibitor_counts() {
        assert!(inhibited_by(&set(&["bar"]), Some(&"app"), mapped_layer));
    }

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
