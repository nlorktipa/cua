//! The viewer: agents on the left, the selected agent's screen live on the right.

use crate::registry::{self, AgentInfo};
use crate::setup::{self, Setup};
use crate::wl::Client;
use anyhow::anyhow;
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle,
    TextureOptions, Vec2, epaint::RectShape,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(122, 162, 247);
const OK: Color32 = Color32::from_rgb(115, 201, 145);
const WARN: Color32 = Color32::from_rgb(224, 175, 104);
const BAD: Color32 = Color32::from_rgb(247, 118, 142);
const SIDEBAR: Color32 = Color32::from_rgb(22, 22, 27);
const MAIN: Color32 = Color32::from_rgb(15, 15, 19);
const CARD: Color32 = Color32::from_rgb(32, 32, 39);
const CARD_HOVER: Color32 = Color32::from_rgb(40, 40, 49);
const MUTED: Color32 = Color32::from_rgb(140, 140, 155);
const TEXT: Color32 = Color32::from_rgb(225, 225, 232);
const PLACEHOLDER: Color32 = Color32::from_rgb(12, 12, 15);

const ICON_PNG: &[u8] = include_bytes!("../assets/logo-256.png");
const LOGO_PNG: &[u8] = include_bytes!("../assets/logo-64.png");

#[derive(Clone)]
enum SetupState {
    Working,
    Done(Setup),
    Failed(String),
}

pub fn run() -> anyhow::Result<()> {
    let setup_state = Arc::new(Mutex::new(SetupState::Working));
    {
        let state = setup_state.clone();
        std::thread::spawn(move || {
            if let Err(e) = setup::install_desktop_entry() {
                eprintln!("cua: installing desktop entry failed: {e:#}");
            }
            let result = match setup::ensure_registered() {
                Ok(s) => SetupState::Done(s),
                Err(e) => SetupState::Failed(format!("{e:#}")),
            };
            *state.lock().unwrap() = result;
        });
    }
    let mut viewport = egui::ViewportBuilder::default();
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport: viewport
            .with_title("cua")
            .with_app_id("cua")
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "cua",
        options,
        Box::new(move |cc| {
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = MAIN;
            visuals.window_fill = MAIN;
            cc.egui_ctx.set_visuals(visuals);
            let logo = eframe::icon_data::from_png_bytes(LOGO_PNG).ok().map(|icon| {
                let image = ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
                cc.egui_ctx.load_texture("logo", image, TextureOptions::LINEAR)
            });
            Ok(Box::new(App::new(setup_state, logo)))
        }),
    )
    .map_err(|e| anyhow!("{e}"))
}

/// Frames from one agent's screen, captured on a background thread.
struct Feed {
    latest: Mutex<Option<Arc<ColorImage>>>,
    generation: AtomicU64,
    live: AtomicBool,
    stop: AtomicBool,
}

