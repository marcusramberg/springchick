//! `[main] wallpaper`: one image behind Home, decoded by Skia and drawn as a
//! GLES texture in the background pass. `wallpaper_light` replaces it while
//! the GNOME colour scheme (what `sc-bar`'s dark-mode toggle sets) is light.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dbus::blocking::Connection;
use dbus::message::MatchRule;
use skia_safe::{
    images, AlphaType, ColorType, Data, FilterMode, Image, ImageInfo, MipmapMode, Pixmap,
    SamplingOptions,
};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::ImportMem;
use smithay::utils::{Buffer, Physical, Rectangle, Size};
use tracing::warn;

const SCHEME: [&str; 2] = ["org.gnome.desktop.interface", "color-scheme"];
const SCHEME_KEY: &str = "/org/gnome/desktop/interface/color-scheme";

#[derive(Default)]
pub struct Wallpaper {
    dark: Option<PathBuf>,
    light: Option<PathBuf>,
    /// The scheme watcher's flag; started by the first `wallpaper_light`.
    is_light: Option<Arc<AtomicBool>>,
    /// The scheme the texture was loaded for, so a bad file is tried once.
    loaded: Option<bool>,
    texture: Option<GlesTexture>,
}

impl Wallpaper {
    /// Also on reload: the files are read again.
    pub fn set(&mut self, dark: Option<PathBuf>, light: Option<PathBuf>) {
        self.dark = dark.map(expand_home);
        self.light = light.map(expand_home);
        self.loaded = None;
        if self.light.is_some() && self.is_light.is_none() {
            self.is_light = Some(watch_scheme());
        }
    }

    /// True when the texture changed.
    pub fn load(&mut self, renderer: &mut GlesRenderer, screen: Size<i32, Physical>) -> bool {
        let light = self
            .is_light
            .as_ref()
            .is_some_and(|l| l.load(Ordering::Relaxed));
        if self.loaded == Some(light) {
            return false;
        }
        self.loaded = Some(light);
        let path = self.light.as_ref().filter(|_| light).or(self.dark.as_ref());
        self.texture = path.and_then(|path| {
            load(renderer, path, screen)
                .map_err(|e| warn!(path = %path.display(), "wallpaper not loaded: {e}"))
                .ok()
        });
        true
    }

    pub fn texture(&self) -> Option<&GlesTexture> {
        self.texture.as_ref()
    }
}

fn expand_home(p: PathBuf) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => p,
    }
}

/// Anything but `prefer-dark` is light, as `sc-bar`'s toggle has it. Dark
/// where gsettings can't say.
fn read_scheme(is_light: &AtomicBool) {
    let out = Command::new("gsettings")
        .arg("get")
        .args(SCHEME)
        .stderr(Stdio::null())
        .output();
    if let Some(out) = out.ok().filter(|o| o.status.success()) {
        is_light.store(!out.stdout.starts_with(b"'prefer-dark'"), Ordering::Relaxed);
    }
}

// ponytail: nothing wakes the frame loop, so a change made while the screen
// is still lands with the next frame. A calloop channel if that shows.
fn watch_scheme() -> Arc<AtomicBool> {
    let is_light = Arc::new(AtomicBool::new(false));
    let flag = is_light.clone();
    let spawned = std::thread::Builder::new()
        .name("sc-scheme".into())
        .spawn(move || {
            let Ok(conn) = Connection::new_session() else {
                return read_scheme(&flag);
            };
            // dconf's change signal: matching it starts no service.
            let rule = MatchRule::new_signal("ca.desrt.dconf.Writer", "Notify");
            let changed = flag.clone();
            let matched = conn.add_match(
                rule,
                move |(prefix, _, _): (String, Vec<String>, String), _, _| {
                    // The key itself, or a directory holding it.
                    if SCHEME_KEY.starts_with(&prefix) {
                        read_scheme(&changed);
                    }
                    true
                },
            );
            read_scheme(&flag);
            if let Err(e) = matched {
                return warn!(%e, "colour scheme changes won't switch the wallpaper");
            }
            while conn.process(Duration::from_secs(3600)).is_ok() {}
        });
    if let Err(e) = spawned {
        warn!(%e, "could not start the colour scheme thread");
    }
    is_light
}

