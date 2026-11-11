//! The viewer: a Mac-style window with agents in the sidebar and the selected agent's screen live on the right.

use crate::registry::{self, AgentInfo, Control};
use crate::setup::{self, Setup};
use crate::wl::Client;
use anyhow::anyhow;
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, Event, FontFamily, FontId, Id, Key, Modifiers, Pos2, Rect,
    Sense, Shadow, Shape, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, UiBuilder, Vec2, ViewportCommand,
    PointerButton, epaint::RectShape,
};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ICON_PNG: &[u8] = include_bytes!("../assets/logo-256.png");
const LOGO_PNG: &[u8] = include_bytes!("../assets/logo-64.png");

const TOOLBAR_H: f32 = 52.0;
const SIDEBAR_W: f32 = 268.0;
const ROW_H: f32 = 56.0;
const FOOTER_H: f32 = 38.0;
/// Width of the sidebar toggle, from the window's left edge.
const CHROME_W: f32 = 56.0;
/// Width of the minimize / maximize / close buttons, plus a divider, at the window's right edge.
const CONTROLS_W: f32 = 120.0;

// ---------------------------------------------------------------------------------------------
// Palette: light and dark are tuned separately, not inverted.

#[derive(Clone, Copy)]
struct Palette {
    dark: bool,
    content: Color32,
    sidebar: Color32,
    stage: Color32,
    text: Color32,
    secondary: Color32,
    tertiary: Color32,
    hover: Color32,
    active: Color32,
    separator: Color32,
    field: Color32,
    hairline: Color32,
    accent: Color32,
    green: Color32,
    orange: Color32,
    red: Color32,
    elevated: Color32,
}

fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, (a * 255.0) as u8)
}

impl Palette {
    fn light() -> Palette {
        Palette {
            dark: false,
            content: Color32::from_rgb(255, 255, 255),
            sidebar: Color32::from_rgb(236, 236, 238),
            stage: Color32::from_rgb(246, 246, 248),
            text: Color32::from_rgb(29, 29, 31),
            secondary: Color32::from_rgb(110, 110, 115),
            tertiary: Color32::from_rgb(160, 160, 166),
            hover: rgba(0, 0, 0, 0.045),
            active: rgba(0, 0, 0, 0.085),
            separator: rgba(0, 0, 0, 0.09),
            field: rgba(0, 0, 0, 0.055),
            hairline: rgba(0, 0, 0, 0.12),
            accent: Color32::from_rgb(0, 122, 255),
            green: Color32::from_rgb(52, 199, 89),
            orange: Color32::from_rgb(255, 149, 0),
            red: Color32::from_rgb(255, 59, 48),
            elevated: Color32::from_rgb(255, 255, 255),
        }
    }

    fn dark() -> Palette {
        Palette {
            dark: true,
            content: Color32::from_rgb(30, 30, 30),
            sidebar: Color32::from_rgb(40, 40, 42),
            stage: Color32::from_rgb(24, 24, 25),
            text: Color32::from_rgb(245, 245, 247),
            secondary: Color32::from_rgb(152, 152, 157),
            tertiary: Color32::from_rgb(99, 99, 102),
            hover: rgba(255, 255, 255, 0.06),
            active: rgba(255, 255, 255, 0.11),
            separator: rgba(0, 0, 0, 0.55),
            field: rgba(255, 255, 255, 0.075),
            hairline: rgba(255, 255, 255, 0.10),
            accent: Color32::from_rgb(10, 132, 255),
            green: Color32::from_rgb(48, 209, 88),
            orange: Color32::from_rgb(255, 159, 10),
            red: Color32::from_rgb(255, 69, 58),
            elevated: Color32::from_rgb(44, 44, 46),
        }
    }

    fn visuals(&self) -> egui::Visuals {
        let mut v = if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        v.panel_fill = self.content;
        v.window_fill = self.elevated;
        v.extreme_bg_color = self.content;
        v.override_text_color = None;
        v.weak_text_color = Some(self.tertiary);
        v.selection.bg_fill = self.accent.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, self.text);
        v.text_cursor.stroke = Stroke::new(1.5, self.accent);
        v.window_corner_radius = CornerRadius::same(8);
        v.window_stroke = Stroke::new(0.5, self.hairline);
        v.window_shadow = Shadow { offset: [0, 4], blur: 16, spread: 0, color: rgba(0, 0, 0, if self.dark { 0.45 } else { 0.14 }) };
        v.popup_shadow = v.window_shadow;
        v
    }
}

// ---------------------------------------------------------------------------------------------
// Fonts: the system's UI sans (SF Pro, Inter or Noto Sans when present) in a regular and an emphasis weight.

fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Ask fontconfig for `pattern`; accept the match only if it really is `family`.
fn fc_match(family: &str, pattern: &str, styles: &[&str]) -> Option<Vec<u8>> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{family[0]}\t%{style[0]}\t%{file}", pattern]).output().ok()?;
    let line = String::from_utf8(out.stdout).ok()?;
    let mut parts = line.splitn(3, '\t');
    let (got_family, style, file) = (parts.next()?, parts.next()?, parts.next()?);
    if !got_family.eq_ignore_ascii_case(family) {
        return None;
    }
    if !styles.is_empty() && !styles.iter().any(|s| style.to_ascii_lowercase().contains(s)) {
        return None;
    }
    std::fs::read(file).ok()
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let proportional = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    fonts.families.insert(FontFamily::Name("semibold".into()), proportional);
    for family in ["SF Pro Text", "SF Pro", "Inter", "Noto Sans", "Cantarell"] {
        let Some(body) = fc_match(family, &format!("{family}:style=Regular"), &["regular", "book"]) else { continue };
        let strong = fc_match(family, &format!("{family}:style=SemiBold"), &["semibold", "semi bold"])
            .or_else(|| fc_match(family, &format!("{family}:style=Medium"), &["medium"]));
        fonts.font_data.insert("ui".into(), Arc::new(egui::FontData::from_owned(body)));
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "ui".into());
        let strong_name = if let Some(strong) = strong {
            fonts.font_data.insert("ui-strong".into(), Arc::new(egui::FontData::from_owned(strong)));
            "ui-strong"
        } else {
            "ui"
        };
        fonts.families.entry(FontFamily::Name("semibold".into())).or_default().insert(0, strong_name.into());
        break;
    }
    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------------------------------------

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
            .with_decorations(false)
            .with_inner_size([1240.0, 800.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "cua",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals_of(egui::Theme::Light, Palette::light().visuals());
            cc.egui_ctx.set_visuals_of(egui::Theme::Dark, Palette::dark().visuals());
            install_fonts(&cc.egui_ctx);
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

/// Input from the viewer to an agent's screen while the user has taken it over.
/// Positions are fractions of the screen size.
enum Input {
    Move(f32, f32),
    Button(u32, bool, String),
    Scroll(i32, i32),
    Text(String),
    Combo(String),
}

/// The user's mouse and keyboard on one agent's screen.
struct Remote {
    id: String,
    tx: Sender<Input>,
    /// Buttons pressed on the screen and not yet released.
    down: HashSet<u32>,
    /// Wheel movement not yet sent as whole clicks.
    scroll: Vec2,
}

fn spawn_remote(display: String) -> Sender<Input> {
    let (tx, rx) = channel::<Input>();
    std::thread::spawn(move || drive_remote(&display, rx));
    tx
}

fn drive_remote(display: &str, rx: Receiver<Input>) {
    let mut client: Option<Client> = None;
    let mut held: HashSet<u32> = HashSet::new();
    for input in rx.iter() {
        if client.is_none() {
            client = Client::connect(display).ok();
        }
        let Some(c) = client.as_mut() else { continue };
        let (w, h) = c.size();
        let result = match input {
            Input::Move(x, y) => c.move_to(x as f64 * w as f64, y as f64 * h as f64),
            Input::Button(button, pressed, mods) => {
                let mut r = Ok(());
                if pressed && !mods.is_empty() {
                    r = c.hold_modifiers(&mods);
                }
                if pressed {
                    held.insert(button);
                } else {
                    held.remove(&button);
                }
                r.and_then(|_| c.button(button, pressed)).and_then(|_| if pressed || mods.is_empty() { Ok(()) } else { c.hold_modifiers("") })
            }
            Input::Scroll(dx, dy) => c.scroll(dx, dy),
            Input::Text(text) => c.type_text(&text),
            Input::Combo(combo) => c.key_combo(&combo),
        };
        if let Err(e) = result {
            eprintln!("cua: input to {display} failed: {e:#}");
            client = None;
        }
    }
    // The user gave control back: let go of anything still pressed.
    if let Some(c) = client.as_mut() {
        for button in held {
            let _ = c.button(button, false);
        }
        let _ = c.hold_modifiers("");
    }
}

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// Keysym name for a key that is not sent as text.
fn key_name(key: Key) -> Option<String> {
    use Key::*;
    let name = match key {
        ArrowDown => "Down",
        ArrowLeft => "Left",
        ArrowRight => "Right",
        ArrowUp => "Up",
        Escape => "Escape",
        Tab => "Tab",
        Backspace => "BackSpace",
        Enter => "Return",
        Insert => "Insert",
        Delete => "Delete",
        Home => "Home",
        End => "End",
        PageUp => "Prior",
        PageDown => "Next",
        Space => "space",
        Minus => "minus",
        Plus => "plus",
        Equals => "equal",
        Comma => "comma",
        Period => "period",
        Slash => "slash",
        Backslash => "backslash",
        Semicolon => "semicolon",
        Colon => "colon",
        Quote => "apostrophe",
        Backtick => "grave",
        OpenBracket => "bracketleft",
        CloseBracket => "bracketright",
        _ => {
            let n = key.name();
            return if n.chars().count() == 1 {
                Some(n.to_lowercase())
            } else if n.starts_with('F') && n[1..].chars().all(|c| c.is_ascii_digit()) {
                Some(n.to_string())
            } else {
                None
            };
        }
    };
    Some(name.to_string())
}

/// Keys that type no text, so they are sent even without modifiers.
fn is_command_key(key: Key) -> bool {
    use Key::*;
    matches!(key, ArrowDown | ArrowLeft | ArrowRight | ArrowUp | Escape | Tab | Backspace | Enter | Insert | Delete | Home | End | PageUp | PageDown)
        || key_name(key).is_some_and(|n| n.len() > 1 && n.starts_with('F') && n[1..].chars().all(|c| c.is_ascii_digit()))
}

fn mods_spec(m: Modifiers) -> String {
    let mut parts = Vec::new();
    if m.ctrl || m.command {
        parts.push("ctrl");
    }
    if m.alt {
        parts.push("alt");
    }
    if m.shift {
        parts.push("shift");
    }
    parts.join("+")
}

/// Something the user asked for from the toolbar, a context menu or a shortcut.
#[derive(Clone)]
enum Action {
    Stop(String),
    Resume(String),
    TakeOver(String),
    GiveBack(String),
    AskRemove(String),
    Copy(String),
    OpenFolder(String),
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

struct Toast {
    text: String,
    at: Instant,
}

const TOAST_SECS: f32 = 2.2;

struct App {
    agents: Vec<AgentInfo>,
    last_scan: Option<Instant>,
    last_gc: Option<Instant>,
    selected: Option<String>,
    slots: HashMap<String, Slot>,
    setup: Arc<Mutex<SetupState>>,
    logo: Option<TextureHandle>,
    query: String,
    sidebar_open: bool,
    shortcuts_open: bool,
    focus_search: bool,
    toast: Option<Toast>,
    /// Whether the window is maximized, as Hyprland reports it.
    maximized: bool,
    /// The selected agent's screen, while the user has taken it over.
    remote: Option<Remote>,
    /// Tab presses kept from egui (which would move its focus) during a takeover.
    tabs: Vec<Modifiers>,
    /// Agent waiting for the user to confirm its removal.
    confirm_remove: Option<String>,
    actions: Vec<Action>,
}

impl App {
    fn new(setup: Arc<Mutex<SetupState>>, logo: Option<TextureHandle>) -> App {
        App {
            agents: Vec::new(),
            last_scan: None,
            last_gc: None,
            selected: None,
            slots: HashMap::new(),
            setup,
            logo,
            query: String::new(),
            sidebar_open: true,
            shortcuts_open: false,
            focus_search: false,
            toast: None,
            maximized: false,
            remote: None,
            tabs: Vec::new(),
            confirm_remove: None,
            actions: Vec::new(),
        }
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
        self.maximized = crate::hypr::own_window_maximized()
            .unwrap_or_else(|| ctx.input(|i| i.viewport().maximized.unwrap_or(false)));
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

    fn selected_agent(&self) -> Option<&AgentInfo> {
        self.selected.as_ref().and_then(|s| self.agents.iter().find(|a| &a.id == s))
    }

    /// Agents matching the search field, in sidebar order.
    fn visible_agents(&self) -> Vec<AgentInfo> {
        let q = self.query.trim().to_lowercase();
        self.agents
            .iter()
            .filter(|a| {
                q.is_empty()
                    || [Some(&a.name), a.task.as_ref(), Some(&a.cwd), a.last_action.as_ref()]
                        .into_iter()
                        .flatten()
                        .any(|s| s.to_lowercase().contains(&q))
            })
            .cloned()
            .collect()
    }

    fn show_toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), at: Instant::now() });
    }

    fn copy_screen(&mut self, ctx: &egui::Context, id: &str) {
        let image = self.slots.get(id).and_then(|s| s.feed.latest.lock().unwrap().clone());
        match image {
            Some(image) => {
                ctx.copy_image(Arc::unwrap_or_clone(image));
                self.show_toast("Screenshot copied");
            }
            None => self.show_toast("Nothing to copy yet"),
        }
    }

    fn open_folder(&mut self, id: &str) {
        let Some(cwd) = self.agents.iter().find(|a| a.id == id).map(|a| a.cwd.clone()) else { return };
        match std::process::Command::new("xdg-open").arg(&cwd).spawn() {
            Ok(_) => self.show_toast(format!("Opened {}", short_name(&cwd))),
            Err(_) => self.show_toast("Couldn't open folder"),
        }
    }

    fn agent(&self, id: &str) -> Option<&AgentInfo> {
        self.agents.iter().find(|a| a.id == id)
    }

    /// Tell an agent what the user wants, and show it right away.
    fn set_control(&mut self, id: &str, control: Control) -> bool {
        if let Err(e) = registry::set_control(id, control) {
            self.show_toast(format!("Couldn't reach the agent: {e}"));
            return false;
        }
        if let Some(a) = self.agents.iter_mut().find(|a| a.id == id) {
            a.control = control;
        }
        true
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Stop(id) => {
                if self.set_control(&id, Control::Stop) {
                    self.show_toast("Agent stopped");
                }
            }
            Action::Resume(id) | Action::GiveBack(id) => {
                let was = self.agent(&id).map(|a| a.control);
                if self.set_control(&id, Control::Run) {
                    self.show_toast(if was == Some(Control::Takeover) { "Agent continues" } else { "Agent resumed" });
                }
            }
            Action::TakeOver(id) => {
                if self.agent(&id).is_some_and(|a| a.status == "ready") && self.set_control(&id, Control::Takeover) {
                    self.selected = Some(id);
                    self.show_toast("You're in control");
                }
            }
            Action::AskRemove(id) => self.confirm_remove = Some(id),
            Action::Copy(id) => self.copy_screen(ctx, &id),
            Action::OpenFolder(id) => self.open_folder(&id),
        }
    }

    fn remove_agent(&mut self, id: &str) {
        if self.set_control(id, Control::Close) {
            let index = self.visible_agents().iter().position(|a| a.id == id);
            self.agents.retain(|a| a.id != id);
            self.slots.remove(id);
            if self.selected.as_deref() == Some(id) {
                let visible = self.visible_agents();
                self.selected = index.and_then(|i| visible.get(i.min(visible.len().saturating_sub(1)))).map(|a| a.id.clone());
            }
            self.show_toast("Agent removed");
        }
    }

    /// The selected agent, when the user has taken it over.
    fn taken_over(&self) -> Option<&AgentInfo> {
        self.selected_agent().filter(|a| a.control == Control::Takeover && a.status == "ready")
    }

    /// Keep an input connection to the taken-over agent's screen, and only to it.
    fn sync_remote(&mut self) {
        let want = self.taken_over().and_then(|a| Some((a.id.clone(), a.wayland_display.clone()?)));
        match (&self.remote, want) {
            (Some(r), Some((id, _))) if r.id == id => {}
            (_, Some((id, display))) => {
                self.remote = Some(Remote { id, tx: spawn_remote(display), down: HashSet::new(), scroll: Vec2::ZERO });
            }
            (_, None) => self.remote = None,
        }
    }

    /// Send the user's mouse and keyboard to the taken-over screen drawn at `screen`.
    fn forward_input(&mut self, ctx: &egui::Context, screen: Rect) {
        let keyboard = ctx.memory(|m| m.focused().is_none()) && self.confirm_remove.is_none() && !self.shortcuts_open;
        let tabs = std::mem::take(&mut self.tabs);
        let Some(remote) = &mut self.remote else { return };
        let events = ctx.input(|i| i.events.clone());
        if keyboard {
            for m in tabs {
                let mods = mods_spec(m);
                let _ = remote.tx.send(Input::Combo(if mods.is_empty() { "Tab".into() } else { format!("{mods}+Tab") }));
            }
        }
        let to_screen = |p: Pos2| {
            let v = (p - screen.min) / screen.size();
            Input::Move(v.x.clamp(0.0, 1.0), v.y.clamp(0.0, 1.0))
        };
        let send = |input: Input| {
            let _ = remote.tx.send(input);
        };
        let hovering = ctx.pointer_hover_pos().is_some_and(|p| screen.contains(p));
        if hovering {
            ctx.set_cursor_icon(egui::CursorIcon::Default);
        }
        for event in events {
            match event {
                Event::PointerMoved(p) if screen.contains(p) || !remote.down.is_empty() => send(to_screen(p)),
                Event::PointerButton { pos, button, pressed, modifiers } => {
                    let code = match button {
                        PointerButton::Primary => BTN_LEFT,
                        PointerButton::Secondary => BTN_RIGHT,
                        PointerButton::Middle => BTN_MIDDLE,
                        _ => continue,
                    };
                    if pressed && screen.contains(pos) {
                        remote.down.insert(code);
                        send(to_screen(pos));
                        send(Input::Button(code, true, mods_spec(modifiers)));
                    } else if !pressed && remote.down.remove(&code) {
                        send(Input::Button(code, false, mods_spec(modifiers)));
                    }
                }
                Event::MouseWheel { unit, delta, .. } if hovering => {
                    let clicks = match unit {
                        egui::MouseWheelUnit::Point => delta / 50.0,
                        egui::MouseWheelUnit::Line => delta,
                        egui::MouseWheelUnit::Page => delta * 10.0,
                    };
                    remote.scroll -= clicks;
                    let (dx, dy) = (remote.scroll.x.trunc(), remote.scroll.y.trunc());
                    if dx != 0.0 || dy != 0.0 {
                        remote.scroll -= Vec2::new(dx, dy);
                        send(Input::Scroll(dx as i32, dy as i32));
                    }
                }
                Event::Text(text) if keyboard => send(Input::Text(text)),
                Event::Paste(text) if keyboard => send(Input::Text(text)),
                Event::Copy if keyboard => send(Input::Combo("ctrl+c".into())),
                Event::Cut if keyboard => send(Input::Combo("ctrl+x".into())),
                Event::Key { key, pressed: true, modifiers, .. } if keyboard => {
                    if !(modifiers.ctrl || modifiers.command || modifiers.alt || is_command_key(key)) {
                        continue; // arrives as text
                    }
                    let Some(name) = key_name(key) else { continue };
                    let mods = mods_spec(modifiers);
                    send(Input::Combo(if mods.is_empty() { name } else { format!("{mods}+{name}") }));
                }
                _ => {}
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context, search_focused: bool) {
        // While the user drives an agent's screen, every key but one goes there.
        if self.confirm_remove.is_none() && !self.shortcuts_open && !search_focused && let Some(id) = self.taken_over().map(|a| a.id.clone()) {
            if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Period)) {
                self.actions.push(Action::GiveBack(id));
            }
            return;
        }
        if let Some(id) = self.confirm_remove.clone() {
            let (cancel, confirm) = ctx.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::Escape), i.consume_key(Modifiers::NONE, Key::Enter)));
            if confirm {
                self.confirm_remove = None;
                self.remove_agent(&id);
            } else if cancel {
                self.confirm_remove = None;
            }
            return;
        }
        let cmd = Modifiers::COMMAND;
        let (stop, take, remove) = ctx.input_mut(|i| {
            (
                i.consume_key(cmd, Key::Period),
                i.consume_key(cmd, Key::T),
                !search_focused && (i.consume_key(cmd, Key::Backspace) || i.consume_key(cmd, Key::Delete)),
            )
        });
        let (find, sidebar, help, open, close, esc, up, down, copy, numbers) = ctx.input_mut(|i| {
            let numbers: Vec<bool> = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9]
                .into_iter()
                .map(|k| i.consume_key(cmd, k))
                .collect();
            (
                i.consume_key(cmd, Key::F) || i.consume_key(cmd, Key::K),
                i.consume_key(cmd, Key::B),
                i.consume_key(cmd, Key::Slash) || i.consume_key(cmd | Modifiers::SHIFT, Key::Slash),
                i.consume_key(cmd, Key::O),
                i.consume_key(cmd, Key::W) || i.consume_key(cmd, Key::Q),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                !search_focused && i.events.iter().any(|e| matches!(e, Event::Copy)),
                numbers,
            )
        });

        if close {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if help {
            self.shortcuts_open = !self.shortcuts_open;
        }
        if esc {
            if self.shortcuts_open {
                self.shortcuts_open = false;
            } else if !self.query.is_empty() {
                self.query.clear();
            } else if search_focused {
                ctx.memory_mut(|m| m.surrender_focus(Id::unique("search")));
            }
        }
        if self.shortcuts_open {
            return;
        }
        if find {
            self.sidebar_open = true;
            self.focus_search = true;
        }
        if sidebar {
            self.sidebar_open = !self.sidebar_open;
        }
        if let Some(agent) = self.selected_agent() {
            let id = agent.id.clone();
            if stop {
                self.actions.push(if agent.control == Control::Run { Action::Stop(id.clone()) } else { Action::Resume(id.clone()) });
            }
            if take {
                self.actions.push(Action::TakeOver(id.clone()));
            }
            if remove {
                self.actions.push(Action::AskRemove(id.clone()));
            }
            if open {
                self.actions.push(Action::OpenFolder(id.clone()));
            }
            if copy {
                self.actions.push(Action::Copy(id));
            }
        }
        let visible = self.visible_agents();
        if let Some(n) = numbers.iter().position(|&b| b)
            && let Some(a) = visible.get(n)
        {
            self.selected = Some(a.id.clone());
        }
        if (up || down) && !visible.is_empty() {
            let current = visible.iter().position(|a| Some(&a.id) == self.selected.as_ref());
            let next = match (current, down) {
                (None, _) => 0,
                (Some(i), true) => (i + 1).min(visible.len() - 1),
                (Some(i), false) => i.saturating_sub(1),
            };
            self.selected = Some(visible[next].id.clone());
        }
    }

    // -----------------------------------------------------------------------------------------
    // Sidebar

    fn sidebar(&mut self, ui: &mut Ui, pal: &Palette, rect: Rect) -> bool {
        let p = ui.painter().clone();
        p.rect_filled(rect, 0.0, pal.sidebar);
        p.line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, pal.separator));

        let has_agents = !self.agents.is_empty();
        let mut y = rect.min.y + TOOLBAR_H;
        let mut search_focused = false;

        if has_agents {
            // Search field.
            let field = Rect::from_min_size(Pos2::new(rect.min.x + 12.0, y), Vec2::new(rect.width() - 24.0, 28.0));
            let focused_anim = ui.ctx().animate_bool_with_time(Id::unique("search-focus"), ui.ctx().memory(|m| m.has_focus(Id::unique("search"))), 0.15);
            p.rect_filled(field, 7.0, pal.field);
            if focused_anim > 0.0 {
                p.rect_stroke(field.expand(1.5), 8.5, Stroke::new(3.0, pal.accent.gamma_multiply(0.45 * focused_anim)), StrokeKind::Outside);
            }
            icon_search(&p, Pos2::new(field.min.x + 15.0, field.center().y), pal.secondary);
            let edit_rect = Rect::from_min_max(Pos2::new(field.min.x + 28.0, field.min.y), Pos2::new(field.max.x - 26.0, field.max.y));
            let edit = egui::TextEdit::singleline(&mut self.query)
                .id(Id::unique("search"))
                .hint_text("Search")
                .frame(egui::Frame::NONE)
                .font(regular(13.0))
                .text_color(pal.text)
                .align(Align2::LEFT_CENTER)
                .desired_width(edit_rect.width());
            let response = ui.put(edit_rect, edit);
            if self.focus_search {
                response.request_focus();
                self.focus_search = false;
            }
            search_focused = response.has_focus();
            if !self.query.is_empty() {
                let clear = Rect::from_center_size(Pos2::new(field.max.x - 14.0, field.center().y), Vec2::splat(16.0));
                let r = ui.interact(clear, Id::unique("search-clear"), Sense::click());
                p.circle_filled(clear.center(), 7.0, if r.hovered() { pal.secondary } else { pal.tertiary });
                icon_x(&p, clear.center(), 2.6, pal.sidebar, 1.4);
                if r.clicked() {
                    self.query.clear();
                }
            } else if !search_focused {
                kbd(&p, Pos2::new(field.max.x - 6.0, field.center().y), &["Ctrl", "F"], pal);
            }
            y += 28.0 + 18.0;

            // Section header.
            let visible = self.visible_agents();
            p.text(Pos2::new(rect.min.x + 20.0, y), Align2::LEFT_CENTER, "Agents", semibold(11.0), pal.tertiary);
            p.text(Pos2::new(rect.max.x - 20.0, y), Align2::RIGHT_CENTER, visible.len().to_string(), regular(11.0), pal.tertiary);
            y += 12.0;

            // Rows.
            let list = Rect::from_min_max(Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, rect.max.y - FOOTER_H));
            let mut list_ui = ui.new_child(UiBuilder::new().max_rect(list).id_salt("agent-list"));
            list_ui.set_clip_rect(list.intersect(ui.clip_rect()));
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut list_ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
                for agent in &visible {
                    let (clicked, action) = self.row(ui, pal, agent);
                    if clicked {
                        self.selected = Some(agent.id.clone());
                    }
                    self.actions.extend(action);
                }
                if visible.is_empty() {
                    ui.add_space(24.0);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("No Results").font(semibold(13.0)).color(pal.secondary));
                        ui.label(egui::RichText::new(format!("Nothing matches “{}”.", self.query.trim())).font(regular(11.5)).color(pal.tertiary));
                    });
                }
            });
        }

        // Footer: connection to Claude Code, and the shortcuts sheet.
        let footer = Rect::from_min_max(Pos2::new(rect.min.x, rect.max.y - FOOTER_H), rect.max);
        p.line_segment([footer.left_top() + Vec2::new(12.0, 0.0), footer.right_top() - Vec2::new(12.0, 0.0)], Stroke::new(1.0, pal.hairline.gamma_multiply(0.7)));
        let (dot, text, hint) = match &*self.setup.lock().unwrap() {
            SetupState::Working => (pal.orange, "Connecting to Claude Code…", None),
            SetupState::Done(Setup::Ready) => (pal.green, "Connected to Claude Code", None),
            SetupState::Done(Setup::Registered) => {
                (pal.green, "Added to Claude Code", Some("Run /mcp in Claude Code sessions that were already open.".to_string()))
            }
            SetupState::Failed(e) => (pal.red, "Couldn't connect to Claude Code", Some(e.clone())),
        };
        let cy = footer.center().y;
        p.circle_filled(Pos2::new(footer.min.x + 22.0, cy), 3.5, dot);
        let help_rect = Rect::from_center_size(Pos2::new(footer.max.x - 24.0, cy), Vec2::splat(24.0));
        text_elided(&p, Pos2::new(footer.min.x + 32.0, cy), Align2::LEFT_CENTER, text, regular(11.5), pal.secondary, help_rect.min.x - footer.min.x - 40.0);
        let status_r = ui.interact(Rect::from_min_max(footer.min, Pos2::new(help_rect.min.x - 4.0, footer.max.y)), Id::unique("footer-status"), Sense::hover());
        if let Some(hint) = hint {
            status_r.on_hover_text(hint);
        }
        if icon_button(ui, pal, help_rect, "help", "Keyboard Shortcuts   Ctrl /", |p, c, col| {
            p.circle_stroke(c, 7.0, Stroke::new(1.3, col));
            p.text(c + Vec2::new(0.0, 0.5), Align2::CENTER_CENTER, "?", semibold(10.5), col);
        })
        .clicked()
        {
            self.shortcuts_open = true;
        }
        search_focused
    }

    /// One agent in the sidebar. Returns whether it was clicked, and what its context menu asked for.
    fn row(&self, ui: &mut Ui, pal: &Palette, agent: &AgentInfo) -> (bool, Option<Action>) {
        let width = ui.available_width();
        let (outer, response) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
        let rect = outer.shrink2(Vec2::new(10.0, 0.0));
        let selected = self.selected.as_deref() == Some(agent.id.as_str());
        let ctx = ui.ctx().clone();
        let menu_open = response.context_menu_opened();
        let hover = ctx.animate_bool_with_time(Id::unique(("row-hover", &agent.id)), response.hovered() || menu_open, 0.12);
        let sel = ctx.animate_bool_with_time(Id::unique(("row-sel", &agent.id)), selected, 0.16);
        let p = ui.painter();
        if hover > 0.0 {
            p.rect_filled(rect, 8.0, pal.hover.gamma_multiply(hover * (1.0 - sel)));
        }
        if sel > 0.0 {
            p.rect_filled(rect, 8.0, pal.active.gamma_multiply(sel));
        }
        if menu_open {
            // The row a context menu belongs to gets an accent outline, as in Finder.
            p.rect_stroke(rect.shrink(1.0), 7.0, Stroke::new(2.0, pal.accent), StrokeKind::Inside);
        }
        if agent.control == Control::Takeover {
            p.rect_stroke(rect.shrink(1.0), 7.0, Stroke::new(1.0, pal.accent.gamma_multiply(0.5)), StrokeKind::Inside);
        }

        // Thumbnail.
        let thumb_h = 36.0;
        let thumb_w = (thumb_h * aspect(agent)).clamp(48.0, 64.0);
        let thumb = Rect::from_min_size(Pos2::new(rect.min.x + 8.0, rect.center().y - thumb_h / 2.0), Vec2::new(thumb_w, thumb_h));
        self.paint_screen(p, pal, agent, thumb, 5.0, None);
        p.rect_stroke(thumb, 5.0, Stroke::new(0.5, pal.hairline), StrokeKind::Outside);

        // Text.
        let (color, label) = status(agent, pal);
        let x = thumb.max.x + 10.0;
        let right = rect.max.x - 10.0;
        let time = relative_time(agent);
        let time_w = if time.is_empty() { 0.0 } else { p.layout_no_wrap(time.clone(), regular(11.0), pal.tertiary).size().x + 8.0 };
        let title_y = rect.center().y - 9.0;
        text_elided(p, Pos2::new(x, title_y), Align2::LEFT_CENTER, &row_title(agent), semibold(13.0), pal.text, right - x - time_w);
        if !time.is_empty() {
            p.text(Pos2::new(right, title_y), Align2::RIGHT_CENTER, time, regular(11.0), pal.tertiary);
        }
        let sub_y = rect.center().y + 9.0;
        p.circle_filled(Pos2::new(x + 3.0, sub_y), 3.0, color);
        text_elided(p, Pos2::new(x + 11.0, sub_y), Align2::LEFT_CENTER, &format!("{label} · {}", agent.name), regular(11.5), pal.secondary, right - x - 11.0);

        let mut action = None;
        response.context_menu(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(0.0, 0.0);
            let id = agent.id.clone();
            let ready = agent.status == "ready";
            let pick = |a: &mut Option<Action>, clicked: bool, act: Action| {
                if clicked {
                    *a = Some(act);
                }
            };
            match agent.control {
                Control::Takeover => pick(&mut action, menu_item(ui, pal, "Give Back Control", "Ctrl .", false, true), Action::GiveBack(id.clone())),
                Control::Stop => pick(&mut action, menu_item(ui, pal, "Resume Agent", "Ctrl .", false, true), Action::Resume(id.clone())),
                _ => pick(&mut action, menu_item(ui, pal, "Stop Agent", "Ctrl .", false, true), Action::Stop(id.clone())),
            }
            if agent.control != Control::Takeover {
                pick(&mut action, menu_item(ui, pal, "Take Over Screen", "Ctrl T", false, ready), Action::TakeOver(id.clone()));
            }
            menu_separator(ui, pal);
            pick(&mut action, menu_item(ui, pal, "Copy Screenshot", "", false, self.slots.get(&id).is_some_and(|s| s.texture.is_some())), Action::Copy(id.clone()));
            pick(&mut action, menu_item(ui, pal, "Open Folder", "", false, true), Action::OpenFolder(id.clone()));
            menu_separator(ui, pal);
            pick(&mut action, menu_item(ui, pal, "Remove from Sidebar…", "Ctrl ⌫", true, true), Action::AskRemove(id));
        });
        (response.clicked(), action)
    }

    /// Draw an agent's latest frame (or a placeholder) into `rect`.
    /// With `detail`, the placeholder says what the screen is waiting for or why it failed.
    fn paint_screen(&self, p: &egui::Painter, pal: &Palette, agent: &AgentInfo, rect: Rect, radius: f32, detail: Option<f32>) {
        let texture = self.slots.get(&agent.id).and_then(|s| s.texture.as_ref());
        if let Some(t) = texture {
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            p.add(RectShape::filled(rect, CornerRadius::same(radius as u8), Color32::WHITE).with_texture(t.id(), uv));
            return;
        }
        p.rect_filled(rect, radius, if pal.dark { Color32::from_rgb(18, 18, 19) } else { Color32::from_rgb(228, 228, 232) });
        let Some(font) = detail else { return };
        let failed = agent.status == "error";
        let title = if failed { "Screen Failed to Start" } else { "Starting Screen…" };
        let mut y = rect.center().y - 10.0;
        if failed {
            let c = Pos2::new(rect.center().x, y - 34.0);
            p.circle_stroke(c, 14.0, Stroke::new(1.6, pal.red));
            p.line_segment([c - Vec2::new(0.0, 6.0), c + Vec2::new(0.0, 2.0)], Stroke::new(1.8, pal.red));
            p.circle_filled(c + Vec2::new(0.0, 6.5), 1.2, pal.red);
        } else {
            spinner(p, Pos2::new(rect.center().x, y - 30.0), 10.0, pal.secondary, p.ctx().input(|i| i.time));
            p.ctx().request_repaint_after(Duration::from_millis(33));
        }
        p.text(Pos2::new(rect.center().x, y), Align2::CENTER_CENTER, title, semibold(font), pal.text);
        y += font;
        if let Some(error) = agent.error.as_deref().filter(|_| failed) {
            let width = (rect.width() - 48.0).clamp(0.0, 520.0);
            let mut job = egui::text::LayoutJob::simple(error.trim().to_string(), regular(12.0), pal.secondary, width);
            job.halign = egui::Align::Center;
            job.wrap.max_rows = 8;
            let galley = p.layout_job(job);
            p.galley(Pos2::new(rect.center().x, y), galley, pal.secondary);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Content: toolbar + live screen

    fn content(&mut self, ui: &mut Ui, pal: &Palette, rect: Rect, title_x: f32) {
        let p = ui.painter().clone();
        p.rect_filled(rect, 0.0, pal.stage);
        let toolbar = Rect::from_min_size(rect.min, Vec2::new(rect.width(), TOOLBAR_H));
        p.rect_filled(toolbar, 0.0, pal.content);
        p.line_segment([toolbar.left_bottom(), toolbar.right_bottom()], Stroke::new(1.0, pal.separator));

        let Some(agent) = self.selected_agent().cloned() else {
            self.empty_state(&p, pal, Rect::from_min_max(Pos2::new(rect.min.x, toolbar.max.y), rect.max));
            return;
        };

        // Toolbar, right side: control buttons, status badge and actions.
        let cy = toolbar.center().y;
        let divider_x = toolbar.max.x - CONTROLS_W + 8.0;
        p.line_segment([Pos2::new(divider_x, cy - 9.0), Pos2::new(divider_x, cy + 9.0)], Stroke::new(1.0, pal.hairline));
        let mut right = divider_x - 8.0;
        let folder = Rect::from_center_size(Pos2::new(right - 14.0, cy), Vec2::splat(28.0));
        right = folder.min.x - 4.0;
        let copy = Rect::from_center_size(Pos2::new(right - 14.0, cy), Vec2::splat(28.0));
        right = copy.min.x - 10.0;
        p.line_segment([Pos2::new(right, cy - 9.0), Pos2::new(right, cy + 9.0)], Stroke::new(1.0, pal.hairline));
        right -= 10.0;

        let id = agent.id.clone();
        let ready = agent.status == "ready";
        let buttons: Vec<(&str, Glyph, bool, &str, Action)> = match agent.control {
            Control::Takeover => vec![("Give Back", Glyph::Play, true, "Hand the screen back to the agent   Ctrl .", Action::GiveBack(id.clone()))],
            Control::Stop => {
                let mut b = vec![("Resume", Glyph::Play, true, "Let the agent continue   Ctrl .", Action::Resume(id.clone()))];
                if ready {
                    b.insert(0, ("Take Over", Glyph::Pointer, false, "Use this screen yourself   Ctrl T", Action::TakeOver(id.clone())));
                }
                b
            }
            _ => {
                let mut b = vec![("Stop", Glyph::Stop, false, "Stop the agent from using the screen   Ctrl .", Action::Stop(id.clone()))];
                if ready {
                    b.insert(0, ("Take Over", Glyph::Pointer, false, "Stop the agent and use its screen yourself   Ctrl T", Action::TakeOver(id.clone())));
                }
                b
            }
        };
        for (label, glyph, primary, tip, action) in buttons.into_iter().rev() {
            let w = pill_width(&p, label);
            let r = Rect::from_min_max(Pos2::new(right - w, cy - 13.0), Pos2::new(right, cy + 13.0));
            if pill_button(ui, pal, r, label, glyph, primary, tip).clicked() {
                self.actions.push(action);
            }
            right = r.min.x - 8.0;
        }
        right -= 4.0;

        // The badge goes first when the title would get too short.
        let (color, label) = status(&agent, pal);
        let badge_text = p.layout_no_wrap(label.to_string(), semibold(11.0), color);
        let badge = Rect::from_min_size(
            Pos2::new(right - badge_text.size().x - 26.0, cy - 11.0),
            Vec2::new(badge_text.size().x + 26.0, 22.0),
        );
        let title_right = if badge.min.x - title_x > 180.0 {
            p.rect_filled(badge, 11.0, color.gamma_multiply(if pal.dark { 0.18 } else { 0.13 }));
            p.circle_filled(Pos2::new(badge.min.x + 11.0, cy), 3.0, color);
            p.galley(Pos2::new(badge.min.x + 18.0, cy - badge_text.size().y / 2.0), badge_text, color);
            badge.min.x
        } else {
            right
        };

        // Toolbar, left side: what the agent is doing, and where.
        let text_w = (title_right - title_x - 16.0).max(0.0);
        text_elided(&p, Pos2::new(title_x, cy - 8.0), Align2::LEFT_CENTER, &row_title(&agent), semibold(13.5), pal.text, text_w);
        text_elided(&p, Pos2::new(title_x, cy + 9.0), Align2::LEFT_CENTER, &tilde(&agent.cwd), regular(11.0), pal.secondary, text_w);

        if icon_button(ui, pal, copy, "copy", "Copy Screenshot   Ctrl C", |p, c, col| {
            let back = Rect::from_center_size(c + Vec2::new(-2.0, -2.0), Vec2::new(10.0, 11.0));
            let front = back.translate(Vec2::new(4.0, 4.0));
            p.rect_stroke(back, 2.5, Stroke::new(1.3, col), StrokeKind::Middle);
            p.rect_filled(front, 2.5, pal.content);
            p.rect_stroke(front, 2.5, Stroke::new(1.3, col), StrokeKind::Middle);
        })
        .clicked()
        {
            self.actions.push(Action::Copy(id.clone()));
        }
        if icon_button(ui, pal, folder, "folder", "Open Folder   Ctrl O", |p, c, col| icon_folder(p, c, col)).clicked() {
            self.actions.push(Action::OpenFolder(id.clone()));
        }

        // Screen, letterboxed to keep its aspect ratio, floating on the stage.
        let caption_h = 30.0;
        let area = Rect::from_min_max(
            Pos2::new(rect.min.x + 28.0, toolbar.max.y + 24.0),
            Pos2::new(rect.max.x - 28.0, rect.max.y - caption_h - 10.0),
        );
        if area.width() <= 0.0 || area.height() <= 0.0 {
            return;
        }
        let aspect = aspect(&agent);
        let mut size = Vec2::new(area.width(), area.width() / aspect);
        if size.y > area.height() {
            size = Vec2::new(area.height() * aspect, area.height());
        }
        let screen = Rect::from_center_size(area.center(), size);
        let shadow_alpha = if pal.dark { 0.5 } else { 0.16 };
        p.add(Shadow { offset: [0, 10], blur: 40, spread: 0, color: rgba(0, 0, 0, shadow_alpha) }.as_shape(screen, 10));
        p.add(Shadow { offset: [0, 1], blur: 4, spread: 0, color: rgba(0, 0, 0, shadow_alpha * 0.8) }.as_shape(screen, 10));
        self.paint_screen(&p, pal, &agent, screen, 10.0, Some(15.0));
        p.rect_stroke(screen, 10.0, Stroke::new(0.5, pal.hairline), StrokeKind::Outside);

        let control = agent.control;
        let taking = ui.ctx().animate_bool_with_time(Id::unique(("takeover", &id)), control == Control::Takeover, 0.2);
        if taking > 0.0 {
            p.rect_stroke(screen.expand(2.0), 12.0, Stroke::new(2.5, pal.accent.gamma_multiply(taking)), StrokeKind::Outside);
        }
        if control == Control::Takeover && ready {
            self.forward_input(ui.ctx(), screen);
        }
        let stopped = ui.ctx().animate_bool_with_time(Id::unique(("stopped", &id)), control == Control::Stop, 0.2);
        if stopped > 0.0 {
            self.stopped_overlay(ui, pal, screen, &agent, stopped);
        }

        // Where the agent last pointed, as a fading ring.
        if let (Some((x, y)), Some(at)) = (agent.last_point, agent.last_action_at) {
            let age = registry::now_ms().saturating_sub(at) as f32 / 1000.0;
            if age < 1.6 {
                let s = screen.width() / agent.width.max(1) as f32;
                let center = screen.min + Vec2::new(x as f32 * s, y as f32 * s);
                let t = age / 1.6;
                let fade = 1.0 - t;
                p.circle_filled(center, 6.0 + 20.0 * t, pal.accent.gamma_multiply(0.18 * fade));
                p.circle_stroke(center, 6.0 + 20.0 * t, Stroke::new(2.0, pal.accent.gamma_multiply(fade)));
                p.circle_filled(center, 3.5, pal.accent.gamma_multiply(fade));
                ui.ctx().request_repaint();
            }
        }

        // Caption under the screen: who is in control, else its resolution.
        let caption_y = screen.max.y + 10.0 + caption_h / 2.0;
        if control == Control::Takeover {
            let text = p.layout_no_wrap("You control this screen; the agent waits.   Give back with".into(), regular(11.5), pal.secondary);
            let keys = ["Ctrl", "."];
            let keys_w: f32 = keys.iter().map(|k| (p.layout_no_wrap(k.to_string(), regular(10.5), pal.secondary).size().x + 10.0).max(18.0) + 3.0).sum::<f32>() - 3.0;
            let total = 14.0 + text.size().x + 8.0 + keys_w;
            if total < screen.width() {
                let x = screen.center().x - total / 2.0;
                p.circle_filled(Pos2::new(x + 4.0, caption_y), 4.0, pal.accent);
                p.galley(Pos2::new(x + 14.0, caption_y - text.size().y / 2.0), text.clone(), pal.secondary);
                kbd(&p, Pos2::new(x + total, caption_y), &keys, pal);
            }
            return;
        }
        if agent.width == 0 {
            return;
        }
        let caption = format!("{} × {}", agent.width, agent.height);
        text_elided(
            &p,
            Pos2::new(screen.center().x, caption_y),
            Align2::CENTER_CENTER,
            &caption,
            regular(11.0),
            pal.tertiary,
            screen.width(),
        );
    }

    /// Dims a stopped agent's screen and offers the way back.
    fn stopped_overlay(&mut self, ui: &mut Ui, pal: &Palette, screen: Rect, agent: &AgentInfo, a: f32) {
        let p = ui.painter().clone();
        p.rect_filled(screen, 10.0, rgba(0, 0, 0, 0.38 * a));
        if a < 1.0 {
            ui.ctx().request_repaint();
        }
        let ready = agent.status == "ready";
        let card_w = 300.0;
        let card_h = 158.0;
        if screen.width() < card_w + 24.0 || screen.height() < card_h + 24.0 {
            return;
        }
        let card = Rect::from_center_size(screen.center() + Vec2::new(0.0, 6.0 * (1.0 - a)), Vec2::new(card_w, card_h));
        p.add(Shadow { offset: [0, 10], blur: 36, spread: 0, color: rgba(0, 0, 0, 0.3 * a) }.as_shape(card, 14));
        p.rect_filled(card, 14.0, pal.elevated.gamma_multiply(a));
        p.rect_stroke(card, 14.0, Stroke::new(0.5, pal.hairline.gamma_multiply(a)), StrokeKind::Inside);
        let c = Pos2::new(card.center().x, card.min.y + 34.0);
        p.circle_filled(c, 16.0, pal.orange.gamma_multiply(0.16 * a));
        for dx in [-3.5, 3.5] {
            p.rect_filled(Rect::from_center_size(c + Vec2::new(dx, 0.0), Vec2::new(3.5, 12.0)), 1.2, pal.orange.gamma_multiply(a));
        }
        p.text(Pos2::new(card.center().x, card.min.y + 66.0), Align2::CENTER_CENTER, "Agent Stopped", semibold(14.0), pal.text.gamma_multiply(a));
        p.text(Pos2::new(card.center().x, card.min.y + 86.0), Align2::CENTER_CENTER, "Claude Code waits until you resume it.", regular(12.0), pal.secondary.gamma_multiply(a));
        if a < 0.99 {
            return;
        }
        let by = card.max.y - 30.0;
        let resume_w = pill_width(&p, "Resume");
        let take_w = pill_width(&p, "Take Over");
        let total = if ready { resume_w + 8.0 + take_w } else { resume_w };
        let mut x = card.center().x - total / 2.0;
        let id = agent.id.clone();
        if ready {
            let r = Rect::from_min_size(Pos2::new(x, by - 13.0), Vec2::new(take_w, 26.0));
            if pill_button(ui, pal, r, "Take Over", Glyph::Pointer, false, "Use this screen yourself   Ctrl T").clicked() {
                self.actions.push(Action::TakeOver(id.clone()));
            }
            x = r.max.x + 8.0;
        }
        let r = Rect::from_min_size(Pos2::new(x, by - 13.0), Vec2::new(resume_w, 26.0));
        if pill_button(ui, pal, r, "Resume", Glyph::Play, true, "Let the agent continue   Ctrl .").clicked() {
            self.actions.push(Action::Resume(id));
        }
    }

    fn empty_state(&self, p: &egui::Painter, pal: &Palette, rect: Rect) {
        let width = (rect.width() - 64.0).clamp(0.0, 380.0);
        let centered = |text: &str, font: FontId, color: Color32| {
            let mut job = egui::text::LayoutJob::simple(text.to_string(), font, color, width);
            job.halign = egui::Align::Center;
            p.layout_job(job)
        };
        let title = centered("No Agents", semibold(17.0), pal.text);
        let body = centered(
            "Ask Claude Code to do something on your computer. Each session gets its own screen, and shows up here live.",
            regular(13.0),
            pal.secondary,
        );
        let hint = centered("Try: “Open Firefox and check the weather”", regular(12.0), pal.tertiary);
        let logo = 64.0;
        let total = logo + 20.0 + title.size().y + 6.0 + body.size().y + 18.0 + hint.size().y;
        let x = rect.center().x;
        let mut y = rect.center().y - total / 2.0;
        if let Some(l) = &self.logo {
            let r = Rect::from_center_size(Pos2::new(x, y + logo / 2.0), Vec2::splat(logo));
            p.image(l.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        y += logo + 20.0;
        let (th, bh) = (title.size().y, body.size().y);
        p.galley(Pos2::new(x, y), title, pal.text);
        y += th + 6.0;
        p.galley(Pos2::new(x, y), body, pal.secondary);
        y += bh + 18.0;
        let hint_rect = Rect::from_center_size(Pos2::new(x, y + hint.size().y / 2.0), hint.size() + Vec2::new(24.0, 12.0));
        p.rect_filled(hint_rect, 8.0, pal.field);
        p.galley(Pos2::new(x, y), hint, pal.tertiary);
    }

    // -----------------------------------------------------------------------------------------
    // Window chrome, overlays

    /// Minimize, maximize and close, right-aligned at `right`.
    fn window_controls(&self, ui: &mut Ui, pal: &Palette, right: Pos2) {
        let ctx = ui.ctx().clone();
        let maximized = self.maximized;
        let close = Rect::from_center_size(Pos2::new(right.x - 14.0, right.y), Vec2::splat(28.0));
        let max = close.translate(Vec2::new(-32.0, 0.0));
        let min = max.translate(Vec2::new(-32.0, 0.0));

        if icon_button(ui, pal, min, "minimize", "Minimize", |p, c, col| {
            p.line_segment([c - Vec2::new(5.0, 0.0), c + Vec2::new(5.0, 0.0)], Stroke::new(1.3, col));
        })
        .clicked()
        {
            minimize_window(&ctx);
        }
        if icon_button(ui, pal, max, "maximize", if maximized { "Restore" } else { "Maximize" }, |p, c, col| {
            if maximized {
                let back = Rect::from_center_size(c + Vec2::new(1.5, -1.5), Vec2::splat(8.0));
                p.rect_stroke(back, 1.5, Stroke::new(1.3, col), StrokeKind::Middle);
                let front = back.translate(Vec2::new(-3.0, 3.0));
                p.rect_filled(front, 1.5, pal.content);
                p.rect_stroke(front, 1.5, Stroke::new(1.3, col), StrokeKind::Middle);
            } else {
                p.rect_stroke(Rect::from_center_size(c, Vec2::splat(10.0)), 2.0, Stroke::new(1.3, col), StrokeKind::Middle);
            }
        })
        .clicked()
        {
            toggle_maximize(&ctx);
        }

        // Close turns red on hover.
        let r = ui.interact(close, Id::unique(("icon-button", "close")), Sense::click());
        let hover = ctx.animate_bool_with_time(r.id.with("hover"), r.hovered(), 0.12);
        let p = ui.painter();
        if hover > 0.0 {
            let fill = if r.is_pointer_button_down_on() { pal.red.gamma_multiply(0.8) } else { pal.red };
            p.rect_filled(close, 6.0, fill.gamma_multiply(hover));
        }
        let glyph = if r.hovered() { Color32::WHITE } else { pal.secondary };
        icon_x(p, close.center(), 4.5, glyph, 1.3);
        if r.on_hover_text("Close   Ctrl W").clicked() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn toast(&mut self, ctx: &egui::Context, pal: &Palette, content: Rect) {
        let Some(toast) = &self.toast else { return };
        let t = toast.at.elapsed().as_secs_f32();
        if t > TOAST_SECS {
            self.toast = None;
            return;
        }
        let enter = ease_out((t / 0.22).min(1.0));
        let exit = ease_out(((TOAST_SECS - t) / 0.25).min(1.0));
        let alpha = enter.min(exit);
        let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::unique("toast")));
        let galley = p.layout_no_wrap(toast.text.clone(), semibold(12.5), pal.text);
        let size = Vec2::new(galley.size().x + 50.0, 36.0);
        let center = Pos2::new(content.center().x, content.max.y - 40.0 + 14.0 * (1.0 - alpha));
        let r = Rect::from_center_size(center, size);
        p.add(Shadow { offset: [0, 6], blur: 24, spread: 0, color: rgba(0, 0, 0, if pal.dark { 0.5 } else { 0.16 } * alpha) }.as_shape(r, 18));
        p.rect_filled(r, 18.0, pal.elevated.gamma_multiply(alpha));
        p.rect_stroke(r, 18.0, Stroke::new(0.5, pal.hairline.gamma_multiply(alpha)), StrokeKind::Inside);
        let ic = Pos2::new(r.min.x + 20.0, center.y);
        p.circle_filled(ic, 8.0, pal.green.gamma_multiply(alpha));
        p.add(Shape::line(
            vec![ic + Vec2::new(-3.5, 0.2), ic + Vec2::new(-1.0, 2.7), ic + Vec2::new(3.6, -2.4)],
            Stroke::new(1.6, Color32::WHITE.gamma_multiply(alpha)),
        ));
        p.galley(Pos2::new(r.min.x + 34.0, center.y - galley.size().y / 2.0), galley, pal.text.gamma_multiply(alpha));
        ctx.request_repaint();
    }

    /// Asks before removing an agent, since its screen and apps close.
    fn remove_sheet(&mut self, ctx: &egui::Context, pal: &Palette, screen: Rect) {
        let open = self.confirm_remove.is_some();
        let anim = ctx.animate_bool_with_time(Id::unique("remove-sheet"), open, 0.18);
        if anim <= 0.0 {
            return;
        }
        let title = self
            .confirm_remove
            .as_deref()
            .and_then(|id| self.agent(id))
            .map(row_title)
            .unwrap_or_default();
        let a = ease_out(anim);
        egui::Area::new(Id::unique("remove-area")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            let backdrop = ui.allocate_rect(screen, Sense::click());
            let p = ui.painter().clone();
            p.rect_filled(screen, 0.0, rgba(0, 0, 0, if pal.dark { 0.45 } else { 0.22 } * a));
            let w = 340.0;
            let mut body = egui::text::LayoutJob::simple(
                "Its screen and the apps on it close, and Claude Code is told to stop using the computer.".into(),
                regular(12.5),
                pal.secondary.gamma_multiply(a),
                w - 48.0,
            );
            body.halign = egui::Align::Center;
            let body = p.layout_job(body);
            let h = 70.0 + 24.0 + body.size().y + 24.0 + 28.0 + 20.0;
            let panel = Rect::from_center_size(screen.center() + Vec2::new(0.0, 10.0 * (1.0 - a)), Vec2::new(w, h) * (0.97 + 0.03 * a));
            p.add(Shadow { offset: [0, 18], blur: 60, spread: 0, color: rgba(0, 0, 0, if pal.dark { 0.6 } else { 0.22 } * a) }.as_shape(panel, 14));
            p.rect_filled(panel, 14.0, pal.elevated.gamma_multiply(a));
            p.rect_stroke(panel, 14.0, Stroke::new(0.5, pal.hairline.gamma_multiply(a)), StrokeKind::Inside);
            let c = Pos2::new(panel.center().x, panel.min.y + 40.0);
            p.circle_filled(c, 20.0, pal.red.gamma_multiply(0.14 * a));
            icon_trash(&p, c, pal.red.gamma_multiply(a));
            let mut y = panel.min.y + 80.0;
            text_elided(&p, Pos2::new(panel.center().x, y), Align2::CENTER_CENTER, &format!("Remove “{title}”?"), semibold(14.0), pal.text.gamma_multiply(a), w - 40.0);
            y += 16.0;
            p.galley(Pos2::new(panel.center().x, y), body.clone(), pal.secondary);
            let by = panel.max.y - 20.0 - 14.0;
            let bw = (w - 40.0 - 10.0) / 2.0;
            let cancel = Rect::from_min_size(Pos2::new(panel.min.x + 20.0, by - 14.0), Vec2::new(bw, 28.0));
            let remove = cancel.translate(Vec2::new(bw + 10.0, 0.0));
            if open && a > 0.98 {
                if sheet_button(ui, pal, cancel, "Cancel", false).clicked() {
                    self.confirm_remove = None;
                } else if sheet_button(ui, pal, remove, "Remove", true).clicked()
                    && let Some(id) = self.confirm_remove.take()
                {
                    self.remove_agent(&id);
                }
            }
            if backdrop.clicked() && !panel.contains(backdrop.interact_pointer_pos().unwrap_or(panel.center())) {
                self.confirm_remove = None;
            }
        });
    }

    fn shortcuts_sheet(&mut self, ctx: &egui::Context, pal: &Palette, screen: Rect) {
        let anim = ctx.animate_bool_with_time(Id::unique("sheet"), self.shortcuts_open, 0.18);
        if anim <= 0.0 {
            return;
        }
        let a = ease_out(anim);
        egui::Area::new(Id::unique("sheet-area")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            let backdrop = ui.allocate_rect(screen, Sense::click());
            let p = ui.painter();
            p.rect_filled(screen, 0.0, rgba(0, 0, 0, if pal.dark { 0.45 } else { 0.22 } * a));

            let rows: &[(&str, &[&str])] = &[
                ("Search agents", &["Ctrl", "F"]),
                ("Next / previous agent", &["↓", "↑"]),
                ("Jump to agent", &["Ctrl", "1–9"]),
                ("Copy screenshot", &["Ctrl", "C"]),
                ("Open agent's folder", &["Ctrl", "O"]),
                ("Stop / resume agent", &["Ctrl", "."]),
                ("Take over screen", &["Ctrl", "T"]),
                ("Give back control", &["Ctrl", "."]),
                ("Remove agent", &["Ctrl", "⌫"]),
                ("Show or hide sidebar", &["Ctrl", "B"]),
                ("Keyboard shortcuts", &["Ctrl", "/"]),
                ("Close window", &["Ctrl", "W"]),
            ];
            let w = 380.0;
            let h = 64.0 + rows.len() as f32 * 34.0 + 16.0;
            let panel = Rect::from_center_size(screen.center() + Vec2::new(0.0, 10.0 * (1.0 - a)), Vec2::new(w, h) * (0.97 + 0.03 * a));
            p.add(Shadow { offset: [0, 18], blur: 60, spread: 0, color: rgba(0, 0, 0, if pal.dark { 0.6 } else { 0.22 } * a) }.as_shape(panel, 12));
            p.rect_filled(panel, 12.0, pal.elevated.gamma_multiply(a));
            p.rect_stroke(panel, 12.0, Stroke::new(0.5, pal.hairline.gamma_multiply(a)), StrokeKind::Inside);
            p.text(panel.min + Vec2::new(20.0, 32.0), Align2::LEFT_CENTER, "Keyboard Shortcuts", semibold(15.0), pal.text.gamma_multiply(a));
            p.text(panel.right_top() + Vec2::new(-20.0, 32.0), Align2::RIGHT_CENTER, "esc", regular(11.0), pal.tertiary.gamma_multiply(a));
            let mut y = panel.min.y + 64.0;
            for (i, (label, keys)) in rows.iter().enumerate() {
                if i > 0 {
                    p.line_segment([Pos2::new(panel.min.x + 20.0, y), Pos2::new(panel.max.x - 20.0, y)], Stroke::new(0.5, pal.hairline.gamma_multiply(a)));
                }
                let cy = y + 17.0;
                p.text(Pos2::new(panel.min.x + 20.0, cy), Align2::LEFT_CENTER, *label, regular(13.0), pal.text.gamma_multiply(a));
                kbd(p, Pos2::new(panel.max.x - 20.0, cy), keys, pal);
                y += 34.0;
            }
            if backdrop.clicked() && !panel.contains(backdrop.interact_pointer_pos().unwrap_or(panel.center())) {
                self.shortcuts_open = false;
            }
        });
    }
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // egui moves focus between its widgets on Tab; during a takeover Tab belongs to the agent's screen.
        let driving = self.remote.is_some() && self.confirm_remove.is_none() && !self.shortcuts_open && ctx.memory(|m| m.focused().is_none());
        if !driving {
            return;
        }
        raw.events.retain(|e| match e {
            Event::Key { key: Key::Tab, pressed, modifiers, .. } => {
                if *pressed {
                    self.tabs.push(*modifiers);
                }
                false
            }
            _ => true,
        });
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.refresh(&ctx);
        self.upload_textures(&ctx);
        let pal = if ctx.theme() == egui::Theme::Dark { Palette::dark() } else { Palette::light() };
        let window = ctx.content_rect();
        ui.painter().rect_filled(window, 0.0, pal.content);

        let search_focused = ctx.memory(|m| m.has_focus(Id::unique("search")));
        self.sync_remote();
        self.handle_keys(&ctx, search_focused);

        let target = if self.sidebar_open { SIDEBAR_W.min(window.width() * 0.42) } else { 0.0 };
        let sidebar_w = ctx.animate_value_with_time(Id::unique("sidebar-w"), target, 0.22);

        // Drag zone: the whole top strip moves the window; double-click maximizes.
        let top = Rect::from_min_size(window.min, Vec2::new(window.width(), TOOLBAR_H));
        let drag = ui.interact(top, Id::unique("titlebar"), Sense::click_and_drag());
        if drag.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            toggle_maximize(&ctx);
        }

        // Content first, then the sidebar slides over its left edge.
        let content = Rect::from_min_max(Pos2::new(window.min.x + sidebar_w, window.min.y), window.max);
        let title_x = (content.min.x + 20.0).max(window.min.x + CHROME_W + 12.0);
        self.content(ui, &pal, content, title_x);

        if sidebar_w > 0.5 {
            let full = Rect::from_min_size(Pos2::new(window.min.x + sidebar_w - SIDEBAR_W.min(window.width() * 0.42), window.min.y), Vec2::new(SIDEBAR_W.min(window.width() * 0.42), window.height()));
            let visible = Rect::from_min_max(window.min, Pos2::new(window.min.x + sidebar_w, window.max.y));
            let mut side = ui.new_child(UiBuilder::new().max_rect(full).id_salt("sidebar"));
            side.set_clip_rect(visible);
            self.sidebar(&mut side, &pal, full);
        }

        // Window controls: sidebar toggle at the top left, minimize / maximize / close at the top right.
        self.window_controls(ui, &pal, Pos2::new(window.max.x - 12.0, window.min.y + TOOLBAR_H / 2.0));
        let toggle = Rect::from_center_size(window.min + Vec2::new(32.0, 26.0), Vec2::new(30.0, 26.0));
        if icon_button(ui, &pal, toggle, "toggle-sidebar", if self.sidebar_open { "Hide Sidebar   Ctrl B" } else { "Show Sidebar   Ctrl B" }, |p, c, col| {
            let r = Rect::from_center_size(c, Vec2::new(16.0, 13.0));
            p.rect_stroke(r, 3.0, Stroke::new(1.3, col), StrokeKind::Middle);
            p.line_segment([Pos2::new(r.min.x + 5.5, r.min.y), Pos2::new(r.min.x + 5.5, r.max.y)], Stroke::new(1.3, col));
        })
        .clicked()
        {
            self.sidebar_open = !self.sidebar_open;
        }

        self.toast(&ctx, &pal, content);
        self.shortcuts_sheet(&ctx, &pal, window);
        self.remove_sheet(&ctx, &pal, window);

        for action in std::mem::take(&mut self.actions) {
            self.apply(&ctx, action);
        }

        // Keep agent list and "x seconds ago" labels fresh.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------------------------------------
