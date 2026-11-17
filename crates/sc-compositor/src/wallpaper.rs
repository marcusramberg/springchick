//! `[main] wallpaper`: one image behind Home, decoded by Skia and drawn as a
//! GLES texture in the background pass.

use std::path::{Path, PathBuf};

use skia_safe::{
    AlphaType, ColorType, Data, FilterMode, Image, ImageInfo, MipmapMode, Pixmap, SamplingOptions,
};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::ImportMem;
use smithay::utils::{Buffer, Physical, Rectangle, Size};
use tracing::warn;

#[derive(Default)]
pub struct Wallpaper {
    /// Taken by the first load, so a bad file is tried once.
    path: Option<PathBuf>,
    texture: Option<GlesTexture>,
}

impl Wallpaper {
    pub fn new(path: Option<PathBuf>) -> Self {
        let path = path.map(|p| match (p.strip_prefix("~"), std::env::var_os("HOME")) {
            (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
            _ => p,
        });
        Self {
            path,
            texture: None,
        }
    }

    pub fn load(&mut self, renderer: &mut GlesRenderer, screen: Size<i32, Physical>) {
        let Some(path) = self.path.take() else {
            return;
        };
        self.texture = load(renderer, &path, screen)
            .map_err(|e| warn!(path = %path.display(), "wallpaper not loaded: {e}"))
            .ok();
    }

    pub fn texture(&self) -> Option<&GlesTexture> {
        self.texture.as_ref()
    }
}

fn load(
    renderer: &mut GlesRenderer,
    path: &Path,
    screen: Size<i32, Physical>,
) -> Result<GlesTexture, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let image = Image::from_encoded(Data::new_copy(&bytes)).ok_or("not a PNG or JPEG")?;
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
    fn cover_crops_the_longer_axis_centred() {
        let wide = cover_src((400, 200).into(), (100, 200).into());
        assert_eq!((wide.loc.x, wide.loc.y), (150.0, 0.0));
        assert_eq!((wide.size.w, wide.size.h), (100.0, 200.0));
        let tall = cover_src((100, 400).into(), (200, 200).into());
        assert_eq!((tall.loc.x, tall.loc.y), (0.0, 150.0));
        assert_eq!((tall.size.w, tall.size.h), (100.0, 100.0));
    }
}