fn load(
    renderer: &mut GlesRenderer,
    path: &Path,
    screen: Size<i32, Physical>,
) -> Result<GlesTexture, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let image = decode(&bytes)?;
    // A camera photo at full size is past what the phone's GPU takes.
    let (w, h) = (image.width() as f32, image.height() as f32);
    let k = (screen.w as f32 / w).max(screen.h as f32 / h).min(1.0);
    let (w, h) = ((w * k).round() as i32, (h * k).round() as i32);
    let info = ImageInfo::new((w, h), ColorType::RGBA8888, AlphaType::Premul, None);
    let mut px = vec![0u8; info.compute_min_byte_size()];
    let scaled = Pixmap::new(&info, &mut px, info.min_row_bytes()).is_some_and(|pixmap| {
        let sampling = SamplingOptions::new(FilterMode::Linear, MipmapMode::Linear);
        image.scale_pixels(&pixmap, sampling, None)
    });
    if !scaled {
        return Err("decode failed".into());
    }
    renderer
        .import_memory(&px, Fourcc::Abgr8888, (w, h).into(), false)
        .map_err(|e| e.to_string())
}

fn decode(bytes: &[u8]) -> Result<Image, String> {
    if let Some(image) = Image::from_encoded(Data::new_copy(bytes)) {
        return Ok(image);
    }
    // The prebuilt Skia we link has no WebP codec.
    let mut webp = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|_| "not a PNG, JPEG or WebP")?;
    let (w, h) = webp.dimensions();
    let mut px = vec![0u8; webp.output_buffer_size().ok_or("too large")?];
    webp.read_image(&mut px).map_err(|e| e.to_string())?;
    if !webp.has_alpha() {
        px = px
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect();
    }
    let info = ImageInfo::new(
        (w as i32, h as i32),
        ColorType::RGBA8888,
        AlphaType::Unpremul,
        None,
    );
    images::raster_from_data(&info, Data::new_copy(&px), info.min_row_bytes())
        .ok_or_else(|| "decode failed".into())
}

/// The centred part of a `tex`-sized image that fills `view` at its aspect.
pub fn cover_src(tex: Size<i32, Buffer>, view: Size<i32, Physical>) -> Rectangle<f64, Buffer> {
    let (tw, th) = (tex.w as f64, tex.h as f64);
    let k = (tw / view.w as f64).min(th / view.h as f64);
    let (w, h) = (view.w as f64 * k, view.h as f64 * k);
    Rectangle::new(((tw - w) / 2.0, (th - h) / 2.0).into(), (w, h).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_webp() {
        let mut file = Vec::new();
        image_webp::WebPEncoder::new(&mut file)
            .encode(&[255, 0, 0, 0, 0, 255], 2, 1, image_webp::ColorType::Rgb8)
            .unwrap();
        let image = decode(&file).unwrap();
        assert_eq!((image.width(), image.height()), (2, 1));
        let info = ImageInfo::new((2, 1), ColorType::RGBA8888, AlphaType::Premul, None);
        let mut px = [0u8; 8];
        assert!(image.read_pixels(
            &info,
            &mut px,
            8,
            (0, 0),
            skia_safe::image::CachingHint::Disallow
        ));
        assert_eq!(px, [255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn cover_crops_the_longer_axis_centred() {
        let wide = cover_src((400, 200).into(), (100, 200).into());
        assert_eq!((wide.loc.x, wide.loc.y), (150.0, 0.0));
        assert_eq!((wide.size.w, wide.size.h), (100.0, 200.0));
        let tall = cover_src((100, 400).into(), (200, 200).into());
        assert_eq!((tall.loc.x, tall.loc.y), (0.0, 150.0));
        assert_eq!((tall.size.w, tall.size.h), (100.0, 100.0));
    }
}
