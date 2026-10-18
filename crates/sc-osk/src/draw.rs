//! Flat dark keys, drawn with Skia straight into the shm buffer.

use skia_safe::{
    surfaces, AlphaType, Color, ColorType, Font, FontMgr, FontStyle, ImageInfo, Paint, RRect, Rect,
};

use crate::grid::{Clip, Key, Placed};
use crate::osk::{Latch, Osk, POPUP_CELL};

const BG: Color = Color::from_rgb(0x1c, 0x1c, 0x1e);
const CHAR: Color = Color::from_rgb(0x6b, 0x6b, 0x70);
const FUNCTION: Color = Color::from_rgb(0x46, 0x46, 0x4a);
const PRESSED: Color = Color::from_rgb(0x9a, 0x9a, 0xa0);
const LATCHED: Color = Color::from_rgb(0xe5, 0xe5, 0xea);
const INK: Color = Color::WHITE;
const INK_ON_LATCHED: Color = Color::BLACK;
const SELECTED: Color = Color::from_rgb(0x0a, 0x84, 0xff);

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
        } else if matches!(p.key, Key::Char { .. } | Key::Text(_))
            || p.key == Key::Plain(crate::grid::code::SPACE)
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

        if let Key::Clip(clip) = p.key {
            clip_icon(canvas, clip, r, fill);
            continue;
        }
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

    let Some(popup) = &osk.popup else { return };
    let Some(key) = grid.iter().find(|p| p.key == popup.key) else {
        return;
    };
    let h = key.rect.h - 8.0;
    let y = (key.rect.y - h - 4.0).max(0.0);
    let panel = Rect::from_xywh(popup.x0, y, popup.options.len() as f32 * POPUP_CELL, h);
    paint.set_color(LATCHED);
    canvas.draw_rrect(RRect::new_rect_xy(panel, 8.0, 8.0), &paint);
    font.set_size(h * 0.5);
    let cap = font.metrics().1.cap_height;
    for (i, option) in popup.options.iter().enumerate() {
        let cell = Rect::from_xywh(popup.x0 + i as f32 * POPUP_CELL, y, POPUP_CELL, h);
        if i == popup.selected {
            paint.set_color(SELECTED);
            canvas.draw_rrect(
                RRect::new_rect_xy(cell.with_inset((2.0, 2.0)), 6.0, 6.0),
                &paint,
            );
        }
        paint.set_color(if i == popup.selected {
            INK
        } else {
            INK_ON_LATCHED
        });
        let (width, _) = font.measure_str(option, Some(&paint));
        canvas.draw_str(
            option,
            (cell.center_x() - width / 2.0, cell.center_y() + cap / 2.0),
            font,
            &paint,
        );
    }
}

/// Scissors, two sheets, a clipboard: narrower than the words.
fn clip_icon(canvas: &skia_safe::Canvas, clip: Clip, r: Rect, fill: Color) {
    let s = r.width().min(r.height()) * 0.5;
    let (cx, cy) = (r.center_x(), r.center_y());
    let mut stroke = Paint::default();
    stroke.set_anti_alias(true);
    stroke.set_color(INK);
    stroke.set_style(skia_safe::paint::Style::Stroke);
    stroke.set_stroke_width((s * 0.09).max(1.2));
    stroke.set_stroke_cap(skia_safe::paint::Cap::Round);
    match clip {
        Clip::Cut => {
            for side in [-1.0, 1.0] {
                let ring = (cx + side * s * 0.24, cy + s * 0.3);
                canvas.draw_circle(ring, s * 0.15, &stroke);
                canvas.draw_line(
                    (ring.0 - side * s * 0.08, ring.1 - s * 0.13),
                    (cx - side * s * 0.26, cy - s * 0.48),
                    &stroke,
                );
            }
        }
        Clip::Copy => {
            let back = Rect::from_xywh(cx - s * 0.42, cy - s * 0.48, s * 0.6, s * 0.72);
            let front = back.with_offset((s * 0.24, s * 0.24));
            canvas.draw_rrect(RRect::new_rect_xy(back, s * 0.1, s * 0.1), &stroke);
            let mut cover = Paint::default();
            cover.set_anti_alias(true);
            cover.set_color(fill);
            canvas.draw_rrect(RRect::new_rect_xy(front, s * 0.1, s * 0.1), &cover);
            canvas.draw_rrect(RRect::new_rect_xy(front, s * 0.1, s * 0.1), &stroke);
        }
        Clip::Paste => {
            let board = Rect::from_xywh(cx - s * 0.36, cy - s * 0.4, s * 0.72, s * 0.9);
            canvas.draw_rrect(RRect::new_rect_xy(board, s * 0.1, s * 0.1), &stroke);
            let mut tab = Paint::default();
            tab.set_anti_alias(true);
            tab.set_color(INK);
            let clip = Rect::from_xywh(cx - s * 0.2, cy - s * 0.52, s * 0.4, s * 0.2);
            canvas.draw_rrect(RRect::new_rect_xy(clip, s * 0.06, s * 0.06), &tab);
        }
    }
}
