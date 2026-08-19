//! `wp_fifo` and `wp_commit_timing`: let a client pace itself against the panel.
//!
//! Both protocols exist because a client that just draws as fast as it can
//! wastes a phone's battery, and one that guesses the refresh interval judders.
//! They give it two tools, and smithay implements the client-facing halves —
//! what is left to us is releasing what it holds:
//!
//! - **fifo** (`wp_fifo_v1`) lets a client say "this update replaces nothing;
//!   show it for at least one refresh". smithay blocks the following commit
//!   behind a [`Barrier`](smithay::wayland::compositor::Barrier) and it stays
//!   blocked until we signal it. [`signal_fifo`] does that for every surface we
//!   drew, once its content is in a frame on the way to the panel. A client
//!   whose barrier is never signalled simply stops committing — so every drawn
//!   surface must go through here, every frame.
//!
//! - **commit-timing** (`wp_commit_timer_v1`) lets a client commit *early* and
//!   name the frame it wants the content shown on. smithay holds the commit
//!   until we release it; [`signal_commit_timers`] releases everything targeted
//!   at or before the frame we are about to present.
//!
//! Together they replace the "commit and hope" loop a video player would
//! otherwise run: it can submit a frame ahead of time, tagged for the vblank it
//! belongs on, and let the compositor land it there.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{Monotonic, Time};
use smithay::wayland::commit_timing::{CommitTimerBarrierStateUserData, Timestamp};
use smithay::wayland::compositor::CompositorHandler;
use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
use smithay::wayland::fifo::FifoBarrierCachedState;

use crate::state::State;

/// Release the fifo barrier held against a surface tree, letting the client's
/// next content update through.
///
/// Called once the surface's current content is in a frame being presented —
/// which is what "shown for at least one refresh" means from the client's side.
/// Clients whose barriers were signalled are pushed onto `unblocked`: signalling
/// only marks the blocker ready, it does not apply the commit that was waiting
/// on it. That is [`clear_blockers`], and without it the client goes quiet after
/// its first fifo frame.
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

/// Release every commit held for a frame at or before `target`.
///
/// `target` is when the frame now being composited is expected to be presented,
/// so a commit asking for *this* frame is released in time to be drawn into it.
/// Anything aimed further out stays held.
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

/// Apply the commits that were waiting on the barriers just signalled.
///
/// smithay queues a blocked commit as a transaction and only applies it when the
/// client is told its blocker cleared, so this is what actually lets a fifo or
/// commit-timing client make progress. Duplicates in `clients` are harmless —
/// the second call finds an empty transaction queue.
pub fn clear_blockers(state: &mut State, clients: Vec<Client>) {
    if clients.is_empty() {
        return;
    }
    let dh = state.dh.clone();
    for client in &clients {
        // The returned reference borrows `client`, not `state`, so the handler
        // can be handed `&mut state` on the next line.
        let compositor_state = state.client_compositor_state(client);
        compositor_state.blocker_cleared(state, &dh);
    }
}