// Window management. Hyprland ignores the standard minimize/maximize requests, so ask it directly.

fn minimize_window(ctx: &egui::Context) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        if let Err(e) = crate::hypr::minimize_own_window() {
            eprintln!("cua: minimize via Hyprland failed ({e:#}), asking the window system");
            ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
        }
    });
}

fn toggle_maximize(ctx: &egui::Context) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        if let Err(e) = crate::hypr::toggle_maximize_own_window() {
            eprintln!("cua: maximize via Hyprland failed ({e:#}), asking the window system");
            let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
        }
        ctx.request_repaint_after(Duration::from_millis(150));
    });
}

// ---------------------------------------------------------------------------------------------
// Drawing helpers

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Draw text on one line, cut with "…" if wider than `max_width`.
fn text_elided(p: &egui::Painter, pos: Pos2, align: Align2, text: &str, font: FontId, color: Color32, max_width: f32) {
    if max_width <= 4.0 {
        return;
    }
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping { max_width, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = p.layout_job(job);
    let rect = align.anchor_size(pos, galley.size());
    p.galley(rect.min, galley, color);
}

/// Keyboard key caps, right-aligned at `right`. Returns the width drawn.
fn kbd(p: &egui::Painter, right: Pos2, keys: &[&str], pal: &Palette) -> f32 {
    let mut x = right.x;
    for key in keys.iter().rev() {
        let galley = p.layout_no_wrap(key.to_string(), regular(10.5), pal.secondary);
        let w = (galley.size().x + 10.0).max(18.0);
        let r = Rect::from_min_max(Pos2::new(x - w, right.y - 9.0), Pos2::new(x, right.y + 9.0));
        p.rect_filled(r, 4.0, pal.field);
        p.rect_stroke(r, 4.0, Stroke::new(0.5, pal.hairline), StrokeKind::Inside);
        p.galley(Pos2::new(r.center().x - galley.size().x / 2.0, r.center().y - galley.size().y / 2.0), galley, pal.secondary);
        x -= w + 3.0;
    }
    right.x - x
}

/// A 28×28-ish toolbar button with a hover background and a tooltip.
fn icon_button(ui: &mut Ui, pal: &Palette, rect: Rect, id: &str, tooltip: &str, draw: impl FnOnce(&egui::Painter, Pos2, Color32)) -> egui::Response {
    let r = ui.interact(rect, Id::unique(("icon-button", id)), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(r.id.with("hover"), r.hovered(), 0.12);
    let p = ui.painter();
    if hover > 0.0 {
        let fill = if r.is_pointer_button_down_on() { pal.active } else { pal.hover };
        p.rect_filled(rect, 6.0, fill.gamma_multiply(hover));
    }
    draw(p, rect.center(), if r.hovered() { pal.text } else { pal.secondary });
    r.on_hover_text(tooltip)
}

#[derive(Clone, Copy)]
enum Glyph {
    Stop,
    Play,
    Pointer,
}

fn draw_glyph(p: &egui::Painter, c: Pos2, glyph: Glyph, col: Color32) {
    match glyph {
        Glyph::Stop => {
            p.rect_filled(Rect::from_center_size(c, Vec2::splat(9.0)), 2.0, col);
        }
        Glyph::Play => {
            p.add(Shape::convex_polygon(
                vec![c + Vec2::new(-3.5, -5.0), c + Vec2::new(5.0, 0.0), c + Vec2::new(-3.5, 5.0)],
                col,
                Stroke::NONE,
            ));
        }
        Glyph::Pointer => {
            let o = c + Vec2::new(-4.0, -6.5);
            let pts = vec![
                o,
                o + Vec2::new(0.0, 11.5),
                o + Vec2::new(3.0, 8.8),
                o + Vec2::new(5.0, 13.0),
                o + Vec2::new(7.0, 12.0),
                o + Vec2::new(5.0, 7.9),
                o + Vec2::new(9.0, 7.9),
            ];
            p.add(Shape::closed_line(pts, Stroke::new(1.3, col)));
        }
    }
}

fn pill_width(p: &egui::Painter, label: &str) -> f32 {
    p.layout_no_wrap(label.to_string(), semibold(12.0), Color32::WHITE).size().x + 12.0 + 14.0 + 6.0 + 12.0
}

/// A labeled toolbar button; `primary` ones are filled with the accent color.
fn pill_button(ui: &mut Ui, pal: &Palette, rect: Rect, label: &str, glyph: Glyph, primary: bool, tooltip: &str) -> egui::Response {
    let r = ui.interact(rect, Id::unique(("pill", label, rect.min.x as i32, rect.min.y as i32)), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(r.id.with("hover"), r.hovered(), 0.12);
    let pressed = r.is_pointer_button_down_on();
    let p = ui.painter();
    let (fill, fg) = if primary {
        let base = pal.accent;
        let fill = if pressed { base.gamma_multiply(0.82) } else { lerp_color(base, Color32::WHITE, 0.1 * hover) };
        (fill, Color32::WHITE)
    } else {
        let fill = if pressed { pal.active } else { lerp_color(pal.field, pal.active, hover) };
        (fill, pal.text)
    };
    p.rect_filled(rect, 7.0, fill);
    if !primary {
        p.rect_stroke(rect, 7.0, Stroke::new(0.5, pal.hairline), StrokeKind::Inside);
    }
    draw_glyph(p, Pos2::new(rect.min.x + 12.0 + 7.0, rect.center().y), glyph, fg);
    p.text(Pos2::new(rect.min.x + 12.0 + 14.0 + 6.0, rect.center().y), Align2::LEFT_CENTER, label, semibold(12.0), fg);
    r.on_hover_text(tooltip)
}

/// A wide button at the bottom of a sheet; `destructive` ones are red.
fn sheet_button(ui: &mut Ui, pal: &Palette, rect: Rect, label: &str, destructive: bool) -> egui::Response {
    let r = ui.interact(rect, Id::unique(("sheet-button", label)), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(r.id.with("hover"), r.hovered(), 0.12);
    let p = ui.painter();
    let (fill, fg) = if destructive {
        (if r.is_pointer_button_down_on() { pal.red.gamma_multiply(0.82) } else { lerp_color(pal.red, Color32::WHITE, 0.1 * hover) }, Color32::WHITE)
    } else {
        (if r.is_pointer_button_down_on() { pal.active } else { lerp_color(pal.field, pal.active, hover) }, pal.text)
    };
    p.rect_filled(rect, 7.0, fill);
    p.text(rect.center(), Align2::CENTER_CENTER, label, semibold(13.0), fg);
    if destructive {
        p.text(rect.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, "↵", regular(12.0), fg.gamma_multiply(0.7));
    } else {
        p.text(rect.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, "esc", regular(10.5), pal.tertiary);
    }
    r
}

const MENU_W: f32 = 232.0;

/// One entry in a context menu: highlighted in the accent color on hover, as on a Mac.
fn menu_item(ui: &mut Ui, pal: &Palette, label: &str, shortcut: &str, destructive: bool, enabled: bool) -> bool {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(MENU_W, 24.0), if enabled { Sense::click() } else { Sense::hover() });
    let hot = enabled && r.hovered();
    let p = ui.painter();
    if hot {
        p.rect_filled(rect, 5.0, if destructive { pal.red } else { pal.accent });
    }
    let fg = match (enabled, hot, destructive) {
        (false, _, _) => pal.tertiary,
        (_, true, _) => Color32::WHITE,
        (_, _, true) => pal.red,
        _ => pal.text,
    };
    p.text(rect.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, label, regular(13.0), fg);
    if !shortcut.is_empty() {
        let col = if hot { Color32::WHITE.gamma_multiply(0.75) } else { pal.tertiary };
        p.text(rect.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, shortcut, regular(12.0), col);
    }
    let clicked = enabled && r.clicked();
    if clicked {
        ui.close();
    }
    clicked
}

fn menu_separator(ui: &mut Ui, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(MENU_W, 9.0), Sense::hover());
    ui.painter().line_segment([Pos2::new(rect.min.x + 10.0, rect.center().y), Pos2::new(rect.max.x - 10.0, rect.center().y)], Stroke::new(1.0, pal.hairline));
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()), mix(a.a(), b.a()))
}

