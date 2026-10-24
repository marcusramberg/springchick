//! Pull-down search: a standalone Wayland app the compositor spawns on the
//! Home pull-down. An app grid ranked by frecency and filtered as you type,
//! then one section per result provider (see `provider.rs`).

mod blur;
mod provider;

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align2, Color32, FontId, Pos2, Rect, Rounding, Sense, Stroke, Vec2};
use provider::{Hit, Provider};
use sc_catalog::AppEntry;
use sc_shell_model::{unix_now, FrecencyStore};

/// Must match `SEARCH_APP_ID` in the compositor.
const APP_ID: &str = "chick.springchick.Search";
const COLS: usize = 4;
/// Two rows of apps.
const APP_LIMIT: usize = 2 * COLS;
const HIT_LIMIT: usize = 6;
/// Matches the compositor's `arrange::HOLD_MS`.
const HOLD_MS: u128 = 500;
/// Travel (egui points) beyond which a hold becomes a list scroll.
const HOLD_SLOP: f32 = 12.0;
/// Providers shell out, so wait for the typing to pause.
const DEBOUNCE: Duration = Duration::from_millis(150);

const TEXT: Color32 = Color32::from_rgb(240, 242, 246);
const DIM: Color32 = Color32::from_rgb(150, 156, 168);
const FIELD: Color32 = Color32::from_rgba_premultiplied(22, 22, 22, 22);
const PRESS: Color32 = Color32::from_rgba_premultiplied(32, 32, 32, 32);

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

type Sections = Vec<(String, Vec<Hit>)>;

struct SearchApp {
    catalog: HashMap<String, AppEntry>,
    frecency: FrecencyStore,
    query: String,
    apps: Vec<String>,
    providers: Arc<Vec<Box<dyn Provider>>>,
    sections: Sections,
    /// Query edited at this instant; providers run once it is `DEBOUNCE` old.
    dirty: Option<Instant>,
    rx: Option<mpsc::Receiver<(String, Sections)>>,
    textures: HashMap<String, egui::TextureHandle>,
    icon_dirs: Vec<std::path::PathBuf>,
    focus_requested: bool,
    /// To spot the edge where we are raised again.
    focused: bool,
    /// Held tile: id, start time, position. Dropped if the finger travels.
    held: Option<(String, Instant, Pos2)>,
    _blur: Option<blur::ExtBackgroundEffectSurfaceV1>,
}