fn spawn_feed(display: String, ctx: egui::Context) -> Arc<Feed> {
    let feed = Arc::new(Feed {
        latest: Mutex::new(None),
        generation: AtomicU64::new(0),
        live: AtomicBool::new(false),
        stop: AtomicBool::new(false),
    });
    let f = feed.clone();
    std::thread::spawn(move || {
        let mut client: Option<Client> = None;
        let mut have_frame = false;
        while !f.stop.load(Ordering::Relaxed) {
            if client.is_none() {
                match Client::connect(&display) {
                    Ok(c) => {
                        client = Some(c);
                        have_frame = false;
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                }
            }
            let live = f.live.load(Ordering::Relaxed);
            let c = client.as_mut().unwrap();
            // Live view: wait for the screen to change instead of polling.
            let result = if live && have_frame {
                c.capture(true, Duration::from_millis(300))
            } else {
                c.capture(false, Duration::from_secs(2))
            };
            match result {
                Ok(Some(frame)) => {
                    let image = ColorImage::from_rgba_premultiplied(
                        [frame.width as usize, frame.height as usize],
                        &frame.rgba,
                    );
                    *f.latest.lock().unwrap() = Some(Arc::new(image));
                    f.generation.fetch_add(1, Ordering::Relaxed);
                    have_frame = true;
                    ctx.request_repaint();
                    if live {
                        std::thread::sleep(Duration::from_millis(16)); // cap at ~60 fps
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    client = None;
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
            if !live {
                // Thumbnails refresh every ~1.5 s, or immediately when selected.
                for _ in 0..30 {
                    if f.stop.load(Ordering::Relaxed) || f.live.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    });
    feed
}

struct Slot {
    display: String,
    feed: Arc<Feed>,
    texture: Option<TextureHandle>,
    generation: u64,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.feed.stop.store(true, Ordering::Relaxed);
    }
}

struct App {
    agents: Vec<AgentInfo>,
    last_scan: Option<Instant>,
    last_gc: Option<Instant>,
    selected: Option<String>,
    slots: HashMap<String, Slot>,
    setup: Arc<Mutex<SetupState>>,
    logo: Option<TextureHandle>,
}

impl App {
    fn new(setup: Arc<Mutex<SetupState>>, logo: Option<TextureHandle>) -> App {
        App { agents: Vec::new(), last_scan: None, last_gc: None, selected: None, slots: HashMap::new(), setup, logo }
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        if self.last_scan.is_some_and(|t| t.elapsed() < Duration::from_millis(400)) {
            return;
        }
        self.last_scan = Some(Instant::now());
        if self.last_gc.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            self.last_gc = Some(Instant::now());
            registry::gc();
        }
        self.agents = registry::list();
        let ready: HashMap<&str, &str> = self
            .agents
            .iter()
            .filter(|a| a.status == "ready")
            .filter_map(|a| Some((a.id.as_str(), a.wayland_display.as_deref()?)))
            .collect();
        self.slots.retain(|id, slot| ready.get(id.as_str()) == Some(&slot.display.as_str()));
        for (id, display) in ready {
            self.slots.entry(id.to_string()).or_insert_with(|| Slot {
                display: display.to_string(),
                feed: spawn_feed(display.to_string(), ctx.clone()),
                texture: None,
                generation: 0,
            });
        }
        if !self.selected.as_ref().is_some_and(|s| self.agents.iter().any(|a| &a.id == s)) {
            self.selected = self.agents.first().map(|a| a.id.clone());
        }
    }

    fn upload_textures(&mut self, ctx: &egui::Context) {
        for (id, slot) in &mut self.slots {
            slot.feed.live.store(self.selected.as_deref() == Some(id.as_str()), Ordering::Relaxed);
            let generation = slot.feed.generation.load(Ordering::Relaxed);
            if generation == slot.generation {
                continue;
            }
            slot.generation = generation;
            let Some(image) = slot.feed.latest.lock().unwrap().clone() else { continue };
            let image = Arc::unwrap_or_clone(image);
            match &mut slot.texture {
                Some(t) => t.set(image, TextureOptions::LINEAR),
                None => slot.texture = Some(ctx.load_texture(format!("screen-{id}"), image, TextureOptions::LINEAR)),
            }
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            if let Some(logo) = &self.logo {
                ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::splat(28.0)));
                ui.add_space(2.0);
            }
            ui.label(egui::RichText::new("cua").size(22.0).strong().color(TEXT));
        });
        ui.add_space(10.0);

        let setup_height = 40.0;
        let list_height = (ui.available_height() - setup_height).max(0.0);
        egui::ScrollArea::vertical().max_height(list_height).auto_shrink([false, false]).show(ui, |ui| {
            let agents = self.agents.clone();
            for agent in &agents {
                if self.card(ui, agent) {
                    self.selected = Some(agent.id.clone());
                }
                ui.add_space(8.0);
            }
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(12.0);
            let (dot, text, hint) = match &*self.setup.lock().unwrap() {
                SetupState::Working => (WARN, "Connecting to Claude Code…", None),
                SetupState::Done(Setup::Ready) => (OK, "Connected to Claude Code", None),
                SetupState::Done(Setup::Registered) => (
                    OK,
                    "Added to Claude Code",
                    Some("Run /mcp in Claude Code sessions that were already open.".to_string()),
                ),
                SetupState::Failed(e) => (BAD, "Couldn't connect to Claude Code", Some(e.clone())),
            };
            let row = ui
                .horizontal(|ui| {
                    ui.add_space(4.0);
                    status_dot(ui, dot);
                    ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(MUTED)).truncate());
                })
                .response;
            if let Some(hint) = hint {
                row.on_hover_text(hint);
            }
        });
    }

    /// One agent in the sidebar. Returns true when clicked.
    fn card(&self, ui: &mut egui::Ui, agent: &AgentInfo) -> bool {
        let width = ui.available_width();
        let thumb_w = width - 16.0;
        let thumb_h = thumb_w / aspect(agent);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, thumb_h + 40.0), Sense::click());
        let selected = self.selected.as_deref() == Some(agent.id.as_str());
        let p = ui.painter();
        p.rect_filled(rect, 10.0, if response.hovered() { CARD_HOVER } else { CARD });
        if selected {
            p.rect_stroke(rect, 10.0, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
        }
        let thumb = Rect::from_min_size(rect.min + Vec2::new(8.0, 8.0), Vec2::new(thumb_w, thumb_h));
        self.paint_screen(p, agent, thumb, 6.0, 12.0, false);

        let text_x = rect.min.x + 12.0;
        let y = thumb.max.y + 8.0;
        let (color, _) = status(agent);
        p.circle_filled(Pos2::new(text_x + 4.0, y + 9.0), 4.0, color);
        let text_w = rect.max.x - text_x - 12.0;
        text_elided(p, Pos2::new(text_x + 14.0, y + 1.0), &sidebar_label(agent), 14.5, TEXT, text_w - 14.0);
        response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
    }

    /// Draw an agent's latest frame (or a placeholder) into `rect`.
    /// With `detail`, the placeholder says what the screen is waiting for or why it failed.
    fn paint_screen(&self, p: &egui::Painter, agent: &AgentInfo, rect: Rect, radius: f32, font: f32, detail: bool) {
        let texture = self.slots.get(&agent.id).and_then(|s| s.texture.as_ref());
        match texture {
            Some(t) => {
                let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                p.add(RectShape::filled(rect, CornerRadius::same(radius as u8), Color32::WHITE).with_texture(t.id(), uv));
            }
            None => {
                p.rect_filled(rect, radius, PLACEHOLDER);
                if !detail {
                    return;
                }
                let failed = agent.status == "error";
                let text = if failed { "Screen failed to start" } else { "Starting screen…" };
                let color = if failed { BAD } else { MUTED };
                match agent.error.as_deref().filter(|_| failed) {
                    Some(error) => {
                        let width = (rect.width() - 48.0).clamp(0.0, 560.0);
                        let mut job = egui::text::LayoutJob::simple(
                            error.trim().to_string(),
                            FontId::proportional(13.0),
                            MUTED,
                            width,
                        );
                        job.halign = egui::Align::Center;
                        job.wrap.max_rows = 8;
                        let galley = p.layout_job(job);
                        let top = rect.center().y - (font + 12.0 + galley.size().y) / 2.0;
                        p.text(Pos2::new(rect.center().x, top), Align2::CENTER_TOP, text, FontId::proportional(font), color);
                        p.galley(Pos2::new(rect.center().x, top + font + 12.0), galley, MUTED);
                    }
                    None => {
                        p.text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(font), color);
                    }
                }
            }
        }
    }

    fn main_view(&self, ui: &mut egui::Ui) {
        let Some(agent) = self.selected.as_ref().and_then(|s| self.agents.iter().find(|a| &a.id == s)) else {
            empty_state(ui, self.logo.as_ref());
            return;
        };
        let full = ui.available_rect_before_wrap().shrink2(Vec2::new(22.0, 18.0));
        let p = ui.painter();

        // Header: the folder the agent works in on the left, status on the right.
        let (color, label) = status(agent);
        let pill_text = p.layout_no_wrap(label.to_string(), FontId::proportional(12.0), color);
        let pill = Rect::from_min_size(
            Pos2::new(full.max.x - pill_text.size().x - 20.0, full.min.y + 3.0),
            pill_text.size() + Vec2::new(20.0, 8.0),
        );
        p.rect_filled(pill, 20.0, color.gamma_multiply(0.16));
        p.galley(pill.min + Vec2::new(10.0, 4.0), pill_text, color);
        let header_w = pill.min.x - full.min.x - 16.0;
        text_elided(p, full.min + Vec2::new(0.0, 2.0), &tilde(&agent.cwd), 18.0, TEXT, header_w);

        // Screen, letterboxed to keep its aspect ratio.
        let area = Rect::from_min_max(full.min + Vec2::new(0.0, 44.0), full.max);
        let aspect = aspect(agent);
        let mut size = Vec2::new(area.width(), area.width() / aspect);
        if size.y > area.height() {
            size = Vec2::new(area.height() * aspect, area.height());
        }
        let screen = Rect::from_center_size(area.center(), size);
        p.rect_filled(screen.expand(1.0), 9.0, Color32::from_rgb(45, 45, 55));
        self.paint_screen(p, agent, screen, 8.0, 16.0, true);

        // Where the agent last pointed, as a fading ring.
        if let (Some((x, y)), Some(at)) = (agent.last_point, agent.last_action_at) {
            let age = registry::now_ms().saturating_sub(at) as f32 / 1000.0;
            if age < 1.6 {
                let s = screen.width() / agent.width.max(1) as f32;
                let center = screen.min + Vec2::new(x as f32 * s, y as f32 * s);
                let t = age / 1.6;
                let alpha = ((1.0 - t) * 255.0) as u8;
                p.circle_stroke(center, 6.0 + 22.0 * t, Stroke::new(2.5, Color32::from_rgba_unmultiplied(122, 162, 247, alpha)));
                p.circle_filled(center, 3.0, Color32::from_rgba_unmultiplied(122, 162, 247, alpha));
                ui.ctx().request_repaint();
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.refresh(&ctx);
        self.upload_textures(&ctx);

        let sidebar = (ctx.content_rect().width() * 0.27).clamp(220.0, 310.0);
        egui::Panel::left("agents")
            .resizable(false)
            .exact_size(sidebar)
            .frame(egui::Frame::NONE.fill(SIDEBAR).inner_margin(egui::Margin::symmetric(12, 0)))
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::no_frame().show(ui, |ui| {
            ui.painter().rect_filled(ui.max_rect(), 0.0, MAIN);
            self.main_view(ui);
        });
        // Keep agent list and "x seconds ago" labels fresh.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

fn empty_state(ui: &mut egui::Ui, logo: Option<&TextureHandle>) {
    let rect = ui.available_rect_before_wrap();
    let p = ui.painter();
    let width = (rect.width() - 64.0).clamp(0.0, 420.0);
    let centered = |text: &str, size: f32, color: Color32| {
        let mut job = egui::text::LayoutJob::simple(text.to_string(), FontId::proportional(size), color, width);
        job.halign = egui::Align::Center;
        p.layout_job(job)
    };
    let title = centered("No agents yet", 20.0, TEXT);
    let body = centered(
        "Ask Claude Code to do something on your computer. Each session gets its own screen, shown here live.",
        14.0,
        MUTED,
    );
    let logo_size = if logo.is_some() { 56.0 } else { 0.0 };
    let x = rect.center().x;
    let mut y = rect.center().y - (logo_size + 18.0 + title.size().y + 8.0 + body.size().y) / 2.0;
    if let Some(logo) = logo {
        let r = Rect::from_center_size(Pos2::new(x, y + logo_size / 2.0), Vec2::splat(logo_size));
        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        p.image(logo.id(), r, uv, Color32::WHITE.gamma_multiply(0.85));
        y += logo_size + 18.0;
    }
    let title_h = title.size().y;
    p.galley(Pos2::new(x, y), title, TEXT);
    p.galley(Pos2::new(x, y + title_h + 8.0), body, MUTED);
}

/// Draw text on one line, cut with "…" if wider than `max_width`. Returns the width drawn.
fn text_elided(p: &egui::Painter, pos: Pos2, text: &str, size: f32, color: Color32, max_width: f32) -> f32 {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), FontId::proportional(size), color);
    job.wrap = egui::text::TextWrapping { max_width, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = p.layout_job(job);
    let width = galley.size().x;
    p.galley(pos, galley, color);
    width
}

/// Width over height of an agent's screen; a screen that has not started yet has no size.
fn aspect(agent: &AgentInfo) -> f32 {
    if agent.width == 0 || agent.height == 0 { 1.6 } else { agent.width as f32 / agent.height as f32 }
}

/// A path with the home directory shortened to "~".
fn tilde(path: &str) -> String {
    let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).filter(|h| h.len() > 1);
    match home.as_deref().and_then(|h| path.strip_prefix(h)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn status(agent: &AgentInfo) -> (Color32, &'static str) {
    match agent.status.as_str() {
        "ready" => {
            let recent = agent.last_action_at.is_some_and(|t| registry::now_ms().saturating_sub(t) < 15_000);
            if recent { (OK, "Working") } else { (MUTED, "Idle") }
        }
        "starting" => (WARN, "Starting"),
        _ => (BAD, "Failed"),
    }
}

/// Sidebar card text: the agent's task, else its last action.
fn sidebar_label(agent: &AgentInfo) -> String {
    agent
        .task
        .clone()
        .or_else(|| agent.last_action.clone())
        .unwrap_or_else(|| "No task yet".into())
}
