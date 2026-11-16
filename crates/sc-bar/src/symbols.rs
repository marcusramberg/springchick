//! Lucide's symbols (ISC, `LICENSE-lucide`) as SVG path data on its 24 unit
//! grid, with circles, rects and lines written out as paths.

use skia_safe::{paint, Canvas, Color, Paint, Path};

#[derive(Clone, Copy)]
pub enum Symbol {
    Plane,
    Wifi,
    /// The dot and the first band, for a weak signal.
    WifiLow,
    /// All but the outer band.
    WifiHigh,
    Signal,
    Bluetooth,
    /// A padlock inside the rotate arrow.
    RotationLock,
    /// Charging.
    Zap,
    BellOff,
    Moon,
    Power,
    LogOut,
    Restart,
    BatteryLow,
    Flashlight,
    Sun,
    Volume,
    Play,
    Pause,
    SkipBack,
    SkipForward,
}

impl Symbol {
    fn path(self) -> &'static str {
        match self {
            Symbol::Plane => "M17.8 19.2 16 11l3.5-3.5C21 6 21.5 4 21 3c-1-.5-3 0-4.5 1.5L13 8 4.8 6.2c-.5-.1-.9.1-1.1.5l-.3.5c-.2.5-.1 1 .3 1.3L9 12l-2 3H4l-1 1 3 2 2 3 1-1v-3l3-2 3.5 5.3c.3.4.8.5 1.3.3l.5-.2c.4-.3.6-.7.5-1.2z",
            Symbol::Wifi => "M12 20h.01 M2 8.82a15 15 0 0 1 20 0 M5 12.859a10 10 0 0 1 14 0 M8.5 16.429a5 5 0 0 1 7 0",
            Symbol::WifiLow => "M12 20h.01 M8.5 16.429a5 5 0 0 1 7 0",
            Symbol::WifiHigh => "M12 20h.01 M5 12.859a10 10 0 0 1 14 0 M8.5 16.429a5 5 0 0 1 7 0",
            Symbol::Signal => "M2 20h.01 M7 20v-4 M12 20v-8 M17 20V8 M22 4v16",
            Symbol::Bluetooth => "m7 7 10 10-5 5V2l5 5L7 17",
            Symbol::RotationLock => "M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8 M21 3v5h-5 M9.5 11.5h5a1 1 0 0 1 1 1v3a1 1 0 0 1-1 1h-5a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1z M10 11.5V10a2 2 0 0 1 4 0v1.5",
            Symbol::Zap => "M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z",
            Symbol::BellOff => "M8.7 3A6 6 0 0 1 18 8a21.3 21.3 0 0 0 .6 5 M17 17H3s3-2 3-9a4.67 4.67 0 0 1 .3-1.7 M10.3 21a1.94 1.94 0 0 0 3.4 0  M2 2l20 20",
            Symbol::Moon => "M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z",
            Symbol::Power => "M12 2v10 M18.4 6.6a9 9 0 1 1-12.77.04",
            Symbol::LogOut => "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4 M16 17l5-5-5-5 M21 12H9",
            Symbol::Restart => "M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8 M3 3v5h5",
            Symbol::BatteryLow => "M4 7h12a2 2 0 0 1 2 2v6a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V9a2 2 0 0 1 2-2z M22 11v2 M6 11v2",
            Symbol::Flashlight => "M18 6c0 2-2 2-2 4v10a2 2 0 0 1-2 2h-4a2 2 0 0 1-2-2V10c0-2-2-2-2-4V2h12z M6 6h12 M12 12h.01",
            Symbol::Sun => "M8 12a4 4 0 1 0 8 0a4 4 0 1 0-8 0 M12 2v2 M12 20v2 M4.93 4.93l1.41 1.41 M17.66 17.66l1.41 1.41 M2 12h2 M20 12h2 M6.34 17.66l-1.41 1.41 M19.07 4.93l-1.41 1.41",
            Symbol::Volume => "M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z M16 9a5 5 0 0 1 0 6 M19.364 18.364a9 9 0 0 0 0-12.728",
            Symbol::Play => "M6 3l14 9-14 9z",
            Symbol::Pause => "M14 4h4v16h-4z M6 4h4v16h-4z",
            Symbol::SkipBack => "M19 20 9 12l10-8z M5 19V5",
            Symbol::SkipForward => "M5 4l10 8-10 8z M19 5v14",
        }
    }

    fn filled(self) -> bool {
        matches!(
            self,
            Symbol::Zap | Symbol::Play | Symbol::Pause | Symbol::SkipBack | Symbol::SkipForward
        )
    }

    /// `size` across, centred on `center`.
    pub fn draw(self, canvas: &Canvas, center: (f32, f32), size: f32, color: Color) {
        let Some(path) = Path::from_svg(self.path()) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(color);
        paint.set_style(if self.filled() {
            paint::Style::StrokeAndFill
        } else {
            paint::Style::Stroke
        });
        paint.set_stroke_width(2.0);
        paint.set_stroke_cap(paint::Cap::Round);
        paint.set_stroke_join(paint::Join::Round);
        canvas.save();
        canvas.translate((center.0 - size / 2.0, center.1 - size / 2.0));
        canvas.scale((size / 24.0, size / 24.0));
        canvas.draw_path(&path, &paint);
        canvas.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_symbol_parses() {
        for s in [
            Symbol::Plane,
            Symbol::Wifi,
            Symbol::WifiLow,
            Symbol::WifiHigh,
            Symbol::Signal,
            Symbol::Bluetooth,
            Symbol::RotationLock,
            Symbol::Zap,
            Symbol::BellOff,
            Symbol::Moon,
            Symbol::Power,
            Symbol::LogOut,
            Symbol::Restart,
            Symbol::BatteryLow,
            Symbol::Flashlight,
            Symbol::Sun,
            Symbol::Volume,
            Symbol::Play,
            Symbol::Pause,
            Symbol::SkipBack,
            Symbol::SkipForward,
        ] {
            assert!(Path::from_svg(s.path()).is_some(), "{}", s.path());
        }
    }
}
