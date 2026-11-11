//! MCP server (stdio, JSON-RPC 2.0). Claude Code starts one per session, so
//! each session is one agent with its own virtual screen, created on first use.

use crate::config::Config;
use crate::display::Display;
use crate::registry::{self, AgentInfo};
use crate::wl::{self, Client};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

const INSTRUCTIONS: &str = "cua gives you your own virtual screen on the user's Linux computer: the same apps, \
files, wallpaper, bar and dock as the user's desktop, but a separate screen with its own mouse, keyboard focus \
and clipboard. Nothing you do here moves the user's cursor or steals their focus, and other agents have their own \
screens. The user can watch your screen in the cua app.\n\
Workflow: call `screenshot` to see the screen, act with `click`/`type`/`key`/`scroll`/`drag`, and read the \
screenshot each action returns. Coordinates are pixels in the screenshot. Start apps with `launch` (shell \
command), or through the dock/launcher on screen. Browsers started here use cua's own profile (kept between sessions), not the user's, \
so they start logged out of the user's accounts. Apps that only allow one instance and are already open on the user's screen \
(e.g. some file managers) may open their window there instead; prefer apps that are not running, or pass \
options for a separate instance.";

pub fn run() -> Result<()> {
    let cfg = crate::config::load();
    let (tx, rx) = channel::<Option<String>>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.send(Some(l)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(None);
    });
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGHUP] {
        signal_hook::flag::register(sig, stop.clone())?;
    }

    registry::gc();
    let mut server = Server::new(cfg);
    let mut stdout = std::io::stdout().lock();
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(line)) => {
                if let Some(reply) = server.handle_line(&line) {
                    writeln!(stdout, "{reply}")?;
                    stdout.flush()?;
                }
            }
            Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => server.tick(),
        }
    }
    server.shutdown();
    Ok(())
}

struct Agent {
    display: Display,
    client: Client,
    last_wake: Instant,
}

struct Server {
    cfg: Config,
    info: AgentInfo,
    agent: Option<Agent>,
}

