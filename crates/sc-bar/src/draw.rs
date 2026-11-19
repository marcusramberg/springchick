//! The bar and the sheet, drawn with Skia straight into the shm buffer.

use skia_safe::{
    images, surfaces, AlphaType, Canvas, ClipOp, Color, ColorType, Data, FilterMode, Font, FontMgr,
    FontStyle, Image, ImageInfo, MipmapMode, Paint, RRect, Rect, SamplingOptions,
};

use crate::center::{self, Center, Notification, Row, View};
use crate::lock::{self, Key, Lock};
use crate::panel::{
    self, Control, Grid, Look, MediaAction, Page, Panel, PowerAction, Slider, Toggle, BAR_H,
};
use crate::symbols::Symbol;
use crate::system::System;
use sc_config::style::{radius, wash};

/// An ON tile, and the filled part of a slider.
const TILE_ON: f32 = 0.92;
/// From the screen's edge to the clock and the battery.
const PAD: f32 = 20.0;

fn rgba(rgb: u32, alpha: f32) -> Color {
    Color::from(((alpha.clamp(0.0, 1.0) * 255.0) as u32) << 24 | rgb)
}

fn fill(color: Color) -> Paint {
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_color(color);
    paint
}

fn skia(r: sc_layout::Rect) -> Rect {
    Rect::from_xywh(r.x, r.y, r.w, r.h)
}

pub fn font() -> Font {
    let mgr = FontMgr::default();
    let typeface = mgr
        .new_from_data(sc_fonts::INTER, None)
        .or_else(|| mgr.match_family_style("sans-serif", FontStyle::normal()))
        .or_else(|| mgr.legacy_make_typeface(None, FontStyle::normal()))
        .expect("a font");
    let mut font = Font::from_typeface(typeface, 14.0);
    font.set_subpixel(true);
    font
}

pub struct Scene<'a> {
    pub font: &'a mut Font,
    pub theme: &'a sc_config::Theme,
    pub system: &'a System,
    pub clock: &'a str,
    /// The shortcuts' icons, by index.
    pub icons: &'a [Image],
}

/// An app icon as `sc-icons` hands it over: straight RGBA.
pub fn icon(px: &sc_icons::IconPixels) -> Option<Image> {
    let info = ImageInfo::new(
        (px.width as i32, px.height as i32),
        ColorType::RGBA8888,
        AlphaType::Unpremul,
        None,
    );
    let image = images::raster_from_data(&info, Data::new_copy(&px.data), px.width as usize * 4)?;
    // Cover art comes at any size, and is kept.
    const KEPT: i32 = 192;
    if image.width().max(image.height()) <= KEPT {
        return Some(image);
    }
    let mut small = skia_safe::surfaces::raster_n32_premul((KEPT, KEPT))?;
    small.canvas().draw_image_rect_with_sampling_options(
        &image,
        None,
        Rect::from_wh(KEPT as f32, KEPT as f32),
        skia_safe::SamplingOptions::from(skia_safe::FilterMode::Linear),
        &Paint::default(),
    );
    Some(small.image_snapshot())
}

/// `x` is the left edge, or the right one when `end`; `y` the vertical centre.
/// Cut short with an ellipsis past `max` wide. Returns the width drawn.
#[allow(clippy::too_many_arguments)]
fn text(
    canvas: &Canvas,
    font: &mut Font,
    s: &str,
    (x, y): (f32, f32),
    size: f32,
    color: Color,
    end: bool,
    max: f32,
) -> f32 {
    font.set_size(size);
    let paint = fill(color);
    let measure = |parts: &[(String, Font)]| -> f32 {
        parts.iter().map(|(t, f)| f.measure_str(t, None).0).sum()
    };
    // ponytail: one pass per character cut; fine for a line, not for a page.
    let mut s: String = s.chars().take(256).collect();
    let mut parts = runs(font, &s);
    let mut width = measure(&parts);
    while width > max && s.chars().count() > 1 {
        s = s.trim_end_matches('…').to_string();
        s.pop();
        s.push('…');
        parts = runs(font, &s);
        width = measure(&parts);
    }
    let cap = font.metrics().1.cap_height;
    let mut at = if end { x - width } else { x };
    for (part, font) in &parts {
        canvas.draw_str(part, (at, y + cap / 2.0), font, &paint);
        at += font.measure_str(part, None).0;
    }
    width
}

