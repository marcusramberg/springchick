//! External-display mirroring (DRM). The phone panel is the only output
//! clients see; every other connector gets its own CRTC and swapchain, and
//! each frame the primary's scanout dmabuf is blitted in, letterboxed.
//!
//! The blit undoes the fullscreen-app rotation: the external panel isn't
//! being held sideways. Mirrors render when the primary does, except while
//! the phone panel is blanked, when the mirror's vblank drives the frame.

use std::error::Error;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, GbmBufferedSurface};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Bind, Color32F, Frame, ImportDma, Renderer};
use smithay::reexports::drm::control::{
    connector, crtc, Device as ControlDevice, Mode, ModeTypeFlags,
};
use smithay::utils::{Physical, Point, Rectangle, Size, Transform};
use tracing::{info, warn};

pub struct MirrorOutput {
    pub connector: connector::Handle,
    pub crtc: crtc::Handle,
    pub size: Size<i32, Physical>,
    pub surface: GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, ()>,
    /// Blanking the phone leaves this one on; it's driven for suspend and
    /// shutdown. `None` if the driver has no DPMS property.
    pub dpms: Option<smithay::reexports::drm::control::property::Handle>,
    /// A mirror still waiting is skipped, so a 60Hz external panel can't drag
    /// the phone to 60.
    pub pending_flip: bool,
}

impl MirrorOutput {
    pub fn new(
        drm: &mut DrmDevice,
        gbm: &GbmDevice<DrmDeviceFd>,
        renderer: &GlesRenderer,
        conn: connector::Handle,
        crtc: crtc::Handle,
        mode: Mode,
    ) -> Result<Self, Box<dyn Error>> {
        let (mw, mh) = mode.size();
        let dpms = crate::drm_backend::find_dpms_prop(drm.device_fd(), conn);
        let drm_surface = drm.create_surface(crtc, mode, &[conn])?;
        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let surface = GbmBufferedSurface::new(
            drm_surface,
            allocator,
            &[Fourcc::Argb8888, Fourcc::Xrgb8888],
            renderer.dmabuf_formats(),
        )?;
        Ok(Self {
            connector: conn,
            crtc,
            size: (mw as i32, mh as i32).into(),
            surface,
            dpms,
            pending_flip: false,
        })
    }

