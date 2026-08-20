//! wlr-output-power-management-unstable-v1: client-driven DPMS.
//!
//! Lets a shell (dms `power off monitors`, swayidle, …) blank and unblank the
//! panel. smithay ships no handler for this protocol, so both interfaces are
//! wired by hand here, the way [`crate::gamma_control`] is.
//!
//! The mode maps straight onto [`crate::blank::Blank`], so a client turning the
//! output off is the same state the power key and the idle timeout produce —
//! and any of those wake paths reports back to the client through
//! [`OutputPower::sync`]. Control is exclusive per output: a second client
//! asking for the output it already holds gets `failed` and an inert object.

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

/// Compositor-side state for the output-power protocol.
pub struct OutputPower {
    /// The control resource owning the output, if any.
    active: Option<ZwlrOutputPowerV1>,
    /// Last mode announced to `active`, so a blank change that the client asked
    /// for itself is not echoed twice and one it did not ask for still is.
    announced: Option<bool>,
}

impl OutputPower {
    /// Create the manager global.
    pub fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<State, ZwlrOutputPowerManagerV1, ()>(1, ());
        OutputPower {
            active: None,
            announced: None,
        }
    }

    /// Announce `blanked` to the controlling client when it differs from what it
    /// was last told. Call after anything that may have flipped
    /// [`crate::blank::Blank`] behind the client's back — the power key, the
    /// idle timeout.
    pub fn sync(&mut self, blanked: bool) {
        if self.announced == Some(blanked) {
            return;
        }
        self.announce(blanked);
    }

    /// Announce `blanked` whether or not it changed. A `set_mode` is always
    /// answered with a mode event, even a no-op one: clients block on that reply
    /// (dms waits 10s), so staying quiet because the panel was already in the
    /// requested state hangs them.
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
                // Unknown output (or one already under control): the object is
                // created but inert, which is what `failed` means here.
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
                // An evicted/inert control drives nothing.
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
                // Unblanking has to draw something: the DRM loop only renders
                // when it has a reason to.
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
        // Releasing control leaves the panel as it is — the protocol has no
        // restore semantics — but frees the output for the next client.
        if state.output_power.active.as_ref() == Some(resource) {
            state.output_power.active = None;
            state.output_power.announced = None;
        }
    }
}