/// `body` broken into the lines of a card, on spaces where there are any.
pub fn wrap(font: &mut Font, body: &str) -> Vec<String> {
    // The card less its icon and its edges, in points like the size.
    let max = center::CARD_WIDTH - 81.0;
    font.set_size(15.0);
    let width = |s: &str| -> f32 {
        runs(font, s)
            .iter()
            .map(|(t, f)| f.measure_str(t, None).0)
            .sum()
    };
    let mut lines = Vec::new();
    for paragraph in body.lines().filter(|l| !l.trim().is_empty()) {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let longer = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if line.is_empty() || width(&longer) <= max {
                line = longer;
            } else {
                lines.push(std::mem::replace(&mut line, word.to_string()));
            }
            // A word wider than the card: cut anywhere.
            while width(&line) > max {
                let mut head = String::new();
                for c in line.chars() {
                    head.push(c);
                    if width(&head) > max {
                        head.pop();
                        break;
                    }
                }
                if head.is_empty() {
                    break;
                }
                line = line[head.len()..].to_string();
                lines.push(head);
            }
        }
        lines.push(line);
    }
    lines
}

/// `s` split where `font` runs out of glyphs: Inter has no emoji and no CJK,
/// and notifications are other people's text. Each run comes with a font
/// that can draw it, at `font`'s size.
fn runs(font: &Font, s: &str) -> Vec<(String, Font)> {
    thread_local! {
        static FOUND: std::cell::RefCell<std::collections::HashMap<char, Option<skia_safe::Typeface>>> =
            Default::default();
    }
    let mut out: Vec<(String, Option<skia_safe::Typeface>)> = Vec::new();
    for c in s.chars() {
        // Joiners and variation selectors draw as boxes unshaped.
        if matches!(c, '\u{200d}' | '\u{fe0e}' | '\u{fe0f}') {
            continue;
        }
        let other = if font.unichar_to_glyph(c as i32) != 0 {
            None
        } else {
            FOUND.with(|found| {
                found
                    .borrow_mut()
                    .entry(c)
                    .or_insert_with(|| {
                        // The emoji font by name first: matched by character
                        // alone, fontconfig hands out DejaVu's few outline
                        // emoji. It answers for any character, so its glyph
                        // is checked.
                        let mgr = FontMgr::default();
                        ["Noto Color Emoji", "sans-serif"]
                            .iter()
                            .find_map(|family| {
                                mgr.match_family_style_character(
                                    family,
                                    FontStyle::normal(),
                                    &[],
                                    c as i32,
                                )
                                .filter(|face| face.unichar_to_glyph(c as i32) != 0)
                            })
                    })
                    .clone()
            })
        };
        let id = |t: &Option<skia_safe::Typeface>| t.as_ref().map(|t| t.unique_id());
        match out.last_mut() {
            Some((run, face)) if id(face) == id(&other) => run.push(c),
            _ => out.push((c.to_string(), other)),
        }
    }
    out.into_iter()
        .map(|(run, face)| {
            let font = match face {
                Some(face) => Font::from_typeface(face, font.size()),
                None => font.clone(),
            };
            (run, font)
        })
        .collect()
}

/// The clock on the left; signal, Wi-Fi and the battery on the right.
/// Centred on `y`, `pad` in from the screen's edges.
fn status_row(canvas: &Canvas, w: f32, y: f32, pad: f32, look: &mut Scene) {
    let ink = rgba(look.theme.foreground, 1.0);
    let faint = rgba(look.theme.foreground, 0.35);
    text(canvas, look.font, look.clock, (pad, y), 15.0, ink, false, w);
    let mut right = w - pad;
    if let Some((percent, plugged_in)) = look.system.battery {
        let body = Rect::from_xywh(right - 2.5 - 23.0, y - 5.5, 23.0, 11.0);
        // Low stays red on the charger: the bolt says it is charging.
        let level = if percent <= 20.0 {
            rgba(look.theme.badge, 1.0)
        } else if plugged_in {
            rgba(look.theme.positive, 1.0)
        } else {
            ink
        };
        let shape = RRect::new_rect_xy(body, 3.5, 3.5);
        canvas.draw_rrect(shape, &fill(faint));
        canvas.save();
        canvas.clip_rrect(shape, ClipOp::Intersect, true);
        let charge = body.width() * (percent / 100.0).clamp(0.0, 1.0);
        canvas.draw_rect(
            Rect::from_xywh(body.left(), body.top(), charge, body.height()),
            &fill(level),
        );
        canvas.restore();
        let tip = Rect::from_xywh(body.right() + 1.0, y - 2.0, 1.5, 4.0);
        canvas.draw_rrect(RRect::new_rect_xy(tip, 0.75, 0.75), &fill(faint));
        right = body.left() - 5.0;
        if plugged_in {
            Symbol::Zap.draw(canvas, (right - 4.0, y), 10.0, ink);
            right -= 11.0;
        }
        let label = format!("{}%", percent.round() as u32);
        right -= text(canvas, look.font, &label, (right, y), 12.0, ink, true, w) + 8.0;
    }
    if Toggle::Airplane.get(&look.system.controls) == Some(true) {
        Symbol::Plane.draw(canvas, (right - 7.0, y), 14.0, ink);
        return;
    }
    if let Some(strength) = look.system.wifi {
        let at = (right - 8.0, y - 1.0);
        Symbol::Wifi.draw(canvas, at, 16.0, faint);
        wifi_symbol(strength).draw(canvas, at, 16.0, ink);
        right -= 16.0 + 7.0;
    }
    if let Some(quality) = look.system.cellular {
        // Phosh's thresholds.
        let lit = [5, 30, 55, 80].iter().filter(|&&t| quality > t).count();
        for i in 0..4 {
            let h = 11.0 - 2.5 * (3 - i) as f32;
            let bar = Rect::from_xywh(right - 17.0 + 4.5 * i as f32, y + 5.5 - h, 3.0, h);
            let color = if i < lit { ink } else { faint };
            canvas.draw_rrect(RRect::new_rect_xy(bar, 1.0, 1.0), &fill(color));
        }
    }
}