impl Server {
    fn new(cfg: Config) -> Server {
        let cwd = std::env::current_dir().unwrap_or_default();
        let name = cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "agent".into());
        let slug: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .take(32)
            .collect();
        let pid = std::process::id();
        let info = AgentInfo {
            id: format!("{slug}-{pid}"),
            pid,
            name,
            cwd: cwd.to_string_lossy().into_owned(),
            client: "unknown".into(),
            status: "starting".into(),
            width: cfg.width,
            height: cfg.height,
            started_at: registry::now_ms() / 1000,
            ..Default::default()
        };
        Server { cfg, info, agent: None }
    }

    fn handle_line(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}}).to_string());
            }
        };
        if let Some(batch) = msg.as_array() {
            let replies: Vec<Value> = batch.iter().filter_map(|m| self.handle(m)).collect();
            return (!replies.is_empty()).then(|| Value::Array(replies).to_string());
        }
        self.handle(&msg).map(|v| v.to_string())
    }

    fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let id = id?; // notifications get no reply
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools(&self.cfg) })),
            "tools/call" => Ok(self.call_tool(&params)),
            "resources/list" => Ok(json!({ "resources": [] })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        })
    }

    fn initialize(&mut self, params: &Value) -> Value {
        if let Some(ci) = params.get("clientInfo") {
            let name = ci.get("name").and_then(Value::as_str).unwrap_or("client");
            let version = ci.get("version").and_then(Value::as_str).unwrap_or("");
            self.info.client = format!("{name} {version}").trim().to_string();
        }
        let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
        let version = if ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"].contains(&requested) {
            requested
        } else {
            "2025-06-18"
        };
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "cua", "title": "Computer use (virtual screen)", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })
    }

    fn call_tool(&mut self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        match self.run_tool(name, &args) {
            Ok(content) => json!({ "content": content }),
            Err(e) => json!({ "content": [{ "type": "text", "text": format!("Error: {e:#}") }], "isError": true }),
        }
    }

    /// The agent's screen, started on first use and restarted if it died.
    fn agent(&mut self) -> Result<&mut Agent> {
        if let Some(a) = &mut self.agent
            && !a.display.alive() {
                a.display.stop();
                self.agent = None;
            }
        if self.agent.is_none() {
            self.info.status = "starting".into();
            self.info.error = None;
            let _ = registry::write(&self.info);
            match start_agent(&self.info.id, &self.cfg) {
                Ok(agent) => {
                    self.info.status = "ready".into();
                    self.info.wayland_display = Some(agent.display.instance.wayland_display.clone());
                    let (w, h) = agent.client.size();
                    (self.info.width, self.info.height) = (w, h);
                    let _ = registry::write(&self.info);
                    self.agent = Some(agent);
                }
                Err(e) => {
                    self.info.status = "error".into();
                    self.info.error = Some(format!("{e:#}"));
                    let _ = registry::write(&self.info);
                    return Err(e);
                }
            }
        }
        Ok(self.agent.as_mut().unwrap())
    }

    fn note(&mut self, action: String, point: Option<(i32, i32)>) {
        self.info.actions += 1;
        self.info.last_action = Some(action);
        self.info.last_action_at = Some(registry::now_ms());
        if point.is_some() {
            self.info.last_point = point;
        }
        let _ = registry::write(&self.info);
    }

    fn run_tool(&mut self, name: &str, args: &Value) -> Result<Vec<Value>> {
        let want_shot = args.get("screenshot").and_then(Value::as_bool).unwrap_or(true);
        let settle = Duration::from_millis(self.cfg.settle_ms);
        let key_delay = Duration::from_millis(self.cfg.key_delay_ms);
        let agent = self.agent()?;
        agent.client.key_delay = key_delay;
        let (w, h) = agent.client.size();
        let scale = shot_scale(w, h);
        // Screenshot pixel -> screen pixel.
        let point = |args: &Value, kx: &str, ky: &str| -> Result<(f64, f64)> {
            let x = args.get(kx).and_then(Value::as_f64).ok_or_else(|| anyhow!("missing {kx}"))?;
            let y = args.get(ky).and_then(Value::as_f64).ok_or_else(|| anyhow!("missing {ky}"))?;
            Ok((x / scale, y / scale))
        };
        let shown = |p: (f64, f64)| ((p.0 * scale).round() as i32, (p.1 * scale).round() as i32);

        let (summary, pt): (String, Option<(i32, i32)>) = match name {
            "screenshot" => {
                let content = screenshot(&mut agent.client, None)?;
                self.note("screenshot".into(), None);
                return Ok(content);
            }
            "click" => {
                let p = point(args, "x", "y")?;
                let button = match args.get("button").and_then(Value::as_str).unwrap_or("left") {
                    "left" => BTN_LEFT,
                    "right" => BTN_RIGHT,
                    "middle" => BTN_MIDDLE,
                    other => bail!("unknown button {other:?}"),
                };
                let clicks = args.get("clicks").and_then(Value::as_u64).unwrap_or(1).clamp(1, 3);
                let mods = args.get("modifiers").and_then(Value::as_str).unwrap_or("");
                with_modifiers(&mut agent.client, mods, |c| {
                    c.move_to(p.0, p.1)?;
                    std::thread::sleep(Duration::from_millis(30));
                    for i in 0..clicks {
                        if i > 0 {
                            std::thread::sleep(Duration::from_millis(60));
                        }
                        c.button(button, true)?;
                        std::thread::sleep(Duration::from_millis(25));
                        c.button(button, false)?;
                    }
                    Ok(())
                })?;
                let label = match clicks {
                    2 => "double-click",
                    3 => "triple-click",
                    _ => "click",
                };
                (format!("{label} at {},{}", shown(p).0, shown(p).1), Some(shown(p)))
            }
            "mouse_move" => {
                let p = point(args, "x", "y")?;
                agent.client.move_to(p.0, p.1)?;
                (format!("move to {},{}", shown(p).0, shown(p).1), Some(shown(p)))
            }
            "drag" => {
                let a = point(args, "from_x", "from_y")?;
                let b = point(args, "to_x", "to_y")?;
                let c = &mut agent.client;
                c.move_to(a.0, a.1)?;
                std::thread::sleep(Duration::from_millis(40));
                c.button(BTN_LEFT, true)?;
                std::thread::sleep(Duration::from_millis(60));
                let steps = 20;
                for i in 1..=steps {
                    let t = i as f64 / steps as f64;
                    c.move_to(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)?;
                    std::thread::sleep(Duration::from_millis(12));
                }
                std::thread::sleep(Duration::from_millis(60));
                c.button(BTN_LEFT, false)?;
                (format!("drag {},{} -> {},{}", shown(a).0, shown(a).1, shown(b).0, shown(b).1), Some(shown(b)))
            }
            "scroll" => {
                let amount = args.get("amount").and_then(Value::as_i64).unwrap_or(3).clamp(1, 50) as i32;
                let dir = args.get("direction").and_then(Value::as_str).unwrap_or("down");
                let (dx, dy) = match dir {
                    "up" => (0, -amount),
                    "down" => (0, amount),
                    "left" => (-amount, 0),
                    "right" => (amount, 0),
                    other => bail!("unknown direction {other:?}"),
                };
                let p = if args.get("x").is_some() { point(args, "x", "y")? } else { agent.client.cursor() };
                agent.client.move_to(p.0, p.1)?;
                agent.client.scroll(dx, dy)?;
                (format!("scroll {dir} x{amount} at {},{}", shown(p).0, shown(p).1), Some(shown(p)))
            }
            "type" => {
                let text = args.get("text").and_then(Value::as_str).ok_or_else(|| anyhow!("missing text"))?;
                agent.client.type_text(text)?;
                let preview: String = text.chars().take(40).collect();
                let more = if text.chars().count() > 40 { "…" } else { "" };
                (format!("type “{}{more}”", preview.replace('\n', "\\n")), None)
            }
            "key" => {
                let keys = args.get("keys").and_then(Value::as_str).ok_or_else(|| anyhow!("missing keys"))?;
                let repeat = args.get("repeat").and_then(Value::as_u64).unwrap_or(1).clamp(1, 100);
                for _ in 0..repeat {
                    for combo in keys.split_whitespace() {
                        agent.client.key_combo(combo)?;
                    }
                }
                let times = if repeat > 1 { format!(" x{repeat}") } else { String::new() };
                (format!("key {keys}{times}"), None)
            }
            "launch" => {
                let command = args.get("command").and_then(Value::as_str).ok_or_else(|| anyhow!("missing command"))?;
                let launched = agent.display.launch(command)?;
                let pid = launched.pid;
                let wait = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(1500).min(30_000);
                let until = Instant::now() + Duration::from_millis(wait);
                while Instant::now() < until {
                    if let Some(failure) = launched.failure() {
                        self.note(format!("launch {command} (failed)"), None);
                        bail!("`{command}` exited with {failure}");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let summary = format!("launch {command}");
                self.note(summary, None);
                let agent = self.agent.as_mut().unwrap();
                let mut content = vec![json!({"type": "text", "text": format!("Started `{command}` (pid {pid}).")})];
                if want_shot {
                    content.extend(screenshot(&mut agent.client, None)?);
                }
                return Ok(content);
            }
            "windows" => {
                let clients = agent.display.ipc_json("clients")?;
                let active = agent.display.ipc_json("activewindow").ok();
                let active_addr = active.as_ref().and_then(|a| a["address"].as_str()).unwrap_or("");
                let list: Vec<Value> = clients
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter(|c| c["mapped"].as_bool().unwrap_or(true))
                    .map(|c| {
                        let s = |v: &Value| (v.as_f64().unwrap_or(0.0) * scale).round() as i64;
                        json!({
                            "title": c["title"], "class": c["class"], "pid": c["pid"],
                            "workspace": c["workspace"]["name"],
                            "x": s(&c["at"][0]), "y": s(&c["at"][1]),
                            "width": s(&c["size"][0]), "height": s(&c["size"][1]),
                            "floating": c["floating"], "focused": c["address"].as_str() == Some(active_addr),
                        })
                    })
                    .collect();
                self.note("list windows".into(), None);
                return Ok(vec![json!({"type": "text", "text": serde_json::to_string_pretty(&list)?})]);
            }
            "wait" => {
                let secs = args.get("seconds").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 30.0);
                std::thread::sleep(Duration::from_secs_f64(secs));
                (format!("wait {secs}s"), None)
            }
            other => bail!("unknown tool {other:?}"),
        };

        let agent = self.agent.as_mut().unwrap();
        agent.last_wake = Instant::now();
        let mut content = vec![json!({"type": "text", "text": format!("Done: {summary}.")})];
        if want_shot {
            content.extend(screenshot(&mut agent.client, Some(settle))?);
        }
        self.note(summary, pt);
        Ok(content)
    }

    /// Runs about once a second while idle.
    fn tick(&mut self) {
        let Some(agent) = &mut self.agent else { return };
        if agent.last_wake.elapsed() > Duration::from_secs(20) {
            agent.last_wake = Instant::now();
            if agent.client.keep_awake().is_err() || !agent.display.alive() {
                // The screen died; the next tool call starts a fresh one.
                agent.display.stop();
                self.agent = None;
                self.info.status = "error".into();
                self.info.error = Some("virtual screen stopped".into());
                let _ = registry::write(&self.info);
            }
        }
    }

    fn shutdown(&mut self) {
        registry::remove(&self.info.id);
        if let Some(mut agent) = self.agent.take() {
            agent.display.stop();
        }
    }
}

