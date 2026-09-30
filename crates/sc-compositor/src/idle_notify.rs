//! ext-idle-notify-v1, wired by hand: smithay's handler needs a
//! `LoopHandle<'static, State>` neither backend has, so timeouts are polled
//! from [`IdleNotify::refresh`] each loop iteration. v2's
//! `get_input_idle_notification` ignores idle inhibitors.

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

/// Shared between the resource's user data (for `destroyed`) and the list.
#[derive(Debug)]
pub struct NotificationData {
    timeout: Duration,
    idled: AtomicBool,
    ignore_inhibitor: bool,
}

pub struct IdleNotify {
    notifications: Vec<(ExtIdleNotificationV1, Arc<NotificationData>)>,
    /// Seeded at construction so a startup notification waits a full timeout.
    last_activity: Instant,
}

impl IdleNotify {
    pub fn new(dh: &DisplayHandle, now: Instant) -> Self {
        dh.create_global::<State, ExtIdleNotifierV1, ()>(2, ());
        IdleNotify {
            notifications: Vec::new(),
            last_activity: now,
        }
    }

    pub fn activity(&mut self, now: Instant) {
        self.last_activity = now;
        for (resource, data) in &self.notifications {
            if data.idled.swap(false, Ordering::AcqRel) {
                resource.resumed();
            }
        }
    }

    /// `inhibited`: a visible surface holds an idle inhibitor. That counts as
    /// input for inhibitor-honouring notifications.
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
        // One seat; `seat` is ignored.
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