/// The bands lit at a strength of 0..100.
fn wifi_symbol(strength: u8) -> Symbol {
    match strength {
        67.. => Symbol::Wifi,
        34.. => Symbol::WifiHigh,
        _ => Symbol::WifiLow,
    }
}

fn canvas_for(pixels: &mut [u8], w: i32, h: i32, scale: f32, draw: impl FnOnce(&Canvas)) {
    canvas_over(pixels, w, h, scale, |canvas| {
        canvas.clear(Color::TRANSPARENT);
        draw(canvas)
    });
}

/// Draws over what `pixels` already hold.
fn canvas_over(pixels: &mut [u8], w: i32, h: i32, scale: f32, draw: impl FnOnce(&Canvas)) {
    let info = ImageInfo::new((w, h), ColorType::BGRA8888, AlphaType::Premul, None);
    let Some(mut surface) = surfaces::wrap_pixels(&info, pixels, None, None) else {
        return;
    };
    let canvas = surface.canvas();
    canvas.scale((scale, scale));
    draw(canvas);
}

/// `pixels`: ARGB8888 (little-endian BGRA), `w`×`h` buffer px at `scale`.
pub fn bar(pixels: &mut [u8], w: i32, h: i32, scale: f32, look: &mut Scene) {
    canvas_for(pixels, w, h, scale, |canvas| {
        status_row(canvas, w as f32 / scale, BAR_H / 2.0, PAD, look)
    });
}

/// Its symbol, and its colour when on: the disc's fill, or the symbol's ink
/// on a tile.
fn toggle_look(toggle: Toggle, theme: &sc_config::Theme) -> (Symbol, u32) {
    match toggle {
        Toggle::Airplane => (Symbol::Plane, theme.caution),
        Toggle::Cellular => (Symbol::Signal, theme.positive),
        Toggle::Wifi => (Symbol::Wifi, theme.accent),
        Toggle::Bluetooth => (Symbol::Bluetooth, theme.accent),
        Toggle::RotationLock => (Symbol::RotationLock, theme.badge),
        Toggle::Silent => (Symbol::BellOff, theme.badge),
        Toggle::DarkMode => (Symbol::Moon, theme.accent),
        Toggle::LowPower => (Symbol::BatteryLow, theme.caution),
        Toggle::Torch => (Symbol::Flashlight, theme.accent),
        Toggle::Hotspot => (Symbol::Link, theme.positive),
    }
}

/// One frame of the sheet from a picture of it open (`open`, from [`sheet`]):
/// its rows down to `edge`, the ones from `bar` on pushed `shift` rows down,
/// and nothing below. Copies, where a redraw every frame cost a core.
///
/// `stride` is a row's bytes. The row at `bar` has to be bare backdrop: it
/// fills the gap the shift opens.
pub fn reveal(dst: &mut [u8], open: &[u8], stride: usize, edge: usize, bar: usize, shift: usize) {
    let rows = dst.len() / stride;
    let edge = edge.min(rows);
    let row = |y: usize| &open[y * stride..(y + 1) * stride];
    if shift == 0 {
        dst[..edge * stride].copy_from_slice(&open[..edge * stride]);
    } else {
        for (y, out) in dst.chunks_exact_mut(stride).take(edge).enumerate() {
            out.copy_from_slice(row(if y < bar {
                y
            } else {
                y.saturating_sub(shift).max(bar)
            }));
        }
    }
    dst[edge * stride..].fill(0);
}

/// The status row and the power glyph, over a frame [`reveal`] made: the row
/// comes down from the bar as the sheet does, and the glyph takes its corner.
pub fn sheet_top(pixels: &mut [u8], w: i32, h: i32, scale: f32, panel: &Panel, look: &mut Scene) {
    canvas_over(pixels, w, h, scale, |canvas| {
        let (w, h) = (w as f32 / scale, h as f32 / scale);
        let down = panel.lowered();
        // Not until the row is clear of it.
        let mut alpha = ((down - 0.6) / 0.4).max(0.0);
        if panel.pressed == Some(Control::PowerMenu) {
            alpha *= 0.4;
        }
        let grid = Grid::new(w, h, &look.system.controls);
        let corner = grid.corner(w);
        let at = (corner.center_x(), corner.center_y());
        Symbol::Power.draw(canvas, at, 20.0, rgba(look.theme.foreground, alpha));
        // With the modules, past the bottom too, and in line with their edges.
        let rest = grid.status_y() + panel.overshoot() * h;
        let y = BAR_H / 2.0 + down * (rest - BAR_H / 2.0);
        status_row(canvas, w, y, PAD + down * (grid.x - PAD), look);
    });
}

