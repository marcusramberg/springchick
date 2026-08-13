//! springchick pull-down search: a standalone screen-filling Wayland app.
//!
//! Launched by the compositor on the Home pull-down gesture. As a normal xdg
//! toplevel it gets keyboard focus, touch, an on-screen keyboard (wvkbd/IME),
//! and a cursor for free — the compositor only spawns it and animates it in.
//! It ranks the app catalog by frecency (read-only from the shared state file),
//! filters by name as you type, and launches the pick (then exits).

mod blur;

use std::collections::HashMap;

use eframe::egui;
use sc_catalog::AppEntry;
use sc_shell_model::{unix_now, FrecencyStore};

/// xdg app_id — the compositor keys on this to slide it in and hide it from the
/// task switcher. Must match `SEARCH_APP_ID` in the compositor.
const APP_ID: &str = "chick.springchick.Search";
const DEFAULT_LIMIT: usize = 5;
const FILTER_LIMIT: usize = 8;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            // Deliberately NOT `.with_fullscreen(true)`. springchick maximizes
            // every toplevel anyway, so fullscreen buys nothing — and it is
            // actively harmful here: the compositor treats a fullscreen app as
            // media wanting landscape, so it configures the *swapped* size and
            // draws the window a quarter-turn round. That left search sized
            // 2088x1902 on a 1901x2088 panel: its blur region (clipped to the
            // surface) stopped short of the bottom of the screen, and a rotated
            // ghost of the search field was drawn over Home.
            // Translucent so the compositor's blurred Home backdrop shows
            // through (see `blur`).
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "springchick-search",
        options,
        Box::new(|cc| Ok(Box::new(SearchApp::new(cc)))),
    )
}

struct SearchApp {
    catalog: HashMap<String, AppEntry>,
    frecency: FrecencyStore,
    query: String,
    results: Vec<String>,
    textures: HashMap<String, egui::TextureHandle>,
    /// Icon search path, built once at startup rather than per icon lookup.
    icon_dirs: Vec<std::path::PathBuf>,
    focus_requested: bool,
    /// Kept alive for the process lifetime: dropping it drops the blur.
    _blur: Option<blur::ExtBackgroundEffectSurfaceV1>,
}

impl SearchApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let catalog: HashMap<String, AppEntry> = sc_catalog::scan_apps()
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect();
        // Frecency is read-only here; the compositor stays the sole writer.
        let frecency = sc_shell_model::persist::load(&sc_shell_model::persist::state_path())
            .map(|m| m.frecency)
            .unwrap_or_default();
        let mut app = Self {
            catalog,
            frecency,
            query: String::new(),
            results: Vec::new(),
            textures: HashMap::new(),
            icon_dirs: sc_icons::theme_dirs(&sc_catalog::xdg_data_dirs()),
            focus_requested: false,
            _blur: blur::blur_whole_window(cc),
        };
        app.recompute();
        app
    }

    fn recompute(&mut self) {
        let limit = if self.query.is_empty() {
            DEFAULT_LIMIT
        } else {
            FILTER_LIMIT
        };
        self.results = sc_catalog::rank(
            &self.catalog,
            &self.frecency,
            unix_now(),
            &self.query,
            limit,
        );
    }

    /// Lazily upload an app's icon into an egui texture.
    fn icon(&mut self, ctx: &egui::Context, id: &str) -> Option<egui::TextureHandle> {
        if let Some(t) = self.textures.get(id) {
            return Some(t.clone());
        }
        let entry = self.catalog.get(id)?;
        let px = sc_icons::resolve_with_dirs(&entry.icon, &self.icon_dirs);
        if px.width == 0 || px.height == 0 {
            return None;
        }
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [px.width as usize, px.height as usize],
            &px.data,
        );
        let tex = ctx.load_texture(id, image, egui::TextureOptions::LINEAR);
        self.textures.insert(id.to_string(), tex.clone());
        Some(tex)
    }

    /// Open `id` and quit.
    ///
    /// Preferably by asking the compositor over its control socket, so the
    /// launch goes through the same path as an icon tap: an already-running app
    /// is raised rather than started twice, and the window that appears is
    /// attributed to this app id instead of to whatever the client calls
    /// itself. Spawning it here directly is the fallback for running outside a
    /// springchick session.
    fn launch(&self, id: &str) {
        if ipc_launch(id) {
            std::process::exit(0);
        }
        if let Some(entry) = self.catalog.get(id) {
            if let Some(command) = sc_catalog::launch_command(entry) {
                if let Some((prog, args)) = command.argv.split_first() {
                    // WAYLAND_DISPLAY etc. are inherited from the compositor.
                    let mut builder = std::process::Command::new(prog);
                    if let Some(cwd) = &command.cwd {
                        builder.current_dir(cwd);
                    }
                    let _ = builder.args(args).spawn();
                }
            }
        }
        std::process::exit(0);
    }
}

