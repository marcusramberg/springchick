//! External-display mirroring for the DRM backend.
//!
//! The phone panel is the *primary* output: everything — layout, input, Skia
//! chrome, the wl_output global clients see — is sized to it and unaffected by
//! what else is plugged in. Any other connected connector becomes a
//! [`MirrorOutput`]: its own CRTC, mode and GBM swapchain, but no scene of its
//! own. Each frame the primary's freshly-rendered scanout dmabuf is imported as
//! a texture and blitted into the mirror's buffer, aspect-fit and letterboxed
//! into its mode.
//!
//! Consequences of that choice, all deliberate for a first cut:
//!
//! - Clients never see a second `wl_output`, so nothing re-lays-out on hotplug.
//! - A mirror renders only when the primary does; a mirror's own vblank never
//!   drives a frame. Both panels therefore run at the primary's pace.
//! - Input from a mirror's seat is irrelevant — touch is mapped to the primary.

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

/// One non-primary connected output, mirroring the primary's scene.
pub struct MirrorOutput {
    pub connector: connector::Handle,
    pub crtc: crtc::Handle,
    /// Mode resolution — the letterbox target for the blit.
    pub size: Size<i32, Physical>,
    pub surface: GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, ()>,
    /// This connector's `DPMS` property, so blanking the phone also powers the
    /// external panel down. `None` if the driver exposes none.
    pub dpms: Option<smithay::reexports::drm::control::property::Handle>,
    /// True while a page-flip is in flight on this connector. A mirror that is
    /// still waiting is simply skipped for the frame rather than stalling the
    /// primary — an external panel at 60Hz must not drag the phone to 60.
    pub pending_flip: bool,
}

impl MirrorOutput {
    /// Modeset `conn` on `crtc` and build its swapchain.
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

    /// Blit `src` (the primary's just-composited scanout buffer) into this
    /// output's next buffer, aspect-fit into its mode.
    ///
    /// Does *not* present — the caller fences the GPU once for all mirrors and
    /// then calls [`Self::queue`], so no panel scans out a half-written buffer.
    ///
    /// Always full-damage: the source is an opaque texture whose damage we do
    /// not track, and the letterbox bars have to be cleared regardless.
    pub fn render_into(
        &mut self,
        renderer: &mut GlesRenderer,
        src: &Dmabuf,
        src_size: Size<i32, Physical>,
    ) -> Result<(), Box<dyn Error>> {
        let full = Rectangle::from_size(self.size);
        let dst = fit(src_size, self.size);

        let texture = renderer.import_dmabuf(src, None)?;
        let (mut buffer, _age) = self.surface.next_buffer()?;
        let mut fb = renderer.bind(&mut buffer)?;
        {
            // `Transform::Normal`: source and destination are both GBM scanout
            // buffers rendered through the same GLES path, so the Y-origin
            // conventions cancel and a straight copy preserves orientation.
            let mut frame = renderer.render(&mut fb, self.size, Transform::Normal)?;
            frame.clear(Color32F::new(0.0, 0.0, 0.0, 1.0), &[full])?;
            frame.render_texture_from_to(
                &texture,
                Rectangle::from_size((src_size.w as f64, src_size.h as f64).into()),
                dst,
                &[full],
                &[],
                Transform::Normal,
                1.0,
                // Default texture program, no extra uniforms: a plain copy, no
                // rounded corners or tint.
                None,
                &[],
            )?;
            let _sync = frame.finish()?;
        }
        drop(fb);
        Ok(())
    }

    /// Queue the page-flip for the buffer [`Self::render_into`] just filled.
    pub fn queue(&mut self) -> Result<(), Box<dyn Error>> {
        let full = Rectangle::from_size(self.size);
        self.surface.queue_buffer(None, Some(vec![full]), ())?;
        self.pending_flip = true;
        Ok(())
    }
}

/// Aspect-fit `src` inside `dst`, centred — the letterboxed destination rect
/// for the mirror blit. Degenerate sizes collapse to an empty rect at the
/// origin rather than dividing by zero.
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

/// A connected connector with a CRTC we can drive and the mode to drive it at.
pub struct Candidate {
    pub connector: connector::Handle,
    pub crtc: crtc::Handle,
    pub mode: Mode,
}

/// Scan every connector on the device and pair each connected one with a free
/// CRTC and its preferred mode.
///
/// `taken` seeds the set of CRTCs already in use, and `skip` the connectors
/// already driven (the primary's, plus any mirror already running), so a rescan
/// on hotplug never hands out a CRTC twice and never re-reports a connector we
/// are already scanning out to. Connectors that are connected but have no
/// reachable free CRTC are skipped with a warning — on a phone SoC there are
/// typically only two.
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

/// Whether a connector is currently reporting `Connected`. Used on hotplug to
/// decide which mirrors to tear down. A connector we can no longer query counts
/// as gone.
pub fn is_connected(drm: &DrmDevice, conn: connector::Handle) -> bool {
    drm.get_connector(conn, false)
        .map(|c| c.state() == connector::State::Connected)
        .unwrap_or(false)
}

/// Bring `mirrors` in line with what is plugged in right now: drop the ones
/// whose connector went away, add one for every newly connected connector that
/// is not the primary. Called from the udev hotplug handler.
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
        // 1080x2160 phone into a 1920x1080 TV: height-limited, bars left/right.
        let got = fit((1080, 2160).into(), (1920, 1080).into());
        assert_eq!(got, r(690, 0, 540, 1080));
    }

    #[test]
    fn landscape_into_portrait_letterboxes() {
        let got = fit((1920, 1080).into(), (1080, 1920).into());
        assert_eq!(got, r(0, 656, 1080, 608));
    }

    #[test]
    fn degenerate_sizes_are_empty() {
        assert_eq!(fit((0, 0).into(), (1920, 1080).into()), r(0, 0, 0, 0));
        assert_eq!(fit((1080, 2160).into(), (0, 1080).into()), r(0, 0, 0, 0));
    }
}