/// The sheet all the way down, but for [`sheet_top`]; [`reveal`] makes each
/// frame from it.
pub fn sheet(pixels: &mut [u8], w: i32, h: i32, scale: f32, panel: &Panel, look: &mut Scene) {
    canvas_for(pixels, w, h, scale, |canvas| {
        let (w, h) = (w as f32 / scale, h as f32 / scale);
        let controls = &look.system.controls;
        let white = |alpha: f32| rgba(0xffffff, alpha);
        // Dark glass, as the reference: black, whatever the theme.
        let module = fill(rgba(0x000000, 0.4));
        canvas.draw_color(rgba(look.theme.background, 0.5), None);

        let grid = Grid::new(w, h, controls);
        let radius = grid.cell * radius::CONTAINER;
        // One size everywhere, so the symbols' lines are one weight.
        let glyph = grid.cell * 0.42;
        let rounded = |r: sc_layout::Rect| RRect::new_rect_xy(skia(r), radius, radius);
        if panel.page == Page::Power {
            canvas.draw_rrect(rounded(grid.power()), &module);
        } else {
            canvas.draw_rrect(rounded(grid.connectivity()), &module);
            let media = grid.media();
            canvas.draw_rrect(rounded(media), &module);
            let pad = media.w * 0.1;
            let (title, artist) = match &controls.media {
                Some(m) if !m.title.is_empty() => (m.title.as_str(), m.artist.as_str()),
                _ => ("Not Playing", ""),
            };
            let size = grid.cell * 0.19;
            let at = |line: f32| (media.x + pad, media.y + pad + size * (0.5 + 1.4 * line));
            text(
                canvas,
                look.font,
                title,
                at(0.0),
                size,
                white(1.0),
                false,
                media.w - 2.0 * pad,
            );
            text(
                canvas,
                look.font,
                artist,
                at(1.0),
                size,
                white(0.6),
                false,
                media.w - 2.0 * pad,
            );
        }
        for item in panel::items(w, h, controls, panel.page) {
            let r = item.rect;
            let center = (r.center_x(), r.center_y());
            let pressed = panel.pressed == Some(item.control);
            match (item.look, item.control) {
                (Look::Round, Control::Toggle(t)) => {
                    let (symbol, color) = toggle_look(t, look.theme);
                    let on = t.get(controls) == Some(true);
                    let alpha = if pressed { wash::HELD } else { wash::FILL };
                    let color = if on { rgba(color, 1.0) } else { white(alpha) };
                    canvas.draw_circle(center, r.w / 2.0, &fill(color));
                    symbol.draw(canvas, center, glyph, white(1.0));
                }
                (Look::Power, Control::Power(action)) => {
                    let (symbol, color) = match action {
                        PowerAction::Sleep => (Symbol::Moon, None),
                        PowerAction::LogOut => (Symbol::LogOut, None),
                        PowerAction::Restart => (Symbol::Restart, None),
                        PowerAction::PowerOff => (Symbol::Power, Some(look.theme.badge)),
                    };
                    let alpha = if pressed { wash::HELD } else { wash::FILL };
                    let color =
                        color.map_or(white(alpha), |c| rgba(c, if pressed { 0.7 } else { 1.0 }));
                    canvas.draw_circle(center, r.w / 2.0, &fill(color));
                    symbol.draw(canvas, center, glyph, white(1.0));
                    let size = grid.cell * 0.19;
                    look.font.set_size(size);
                    let width = look.font.measure_str(action.label(), None).0;
                    let at = (center.0 - width / 2.0, r.y + r.h + size * 1.2);
                    text(
                        canvas,
                        look.font,
                        action.label(),
                        at,
                        size,
                        white(1.0),
                        false,
                        w,
                    );
                }
                (Look::Tile, Control::Shortcut(i)) => {
                    canvas.draw_rrect(rounded(r), &module);
                    if pressed {
                        canvas.draw_rrect(rounded(r), &fill(white(wash::PRESSED)));
                    }
                    if let Some(image) = look.icons.get(i) {
                        let d = r.w * 0.62;
                        let dst = Rect::from_xywh(center.0 - d / 2.0, center.1 - d / 2.0, d, d);
                        let sampling = SamplingOptions::new(FilterMode::Linear, MipmapMode::Linear);
                        canvas.draw_image_rect_with_sampling_options(
                            image,
                            None,
                            dst,
                            sampling,
                            &Paint::default(),
                        );
                    }
                }
                (Look::Tile, Control::Toggle(t)) => {
                    let (symbol, color) = toggle_look(t, look.theme);
                    let on = t.get(controls) == Some(true);
                    if on {
                        canvas.draw_rrect(rounded(r), &fill(white(TILE_ON)));
                    } else {
                        canvas.draw_rrect(rounded(r), &module);
                        if pressed {
                            canvas.draw_rrect(rounded(r), &fill(white(wash::PRESSED)));
                        }
                    }
                    let ink = if on { rgba(color, 1.0) } else { white(1.0) };
                    symbol.draw(canvas, center, glyph, ink);
                }
                (Look::Slider, Control::Slider(slider)) => {
                    let value = slider.get(controls).unwrap_or(0.0);
                    canvas.save();
                    canvas.clip_rrect(rounded(r), ClipOp::Intersect, true);
                    canvas.draw_rect(skia(r), &module);
                    let level = Rect::from_xywh(r.x, r.y + r.h * (1.0 - value), r.w, r.h * value);
                    canvas.draw_rect(level, &fill(white(TILE_ON)));
                    canvas.restore();
                    let symbol = match slider {
                        Slider::Brightness => Symbol::Sun,
                        Slider::Volume => Symbol::Volume,
                    };
                    // Dark once the level reaches it.
                    let ink = if r.h * value > r.w * 0.5 {
                        rgba(look.theme.background, 1.0)
                    } else {
                        white(1.0)
                    };
                    symbol.draw(canvas, (center.0, r.y + r.h - r.w * 0.5), glyph, ink);
                }
                (Look::Media, Control::Media(action)) => {
                    let playing = controls.media.as_ref().is_some_and(|m| m.playing);
                    let symbol = match action {
                        MediaAction::Previous => Symbol::SkipBack,
                        MediaAction::PlayPause if playing => Symbol::Pause,
                        MediaAction::PlayPause => Symbol::Play,
                        MediaAction::Next => Symbol::SkipForward,
                    };
                    let alpha = if pressed || controls.media.is_none() {
                        0.4
                    } else {
                        1.0
                    };
                    symbol.draw(canvas, center, r.w * 0.75, white(alpha));
                }
                _ => {}
            }
        }
    });
}