/// Ask the running compositor to open `app_id`. True when it accepted.
///
/// Mirrors `springchick ipc launch <id>`: same socket resolution, same one-line
/// protocol. Any failure (no compositor, no socket, an error reply) returns
/// false so the caller can fall back to spawning the app itself.
fn ipc_launch(app_id: &str) -> bool {
    use std::io::{BufRead, BufReader, Write};

    let path = std::env::var("SPRINGCHICK_IPC_SOCK")
        .or_else(|_| std::env::var("SPRINGCHICK_DEBUG_SOCK"))
        .unwrap_or_else(|_| {
            let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
            format!("{dir}/springchick-ipc.sock")
        });
    let Ok(stream) = std::os::unix::net::UnixStream::connect(path) else {
        return false;
    };
    if writeln!(&stream, "launch {app_id}").is_err() {
        return false;
    }
    let mut reply = String::new();
    if BufReader::new(&stream).read_line(&mut reply).is_err() {
        return false;
    }
    reply.starts_with("ok")
}

impl eframe::App for SearchApp {
    /// Transparent clear so the compositor composites us over the blurred Home
    /// screen instead of over black.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            std::process::exit(0);
        }
        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));

        // Translucent panel over the blurred backdrop. Without the compositor's
        // blur this is still legible, just a plain dark scrim.
        let frame = egui::Frame::central_panel(&ctx.style())
            .fill(egui::Color32::from_rgba_unmultiplied(12, 14, 18, 170));
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            ui.add_space(24.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Search apps")
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Heading),
            );
            if !self.focus_requested {
                edit.request_focus();
                self.focus_requested = true;
            }
            if edit.changed() {
                self.recompute();
            }

            ui.add_space(16.0);

            let ids: Vec<String> = self.results.clone();
            let mut launch: Option<String> = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for id in &ids {
                    let name = self
                        .catalog
                        .get(id)
                        .map(|e| e.name.clone())
                        .unwrap_or_default();
                    let tex = self.icon(ctx, id);
                    let resp = ui.add(row_widget(tex.as_ref(), &name));
                    if resp.clicked() {
                        launch = Some(id.clone());
                    }
                }
            });

            if let Some(id) = launch {
                self.launch(&id);
            }
            if enter {
                if let Some(id) = ids.first() {
                    self.launch(id);
                }
            }
        });
    }
}

/// One result row: icon on the left, name filling the rest. Returns a clickable
/// response covering the whole row.
fn row_widget<'a>(tex: Option<&'a egui::TextureHandle>, name: &'a str) -> impl egui::Widget + 'a {
    move |ui: &mut egui::Ui| {
        let row_h = 64.0;
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_h),
            egui::Sense::click(),
        );
        if resp.hovered() {
            ui.painter()
                .rect_filled(rect, 8.0, ui.visuals().widgets.hovered.bg_fill);
        }
        let icon_side = 44.0;
        let icon_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 12.0, rect.center().y - icon_side / 2.0),
            egui::vec2(icon_side, icon_side),
        );
        if let Some(tex) = tex {
            ui.painter().image(
                tex.id(),
                icon_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        ui.painter().text(
            egui::pos2(icon_rect.right() + 16.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::proportional(22.0),
            ui.visuals().text_color(),
        );
        resp
    }
}