fn start_agent(id: &str, cfg: &Config) -> Result<Agent> {
    let mut display = Display::start(id, cfg)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let client = loop {
        match Client::connect(&display.instance.wayland_display) {
            Ok(c) if c.size().0 > 1 => break c,
            Ok(_) | Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Ok(_) => bail!("virtual screen reported no size"),
            Err(e) => {
                display.stop();
                return Err(e).context("connecting to the virtual screen");
            }
        }
    };
    // Let the desktop shell draw before anyone looks.
    let mut client = client;
    client.move_to(cfg.width as f64 / 2.0, cfg.height as f64 / 2.0)?;
    if display.wait_for_shell(Duration::from_secs(8)) {
        // Surfaces exist before they are painted; wait for actual pixels.
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Ok(Some(frame)) = client.capture(false, Duration::from_secs(2))
                && painted(&frame)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(Agent { display, client, last_wake: Instant::now() })
}

/// Whether more than a few percent of the screen is something other than black.
fn painted(frame: &wl::Frame) -> bool {
    let pixels = frame.rgba.as_chunks::<4>().0;
    let step = (pixels.len() / 4000).max(1);
    let sampled = pixels.iter().step_by(step);
    let total = pixels.len() / step;
    let lit = sampled.filter(|p| p[0] as u32 + p[1] as u32 + p[2] as u32 > 30).count();
    lit * 100 > total * 5
}

fn with_modifiers(c: &mut Client, mods: &str, f: impl FnOnce(&mut Client) -> Result<()>) -> Result<()> {
    if mods.trim().is_empty() {
        return f(c);
    }
    c.hold_modifiers(mods)?;
    let result = f(c);
    c.hold_modifiers("")?;
    result
}

/// Screenshots are kept within what Claude reads at full detail; coordinates
/// in tool calls are relative to the (possibly scaled) screenshot.
fn shot_scale(w: u32, h: u32) -> f64 {
    let s = (1568.0 / w.max(h) as f64).min((1_150_000.0 / (w as f64 * h as f64)).sqrt());
    s.min(1.0)
}

fn screenshot(client: &mut Client, settle: Option<Duration>) -> Result<Vec<Value>> {
    if let Some(d) = settle {
        std::thread::sleep(d);
    }
    client.sync()?;
    let frame = client.capture(false, Duration::from_secs(2))?.ok_or_else(|| anyhow!("no frame"))?;
    let scale = shot_scale(frame.width, frame.height);
    let frame = if scale < 1.0 { downscale(&frame, scale) } else { frame };
    let jpeg = encode_jpeg(&frame)?;
    Ok(vec![json!({
        "type": "image",
        "data": base64::engine::general_purpose::STANDARD.encode(jpeg),
        "mimeType": "image/jpeg",
    })])
}

/// JPEG keeps photo wallpapers ~10x smaller than PNG; no chroma subsampling
/// so small text stays sharp.
fn encode_jpeg(frame: &wl::Frame) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(frame.rgba.len() / 10);
    let mut enc = jpeg_encoder::Encoder::new(&mut out, 88);
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
    enc.encode(&frame.rgba, frame.width as u16, frame.height as u16, jpeg_encoder::ColorType::Rgba)?;
    Ok(out)
}