/// What Notification Centre draws from besides the [`Scene`].
pub struct Notes<'a> {
    pub center: &'a Center,
    /// `Monday 16 November`.
    pub date: &'a str,
    /// Unix seconds.
    pub now: u64,
    /// By [`icon_key`].
    pub icons: &'a std::collections::HashMap<String, Option<Image>>,
    /// sc-bar is the notification daemon: another one would get them all.
    pub serving: bool,
}

/// What a notification's icon is kept under: its app, else the icon it
/// named, else the name it goes by.
pub fn icon_key(n: &Notification) -> &str {
    match &n.app {
        Some(app) => app,
        None if !n.icon.is_empty() => &n.icon,
        None => &n.app_name,
    }
}

/// Where Notification Centre's list starts on a sheet `w` wide: below the
/// date and the clock.
pub fn list_view(w: f32, h: f32) -> View {
    let mut view = View { top: 0.0, w, h };
    view.top = BAR_H + 150.0 * view.k();
    view
}

fn centred(canvas: &Canvas, font: &mut Font, s: &str, (x, y): (f32, f32), size: f32, color: Color) {
    font.set_size(size);
    let width = font.measure_str(s, None).0;
    text(
        canvas,
        font,
        s,
        (x - width / 2.0, y),
        size,
        color,
        false,
        f32::MAX,
    );
}

