//! Screencopy (`ext-image-copy-capture-v1`) buffers. dmabuf is the fast path,
//! but grim and most tools only allocate shm; for those the scene is drawn
//! offscreen and read back into the client's pool.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Color32F, ExportMem, Frame, Offscreen, Renderer, RendererSuper};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::shm;

use crate::rotation::Rotation;
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

/// Blit a panel-space scene into `fb`, undoing the view rotation and
/// aspect-fitting into `dst_size`. Shared by external mirroring (where the
/// external panel is not being held sideways) and screen capture (where
/// clients are owed pixels in the space the output advertises). An exact
/// aspect match fills; anything else letterboxes on black.
pub fn blit_upright(
    renderer: &mut GlesRenderer,
    fb: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    dst_size: Size<i32, Physical>,
    src: &GlesTexture,
    src_size: Size<i32, Physical>,
    rotation: Rotation,
) -> Result<(), smithay::backend::renderer::gles::GlesError> {
    let full = Rectangle::from_size(dst_size);
    let dst = dst_rect(src_size, dst_size, rotation);
    let mut frame = renderer.render(fb, dst_size, Transform::Normal)?;
    frame.clear(Color32F::new(0.0, 0.0, 0.0, 1.0), &[full])?;
    frame.render_texture_from_to(
        src,
        Rectangle::from_size((src_size.w as f64, src_size.h as f64).into()),
        dst,
        &[full],
        &[],
        src_transform(rotation),
        1.0,
        None,
        &[],
    )?;
    let _sync = frame.finish()?;
    Ok(())
}

/// Inverse of the primary's transform: that one is a frame transform, this is
/// a source transform that gets inverted again. The same value in both places
/// turns the image 180°.
fn src_transform(rotation: Rotation) -> Transform {
    rotation.transform().invert()
}

/// A rotated scene in a portrait buffer reads as landscape once un-rotated,
/// and that is what gets aspect-fit.
pub fn dst_rect(
    src: Size<i32, Physical>,
    dst: Size<i32, Physical>,
    rotation: Rotation,
) -> Rectangle<i32, Physical> {
    let shown: Size<i32, Physical> = rotation.app_size((src.w, src.h)).into();
    fit(shown, dst)
}

/// Degenerate sizes give an empty rect.
fn fit(src: Size<i32, Physical>, dst: Size<i32, Physical>) -> Rectangle<i32, Physical> {
    if src.w <= 0 || src.h <= 0 || dst.w <= 0 || dst.h <= 0 {
        return Rectangle::new(Point::from((0, 0)), Size::from((0, 0)));
    }
    let scale = f64::min(
        f64::from(dst.w) / f64::from(src.w),
        f64::from(dst.h) / f64::from(src.h),
    );
    let w = (f64::from(src.w) * scale).round() as i32;
    let h = (f64::from(src.h) * scale).round() as i32;
    Rectangle::new(
        Point::from(((dst.w - w) / 2, (dst.h - h) / 2)),
        Size::from((w, h)),
    )
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
    for px in out.as_chunks_mut::<4>().0 {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn same_aspect_fills() {
        assert_eq!(
            fit((1080, 2160).into(), (1080, 2160).into()),
            r(0, 0, 1080, 2160)
        );
        assert_eq!(
            fit((1080, 2160).into(), (540, 1080).into()),
            r(0, 0, 540, 1080)
        );
    }

    #[test]
    fn portrait_into_landscape_pillarboxes() {
        let got = fit((1080, 2160).into(), (1920, 1080).into());
        assert_eq!(got, r(690, 0, 540, 1080));
    }

    #[test]
    fn landscape_into_portrait_letterboxes() {
        let got = fit((1920, 1080).into(), (1080, 1920).into());
        assert_eq!(got, r(0, 656, 1080, 608));
    }

    #[test]
    fn unrotated_phone_pillarboxes_on_a_tv() {
        assert_eq!(
            dst_rect((1080, 2160).into(), (1920, 1080).into(), Rotation::None),
            r(690, 0, 540, 1080)
        );
    }

    #[test]
    fn rotated_app_fills_a_landscape_tv() {
        for rot in [Rotation::LeftUp, Rotation::RightUp] {
            assert_eq!(
                dst_rect((1080, 2160).into(), (1920, 1080).into(), rot),
                r(0, 60, 1920, 960)
            );
        }
    }

    #[test]
    fn the_blit_undoes_the_primarys_turn() {
        for rot in [Rotation::None, Rotation::LeftUp, Rotation::RightUp] {
            assert_eq!(src_transform(rot), rot.transform().invert());
        }
        assert_eq!(src_transform(Rotation::None), Transform::Normal);
        assert_eq!(src_transform(Rotation::LeftUp), Transform::_90);
        assert_eq!(src_transform(Rotation::RightUp), Transform::_270);
    }

    #[test]
    fn a_turned_capture_fills_its_view_sized_target() {
        // What `capture_scene` asks for: the view is the panel's own size
        // swapped, so the upright blit must fill it with no letterbox.
        for rot in [Rotation::LeftUp, Rotation::RightUp] {
            assert_eq!(
                dst_rect((1224, 2700).into(), (2700, 1224).into(), rot),
                r(0, 0, 2700, 1224)
            );
        }
        assert_eq!(
            dst_rect((1224, 2700).into(), (1224, 2700).into(), Rotation::None),
            r(0, 0, 1224, 2700)
        );
    }

    #[test]
    fn degenerate_sizes_are_empty() {
        assert_eq!(fit((0, 0).into(), (1920, 1080).into()), r(0, 0, 0, 0));
        assert_eq!(fit((1080, 2160).into(), (0, 1080).into()), r(0, 0, 0, 0));
    }
}
