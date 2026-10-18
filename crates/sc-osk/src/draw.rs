//! Flat dark keys, drawn with Skia straight into the shm buffer.

use skia_safe::{
    surfaces, AlphaType, Color, ColorType, Font, FontMgr, FontStyle, ImageInfo, Paint, RRect, Rect,
};

use crate::grid::{Key, Placed};
use crate::osk::{Latch, Osk};

const BG: Color = Color::from_rgb(0x1c, 0x1c, 0x1e);
const CHAR: Color = Color::from_rgb(0x6b, 0x6b, 0x70);
const FUNCTION: Color = Color::from_rgb(0x46, 0x46, 0x4a);
const PRESSED: Color = Color::from_rgb(0x9a, 0x9a, 0xa0);
const LATCHED: Color = Color::from_rgb(0xe5, 0xe5, 0xea);
const INK: Color = Color::WHITE;
const INK_ON_LATCHED: Color = Color::BLACK;

pub fn font() -> Font {
    let mgr = FontMgr::default();
    let typeface = mgr
        .match_family_style("sans-serif", FontStyle::normal())
        .or_else(|| mgr.legacy_make_typeface(None, FontStyle::normal()))
        .expect("a font");
    let mut font = Font::from_typeface(typeface, 16.0);
    font.set_subpixel(true);
    font
}

/// `pixels`: ARGB8888 (little-endian BGRA), `w`×`h` buffer px at `scale`.
pub fn draw(
    pixels: &mut [u8],
    w: i32,
    h: i32,
    scale: f32,
    grid: &[Placed],
    osk: &Osk,
    font: &mut Font,
) {
    let info = ImageInfo::new((w, h), ColorType::BGRA8888, AlphaType::Premul, None);
    let Some(mut surface) = surfaces::wrap_pixels(&info, pixels, None, None) else {
        return;
    };
    let canvas = surface.canvas();
    canvas.clear(BG);
    canvas.scale((scale, scale));

    let pressed: Vec<Key> = osk.pressed().collect();
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    for p in grid {
        let latch = match p.key {
            Key::Shift => osk.shift,
            Key::Mod(m) => osk.mod_latch(m),
            _ => Latch::Off,
        };
        let fill = if pressed.contains(&p.key) {
            PRESSED
        } else if latch.on() {
            LATCHED
        } else if matches!(p.key, Key::Char { .. }) || p.key == Key::Plain(crate::grid::code::SPACE)
        {
            CHAR
        } else {
            FUNCTION
        };
        let r = Rect::from_xywh(
            p.rect.x + 3.0,
            p.rect.y + 4.0,
            p.rect.w - 6.0,
            p.rect.h - 8.0,
        );
        paint.set_color(fill);
        canvas.draw_rrect(RRect::new_rect_xy(r, 6.0, 6.0), &paint);

        let single = p.label.chars().count() == 1;
        font.set_size(r.height() * if single { 0.5 } else { 0.32 });
        paint.set_color(if latch.on() { INK_ON_LATCHED } else { INK });
        let (width, _) = font.measure_str(&p.label, Some(&paint));
        let cap = font.metrics().1.cap_height;
        canvas.draw_str(
            &p.label,
            (r.center_x() - width / 2.0, r.center_y() + cap / 2.0),
            font,
            &paint,
        );
        if latch == Latch::Locked {
            let bar = Rect::from_xywh(r.center_x() - 6.0, r.bottom() - 6.0, 12.0, 2.0);
            canvas.draw_rect(bar, &paint);
        }
    }
}
