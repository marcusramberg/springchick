//! Pull-down search: a standalone Wayland app the compositor spawns on the
//! Home pull-down. Ranks the catalog by frecency, filters as you type,
//! launches the pick.

mod blur;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui;
use sc_catalog::AppEntry;
use sc_shell_model::{unix_now, FrecencyStore};

/// Must match `SEARCH_APP_ID` in the compositor.
const APP_ID: &str = "chick.springchick.Search";
const DEFAULT_LIMIT: usize = 5;
/// Matches the compositor's `arrange::HOLD_MS`.
const HOLD_MS: u128 = 500;
/// Travel (egui points) beyond which a hold becomes a list scroll.
const HOLD_SLOP: f32 = 12.0;
const FILTER_LIMIT: usize = 8;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            // Not `.with_fullscreen(true)`: the compositor treats fullscreen as media
            // wanting landscape and would rotate us.
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
    icon_dirs: Vec<std::path::PathBuf>,
    focus_requested: bool,
    /// To spot the edge where we are raised again.
    focused: bool,
    /// Held row: id, start time, position. Dropped if the finger travels.
    held: Option<(String, Instant, egui::Pos2)>,
    _blur: Option<blur::ExtBackgroundEffectSurfaceV1>,
}

impl SearchApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self {
            catalog: HashMap::new(),
            frecency: FrecencyStore::default(),
            query: String::new(),
            results: Vec::new(),
            textures: HashMap::new(),
            icon_dirs: sc_icons::theme_dirs(&sc_catalog::xdg_data_dirs()),
            focus_requested: false,
            focused: true,
            held: None,
            _blur: blur::blur_whole_window(cc),
        };
        app.rescan();
        app
    }

    /// The compositor raises this process instead of respawning it.
    fn rescan(&mut self) {
        self.catalog = sc_catalog::scan_apps()
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect();
        // Read-only; the compositor is the sole writer.
        self.frecency = sc_shell_model::persist::load(&sc_shell_model::persist::state_path())
            .map(|m| m.frecency)
            .unwrap_or_default();
        self.recompute();
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

    /// Launches via the compositor so a running app is raised and the window is
    /// attributed to `id`. Spawns directly outside springchick.
    fn launch(&self, id: &str) {
        if ipc_launch(id) {
            std::process::exit(0);
        }
        if let Some(entry) = self.catalog.get(id) {
            if let Some(command) = sc_catalog::launch_command(entry) {
                if let Some((prog, args)) = command.argv.split_first() {
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

fn ipc_launch(app_id: &str) -> bool {
    ipc_cmd(&format!("launch {app_id}"))
}

/// On success the compositor has cancelled our touch and owns the drag, so
/// this process is done.
fn ipc_drag(app_id: &str) -> bool {
    ipc_cmd(&format!("drag {app_id}"))
}

/// Same protocol as `springchick ipc`. False on any failure.
fn ipc_cmd(line: &str) -> bool {
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
    if writeln!(&stream, "{line}").is_err() {
        return false;
    }
    let mut reply = String::new();
    if BufReader::new(&stream).read_line(&mut reply).is_err() {
        return false;
    }
    reply.starts_with("ok")
}

impl eframe::App for SearchApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            std::process::exit(0);
        }
        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));

        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if focused && !self.focused {
            self.rescan();
        }
        self.focused = focused;

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
            let mut drag: Option<String> = None;
            let mut still_held: Option<(String, Instant, egui::Pos2)> = None;
            let pointer = ctx.pointer_interact_pos();
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
                    if !resp.is_pointer_button_down_on() {
                        continue;
                    }
                    let Some(now_at) = pointer else { continue };
                    let (since, from) = match &self.held {
                        Some((held_id, at, from)) if held_id == id => (*at, *from),
                        _ => (Instant::now(), now_at),
                    };
                    if (now_at - from).length() > HOLD_SLOP {
                        continue;
                    }
                    if since.elapsed().as_millis() >= HOLD_MS {
                        drag = Some(id.clone());
                    } else {
                        still_held = Some((id.clone(), since, from));
                    }
                }
            });
            self.held = still_held;
            // A still finger produces no events; schedule the wake-up at the threshold.
            if let Some((_, since, _)) = &self.held {
                let remain = HOLD_MS.saturating_sub(since.elapsed().as_millis());
                ctx.request_repaint_after(Duration::from_millis(remain as u64));
            }

            if let Some(id) = drag {
                if ipc_drag(&id) {
                    std::process::exit(0);
                }
            } else if let Some(id) = launch {
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
