//! External-display mirroring (DRM). The phone panel is the only output
//! clients see; every other connector gets its own CRTC and swapchain, and
//! each frame the primary's scanout dmabuf is blitted in, letterboxed.
//!
//! The blit undoes the fullscreen-app rotation: the external panel isn't
//! being held sideways. It is [`crate::capture::blit_upright`], shared with
//! screen capture. Mirrors render when the primary does, except while the
//! phone panel is blanked, when the mirror's vblank drives the frame.

use std::error::Error;

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, GbmBufferedSurface};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{Bind, ImportDma};
use smithay::reexports::drm::control::{
    connector, crtc, Device as ControlDevice, Mode, ModeTypeFlags,
};
use smithay::utils::{Physical, Rectangle, Size};
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
        src: &GlesTexture,
        src_size: Size<i32, Physical>,
        rotation: crate::rotation::Rotation,
    ) -> Result<(), Box<dyn Error>> {
        let (mut buffer, _age) = self.surface.next_buffer()?;
        let mut fb = renderer.bind(&mut buffer)?;
        // Both sides are GBM scanout buffers, so the Y-origin conventions cancel.
        crate::capture::blit_upright(renderer, &mut fb, self.size, src, src_size, rotation)?;
        Ok(())
    }

    pub fn queue(&mut self, sync: Option<SyncPoint>) -> Result<(), Box<dyn Error>> {
        let full = Rectangle::from_size(self.size);
        self.surface.queue_buffer(sync, Some(vec![full]), ())?;
        self.pending_flip = true;
        Ok(())
    }
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