fn icon_trash(p: &egui::Painter, c: Pos2, color: Color32) {
    let s = Stroke::new(1.5, color);
    p.line_segment([c + Vec2::new(-7.0, -5.0), c + Vec2::new(7.0, -5.0)], s);
    p.line_segment([c + Vec2::new(-2.5, -7.5), c + Vec2::new(2.5, -7.5)], s);
    p.add(Shape::line(vec![c + Vec2::new(-5.5, -5.0), c + Vec2::new(-4.5, 7.5), c + Vec2::new(4.5, 7.5), c + Vec2::new(5.5, -5.0)], s));
    p.line_segment([c + Vec2::new(0.0, -2.0), c + Vec2::new(0.0, 4.5)], Stroke::new(1.2, color));
}

fn icon_search(p: &egui::Painter, c: Pos2, color: Color32) {
    let ring = c + Vec2::new(-1.0, -1.0);
    p.circle_stroke(ring, 4.2, Stroke::new(1.4, color));
    p.line_segment([ring + Vec2::new(3.1, 3.1), ring + Vec2::new(6.0, 6.0)], Stroke::new(1.6, color));
}

fn icon_x(p: &egui::Painter, c: Pos2, r: f32, color: Color32, width: f32) {
    p.line_segment([c + Vec2::new(-r, -r), c + Vec2::new(r, r)], Stroke::new(width, color));
    p.line_segment([c + Vec2::new(-r, r), c + Vec2::new(r, -r)], Stroke::new(width, color));
}