/// A notification's card: icon, title, body and when it came, over its first
/// `buttons` action buttons.
#[allow(clippy::too_many_arguments)]
fn card(
    canvas: &Canvas,
    r: sc_layout::Rect,
    n: &Notification,
    notes: &Notes,
    font: &mut Font,
    fill_color: Color,
    alpha: f32,
    lines: usize,
    buttons: usize,
) {
    // Its own scale: a card growing in is smaller than its place.
    let k = r.h / center::card_height(lines, buttons);
    let ink = |a: f32| rgba(0xffffff, a * alpha);
    canvas.draw_rrect(
        RRect::new_rect_xy(skia(r), center::RADIUS * k, center::RADIUS * k),
        &fill(fill_color),
    );
    let d = 38.0 * k;
    let icon = Rect::from_xywh(r.x + 13.5 * k, r.y + (center::HEIGHT * k - d) / 2.0, d, d);
    let shape = RRect::new_rect_xy(icon, radius::PANEL * k, radius::PANEL * k);
    match notes.icons.get(icon_key(n)).and_then(Option::as_ref) {
        Some(image) => {
            canvas.save();
            canvas.clip_rrect(shape, ClipOp::Intersect, true);
            let mut paint = Paint::default();
            paint.set_alpha_f(alpha);
            let sampling = SamplingOptions::new(FilterMode::Linear, MipmapMode::Linear);
            canvas.draw_image_rect_with_sampling_options(image, None, icon, sampling, &paint);
            canvas.restore();
        }
        None => {
            canvas.draw_rrect(shape, &fill(ink(0.3)));
        }
    }
    let x = r.x + 64.0 * k;
    let right = r.x + r.w - 17.0 * k;
    let when = center::ago(n.received, notes.now);
    let time_w = text(
        canvas,
        font,
        &when,
        (right, r.y + 21.0 * k),
        13.0 * k,
        ink(0.6),
        true,
        r.w,
    );
    let title = if n.summary.is_empty() {
        &n.app_name
    } else {
        &n.summary
    };
    font_weighted(font, true);
    text(
        canvas,
        font,
        title,
        (x, r.y + 22.0 * k),
        15.0 * k,
        ink(1.0),
        false,
        right - time_w - 10.0 * k - x,
    );
    font_weighted(font, false);
    let first = [n.body.lines().next().unwrap_or_default().to_string()];
    let body = if n.lines.is_empty() {
        &first[..]
    } else {
        &n.lines[..]
    };
    for (i, line) in body.iter().take(lines).enumerate() {
        // The last one shown says so when there is more.
        let more = i + 1 == lines && body.len() > lines;
        let line = if more {
            format!("{line}…")
        } else {
            line.clone()
        };
        text(
            canvas,
            font,
            &line,
            (x, r.y + (43.0 + center::LINE * i as f32) * k),
            15.0 * k,
            ink(0.85),
            false,
            right - x,
        );
    }
    let rects = center::button_rects(r, lines, buttons);
    for (b, (_, label)) in rects.iter().zip(n.buttons()) {
        canvas.draw_rrect(
            RRect::new_rect_xy(skia(*b), b.h / 2.0, b.h / 2.0),
            &fill(ink(wash::FILL)),
        );
        font.set_size(15.0 * k);
        let width = font.measure_str(label, None).0.min(b.w - 16.0 * k);
        text(
            canvas,
            font,
            label,
            (b.center_x() - width / 2.0, b.center_y()),
            15.0 * k,
            ink(0.95),
            false,
            width,
        );
    }
}

/// Notification Centre all the way down, without its list: the date and the
/// clock. [`reveal`] makes each frame of the sheet's own movement from it,
/// and [`notification_list`] goes over that.
pub fn notifications(
    pixels: &mut [u8],
    w: i32,
    h: i32,
    scale: f32,
    look: &mut Scene,
    notes: &Notes,
) {
    canvas_for(pixels, w, h, scale, |canvas| {
        let (w, h) = (w as f32 / scale, h as f32 / scale);
        canvas.draw_color(rgba(look.theme.background, 0.5), None);
        status_row(canvas, w, BAR_H / 2.0, PAD, look);
        heading(canvas, w, h, look, notes.date);
    });
}

/// The date over the clock.
fn heading(canvas: &Canvas, w: f32, h: f32, look: &mut Scene, date: &str) {
    let k = list_view(w, h).k();
    let ink = |a: f32| rgba(look.theme.foreground, a);
    centred(
        canvas,
        look.font,
        date,
        (w / 2.0, BAR_H + 34.0 * k),
        20.0 * k,
        ink(0.85),
    );
    font_weighted(look.font, true);
    centred(
        canvas,
        look.font,
        look.clock,
        (w / 2.0, BAR_H + 92.0 * k),
        78.0 * k,
        ink(1.0),
    );
    font_weighted(look.font, false);
}

/// The list, over a frame [`reveal`] made: down to `edge` (logical px) of a
/// sheet still coming down, and `shift` lower while it swings past the
/// bottom. Drawn every frame it moves, so only the cards are touched.
#[allow(clippy::too_many_arguments)]
pub fn notification_list(
    pixels: &mut [u8],
    w: i32,
    h: i32,
    scale: f32,
    look: &mut Scene,
    notes: &Notes,
    edge: f32,
    shift: f32,
) {
    canvas_over(pixels, w, h, scale, |canvas| {
        let (w, h) = (w as f32 / scale, h as f32 / scale);
        canvas.clip_rect(Rect::from_xywh(0.0, 0.0, w, edge), None, false);
        canvas.translate((0.0, shift));
        list(canvas, w, h, look, notes);
    });
}

