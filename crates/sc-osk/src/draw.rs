//! Flat dark keys, drawn with Skia straight into the shm buffer.

use skia_safe::{
    surfaces, AlphaType, Color, ColorType, Font, FontMgr, FontStyle, ImageInfo, Paint, RRect, Rect,
};

use crate::emoji::Picker;
use crate::grid::{Clip, Key, Page, Placed};
use crate::osk::{Latch, Osk, POPUP_CELL};

const BG: Color = Color::from_rgb(0x1c, 0x1c, 0x1e);
const CHAR: Color = Color::from_rgb(0x6b, 0x6b, 0x70);
const FUNCTION: Color = Color::from_rgb(0x46, 0x46, 0x4a);
const PRESSED: Color = Color::from_rgb(0x9a, 0x9a, 0xa0);
const LATCHED: Color = Color::from_rgb(0xe5, 0xe5, 0xea);
const INK: Color = Color::WHITE;
const INK_ON_LATCHED: Color = Color::BLACK;
const SELECTED: Color = Color::from_rgb(0x0a, 0x84, 0xff);

pub struct Fonts {
    text: Font,
    /// A colour emoji font; emoji show as boxes without one.
    emoji: Option<Font>,
    /// The same font for shaping: ZWJ sequences and flags are ligatures,
    /// which `draw_str` leaves as separate glyphs. Built on the first emoji
    /// drawn: `to_font_data` copies the whole colour font, tens of megabytes
    /// a keyboard that never opens the picker shouldn't hold.
    shaper: Option<rustybuzz::Face<'static>>,
    shaper_built: bool,
}

impl Fonts {
    fn build_shaper(&mut self) {
        if std::mem::replace(&mut self.shaper_built, true) {
            return;
        }
        self.shaper = self.emoji.as_ref().and_then(|f| {
            let (data, index) = f.typeface().to_font_data()?;
            // Read once for the process; the face borrows it.
            let data: &'static [u8] = Box::leak(data.into_boxed_slice());
            rustybuzz::Face::from_slice(data, index as u32)
        });
    }
}

pub fn fonts() -> Fonts {
    let mgr = FontMgr::default();
    let typeface = mgr
        .new_from_data(sc_fonts::INTER, None)
        .or_else(|| mgr.match_family_style("sans-serif", FontStyle::normal()))
        .or_else(|| mgr.legacy_make_typeface(None, FontStyle::normal()))
        .expect("a font");
    let mut text = Font::from_typeface(typeface, 16.0);
    text.set_subpixel(true);
    // By name first: skia's bundled fontconfig rejects newer configs' `emoji`
    // alias, and matching by character then finds DejaVu's few mono emoji.
    let emoji = ["Noto Color Emoji", "Twemoji", "emoji"]
        .iter()
        .find_map(|f| mgr.match_family_style(f, FontStyle::normal()))
        .or_else(|| mgr.match_family_style_character("", FontStyle::normal(), &[], '😀' as i32))
        .map(|t| Font::from_typeface(t, 16.0));
    Fonts {
        text,
        emoji,
        shaper: None,
        shaper_built: false,
    }
}

/// `text` shaped as one cluster, centred on `center`.
fn draw_shaped(
    canvas: &skia_safe::Canvas,
    text: &str,
    center: (f32, f32),
    font: &Font,
    face: &rustybuzz::Face,
    paint: &Paint,
) {
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    let shaped = rustybuzz::shape(face, &[], buffer);
    let scale = font.size() / face.units_per_em() as f32;
    let (mut x, mut glyphs, mut positions) = (0.0, Vec::new(), Vec::new());
    for (info, p) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
        glyphs.push(info.glyph_id as u16);
        positions.push(skia_safe::Point::new(
            x + p.x_offset as f32 * scale,
            -p.y_offset as f32 * scale,
        ));
        x += p.x_advance as f32 * scale;
    }
    let cap = font.metrics().1.cap_height;
    canvas.draw_glyphs_at(
        &glyphs,
        positions.as_slice(),
        (center.0 - x / 2.0, center.1 + cap / 2.0),
        font,
        paint,
    );
}

