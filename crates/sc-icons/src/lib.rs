#![forbid(unsafe_code)]

//! Icon resolution for springchick.
//!
//! Given an icon name (from a .desktop file), resolves to decoded RGBA pixels.
//! Pragmatic theme lookup + resvg for SVG + first-letter placeholder fallback.
//! Returns raw pixels — no Skia dependency; the compositor uploads to GPU.

use std::path::{Path, PathBuf};

/// Decoded icon: RGBA8 pixel data at a known size.
#[derive(Clone, Debug)]
pub struct IconPixels {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Target icon render size in logical pixels.
const TARGET_SIZE: u32 = 128;

/// Icon theme subdirectories to search under each XDG data dir's `icons/`.
const ICON_THEMES: &[&str] = &["hicolor", "Adwaita"];

/// Build the icon search directories from `$XDG_DATA_DIRS` (plus `$XDG_DATA_HOME`).
/// For each data dir `<d>` we search `<d>/icons/<theme>` and `<d>/pixmaps`.
fn theme_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for base in xdg_data_dirs() {
        for theme in ICON_THEMES {
            dirs.push(base.join("icons").join(theme));
        }
        dirs.push(base.join("pixmaps"));
    }
    dirs
}

/// XDG base data directories, highest precedence first: `$XDG_DATA_HOME`
/// (default `~/.local/share`), then each `$XDG_DATA_DIRS` entry
/// (default `/usr/local/share:/usr/share`) left-to-right.
fn xdg_data_dirs() -> Vec<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());

    let mut dirs: Vec<PathBuf> = data_home.into_iter().collect();
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

/// Size subdirectories to search, largest first.
const SIZE_SUBDIRS: &[&str] = &[
    "256x256/apps",
    "128x128/apps",
    "scalable/apps",
    "96x96/apps",
    "64x64/apps",
    "48x48/apps",
];

/// Resolve an icon name to RGBA pixels. Falls back to a placeholder on failure.
pub fn resolve(icon_name: &str) -> IconPixels {
    resolve_with_dirs(icon_name, &theme_dirs())
}

/// Resolve with custom search directories (for testing).
pub fn resolve_with_dirs<P: AsRef<Path>>(icon_name: &str, theme_dirs: &[P]) -> IconPixels {
    // If icon_name is an absolute path, try it directly.
    if icon_name.starts_with('/') {
        if let Some(pixels) = load_icon_file(Path::new(icon_name)) {
            return pixels;
        }
    }

    // Search theme directories.
    if let Some(path) = find_icon(icon_name, theme_dirs) {
        if let Some(pixels) = load_icon_file(&path) {
            return pixels;
        }
    }

    // Placeholder fallback: first letter on a colored background.
    placeholder(icon_name)
}

/// Search theme dirs for an icon file matching the name.
fn find_icon<P: AsRef<Path>>(icon_name: &str, theme_dirs: &[P]) -> Option<PathBuf> {
    for dir in theme_dirs {
        let base = dir.as_ref();

        // Check size subdirs.
        for subdir in SIZE_SUBDIRS {
            let dir_path = base.join(subdir);
            for ext in &["png", "svg"] {
                let path = dir_path.join(format!("{icon_name}.{ext}"));
                if path.exists() {
                    return Some(path);
                }
            }
        }

        // Check directly in the dir (e.g. /usr/share/pixmaps/foo.png).
        for ext in &["png", "svg", "xpm"] {
            let path = base.join(format!("{icon_name}.{ext}"));
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

/// Load and decode an icon file (PNG or SVG) to RGBA pixels.
fn load_icon_file(path: &Path) -> Option<IconPixels> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "svg" => load_svg(path),
        "png" => load_png(path),
        _ => None,
    }
}

/// Rasterize an SVG to TARGET_SIZE pixels using resvg.
fn load_svg(path: &Path) -> Option<IconPixels> {
    let data = std::fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
    let size = TARGET_SIZE;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;

    let tree_size = tree.size();
    let sx = size as f32 / tree_size.width();
    let sy = size as f32 / tree_size.height();
    let scale = sx.min(sy);
    let tx = (size as f32 - tree_size.width() * scale) / 2.0;
    let ty = (size as f32 - tree_size.height() * scale) / 2.0;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);

    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // Convert from premultiplied RGBA to straight RGBA.
    let mut pixels = pixmap.take();
    for chunk in pixels.chunks_exact_mut(4) {
        let a = chunk[3] as f32 / 255.0;
        if a > 0.0 {
            chunk[0] = (chunk[0] as f32 / a).min(255.0) as u8;
            chunk[1] = (chunk[1] as f32 / a).min(255.0) as u8;
            chunk[2] = (chunk[2] as f32 / a).min(255.0) as u8;
        }
    }

    Some(IconPixels {
        data: pixels,
        width: size,
        height: size,
    })
}