impl SearchApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Inter first; egui's own fonts stay behind it for what Inter lacks.
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("Inter".into(), egui::FontData::from_static(sc_fonts::INTER));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "Inter".into());
        cc.egui_ctx.set_fonts(fonts);
        let mut app = Self {
            catalog: HashMap::new(),
            frecency: FrecencyStore::default(),
            query: String::new(),
            apps: Vec::new(),
            providers: Arc::new(provider::detect()),
            sections: Vec::new(),
            dirty: None,
            rx: None,
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
        self.apps = sc_catalog::rank(
            &self.catalog,
            &self.frecency,
            unix_now(),
            &self.query,
            APP_LIMIT,
        );
        if self.query.trim().is_empty() {
            self.sections.clear();
            self.dirty = None;
        } else {
            self.dirty = Some(Instant::now());
        }
    }

    /// Runs the providers off-thread once typing has paused; stale replies are dropped.
    fn poll_providers(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.rx {
            while let Ok((q, sections)) = rx.try_recv() {
                if q == self.query {
                    self.sections = sections;
                }
            }
        }
        let Some(at) = self.dirty else { return };
        let elapsed = at.elapsed();
        if elapsed < DEBOUNCE {
            ctx.request_repaint_after(DEBOUNCE - elapsed);
            return;
        }
        self.dirty = None;
        if self.providers.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let query = self.query.clone();
        let providers = Arc::clone(&self.providers);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let sections = providers
                .iter()
                .map(|p| (p.title().to_string(), p.search(&query, HIT_LIMIT)))
                .filter(|(_, hits)| !hits.is_empty())
                .collect();
            let _ = tx.send((query, sections));
            ctx.request_repaint();
        });
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

    fn open_hit(&self, hit: &Hit) {
        use std::process::Stdio;
        let _ = std::process::Command::new("xdg-open")
            .arg(&hit.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        std::process::exit(0);
    }

    fn activate_first(&self) {
        if let Some(id) = self.apps.first() {
            self.launch(id);
        }
        if let Some(hit) = self.sections.first().and_then(|(_, h)| h.first()) {
            self.open_hit(hit);
        }
    }

    /// Returns the app to launch, or the one held long enough to drag.
    fn app_grid(&mut self, ui: &mut egui::Ui) -> (Option<String>, Option<String>) {
        let ids = self.apps.clone();
        let mut launch = None;
        let mut drag = None;
        let mut still_held = None;
        let pointer = ui.ctx().pointer_interact_pos();
        let cell_w = ui.available_width() / COLS as f32;
        let icon = 60.0;
        let cell_h = icon + 40.0;
        for row in ids.chunks(COLS) {
            ui.horizontal(|ui| {
                for id in row {
                    let (rect, resp) =
                        ui.allocate_exact_size(Vec2::new(cell_w, cell_h), Sense::click());
                    let name = self
                        .catalog
                        .get(id)
                        .map(|e| e.name.clone())
                        .unwrap_or_default();
                    let tex = self.icon(ui.ctx(), id);
                    let p = ui.painter();
                    if resp.is_pointer_button_down_on() {
                        p.rect_filled(rect.shrink(4.0), 16.0, PRESS);
                    }
                    let icon_rect = Rect::from_center_size(
                        Pos2::new(rect.center().x, rect.top() + 8.0 + icon / 2.0),
                        Vec2::splat(icon),
                    );
                    if let Some(tex) = tex {
                        p.image(
                            tex.id(),
                            icon_rect,
                            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    let label = truncate(&name, 12);
                    p.text(
                        Pos2::new(rect.center().x, icon_rect.bottom() + 8.0),
                        Align2::CENTER_TOP,
                        label,
                        FontId::proportional(13.0),
                        TEXT,
                    );

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
        }
        self.held = still_held;
        // A still finger produces no events; schedule the wake-up at the threshold.
        if let Some((_, since, _)) = &self.held {
            let remain = HOLD_MS.saturating_sub(since.elapsed().as_millis());
            ui.ctx()
                .request_repaint_after(Duration::from_millis(remain as u64));
        }
        (launch, drag)
    }

    /// Returns the tapped hit as (section, index).
    fn hit_sections(&self, ui: &mut egui::Ui) -> Option<(usize, usize)> {
        let mut open = None;
        for (si, (title, hits)) in self.sections.iter().enumerate() {
            ui.add_space(20.0);
            ui.label(
                egui::RichText::new(title)
                    .font(FontId::proportional(13.0))
                    .color(DIM),
            );
            ui.add_space(6.0);
            for (hi, hit) in hits.iter().enumerate() {
                let (rect, resp) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 56.0), Sense::click());
                let p = ui.painter();
                if resp.is_pointer_button_down_on() {
                    p.rect_filled(rect, 12.0, PRESS);
                }
                let glyph = Rect::from_center_size(
                    Pos2::new(rect.left() + 28.0, rect.center().y),
                    Vec2::splat(36.0),
                );
                p.rect_filled(glyph, 9.0, FIELD);
                let ext = hit
                    .path
                    .extension()
                    .map(|e| e.to_string_lossy().to_uppercase())
                    .unwrap_or_default();
                let tag = if hit.path.is_dir() {
                    "DIR".into()
                } else if ext.is_empty() {
                    "FILE".into()
                } else {
                    truncate(&ext, 4)
                };
                p.text(
                    glyph.center(),
                    Align2::CENTER_CENTER,
                    tag,
                    FontId::proportional(10.0),
                    DIM,
                );
                let x = glyph.right() + 14.0;
                p.text(
                    Pos2::new(x, rect.center().y - 2.0),
                    Align2::LEFT_BOTTOM,
                    &hit.title,
                    FontId::proportional(16.0),
                    TEXT,
                );
                p.text(
                    Pos2::new(x, rect.center().y + 2.0),
                    Align2::LEFT_TOP,
                    &hit.subtitle,
                    FontId::proportional(12.0),
                    DIM,
                );
                if resp.clicked() {
                    open = Some((si, hi));
                }
            }
        }
        open
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
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

/// Rounded pill with a magnifier glyph and a frameless text edit inside.
fn search_field(ui: &mut egui::Ui, query: &mut String) -> egui::Response {
    let h = 48.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, Rounding::same(h / 2.0), FIELD);
    let c = Pos2::new(rect.left() + 26.0, rect.center().y - 1.0);
    p.circle_stroke(c, 7.0, Stroke::new(2.0, DIM));
    p.line_segment(
        [c + Vec2::new(5.0, 5.0), c + Vec2::new(10.5, 10.5)],
        Stroke::new(2.0, DIM),
    );
    let edit_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 46.0, rect.top()),
        Pos2::new(rect.right() - 16.0, rect.bottom()),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(edit_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.add(
        egui::TextEdit::singleline(query)
            .frame(false)
            .hint_text(egui::RichText::new("Search").color(DIM))
            .text_color(TEXT)
            .desired_width(f32::INFINITY)
            .font(FontId::proportional(18.0)),
    )
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
        self.poll_providers(ctx);

        let frame = egui::Frame::central_panel(&ctx.style())
            .fill(Color32::from_rgba_unmultiplied(12, 14, 18, 170))
            .inner_margin(egui::Margin::symmetric(16.0, 0.0));
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            ui.add_space(20.0);
            let edit = search_field(ui, &mut self.query);
            if !self.focus_requested {
                edit.request_focus();
                self.focus_requested = true;
            }
            if edit.changed() {
                self.recompute();
            }
            ui.add_space(24.0);

            let (launch, drag, open) = egui::ScrollArea::vertical()
                .show(ui, |ui| {
                    let (launch, drag) = self.app_grid(ui);
                    (launch, drag, self.hit_sections(ui))
                })
                .inner;

            if let Some(id) = drag {
                if ipc_drag(&id) {
                    std::process::exit(0);
                }
            } else if let Some(id) = launch {
                self.launch(&id);
            } else if let Some((si, hi)) = open {
                self.open_hit(&self.sections[si].1[hi]);
            }
            if enter {
                self.activate_first();
            }
        });
    }
}