fn list(canvas: &Canvas, w: f32, h: f32, look: &mut Scene, notes: &Notes) {
    let view = list_view(w, h);
    let k = view.k();
    let ink = |a: f32| rgba(look.theme.foreground, a);
    let rows = notes.center.rows(view);
    if rows.is_empty() {
        centred(
            canvas,
            look.font,
            if notes.serving {
                "No Notifications"
            } else {
                "Another notification daemon is running"
            },
            (w / 2.0, h * 0.5),
            17.0 * k,
            ink(0.6),
        );
    }
    // The list scrolls under the clock, not over it.
    canvas.save();
    canvas.clip_rect(Rect::from_xywh(0.0, view.top - 6.0 * k, w, h), None, false);
    let glass = |a: f32| rgba(0xffffff, wash::FILL * a);
    for row in rows.iter().filter(|r| {
        let r = r.rect();
        r.y + r.h > 0.0 && r.y < h
    }) {
        match row {
            Row::Header { rect } => {
                let y = rect.center_y();
                font_weighted(look.font, true);
                text(
                    canvas,
                    look.font,
                    "Notification Centre",
                    (rect.x + 8.0 * k, y),
                    20.0 * k,
                    ink(1.0),
                    false,
                    rect.w,
                );
                font_weighted(look.font, false);
                let b = center::clear_button(*rect, view);
                let c = (b.center_x(), b.center_y());
                canvas.draw_circle(c, b.w / 2.0, &fill(glass(1.0)));
                let mut stroke = fill(ink(0.9));
                stroke.set_style(skia_safe::paint::Style::Stroke);
                stroke.set_stroke_width(1.8 * k);
                stroke.set_stroke_cap(skia_safe::paint::Cap::Round);
                let arm = 5.0 * k;
                canvas.draw_line((c.0 - arm, c.1 - arm), (c.0 + arm, c.1 + arm), &stroke);
                canvas.draw_line((c.0 - arm, c.1 + arm), (c.0 + arm, c.1 - arm), &stroke);
            }
            Row::Card {
                ids,
                stacked,
                buttons,
                lines,
                rect,
            } => {
                let Some(entry) = notes.center.entry(ids[0]) else {
                    continue;
                };
                let alpha = notes.center.fade(ids[0], view);
                // Older ones of a stack peek out from under its card.
                if *stacked {
                    // Only what sticks out: the card is see-through.
                    canvas.save();
                    let top =
                        RRect::new_rect_xy(skia(*rect), center::RADIUS * k, center::RADIUS * k);
                    canvas.clip_rrect(top, ClipOp::Difference, true);
                    for i in (1..=(ids.len() - 1).min(2)).rev() {
                        let i = i as f32;
                        let peek = sc_layout::Rect {
                            x: rect.x + center::PEEK_INSET * k * i,
                            y: rect.y + center::PEEK * k * i,
                            w: rect.w - 2.0 * center::PEEK_INSET * k * i,
                            h: rect.h,
                        };
                        let radius = center::RADIUS * k;
                        canvas.draw_rrect(
                            RRect::new_rect_xy(skia(peek), radius, radius),
                            &fill(glass(alpha * (1.0 - 0.3 * i))),
                        );
                    }
                    canvas.restore();
                }
                card(
                    canvas,
                    *rect,
                    &entry.notification,
                    notes,
                    look.font,
                    glass(alpha),
                    alpha,
                    *lines,
                    *buttons,
                );
            }
        }
    }
    canvas.restore();
}

/// The lock screen: the passcode pad, and over it the cover (the date, the
/// clock and the notifications) as far up as it has been pushed.
#[allow(clippy::too_many_arguments)]
pub fn lock(
    pixels: &mut [u8],
    w: i32,
    h: i32,
    scale: f32,
    look: &mut Scene,
    notes: &Notes,
    lock: &Lock,
    hint: &str,
) {
    canvas_for(pixels, w, h, scale, |canvas| {
        let (w, h) = (w as f32 / scale, h as f32 / scale);
        let k = list_view(w, h).k();
        let ink = |a: f32| rgba(look.theme.foreground, a);
        canvas.draw_color(rgba(look.theme.background, 1.0), None);
        let up = lock.reveal();
        if up > 0.0 {
            pad(canvas, w, h, look, lock);
        }
        if up < h {
            canvas.save();
            canvas.translate((0.0, -up));
            canvas.clip_rect(Rect::from_xywh(0.0, 0.0, w, h), None, false);
            canvas.draw_color(rgba(look.theme.background, 1.0), None);
            heading(canvas, w, h, look, notes.date);
            list(canvas, w, h, look, notes);
            centred(
                canvas,
                look.font,
                hint,
                (w / 2.0 + lock.shake() * k, h - 40.0 * k),
                15.0 * k,
                ink(0.6),
            );
            let pill = skia(sc_layout::pill_rect(w, h));
            let r = pill.height() / 2.0;
            canvas.draw_rrect(RRect::new_rect_xy(pill, r, r), &fill(ink(wash::PILL)));
            canvas.restore();
        }
        status_row(canvas, w, BAR_H / 2.0, PAD, look);
    });
}