/// Load a PNG file and decode to RGBA.
fn load_png(path: &Path) -> Option<IconPixels> {
    let file = std::fs::File::open(path).ok()?;
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());

    let (width, height) = (info.width, info.height);

    // Convert to RGBA8 if needed.
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity((width * height * 4) as usize);
            for chunk in buf.chunks_exact(3) {
                out.extend_from_slice(chunk);
                out.push(255);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity((width * height * 4) as usize);
            for chunk in buf.chunks_exact(2) {
                out.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity((width * height * 4) as usize);
            for &g in &buf {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        _ => return None,
    };

    Some(IconPixels {
        data: rgba,
        width,
        height,
    })
}

/// Generate a placeholder icon: first letter of the name on a colored background.
pub fn placeholder(name: &str) -> IconPixels {
    let size = TARGET_SIZE;
    let mut data = vec![0u8; (size * size * 4) as usize];

    // Deterministic color from name hash.
    let hash = name
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    let hue = (hash % 360) as f32;
    let (r, g, b) = hsl_to_rgb(hue, 0.5, 0.45);

    // Fill background with rounded-rect-ish solid (just fill the whole square for simplicity).
    for pixel in data.chunks_exact_mut(4) {
        pixel[0] = r;
        pixel[1] = g;
        pixel[2] = b;
        pixel[3] = 255;
    }

    // Draw first letter as a simple block in the center (crude but functional).
    let letter = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .next()
        .unwrap_or('?');
    draw_letter(&mut data, size, letter);

    IconPixels {
        data,
        width: size,
        height: size,
    }
}

/// Crude letter drawing: renders a character using a built-in 5x7 bitmap font scaled up.
fn draw_letter(data: &mut [u8], size: u32, ch: char) {
    let glyph = get_glyph(ch);
    let scale = size / 10; // each font pixel = scale×scale output pixels
    let glyph_w = 5 * scale;
    let glyph_h = 7 * scale;
    let ox = (size - glyph_w) / 2;
    let oy = (size - glyph_h) / 2;

    for row in 0..7u32 {
        for col in 0..5u32 {
            if glyph[row as usize] & (1 << (4 - col)) != 0 {
                // Fill a scale×scale block.
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = ox + col * scale + dx;
                        let py = oy + row * scale + dy;
                        if px < size && py < size {
                            let idx = ((py * size + px) * 4) as usize;
                            data[idx] = 255;
                            data[idx + 1] = 255;
                            data[idx + 2] = 255;
                            data[idx + 3] = 255;
                        }
                    }
                }
            }
        }
    }
}

/// Minimal 5x7 bitmap font for A-Z and digits (enough for placeholder icons).
fn get_glyph(ch: char) -> [u8; 7] {
    match ch {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b11110, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b01110, 0b00001,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01110, 0b10001, 0b10000, 0b01110, 0b00001, 0b10001, 0b01110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
        ],
        '3' => [
            0b01110, 0b10001, 0b00001, 0b00110, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b01110, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        _ => [
            0b01110, 0b10001, 0b00010, 0b00100, 0b00100, 0b00000, 0b00100,
        ], // '?'
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (
        ((r1 + m) * 255.0) as u8,
        ((g1 + m) * 255.0) as u8,
        ((b1 + m) * 255.0) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_produces_correct_size() {
        let p = placeholder("Firefox");
        assert_eq!(p.width, TARGET_SIZE);
        assert_eq!(p.height, TARGET_SIZE);
        assert_eq!(p.data.len(), (TARGET_SIZE * TARGET_SIZE * 4) as usize);
    }

    #[test]
    fn placeholder_different_names_different_colors() {
        let p1 = placeholder("Firefox");
        let p2 = placeholder("Chrome");
        // First pixel (background color) should differ
        assert_ne!(&p1.data[..3], &p2.data[..3]);
    }

    #[test]
    fn resolve_missing_icon_returns_placeholder() {
        let p = resolve_with_dirs("nonexistent_icon_xyz", &["/tmp/no_such_dir_springchick"]);
        assert_eq!(p.width, TARGET_SIZE);
        assert_eq!(p.height, TARGET_SIZE);
    }

    #[test]
    fn find_icon_in_fixture_dir() {
        let dir = tempfile::tempdir().unwrap();
        let apps_dir = dir.path().join("256x256/apps");
        std::fs::create_dir_all(&apps_dir).unwrap();
        std::fs::write(apps_dir.join("test-app.png"), minimal_png()).unwrap();

        let dirs = [dir.path().to_str().unwrap()];
        let found = find_icon("test-app", &dirs);
        assert!(found.is_some());
        assert!(found.unwrap().ends_with("test-app.png"));
    }

    #[test]
    fn resolve_svg_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let apps_dir = dir.path().join("scalable/apps");
        std::fs::create_dir_all(&apps_dir).unwrap();
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48"><rect width="48" height="48" fill="red"/></svg>"#;
        std::fs::write(apps_dir.join("red-icon.svg"), svg).unwrap();

        let dirs = [dir.path().to_str().unwrap()];
        let p = resolve_with_dirs("red-icon", &dirs);
        assert_eq!(p.width, TARGET_SIZE);
        assert_eq!(p.height, TARGET_SIZE);
        // Should have non-zero red pixels
        let has_red = p.data.chunks_exact(4).any(|px| px[0] > 200 && px[1] < 50);
        assert!(has_red, "SVG rasterization should produce red pixels");
    }

    /// Minimal valid 1x1 red PNG.
    fn minimal_png() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut buf, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 255]).unwrap();
        }
        buf
    }
}
