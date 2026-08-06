//! ext-idle-notify-v1: tell clients when the user goes idle.
//!
//! Idle daemons (swayidle and friends) create a notification with a timeout and
//! get `idled` once that much time passes without input, then `resumed` on the
//! next input event. That is how a phone shell gets "dim after 30s, blank after
//! 60s" driven from userspace instead of hard-coded here.
//!
//! smithay ships a handler for this protocol, but it drives the timeouts with
//! calloop timers keyed to a `LoopHandle<'static, State>` — which neither
//! backend has (the winit loop is a plain `while`, and the DRM loop's calloop
//! data type is `App`, not `State`). Both loops already spin at ~1ms, so the
//! two interfaces are wired by hand here and the timeouts are polled once per
//! iteration from [`IdleNotify::refresh`].
//!
//! Version 2 is advertised: `get_input_idle_notification` ignores idle
//! *inhibitors* (a media player holding the screen on via
//! `zwp_idle_inhibit_manager_v1`, see [`crate::idle_inhibit`]), where plain
//! `get_idle_notification` honours them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::reexports::wayland_protocols::ext::idle_notify::v1::server::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

use crate::State;

/// Per-notification data. Lives in the resource's user data so `destroyed` can
/// find it, and is shared with the list in [`IdleNotify`].
#[derive(Debug)]
pub struct NotificationData {
    timeout: Duration,
    /// Whether `idled` has been sent and not yet followed by `resumed`.
    idled: AtomicBool,
    /// Created with `get_input_idle_notification`: tracks raw input only, so an
    /// idle inhibitor does not hold it back.
    ignore_inhibitor: bool,
}

/// Compositor-side state for the idle-notify protocol.
pub struct IdleNotify {
    notifications: Vec<(ExtIdleNotificationV1, Arc<NotificationData>)>,
    /// Last input event. Seeded at construction so a notification created at
    /// startup waits a full timeout before firing.
    last_activity: Instant,
}

impl IdleNotify {
    /// Create the notifier global.
    pub fn new(dh: &DisplayHandle, now: Instant) -> Self {
        dh.create_global::<State, ExtIdleNotifierV1, ()>(2, ());
        IdleNotify {
            notifications: Vec::new(),
            last_activity: now,
        }
    }

    /// Record input. Any notification currently idled gets `resumed`.
    pub fn activity(&mut self, now: Instant) {
        self.last_activity = now;
        for (resource, data) in &self.notifications {
            if data.idled.swap(false, Ordering::AcqRel) {
                resource.resumed();
            }
        }
    }

    /// Poll the timeouts; sends `idled` to whatever has just crossed its own.
    /// Called once per frame-loop iteration by both backends.
    ///
    /// `inhibited` is whether a visible surface currently holds an idle
    /// inhibitor. While it does, inhibitor-honouring notifications never idle,
    /// and any that already had are resumed — the same treatment as input, so a
    /// video that starts playing pulls the screen back out of "idle".
    pub fn refresh(&mut self, now: Instant, inhibited: bool) {
        let elapsed = now.duration_since(self.last_activity);
        for (resource, data) in &self.notifications {
            if inhibited && !data.ignore_inhibitor {
                if data.idled.swap(false, Ordering::AcqRel) {
                    resource.resumed();
                }
                continue;
            }
            if elapsed >= data.timeout && !data.idled.swap(true, Ordering::AcqRel) {
                resource.idled();
            }
        }
    }
}

impl GlobalDispatch<ExtIdleNotifierV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtIdleNotifierV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtIdleNotifierV1,
        request: ext_idle_notifier_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        // `seat` is ignored: springchick has exactly one seat, and idleness is a
        // property of the whole shell here.
        let (id, timeout, ignore_inhibitor) = match request {
            ext_idle_notifier_v1::Request::GetIdleNotification { id, timeout, .. } => {
                (id, timeout, false)
            }
            ext_idle_notifier_v1::Request::GetInputIdleNotification { id, timeout, .. } => {
                (id, timeout, true)
            }
            ext_idle_notifier_v1::Request::Destroy => return,
            _ => return,
        };
        let data = Arc::new(NotificationData {
            timeout: Duration::from_millis(timeout as u64),
            idled: AtomicBool::new(false),
            ignore_inhibitor,
        });
        let resource = data_init.init(id, data.clone());
        // A zero timeout means "idle immediately"; refresh() picks that up on
        // the next iteration, so nothing special is needed here.
        state.idle_notify.notifications.push((resource, data));
    }
}

impl Dispatch<ExtIdleNotificationV1, Arc<NotificationData>> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtIdleNotificationV1,
        _request: ext_idle_notification_v1::Request,
        _data: &Arc<NotificationData>,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // The only request is `destroy`; cleanup happens in `destroyed`.
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtIdleNotificationV1,
        _data: &Arc<NotificationData>,
    ) {
        state
            .idle_notify
            .notifications
            .retain(|(r, _)| r != resource);
    }
}
