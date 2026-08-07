//! wlr-screencopy-unstable-v1: screen capture for the pre-`ext` tool ecosystem.
//!
//! springchick's primary capture protocol is `ext-image-copy-capture-v1` (see
//! [`crate::capture`] and the handlers in [`crate::handlers`]). This module adds
//! the older wlr protocol *as well*, because the tools that matter on this
//! device only speak it: `wf-recorder` (the one recorder that reaches the FP5's
//! v4l2 h264 encoder without VAAPI), OBS's wlrobs, and xdg-desktop-portal-wlr.
//!
//! smithay ships no handler for it, so the two interfaces are wired by hand
//! here, like [`crate::gamma_control`]. The protocol itself is much simpler than
//! `ext`: no sessions and no constraint negotiation — the compositor states the
//! buffer parameters up front, the client allocates one and asks for a copy.
//!
//! Only **shm** buffers are offered (no `linux_dmabuf` event). That is what
//! wf-recorder uses, and it keeps this path on the same readback code the `ext`
//! shm path uses. A frame is filled from the render loop, so a copy always shows
//! a complete, just-composited scene.
//!
//! Deliberate simplifications, all invisible to the tools above:
//! - `copy_with_damage` copies the next frame like `copy` does, and reports the
//!   whole captured region as damaged. Recorders use it as a "when should I grab
//!   the next frame" signal, not as a partial-update optimisation.
//! - `overlay_cursor` is ignored: this is a touch device and no cursor is ever
//!   composited.
//! - The `flags` event always reports 0 (not `y_invert`): the readback in
//!   [`crate::capture`] is already top-down.

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

/// The shm format advertised to clients. `Xrgb8888` is what every wlr recorder
/// handles, and [`crate::capture`] can read the framebuffer back into it.
const FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;

/// Per-frame-object state. One `zwlr_screencopy_frame_v1` may be copied once.
pub struct FrameData {
    inner: Mutex<FrameInner>,
}

struct FrameInner {
    /// Region of the output this frame captures, in physical pixels.
    region: Rectangle<i32, BufferCoord>,
    /// Set once a copy request has been accepted — a second one is a protocol
    /// error (`already_used`).
    used: bool,
}

/// A copy request accepted by the protocol and awaiting the render loop.
pub struct PendingCopy {
    obj: ZwlrScreencopyFrameV1,
    /// The client's shm buffer, already validated against `region`.
    pub buffer: WlBuffer,
    /// Where in the output to read from, in physical pixels.
    pub region: Rectangle<i32, BufferCoord>,
    /// `copy_with_damage` was used, so a `damage` event is owed before `ready`.
    with_damage: bool,
}

impl PendingCopy {
    /// Report a completed copy: `flags`, the optional `damage`, then `ready`.
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

    /// Report a failed copy. The client is expected to destroy the frame.
    pub fn failed(self) {
        if self.obj.is_alive() {
            self.obj.failed();
        }
    }

    /// The shm buffer geometry to read back into.
    pub fn target(&self) -> ShmTarget {
        ShmTarget {
            size: self.region.size,
            stride: self.region.size.w * 4,
            fourcc: smithay::backend::allocator::Fourcc::Xrgb8888,
        }
    }
}

/// Advertise `zwlr_screencopy_manager_v1`.
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
                // Region arrives in output *logical* coordinates (xdg_output),
                // and the output is scaled by `dpi`; the framebuffer we read
                // back is physical, so scale before clipping.
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

        // An empty region (fully off-output, or a zero-size request) can never
        // produce a buffer, so fail the frame instead of advertising one.
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
        // shm only — no `linux_dmabuf` event, see the module docs.
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

        // The buffer must match what we advertised, exactly.
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
        // The copy is served from the next composited frame, so make sure one
        // happens even if the screen is otherwise idle.
        state.needs_render = true;
    }
}

/// Validate a client buffer against the parameters advertised for the frame.
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
    /// The whole output, in the physical pixels a capture reads back.
    fn output_rect(&self) -> Rectangle<i32, BufferCoord> {
        Rectangle::from_size((self.output_size.0, self.output_size.1).into())
    }
}
