//! wlr-gamma-control-unstable-v1, wired by hand (smithay has no handler).
//! DRM programs the CRTC LUT; winit accepts and ignores the ramps.

use std::io::{Read, Seek, SeekFrom};
use std::os::fd::OwnedFd;

use smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use tracing::warn;

use crate::State;

pub enum GammaUpdate {
    Set([Vec<u16>; 3]),
    /// Control released: restore the CRTC's original gamma.
    Reset,
}

pub struct GammaControl {
    /// Placeholder until the DRM backend sets the CRTC's `gamma_length`.
    pub size: u32,
    /// Exclusive owner.
    active: Option<ZwlrGammaControlV1>,
    pending: Option<GammaUpdate>,
}

impl GammaControl {
    pub fn new(dh: &DisplayHandle, size: u32) -> Self {
        dh.create_global::<State, ZwlrGammaControlManagerV1, ()>(1, ());
        GammaControl {
            size,
            active: None,
            pending: None,
        }
    }

    pub fn take_pending(&mut self) -> Option<GammaUpdate> {
        self.pending.take()
    }
}

impl GlobalDispatch<ZwlrGammaControlManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output: _ } => {
                let control = data_init.init(id, ());
                control.gamma_size(state.gamma.size);
                if let Some(old) = state.gamma.active.replace(control) {
                    old.failed();
                }
            }
            zwlr_gamma_control_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrGammaControlV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                match read_ramps(fd, state.gamma.size) {
                    Ok(ramps) => state.gamma.pending = Some(GammaUpdate::Set(ramps)),
                    Err(e) => {
                        warn!("gamma set_gamma rejected: {e}");
                        resource.post_error(
                            zwlr_gamma_control_v1::Error::InvalidGamma,
                            "invalid gamma table",
                        );
                    }
                }
            }
            zwlr_gamma_control_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &ZwlrGammaControlV1, _data: &()) {
        if state.gamma.active.as_ref() == Some(resource) {
            state.gamma.active = None;
            state.gamma.pending = Some(GammaUpdate::Reset);
        }
    }
}

/// The fd holds `3 * size` native-endian u16s (r, g, b).
fn read_ramps(fd: OwnedFd, size: u32) -> Result<[Vec<u16>; 3], String> {
    if size == 0 {
        return Err("gamma control unsupported (LUT size 0)".into());
    }
    let n = size as usize;
    let mut file = std::fs::File::from(fd);
    file.seek(SeekFrom::Start(0)).ok();
    let mut buf = vec![0u8; n * 3 * 2];
    file.read_exact(&mut buf)
        .map_err(|e| format!("read gamma fd: {e}"))?;

    let mut channels = [Vec::new(), Vec::new(), Vec::new()];
    for (c, chan) in channels.iter_mut().enumerate() {
        chan.reserve(n);
        for i in 0..n {
            let off = (c * n + i) * 2;
            chan.push(u16::from_ne_bytes([buf[off], buf[off + 1]]));
        }
    }
    Ok(channels)
}
