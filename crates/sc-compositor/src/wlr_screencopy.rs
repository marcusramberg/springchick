//! wlr-screencopy-unstable-v1 for wlr-era tools (wf-recorder, wlrobs,
//! xdg-desktop-portal-wlr), wired by hand. shm only, filled from the render
//! loop. `copy_with_damage` reports the whole region, `overlay_cursor` is
//! ignored, and `flags` is 0 since the readback is already top-down.

use std::sync::Mutex;
use std::time::Duration;

use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Buffer as BufferCoord, Rectangle, Size};

use tracing::warn;

use crate::capture::{self, ShmTarget};
use crate::State;

const FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;

/// A frame object may be copied once.
pub struct FrameData {
    inner: Mutex<FrameInner>,
}

struct FrameInner {
    /// Physical pixels.
    region: Rectangle<i32, BufferCoord>,
    /// A second copy is an `already_used` protocol error.
    used: bool,
}

pub struct PendingCopy {
    obj: ZwlrScreencopyFrameV1,
    /// Already validated against `region`.
    pub buffer: WlBuffer,
    /// Physical pixels.
    pub region: Rectangle<i32, BufferCoord>,
    /// A `damage` event is owed before `ready`.
    with_damage: bool,
}

impl PendingCopy {
    pub fn success(self, presented: impl Into<Duration>) {
        let presented: Duration = presented.into();
        if !self.obj.is_alive() {
            return;
        }
        self.obj.flags(zwlr_screencopy_frame_v1::Flags::empty());
        if self.with_damage {
            self.obj.damage(
                self.region.loc.x as u32,
                self.region.loc.y as u32,
                self.region.size.w as u32,
                self.region.size.h as u32,
            );
        }
        let secs = presented.as_secs();
        self.obj.ready(
            (secs >> 32) as u32,
            (secs & 0xFFFF_FFFF) as u32,
            presented.subsec_nanos(),
        );
    }

    pub fn failed(self) {
        if self.obj.is_alive() {
            self.obj.failed();
        }
    }

    pub fn target(&self) -> ShmTarget {
        ShmTarget {
            size: self.region.size,
            stride: self.region.size.w * 4,
            fourcc: smithay::backend::allocator::Fourcc::Xrgb8888,
        }
    }
}

pub fn init(dh: &DisplayHandle) {
    dh.create_global::<State, ZwlrScreencopyManagerV1, ()>(3, ());
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let (frame, region) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput { frame, .. } => {
                (frame, state.output_rect())
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                x,
                y,
                width,
                height,
                ..
            } => {
                // Logical (xdg_output) coords; the readback is physical.
                let scale = state.dpi;
                let to_phys = |v: i32| (f64::from(v) * scale).round() as i32;
                let asked = Rectangle::new(
                    (to_phys(x), to_phys(y)).into(),
                    (to_phys(width), to_phys(height)).into(),
                );
                (
                    frame,
                    asked.intersection(state.output_rect()).unwrap_or_default(),
                )
            }
            zwlr_screencopy_manager_v1::Request::Destroy => return,
            _ => return,
        };

        // An empty region can never produce a buffer.
        if region.size.w <= 0 || region.size.h <= 0 {
            let obj = data_init.init(
                frame,
                FrameData {
                    inner: Mutex::new(FrameInner {
                        region: Rectangle::default(),
                        used: true,
                    }),
                },
            );
            obj.failed();
            return;
        }

        let obj = data_init.init(
            frame,
            FrameData {
                inner: Mutex::new(FrameInner {
                    region,
                    used: false,
                }),
            },
        );
        obj.buffer(
            FORMAT,
            region.size.w as u32,
            region.size.h as u32,
            (region.size.w * 4) as u32,
        );
        if obj.version() >= 3 {
            obj.buffer_done();
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let (buffer, with_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            zwlr_screencopy_frame_v1::Request::Destroy => return,
            _ => return,
        };

        let mut inner = data.inner.lock().unwrap();
        if inner.used {
            resource.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "frame already copied",
            );
            return;
        }
        let region = inner.region;

        if let Err(e) = check_buffer(&buffer, region.size) {
            warn!("wlr-screencopy: {e}");
            resource.post_error(zwlr_screencopy_frame_v1::Error::InvalidBuffer, e);
            return;
        }
        inner.used = true;
        drop(inner);

        state.wlr_captures.push(PendingCopy {
            obj: resource.clone(),
            buffer,
            region,
            with_damage,
        });
        // Served from the next frame; make sure there is one.
        state.needs_render = true;
    }
}

fn check_buffer(buffer: &WlBuffer, size: Size<i32, BufferCoord>) -> Result<(), &'static str> {
    let Some(target) = capture::shm_target(buffer) else {
        return Err("buffer is not a supported shm buffer");
    };
    if target.size != size {
        return Err("buffer size does not match the advertised frame size");
    }
    if target.stride != size.w * 4 {
        return Err("buffer stride does not match the advertised stride");
    }
    Ok(())
}

impl State {
    fn output_rect(&self) -> Rectangle<i32, BufferCoord> {
        Rectangle::from_size((self.panel_size.0, self.panel_size.1).into())
    }
}
