//! `wp_fifo` and `wp_commit_timing`. smithay implements the protocols; we
//! release what it holds once a surface's content is in a presented frame.
//! A fifo barrier that is never signalled stops the client committing, so
//! every drawn surface must go through here every frame.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{Monotonic, Time};
use smithay::wayland::commit_timing::{CommitTimerBarrierStateUserData, Timestamp};
use smithay::wayland::compositor::CompositorHandler;
use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
use smithay::wayland::fifo::FifoBarrierCachedState;

use crate::state::State;

/// Signal the fifo barriers on a surface tree. Signalled clients go into
/// `unblocked`; the waiting commit only applies after [`clear_blockers`].
pub fn signal_fifo(surface: &WlSurface, unblocked: &mut Vec<Client>) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |surf, states, &()| {
            let barrier = states
                .cached_state
                .get::<FifoBarrierCachedState>()
                .current()
                .barrier
                .take();
            if let Some(barrier) = barrier {
                barrier.signal();
                if let Some(client) = surf.client() {
                    unblocked.push(client);
                }
            }
        },
        |_, _, &()| true,
    );
}

/// Release every commit timed for `target` (the frame being composited) or
/// earlier.
pub fn signal_commit_timers(
    surface: &WlSurface,
    target: Time<Monotonic>,
    unblocked: &mut Vec<Client>,
) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |surf, states, &()| {
            if let Some(timer_state) = states.data_map.get::<CommitTimerBarrierStateUserData>() {
                let signalled = timer_state
                    .lock()
                    .unwrap()
                    .signal_until(Timestamp::from(target));
                if signalled {
                    if let Some(client) = surf.client() {
                        unblocked.push(client);
                    }
                }
            }
        },
        |_, _, &()| true,
    );
}

/// Apply the commits waiting on the barriers just signalled. Duplicate
/// clients are harmless.
pub fn clear_blockers(state: &mut State, clients: Vec<Client>) {
    if clients.is_empty() {
        return;
    }
    let dh = state.dh.clone();
    for client in &clients {
        // The reference borrows `client`, not `state`.
        let compositor_state = state.client_compositor_state(client);
        compositor_state.blocker_cleared(state, &dh);
    }
}