fn icon_folder(p: &egui::Painter, c: Pos2, color: Color32) {
    let (l, r, t, b) = (c.x - 8.0, c.x + 8.0, c.y - 6.0, c.y + 6.0);
    p.add(Shape::closed_line(
        vec![
            Pos2::new(l, t + 1.5),
            Pos2::new(l + 1.5, t),
            Pos2::new(l + 5.5, t),
            Pos2::new(l + 7.0, t + 2.0),
            Pos2::new(r - 1.5, t + 2.0),
            Pos2::new(r, t + 3.5),
            Pos2::new(r, b - 1.5),
            Pos2::new(r - 1.5, b),
            Pos2::new(l + 1.5, b),
            Pos2::new(l, b - 1.5),
        ],
        Stroke::new(1.3, color),
    ));
    p.line_segment([Pos2::new(l, t + 4.5), Pos2::new(r, t + 4.5)], Stroke::new(1.0, color));
}

fn spinner(p: &egui::Painter, c: Pos2, r: f32, color: Color32, time: f64) {
    let spokes = 8;
    let head = ((time * 10.0) as usize) % spokes;
    for i in 0..spokes {
        let angle = i as f32 / spokes as f32 * std::f32::consts::TAU;
        let dir = Vec2::angled(angle);
        let age = (head + spokes - i) % spokes;
        let alpha = 1.0 - age as f32 / spokes as f32 * 0.8;
        p.line_segment([c + dir * r * 0.45, c + dir * r], Stroke::new(2.0, color.gamma_multiply(alpha)));
    }
}

// ---------------------------------------------------------------------------------------------
// Agent text

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

fn short_name(path: &str) -> String {
    let t = tilde(path);
    t.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&t).to_string()
}

fn status(agent: &AgentInfo, pal: &Palette) -> (Color32, &'static str) {
    match agent.control {
        Control::Takeover => return (pal.accent, "You're in Control"),
        Control::Stop => return (pal.orange, "Stopped"),
        _ => {}
    }
    match agent.status.as_str() {
        "ready" => {
            let recent = agent.last_action_at.is_some_and(|t| registry::now_ms().saturating_sub(t) < 15_000);
            if recent { (pal.green, "Working") } else { (pal.secondary, "Idle") }
        }
        "starting" => (pal.orange, "Starting"),
        _ => (pal.red, "Failed"),
    }
}

/// The agent's task, else its last action, else its project.
fn row_title(agent: &AgentInfo) -> String {
    agent.task.clone().or_else(|| agent.last_action.clone()).unwrap_or_else(|| agent.name.clone())
}

fn relative_time(agent: &AgentInfo) -> String {
    let Some(at) = agent.last_action_at else { return String::new() };
    let secs = registry::now_ms().saturating_sub(at) / 1000;
    match secs {
        0..=4 => "now".into(),
        5..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}
