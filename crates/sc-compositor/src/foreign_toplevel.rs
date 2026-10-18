//! wlr-foreign-toplevel-management-unstable-v1, wired by hand (smithay has
//! none). Diffed once per loop iteration against `toplevels` and the UI state,
//! so no lifecycle path needs a hook. `app_id` is the launch identity, not the
//! client's. Activate raises and close asks the client; minimize, maximize and
//! fullscreen requests are ignored.

use std::collections::HashMap;

use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, State as HandleState, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::state::SEARCH_APP_ID;
use crate::ui_state::{self, ToplevelId};
use crate::State;

#[derive(Default)]
pub struct ForeignToplevels {
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    known: HashMap<ToplevelId, Known>,
}

#[derive(Default)]
struct Known {
    title: String,
    app_id: String,
    activated: bool,
    /// One per manager. A handle the client destroyed stays (events to it are
    /// dropped), or the next refresh would announce the window again.
    handles: Vec<(ZwlrForeignToplevelManagerV1, ZwlrForeignToplevelHandleV1)>,
}

impl ForeignToplevels {
    pub fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<State, ZwlrForeignToplevelManagerV1, ()>(3, ());
        Self::default()
    }

    fn forget_manager(&mut self, manager: &ZwlrForeignToplevelManagerV1) {
        self.managers.retain(|m| m != manager);
        for k in self.known.values_mut() {
            k.handles.retain(|(m, _)| m != manager);
        }
    }
}

fn state_array(activated: bool) -> Vec<u8> {
    if activated {
        (HandleState::Activated as u32).to_ne_bytes().to_vec()
    } else {
        Vec::new()
    }
}

impl State {
    /// Cheap enough per loop iteration: a handful of windows.
    pub(crate) fn refresh_foreign_toplevels(&mut self) {
        if self.foreign_toplevels.managers.is_empty() {
            return;
        }
        let focus = ui_state::desired_focus(&self.ui);
        let current: Vec<(ToplevelId, String, String, bool)> = self
            .toplevels
            .iter()
            .enumerate()
            .filter_map(|(id, slot)| {
                let tl = slot.as_ref()?;
                (tl.app_id != SEARCH_APP_ID).then(|| {
                    (
                        id,
                        self.toplevel_title(id),
                        tl.app_id.clone(),
                        focus == Some(id),
                    )
                })
            })
            .collect();

        let ft = &mut self.foreign_toplevels;
        ft.known.retain(|id, k| {
            let alive = current.iter().any(|c| c.0 == *id);
            if !alive {
                for (_, h) in &k.handles {
                    h.closed();
                }
            }
            alive
        });

        for (id, title, app_id, activated) in current {
            let k = ft.known.entry(id).or_default();
            let mut changed = false;
            if k.title != title {
                k.handles.iter().for_each(|(_, h)| h.title(title.clone()));
                k.title = title;
                changed = true;
            }
            if k.app_id != app_id {
                k.handles.iter().for_each(|(_, h)| h.app_id(app_id.clone()));
                k.app_id = app_id;
                changed = true;
            }
            if k.activated != activated {
                k.handles
                    .iter()
                    .for_each(|(_, h)| h.state(state_array(activated)));
                k.activated = activated;
                changed = true;
            }
            if changed {
                k.handles.iter().for_each(|(_, h)| h.done());
            }

            for manager in &ft.managers {
                if k.handles.iter().any(|(m, _)| m == manager) {
                    continue;
                }
                let Some(client) = manager.client() else {
                    continue;
                };
                let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, State>(
                    &self.dh,
                    manager.version(),
                    id,
                ) else {
                    continue;
                };
                manager.toplevel(&handle);
                handle.title(k.title.clone());
                handle.app_id(k.app_id.clone());
                for output in self.output.client_outputs(&client) {
                    handle.output_enter(&output);
                }
                handle.state(state_array(k.activated));
                handle.done();
                k.handles.push((manager.clone(), handle));
            }
        }
    }
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn bind(
        state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        state.foreign_toplevels.managers.push(manager);
        // Now, not next iteration: clients roundtrip after binding and take
        // whatever arrived by then as the full list (lswt).
        state.refresh_foreign_toplevels();
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            state.foreign_toplevels.forget_manager(resource);
            resource.finished();
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        state.foreign_toplevels.forget_manager(resource);
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ToplevelId> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        id: &ToplevelId,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;
        match request {
            Request::Activate { .. } if !state.session_lock.is_locked() => {
                state.raise_toplevel_activated(*id);
                state.needs_render = true;
            }
            // Not `detach_toplevel`: the client may refuse (unsaved work).
            Request::Close => {
                if let Some(Some(tl)) = state.toplevels.get(*id) {
                    tl.surface.send_close();
                }
            }
            _ => {}
        }
    }
}
