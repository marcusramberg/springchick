//! Shared screencopy (`ext-image-copy-capture-v1`) buffer plumbing.
//!
//! Clients pick *one* of the buffer types we advertise in
//! [`crate::handlers`]'s `capture_constraints`. dmabuf is the fast path (the
//! backend blits the scene straight into the client's buffer), but the common
//! screenshot tools — grim included — only ever allocate **shm**, so a
//! dmabuf-only implementation makes `grim` fail with
//! "failed to copy output <name>" and nothing in our log.
//!
//! The shm path here renders the scene into an offscreen GLES texture, reads it
//! back with `glReadPixels` (via smithay's `ExportMem`) and memcpys it into the
//! client's pool, honouring its stride.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ExportMem, Offscreen, RendererSuper};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{Rectangle, Size};
use smithay::wayland::shm;

use tracing::warn;

/// Geometry of a client shm capture buffer.
pub struct ShmTarget {
    pub size: Size<i32, smithay::utils::Buffer>,
    pub stride: i32,
    pub fourcc: Fourcc,
}

/// Inspect a capture frame's buffer: `Some` if it is an shm buffer in a format
/// we can render into, `None` for dmabuf (handled elsewhere) or anything else.
pub fn shm_target(buffer: &WlBuffer) -> Option<ShmTarget> {
    let data = shm::with_buffer_contents(buffer, |_, _, data| data).ok()?;
    let fourcc = match data.format {
        wl_shm::Format::Xrgb8888 => Fourcc::Xrgb8888,
        wl_shm::Format::Argb8888 => Fourcc::Argb8888,
        other => {
            warn!(?other, "screencopy: unsupported shm format");
            return None;
        }
    };
    Some(ShmTarget {
        size: (data.width, data.height).into(),
        stride: data.stride,
        fourcc,
    })
}

/// Allocate the offscreen texture a scene is drawn into before readback.
///
/// `size` is the whole scene, which is not necessarily the client's buffer:
/// wlr-screencopy can ask for a sub-region, and the scene still has to be
/// composited at full output size before that region is read out of it.
pub fn offscreen(
    renderer: &mut GlesRenderer,
    fourcc: Fourcc,
    size: Size<i32, smithay::utils::Buffer>,
) -> Option<GlesTexture> {
    match Offscreen::<GlesTexture>::create_buffer(renderer, fourcc, size) {
        Ok(t) => Some(t),
        Err(e) => {
            warn!("screencopy: offscreen alloc failed: {e}");
            None
        }
    }
}

/// Read the drawn framebuffer back and copy it into the client's shm pool.
///
/// `framebuffer` must be the one bound to the texture from [`offscreen`], with
/// the scene already drawn into it. `src` is the part of it to read; its size
/// must be the client buffer's size. The framebuffer's row 0 is the top of the
/// image (the scene is drawn y-flipped, as it is for scanout), so `src.loc` is
/// measured from the top-left like every other rect in the shell.
pub fn readback_into_shm(
    renderer: &mut GlesRenderer,
    framebuffer: &<GlesRenderer as RendererSuper>::Framebuffer<'_>,
    buffer: &WlBuffer,
    target: &ShmTarget,
    src: Rectangle<i32, smithay::utils::Buffer>,
) -> bool {
    debug_assert_eq!(src.size, target.size);
    let mapping = match renderer.copy_framebuffer(framebuffer, src, target.fourcc) {
        Ok(m) => m,
        Err(e) => {
            warn!("screencopy: copy_framebuffer failed: {e}");
            return false;
        }
    };
    let pixels = match renderer.map_texture(&mapping) {
        Ok(p) => p,
        Err(e) => {
            warn!("screencopy: map_texture failed: {e}");
            return false;
        }
    };

    // `copy_framebuffer` hands back tightly packed rows; the client's pool may
    // be wider, so copy row by row rather than in one shot.
    let src_stride = (target.size.w * 4) as usize;
    let dst_stride = target.stride as usize;
    let rows = target.size.h as usize;
    let copy = src_stride.min(dst_stride);
    let res = shm::with_buffer_contents_mut(buffer, |ptr, len, data| {
        let offset = data.offset as usize;
        if offset + dst_stride * rows > len || pixels.len() < src_stride * rows {
            warn!("screencopy: shm buffer too small");
            return false;
        }
        for y in 0..rows {
            // SAFETY: `ptr`/`len` describe the client's mapped pool for the
            // duration of this closure, and the bounds check above keeps every
            // write inside `[offset, offset + dst_stride * rows)`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(y * src_stride),
                    ptr.add(offset + y * dst_stride),
                    copy,
                );
            }
        }
        true
    });
    match res {
        Ok(ok) => ok,
        Err(e) => {
            warn!("screencopy: shm access failed: {e}");
            false
        }
    }
}