    /// Doesn't present: the caller fences once for all mirrors, then
    /// [`Self::queue`]s. Always full damage.
    pub fn render_into(
        &mut self,
        renderer: &mut GlesRenderer,
        src: &Dmabuf,
        src_size: Size<i32, Physical>,
        rotation: crate::rotation::Rotation,
    ) -> Result<(), Box<dyn Error>> {
        let full = Rectangle::from_size(self.size);
        let dst = dst_rect(src_size, self.size, rotation);

        let texture = renderer.import_dmabuf(src, None)?;
        let (mut buffer, _age) = self.surface.next_buffer()?;
        let mut fb = renderer.bind(&mut buffer)?;
        {
            // Both sides are GBM scanout buffers, so the Y-origin conventions cancel.
            let mut frame = renderer.render(&mut fb, self.size, Transform::Normal)?;
            frame.clear(Color32F::new(0.0, 0.0, 0.0, 1.0), &[full])?;
            frame.render_texture_from_to(
                &texture,
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
        }
        drop(fb);
        Ok(())
    }

    pub fn queue(&mut self) -> Result<(), Box<dyn Error>> {
        let full = Rectangle::from_size(self.size);
        self.surface.queue_buffer(None, Some(vec![full]), ())?;
        self.pending_flip = true;
        Ok(())
    }
}

/// Inverse of the primary's transform: that one is a frame transform, this is
/// a source transform that gets inverted again. The same value in both places
/// turns the image 180°.
fn src_transform(rotation: crate::rotation::Rotation) -> Transform {
    rotation.transform().invert()
}

/// A rotated app in a portrait buffer reads as landscape once un-rotated, and
/// that is what gets aspect-fit.
fn dst_rect(
    src: Size<i32, Physical>,
    dst: Size<i32, Physical>,
    rotation: crate::rotation::Rotation,
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

pub struct Candidate {
    pub connector: connector::Handle,
    pub crtc: crtc::Handle,
    pub mode: Mode,
}

/// `taken` CRTCs and `skip` connectors are already in use, so a hotplug rescan
/// never hands out a CRTC twice. Phones typically have only two CRTCs.
pub fn scan(
    drm: &DrmDevice,
    taken: &[crtc::Handle],
    skip: &[connector::Handle],
) -> Result<Vec<Candidate>, Box<dyn Error>> {
    let res = drm.resource_handles()?;
    let mut used: Vec<crtc::Handle> = taken.to_vec();
    let mut out = Vec::new();

    for &conn_handle in res.connectors() {
        if skip.contains(&conn_handle) {
            continue;
        }
        let conn = drm.get_connector(conn_handle, false)?;
        if conn.state() != connector::State::Connected || conn.modes().is_empty() {
            continue;
        }
        let mode = conn
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .copied()
            .unwrap_or_else(|| conn.modes()[0]);

        let crtc = conn
            .encoders()
            .iter()
            .filter_map(|&enc| drm.get_encoder(enc).ok())
            .flat_map(|enc| res.filter_crtcs(enc.possible_crtcs()))
            .find(|c| !used.contains(c));
        match crtc {
            Some(crtc) => {
                used.push(crtc);
                out.push(Candidate {
                    connector: conn_handle,
                    crtc,
                    mode,
                });
            }
            None => warn!(
                ?conn_handle,
                "connected connector has no free crtc; skipped"
            ),
        }
    }
    Ok(out)
}

/// An unqueryable connector counts as gone.
pub fn is_connected(drm: &DrmDevice, conn: connector::Handle) -> bool {
    drm.get_connector(conn, false)
        .map(|c| c.state() == connector::State::Connected)
        .unwrap_or(false)
}

/// Called from the udev hotplug handler.
pub fn refresh(
    mirrors: &mut Vec<MirrorOutput>,
    drm: &mut DrmDevice,
    gbm: &GbmDevice<DrmDeviceFd>,
    renderer: &GlesRenderer,
    primary_conn: connector::Handle,
    primary_crtc: crtc::Handle,
) {
    mirrors.retain(|m| {
        let alive = is_connected(drm, m.connector);
        if !alive {
            info!(connector = ?m.connector, "external display disconnected");
        }
        alive
    });

    let mut taken = vec![primary_crtc];
    taken.extend(mirrors.iter().map(|m| m.crtc));
    let mut skip = vec![primary_conn];
    skip.extend(mirrors.iter().map(|m| m.connector));

    let candidates = match scan(drm, &taken, &skip) {
        Ok(c) => c,
        Err(e) => {
            warn!("connector rescan failed: {e}");
            return;
        }
    };
    for cand in candidates {
        let (w, h) = cand.mode.size();
        match MirrorOutput::new(drm, gbm, renderer, cand.connector, cand.crtc, cand.mode) {
            Ok(m) => {
                info!(connector = ?cand.connector, w, h, "mirroring to external display");
                mirrors.push(m);
            }
            Err(e) => warn!(connector = ?cand.connector, "mirror setup failed: {e}"),
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
        use crate::rotation::Rotation;
        assert_eq!(
            dst_rect((1080, 2160).into(), (1920, 1080).into(), Rotation::None),
            r(690, 0, 540, 1080)
        );
    }

    #[test]
    fn rotated_app_fills_a_landscape_tv() {
        use crate::rotation::Rotation;
        for rot in [Rotation::LeftUp, Rotation::RightUp] {
            assert_eq!(
                dst_rect((1080, 2160).into(), (1920, 1080).into(), rot),
                r(0, 60, 1920, 960)
            );
        }
    }

    #[test]
    fn the_blit_undoes_the_primarys_turn() {
        use crate::rotation::Rotation;
        for rot in [Rotation::None, Rotation::LeftUp, Rotation::RightUp] {
            assert_eq!(src_transform(rot), rot.transform().invert());
        }
        assert_eq!(src_transform(Rotation::None), Transform::Normal);
        assert_eq!(src_transform(Rotation::LeftUp), Transform::_90);
        assert_eq!(src_transform(Rotation::RightUp), Transform::_270);
    }

    #[test]
    fn degenerate_sizes_are_empty() {
        assert_eq!(fit((0, 0).into(), (1920, 1080).into()), r(0, 0, 0, 0));
        assert_eq!(fit((1080, 2160).into(), (0, 1080).into()), r(0, 0, 0, 0));
    }
}
