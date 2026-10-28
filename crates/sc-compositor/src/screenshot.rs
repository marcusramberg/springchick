//! Screenshot: PNG-encode the composited frame, offer it on the clipboard and
//! optionally save it to `$XDG_PICTURES_DIR/Screenshots/`.

use std::io::Write;
use std::sync::Arc;

use smithay::utils::{Buffer, Size};
use smithay::wayland::selection::data_device::set_data_device_selection;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub const MIME: &str = "image/png";

const FLASH: Duration = Duration::from_millis(900);
const SPARKLES: u32 = 28;
/// Each star's share of the flash; start times spread over the rest.
const SPARKLE_LIFE: f32 = 0.5;

/// 0..1 through the post-capture sparkle, `None` once it's over.
pub fn flash_progress(started: Option<Instant>, now: Instant) -> Option<f32> {
    let t = now.saturating_duration_since(started?).as_secs_f32() / FLASH.as_secs_f32();
    (t < 1.0).then_some(t)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sparkle {
    pub x: f32,
    pub y: f32,
    /// Tip-to-centre.
    pub size: f32,
    /// Radians.
    pub angle: f32,
    pub alpha: f32,
}

fn frac(x: f32) -> f32 {
    x - x.floor()
}

/// Stars twinkling in along the screen edge at `t` (0..1). Fixed positions
/// (golden-ratio spread), so no RNG.
pub fn sparkles(w: f32, h: f32, t: f32) -> Vec<Sparkle> {
    let margin = w.min(h) * 0.035;
    let (iw, ih) = (w - 2.0 * margin, h - 2.0 * margin);
    let perimeter = 2.0 * (iw + ih);
    (0..SPARKLES)
        .filter_map(|i| {
            let i = i as f32;
            let start = frac(i * 0.381_966) * (1.0 - SPARKLE_LIFE);
            let u = (t - start) / SPARKLE_LIFE;
            if !(0.0..=1.0).contains(&u) {
                return None;
            }
            let s = frac(i * 0.618_034 + 0.13) * perimeter;
            // Walk the inset rect clockwise from top-left; drift inward as it fades.
            let drift = u * margin;
            let (x, y) = if s < iw {
                (s, drift)
            } else if s < iw + ih {
                (iw - drift, s - iw)
            } else if s < 2.0 * iw + ih {
                (iw - (s - iw - ih), ih - drift)
            } else {
                (drift, ih - (s - 2.0 * iw - ih))
            };
            let pulse = (u * std::f32::consts::PI).sin().max(0.0);
            Some(Sparkle {
                x: margin + x,
                y: margin + y,
                size: w.min(h) * 0.03 * (0.6 + 0.8 * frac(i * 0.754_878)) * pulse,
                angle: u * std::f32::consts::FRAC_PI_2,
                alpha: pulse,
            })
        })
        .collect()
}

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

/// The bytes live in the seat's selection until a client sets its own.
pub fn to_clipboard(state: &mut crate::State, rgba: &[u8], size: Size<i32, Buffer>, flash: bool) {
    let Some(png) = encode_png(rgba, size) else {
        return;
    };
    info!(
        bytes = png.len(),
        w = size.w,
        h = size.h,
        "screenshot copied"
    );
    if flash {
        // Started after the capture, so it isn't in the shot.
        state.screenshot_flash = Some(Instant::now());
        state.needs_render = true;
    }
    let png = Arc::new(png);
    if state.save_screenshots {
        save(png.clone());
    }
    let dh = state.dh.clone();
    let seat = state.seat.clone();
    set_data_device_selection(&dh, &seat, vec![MIME.to_string()], png);
}

/// `Screenshot_2026-10-13_14-05-09.png`, local time.
fn file_name() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!(
        "Screenshot_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.png",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

fn save(png: Arc<Vec<u8>>) {
    let Some(dir) = sc_config::screenshot_dir() else {
        warn!("screenshot: HOME unset, not saving");
        return;
    };
    let path = dir.join(file_name());
    std::thread::spawn(move || {
        match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &*png)) {
            Ok(()) => info!(path = %path.display(), "screenshot saved"),
            Err(e) => warn!("screenshot: saving {} failed: {e}", path.display()),
        }
    });
}

/// A screenshot dwarfs the 64K pipe buffer; writing on the compositor thread
/// would deadlock against the pasting client.
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
    fn flash_runs_its_course_then_stops() {
        let t0 = Instant::now();
        assert_eq!(flash_progress(None, t0), None);
        assert_eq!(flash_progress(Some(t0), t0), Some(0.0));
        assert!(flash_progress(Some(t0), t0 + FLASH / 2).is_some());
        assert_eq!(flash_progress(Some(t0), t0 + FLASH), None);
    }

    #[test]
    fn sparkles_stay_on_screen_and_come_and_go() {
        let (w, h) = (1080.0, 2340.0);
        let mut seen = 0;
        for step in 0..=100 {
            let stars = sparkles(w, h, step as f32 / 100.0);
            seen = seen.max(stars.len());
            for s in stars {
                assert!(
                    (0.0..=w).contains(&s.x) && (0.0..=h).contains(&s.y),
                    "{s:?}"
                );
                assert!((0.0..=1.0).contains(&s.alpha));
            }
        }
        assert!(seen > SPARKLES as usize / 3);
        assert!(sparkles(w, h, 1.0).iter().all(|s| s.alpha < 0.05));
    }

    #[test]
    fn file_name_is_timestamped_png() {
        let name = file_name();
        assert!(
            name.starts_with("Screenshot_20") && name.ends_with(".png"),
            "{name}"
        );
        assert_eq!(name.len(), "Screenshot_2026-10-13_14-05-09.png".len());
    }

    #[test]
    fn rejects_short_input() {
        let size: Size<i32, Buffer> = (2, 2).into();
        assert!(encode_png(&[0xff; 4], size).is_none());
    }
}
