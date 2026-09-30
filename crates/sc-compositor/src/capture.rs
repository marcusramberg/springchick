//! Screencopy (`ext-image-copy-capture-v1`) buffers. dmabuf is the fast path,
//! but grim and most tools only allocate shm; for those the scene is drawn
//! offscreen and read back into the client's pool.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ExportMem, Offscreen, RendererSuper};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{Rectangle, Size};
use smithay::wayland::shm;

use tracing::warn;

pub struct ShmTarget {
    pub size: Size<i32, smithay::utils::Buffer>,
    pub stride: i32,
    pub fourcc: Fourcc,
}

/// `None` for dmabuf (handled elsewhere) or unsupported formats.
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

/// `size` is the whole scene, not the client buffer: wlr-screencopy can ask
/// for a sub-region.
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

pub fn readback_rgba(
    renderer: &mut GlesRenderer,
    framebuffer: &<GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, smithay::utils::Buffer>,
) -> Option<Vec<u8>> {
    let src = Rectangle::from_size(size);
    let mapping = match renderer.copy_framebuffer(framebuffer, src, Fourcc::Xrgb8888) {
        Ok(m) => m,
        Err(e) => {
            warn!("screenshot: copy_framebuffer failed: {e}");
            return None;
        }
    };
    let mut out = match renderer.map_texture(&mapping) {
        Ok(p) => p.to_vec(),
        Err(e) => {
            warn!("screenshot: map_texture failed: {e}");
            return None;
        }
    };
    // Xrgb8888 is B,G,R,X in memory; PNG wants R,G,B,A.
    for px in out.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 0xff;
    }
    Some(out)
}

/// `framebuffer` must be bound to the [`offscreen`] texture with the scene
/// drawn. `src` is sized like the client buffer; row 0 is the image top.
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

    // The client's stride may be wider than tightly packed rows.
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
            // SAFETY: `ptr`/`len` are the client's mapped pool for this closure and the
            // bounds check above keeps writes inside it.
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
