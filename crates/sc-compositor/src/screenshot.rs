//! Native screenshot: grab the composited frame, PNG-encode it, own the
//! clipboard offer so the next paste yields the image.
//!
//! The pixels are captured by the backends (they own the renderer) through the
//! same offscreen-draw path screencopy uses; everything after that is here.

use std::io::Write;
use std::sync::Arc;

use smithay::utils::{Buffer, Size};
use smithay::wayland::selection::data_device::set_data_device_selection;
use std::os::fd::OwnedFd;
use tracing::{info, warn};

pub const MIME: &str = "image/png";

/// Encode tightly-packed RGBA into PNG.
pub fn encode_png(rgba: &[u8], size: Size<i32, Buffer>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let res = (|| -> Result<(), png::EncodingError> {
        let mut enc = png::Encoder::new(&mut out, size.w as u32, size.h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(rgba)
    })();
    match res {
        Ok(()) => Some(out),
        Err(e) => {
            warn!("screenshot: png encode failed: {e}");
            None
        }
    }
}

/// Encode and take ownership of the clipboard. The bytes live in the seat's
/// selection user data until some client sets a selection of its own.
pub fn to_clipboard(state: &mut crate::State, rgba: &[u8], size: Size<i32, Buffer>) {
    let Some(png) = encode_png(rgba, size) else {
        return;
    };
    info!(
        bytes = png.len(),
        w = size.w,
        h = size.h,
        "screenshot copied"
    );
    let dh = state.dh.clone();
    let seat = state.seat.clone();
    set_data_device_selection(&dh, &seat, vec![MIME.to_string()], Arc::new(png));
}

/// Serve a paste of our selection. A screenshot dwarfs the 64K pipe buffer, so
/// the write has to happen off the compositor thread or the pasting client
/// deadlocks us.
pub fn serve(fd: OwnedFd, data: Arc<Vec<u8>>) {
    std::thread::spawn(move || {
        if let Err(e) = std::fs::File::from(fd).write_all(&data) {
            warn!("screenshot: clipboard write failed: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_png() {
        let size: Size<i32, Buffer> = (2, 2).into();
        let png = encode_png(&[0xff; 16], size).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn rejects_short_input() {
        let size: Size<i32, Buffer> = (2, 2).into();
        assert!(encode_png(&[0xff; 4], size).is_none());
    }
}