/// Area-average downscale.
fn downscale(frame: &wl::Frame, scale: f64) -> wl::Frame {
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let dw = ((sw as f64 * scale).round() as usize).max(1);
    let dh = ((sh as f64 * scale).round() as usize).max(1);
    let mut rgba = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        let y0 = y * sh / dh;
        let y1 = ((y + 1) * sh / dh).max(y0 + 1);
        for x in 0..dw {
            let x0 = x * sw / dw;
            let x1 = ((x + 1) * sw / dw).max(x0 + 1);
            let mut acc = [0u32; 3];
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let i = (yy * sw + xx) * 4;
                    acc[0] += frame.rgba[i] as u32;
                    acc[1] += frame.rgba[i + 1] as u32;
                    acc[2] += frame.rgba[i + 2] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (y * dw + x) * 4;
            rgba[o] = (acc[0] / n) as u8;
            rgba[o + 1] = (acc[1] / n) as u8;
            rgba[o + 2] = (acc[2] / n) as u8;
            rgba[o + 3] = 255;
        }
    }
    wl::Frame { width: dw as u32, height: dh as u32, rgba }
}

fn tools(cfg: &Config) -> Value {
    let scale = shot_scale(cfg.width, cfg.height);
    let (sw, sh) = ((cfg.width as f64 * scale).round(), (cfg.height as f64 * scale).round());
    let screen = format!("The screen is {sw}x{sh} pixels; (0,0) is the top-left corner.");
    let shot = json!({"type": "boolean", "description": "Return a screenshot after the action (default true).", "default": true});
    let xy = |what: &str| json!({"type": "integer", "description": what});
    json!([
        {
            "name": "screenshot",
            "title": "Screenshot",
            "description": format!("Capture your virtual screen. {screen} The first call starts the screen (takes a couple of seconds)."),
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "click",
            "title": "Click",
            "description": format!("Move the mouse to (x, y) and click. {screen}"),
            "inputSchema": {"type": "object", "properties": {
                "x": xy("X coordinate in screenshot pixels"),
                "y": xy("Y coordinate in screenshot pixels"),
                "button": {"type": "string", "enum": ["left", "right", "middle"], "default": "left"},
                "clicks": {"type": "integer", "minimum": 1, "maximum": 3, "default": 1, "description": "2 for double-click, 3 for triple-click."},
                "modifiers": {"type": "string", "description": "Keys to hold while clicking, e.g. \"ctrl\" or \"ctrl+shift\"."},
                "screenshot": shot,
            }, "required": ["x", "y"]},
        },
        {
            "name": "mouse_move",
            "title": "Move mouse",
            "description": "Move the mouse to (x, y) without clicking (e.g. to reveal hover menus or tooltips).",
            "inputSchema": {"type": "object", "properties": {"x": xy("X coordinate"), "y": xy("Y coordinate"), "screenshot": shot}, "required": ["x", "y"]},
        },
        {
            "name": "drag",
            "title": "Drag",
            "description": "Press the left button at (from_x, from_y), drag to (to_x, to_y) and release.",
            "inputSchema": {"type": "object", "properties": {
                "from_x": xy("Start X"), "from_y": xy("Start Y"), "to_x": xy("End X"), "to_y": xy("End Y"), "screenshot": shot,
            }, "required": ["from_x", "from_y", "to_x", "to_y"]},
        },
        {
            "name": "scroll",
            "title": "Scroll",
            "description": "Scroll with the mouse wheel at (x, y) (defaults to the current mouse position).",
            "inputSchema": {"type": "object", "properties": {
                "direction": {"type": "string", "enum": ["up", "down", "left", "right"]},
                "amount": {"type": "integer", "minimum": 1, "maximum": 50, "default": 3, "description": "Wheel clicks."},
                "x": xy("X coordinate"), "y": xy("Y coordinate"), "screenshot": shot,
            }, "required": ["direction"]},
        },
        {
            "name": "type",
            "title": "Type text",
            "description": "Type text into the focused window. Any Unicode works; \\n presses Enter. Click the target field first.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}, "screenshot": shot}, "required": ["text"]},
        },
        {
            "name": "key",
            "title": "Press keys",
            "description": "Press a key or key combination, e.g. \"Return\", \"ctrl+l\", \"ctrl+shift+t\", \"alt+F4\", \"super\". Separate several with spaces to press them in order: \"ctrl+a BackSpace\". Names follow X keysyms (Page_Down, Escape, Tab, F5, Left...).",
            "inputSchema": {"type": "object", "properties": {
                "keys": {"type": "string"},
                "repeat": {"type": "integer", "minimum": 1, "maximum": 100, "default": 1},
                "screenshot": shot,
            }, "required": ["keys"]},
        },
        {
            "name": "launch",
            "title": "Launch app",
            "description": "Run a shell command on your virtual screen, e.g. \"firefox https://example.com\", \"nautilus ~/Downloads\", \"gnome-calculator\". Returns immediately after wait_ms with a screenshot.",
            "inputSchema": {"type": "object", "properties": {
                "command": {"type": "string"},
                "wait_ms": {"type": "integer", "minimum": 0, "maximum": 30000, "default": 1500, "description": "How long to wait before the screenshot."},
                "screenshot": shot,
            }, "required": ["command"]},
        },
        {
            "name": "windows",
            "title": "List windows",
            "description": "List the windows on your virtual screen with their titles, app classes, positions and which one has focus.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "wait",
            "title": "Wait",
            "description": "Wait for something to finish loading, then take a screenshot.",
            "inputSchema": {"type": "object", "properties": {"seconds": {"type": "number", "minimum": 0, "maximum": 30, "default": 1}, "screenshot": shot}},
            "annotations": {"readOnlyHint": true},
        },
    ])
}
