//! springchick pull-down search: a standalone fullscreen Wayland app.
//!
//! Launched by the compositor on the Home pull-down gesture. As a normal xdg
//! toplevel it gets keyboard focus, touch, an on-screen keyboard (wvkbd/IME),
//! and a cursor for free — the compositor only spawns it and animates it in.
//! It ranks the app catalog by frecency (read-only from the shared state file),
//! filters by name as you type, and launches the pick (then exits).

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
            .with_fullscreen(true),
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
    focus_requested: bool,
}

impl SearchApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let catalog: HashMap<String, AppEntry> =
            sc_catalog::scan_apps().into_iter().map(|e| (e.id.clone(), e)).collect();
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
            focus_requested: false,
        };
        app.recompute();
        app
    }

    fn recompute(&mut self) {
        let limit = if self.query.is_empty() { DEFAULT_LIMIT } else { FILTER_LIMIT };
        self.results = sc_catalog::rank(&self.catalog, &self.frecency, unix_now(), &self.query, limit);
    }

    /// Lazily upload an app's icon into an egui texture.
    fn icon(&mut self, ctx: &egui::Context, id: &str) -> Option<egui::TextureHandle> {
        if let Some(t) = self.textures.get(id) {
            return Some(t.clone());
        }
        let entry = self.catalog.get(id)?;
        let px = sc_icons::resolve(&entry.icon);
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

    /// Launch `id` (strip field codes, spawn detached) and quit.
    fn launch(&self, id: &str) {
        if let Some(entry) = self.catalog.get(id) {
            let clean = sc_catalog::strip_field_codes(&entry.exec);
            let parts: Vec<&str> = clean.split_whitespace().collect();
            if let Some((prog, args)) = parts.split_first() {
                // WAYLAND_DISPLAY etc. are inherited from the compositor.
                let _ = std::process::Command::new(prog).args(args).spawn();
            }
        }
        std::process::exit(0);
    }
}

impl eframe::App for SearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            std::process::exit(0);
        }
        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));

        egui::CentralPanel::default().show(ctx, |ui| {
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
                    let name = self.catalog.get(id).map(|e| e.name.clone()).unwrap_or_default();
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
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, 8.0, ui.visuals().widgets.hovered.bg_fill);
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