fn pad(canvas: &Canvas, w: f32, h: f32, look: &mut Scene, lock: &Lock) {
    let k = list_view(w, h).k();
    let ink = |a: f32| rgba(look.theme.foreground, a);
    let top = lock::pad_top(w, h);
    centred(
        canvas,
        look.font,
        "Enter Passcode",
        (w / 2.0, top),
        20.0 * k,
        ink(1.0),
    );
    let n = lock.digits() as f32;
    let dot = fill(ink(if lock.checking() { 0.5 } else { 1.0 }));
    for i in 0..lock.digits() {
        let x = w / 2.0 + (i as f32 - (n - 1.0) / 2.0) * 22.0 * k + lock.shake() * k;
        canvas.draw_circle((x, top + lock::DOTS * k), 6.0 * k, &dot);
    }
    for (row, column) in (0..4).flat_map(|row| (0..3).map(move |column| (row, column))) {
        let c = lock::key_center(row, column, w, h);
        let key = lock::key_for(row, column);
        let pressed = lock.pressed() == Some(key);
        match key {
            Key::Digit(d) => {
                let glass = rgba(0xffffff, if pressed { wash::HELD } else { wash::FILL });
                canvas.draw_circle(c, lock::KEY_DIAMETER / 2.0 * k, &fill(glass));
                centred(canvas, look.font, &d.to_string(), c, 34.0 * k, ink(1.0));
            }
            Key::Back | Key::Submit => {
                let (label, enabled) = match key {
                    Key::Back if lock.digits() == 0 => ("Cancel", true),
                    Key::Back => ("Delete", true),
                    _ => ("OK", lock.digits() > 0),
                };
                let alpha = match (pressed, enabled) {
                    (true, _) => 0.5,
                    (false, true) => 1.0,
                    (false, false) => 0.4,
                };
                centred(canvas, look.font, label, c, 17.0 * k, ink(alpha));
            }
        }
    }
}

/// Regular, or SemiBold for the few titles.
fn font_weighted(font: &mut Font, bold: bool) {
    thread_local! {
        static FACES: [Option<skia_safe::Typeface>; 2] = [sc_fonts::INTER, sc_fonts::INTER_SEMIBOLD]
            .map(|data| FontMgr::default().new_from_data(data, None));
    }
    if let Some(face) = FACES.with(|faces| faces[bold as usize].clone()) {
        font.set_typeface(face);
    }
}

/// The banner's surface: clear but for the card at `rect`.
#[allow(clippy::too_many_arguments)]
pub fn banner(
    pixels: &mut [u8],
    w: i32,
    h: i32,
    scale: f32,
    look: &mut Scene,
    notes: &Notes,
    n: &Notification,
    rect: sc_layout::Rect,
) {
    canvas_for(pixels, w, h, scale, |canvas| {
        let k = rect.h / center::HEIGHT;
        // Nothing blurs behind it, so it is nearly solid, with a shadow to
        // lift it off what it covers.
        let shadow = sc_layout::Rect {
            y: rect.y + 3.0 * k,
            ..rect
        };
        let mut soft = fill(rgba(0x000000, 0.35));
        soft.set_mask_filter(skia_safe::MaskFilter::blur(
            skia_safe::BlurStyle::Normal,
            6.0 * k,
            false,
        ));
        canvas.draw_rrect(
            RRect::new_rect_xy(skia(shadow), center::RADIUS * k, center::RADIUS * k),
            &soft,
        );
        card(
            canvas,
            rect,
            n,
            notes,
            look.font,
            rgba(look.theme.surface, 0.96),
            1.0,
            1,
            0,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_spaces_and_cuts_a_word_wider_than_the_card() {
        let mut font = font();
        let body = "IBus should be called from the desktop session in Wayland.\n\nSecond.";
        let lines = wrap(&mut font, body);
        assert!(lines.len() >= 3, "{lines:?}");
        assert_eq!(lines.join(" "), body.replace("\n\n", " "));
        assert_eq!(lines.last().unwrap(), "Second.");

        let long = "x".repeat(200);
        let cut = wrap(&mut font, &long);
        assert!(cut.len() > 1);
        assert_eq!(cut.concat(), long);
    }

    #[test]
    fn reveal_copies_down_to_the_edge_and_shifts_under_the_bar() {
        // Six rows of one byte: the bar, bare backdrop, then modules.
        let open = [9, 1, 5, 6, 7, 8];
        let mut dst = [3; 6];
        reveal(&mut dst, &open, 1, 4, 1, 0);
        assert_eq!(dst, [9, 1, 5, 6, 0, 0]);
        reveal(&mut dst, &open, 1, 6, 1, 2);
        assert_eq!(dst, [9, 1, 1, 1, 5, 6]);
    }

    #[test]
    fn wifi_lights_one_to_three_bands() {
        let bands = |s| match wifi_symbol(s) {
            Symbol::WifiLow => 1,
            Symbol::WifiHigh => 2,
            Symbol::Wifi => 3,
            _ => 0,
        };
        assert_eq!([0, 33, 34, 66, 67, 100].map(bands), [1, 1, 2, 2, 3, 3]);
    }
}
