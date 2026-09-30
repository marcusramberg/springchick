//! wlr-output-power-management-unstable-v1, wired by hand (smithay has no
//! handler). Modes map onto [`crate::blank::Blank`]. Exclusive per output: a
//! second client gets `failed` and an inert object.

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, Mode, ZwlrOutputPowerV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};

use crate::State;

pub struct OutputPower {
    active: Option<ZwlrOutputPowerV1>,
    /// Last mode sent to `active`, so client-requested changes aren't echoed
    /// twice.
    announced: Option<bool>,
}

impl OutputPower {
    pub fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<State, ZwlrOutputPowerManagerV1, ()>(1, ());
        OutputPower {
            active: None,
            announced: None,
        }
    }

    /// Tell the client about blank changes it didn't cause (power key, idle).
    pub fn sync(&mut self, blanked: bool) {
        if self.announced == Some(blanked) {
            return;
        }
        self.announce(blanked);
    }

    /// Always answer `set_mode`, even a no-op: clients block on the reply (dms
    /// waits 10s).
    fn announce(&mut self, blanked: bool) {
        let Some(control) = self.active.as_ref() else {
            self.announced = None;
            return;
        };
        control.mode(if blanked { Mode::Off } else { Mode::On });
        self.announced = Some(blanked);
    }
}

impl GlobalDispatch<ZwlrOutputPowerManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } => {
                let control = data_init.init(id, ());
                let ours = Output::from_resource(&output).is_some_and(|o| o == state.output);
                if !ours || state.output_power.active.is_some() {
                    control.failed();
                    return;
                }
                let blanked = state.blank.is_blanked();
                control.mode(if blanked { Mode::Off } else { Mode::On });
                state.output_power.active = Some(control);
                state.output_power.announced = Some(blanked);
            }
            zwlr_output_power_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputPowerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_power_v1::Request::SetMode { mode } => {
                if state.output_power.active.as_ref() != Some(resource) {
                    return;
                }
                let blanked = match mode {
                    WEnum::Value(Mode::Off) => true,
                    WEnum::Value(Mode::On) => false,
                    _ => {
                        resource.post_error(
                            zwlr_output_power_v1::Error::InvalidMode,
                            "invalid power mode",
                        );
                        return;
                    }
                };
                state.blank.set(blanked);
                // The DRM loop only renders when it has a reason to.
                if !blanked {
                    state.needs_render = true;
                }
                state.output_power.announce(blanked);
            }
            zwlr_output_power_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &ZwlrOutputPowerV1, _data: &()) {
        // No restore semantics: the panel stays as it is.
        if state.output_power.active.as_ref() == Some(resource) {
            state.output_power.active = None;
            state.output_power.announced = None;
        }
    }
}