/// `pixels`: ARGB8888 (little-endian BGRA), `w`×`h` buffer px at `scale`.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    pixels: &mut [u8],
    w: i32,
    h: i32,
    scale: f32,
    grid: &[Placed],
    osk: &Osk,
    picker: Option<&Picker>,
    strip: &[(String, sc_layout::Rect)],
    fonts: &mut Fonts,
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
    // The strip sits where the bar keys are, so they are not drawn under it.
    let bar_h = strip.first().map_or(0.0, |(_, r)| r.y + r.h);
    for p in grid.iter().filter(|p| p.rect.y >= bar_h) {
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
        let is_emoji = matches!(p.key, Key::Category(_));
        // Categories sit on the background, as on iOS, unless current.
        let current = picker.map(|pk| pk.current());
        let shown = match p.key {
            Key::Category(i) => current == Some(i as usize) || fill == PRESSED,
            _ => true,
        };
        if shown {
            paint.set_color(fill);
            canvas.draw_rrect(RRect::new_rect_xy(r, 6.0, 6.0), &paint);
        }

        if icon(canvas, p.key, r, fill) {
            continue;
        }
        let font = match (&mut fonts.emoji, is_emoji) {
            (Some(emoji), true) => emoji,
            _ => &mut fonts.text,
        };
        let single = is_emoji || p.label.chars().count() == 1;
        font.set_size(r.height() * if single { 0.5 } else { 0.32 });
        paint.set_color(if latch.on() { INK_ON_LATCHED } else { INK });
        // A variation selector (❤️) measures as width but draws nothing.
        let label = p.label.trim_end_matches('\u{fe0f}');
        let (width, _) = font.measure_str(label, Some(&paint));
        let cap = font.metrics().1.cap_height;
        canvas.draw_str(
            label,
            (r.center_x() - width / 2.0, r.center_y() + cap / 2.0),
            font,
            &paint,
        );
        if latch == Latch::Locked {
            let bar = Rect::from_xywh(r.center_x() - 6.0, r.bottom() - 6.0, 12.0, 2.0);
            canvas.draw_rect(bar, &paint);
        }
    }

    for (i, (word, r)) in strip.iter().enumerate() {
        let r = Rect::from_xywh(r.x, r.y, r.w, r.h);
        if i > 0 {
            // Hairline separators, as iOS has: no boxes, just divided space.
            paint.set_color(FUNCTION);
            canvas.draw_rect(
                Rect::from_xywh(r.left(), r.top() + r.height() * 0.2, 1.0, r.height() * 0.6),
                &paint,
            );
        }
        fonts.text.set_size(r.height() * 0.42);
        paint.set_color(INK);
        let (width, _) = fonts.text.measure_str(word, Some(&paint));
        let cap = fonts.text.metrics().1.cap_height;
        canvas.draw_str(
            word,
            (r.center_x() - width / 2.0, r.center_y() + cap / 2.0),
            &fonts.text,
            &paint,
        );
    }

    if picker.is_some() {
        fonts.build_shaper();
    }
    if let (Some(picker), Some(emoji)) = (picker, &mut fonts.emoji) {
        let a = picker.area();
        canvas.save();
        canvas.clip_rect(Rect::from_xywh(a.x, a.y, a.w, a.h), None, true);
        for (cell, e) in picker.visible() {
            emoji.set_size(cell.h * 0.62);
            if let Some(face) = &fonts.shaper {
                let center = (cell.center_x(), cell.center_y());
                draw_shaped(canvas, e, center, emoji, face, &paint);
                continue;
            }
            let e = e.trim_end_matches('\u{fe0f}');
            let (width, _) = emoji.measure_str(e, Some(&paint));
            let cap = emoji.metrics().1.cap_height;
            canvas.draw_str(
                e,
                (cell.center_x() - width / 2.0, cell.center_y() + cap / 2.0),
                emoji,
                &paint,
            );
        }
        canvas.restore();
    }

    let Some(popup) = &osk.popup else { return };
    let Some(key) = grid.iter().find(|p| p.key == popup.key) else {
        return;
    };
    let h = key.rect.h - 8.0;
    let y = (key.rect.y - h - 4.0).max(0.0);
    let panel = Rect::from_xywh(popup.x0, y, popup.options.len() as f32 * POPUP_CELL, h);
    // Key-coloured like the keys under it; a shadow lifts it off them.
    paint.set_color(Color::from_argb(140, 0, 0, 0));
    canvas.draw_rrect(
        RRect::new_rect_xy(panel.with_offset((0.0, 2.0)), 8.0, 8.0),
        &paint,
    );
    paint.set_color(CHAR);
    canvas.draw_rrect(RRect::new_rect_xy(panel, 8.0, 8.0), &paint);
    let font = &mut fonts.text;
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
        paint.set_color(INK);
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
/// Draws `key` as an icon, if it has one.
fn icon(canvas: &skia_safe::Canvas, key: Key, r: Rect, fill: Color) -> bool {
    let s = r.width().min(r.height()) * 0.5;
    let (cx, cy) = (r.center_x(), r.center_y());
    let mut stroke = Paint::default();
    stroke.set_anti_alias(true);
    stroke.set_color(INK);
    stroke.set_style(skia_safe::paint::Style::Stroke);
    stroke.set_stroke_width((s * 0.09).max(1.2));
    stroke.set_stroke_cap(skia_safe::paint::Cap::Round);
    match key {
        // A plain smiley: the colour one shouts among grey keys.
        Key::Page(Page::Emoji) => {
            canvas.draw_circle((cx, cy), s * 0.45, &stroke);
            let mut dot = stroke.clone();
            dot.set_style(skia_safe::paint::Style::Fill);
            for side in [-1.0, 1.0] {
                canvas.draw_circle((cx + side * s * 0.16, cy - s * 0.1), s * 0.06, &dot);
            }
            let smile = Rect::from_xywh(cx - s * 0.24, cy - s * 0.12, s * 0.48, s * 0.4);
            canvas.draw_arc(smile, 20.0, 140.0, false, &stroke);
        }
        // A move cross: one arrow per direction the swipe steps.
        Key::Cursor => {
            let a = s * 0.5;
            let head = s * 0.16;
            canvas.draw_line((cx - a, cy), (cx + a, cy), &stroke);
            canvas.draw_line((cx, cy - a), (cx, cy + a), &stroke);
            for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
                let tip = (cx + dx * a, cy + dy * a);
                let back = (tip.0 - dx * head, tip.1 - dy * head);
                canvas.draw_line(tip, (back.0 - dy * head, back.1 - dx * head), &stroke);
                canvas.draw_line(tip, (back.0 + dy * head, back.1 + dx * head), &stroke);
            }
        }
        // A globe: equator, one meridian, as the lang key everywhere else.
        Key::NextLayout => {
            let rr = s * 0.45;
            canvas.draw_circle((cx, cy), rr, &stroke);
            canvas.draw_line((cx - rr, cy), (cx + rr, cy), &stroke);
            canvas.draw_oval(
                Rect::from_xywh(cx - rr * 0.5, cy - rr, rr, rr * 2.0),
                &stroke,
            );
        }
        Key::Clip(Clip::Cut) => {
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
        Key::Clip(Clip::Copy) => {
            let back = Rect::from_xywh(cx - s * 0.42, cy - s * 0.48, s * 0.6, s * 0.72);
            let front = back.with_offset((s * 0.24, s * 0.24));
            canvas.draw_rrect(RRect::new_rect_xy(back, s * 0.1, s * 0.1), &stroke);
            let mut cover = Paint::default();
            cover.set_anti_alias(true);
            cover.set_color(fill);
            canvas.draw_rrect(RRect::new_rect_xy(front, s * 0.1, s * 0.1), &cover);
            canvas.draw_rrect(RRect::new_rect_xy(front, s * 0.1, s * 0.1), &stroke);
        }
        Key::Clip(Clip::Paste) => {
            let board = Rect::from_xywh(cx - s * 0.36, cy - s * 0.4, s * 0.72, s * 0.9);
            canvas.draw_rrect(RRect::new_rect_xy(board, s * 0.1, s * 0.1), &stroke);
            let mut tab = Paint::default();
            tab.set_anti_alias(true);
            tab.set_color(INK);
            let clip = Rect::from_xywh(cx - s * 0.2, cy - s * 0.52, s * 0.4, s * 0.2);
            canvas.draw_rrect(RRect::new_rect_xy(clip, s * 0.06, s * 0.06), &tab);
        }
        _ => return false,
    }
    true
}
