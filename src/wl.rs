//! Native Wayland client for one agent screen: captures frames with
//! wlr-screencopy and injects input with virtual pointer/keyboard devices that
//! exist only inside that screen. Nothing here touches the user's own input.

use crate::keys::Keyboard;
use anyhow::{Context, Result, anyhow, bail};
use memmap2::MmapMut;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

/// An RGBA frame.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Default)]
struct FrameState {
    formats: Vec<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    ready: bool,
    failed: bool,
    y_invert: bool,
}

#[derive(Default)]
struct State {
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    output: Option<wl_output::WlOutput>,
    output_size: (i32, i32),
    screencopy: Option<ZwlrScreencopyManagerV1>,
    pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    frame: FrameState,
}

struct ShmBuffer {
    _fd: OwnedFd,
    map: MmapMut,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    key: (wl_shm::Format, u32, u32, u32),
}

pub struct Client {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    shm_buffer: Option<ShmBuffer>,
    pointer: Option<ZwlrVirtualPointerV1>,
    keyboard: Option<(ZwpVirtualKeyboardV1, Keyboard)>,
    cursor: (f64, f64),
    epoch: Instant,
    pub key_delay: Duration,
}

impl Client {
    /// Connect to a Wayland display by socket name (e.g. "wayland-2").
    pub fn connect(display: &str) -> Result<Client> {
        let path = crate::paths::xdg_runtime().join(display);
        let stream = UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))?;
        let conn = Connection::from_socket(stream)?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?; // output mode events
        if state.output.is_none() {
            bail!("{display} has no output yet");
        }
        Ok(Client {
            conn,
            queue,
            qh,
            state,
            shm_buffer: None,
            pointer: None,
            keyboard: None,
            cursor: (0.0, 0.0),
            epoch: Instant::now(),
            key_delay: Duration::from_millis(4),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.state.output_size.0.max(1) as u32, self.state.output_size.1.max(1) as u32)
    }

    fn time(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    /// Dispatch events, waiting at most `timeout` for new ones.
    fn dispatch_timeout(&mut self, timeout: Duration) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        let _ = self.conn.flush();
        if let Some(guard) = self.queue.prepare_read() {
            let mut fds = [libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout.as_millis().min(i32::MAX as u128) as i32) };
            if n > 0 {
                if fds[0].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                    bail!("compositor connection closed");
                }
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    // ---------------------------------------------------------------- capture

    /// Capture the screen. With `wait_for_damage`, the compositor answers only
    /// once something on screen changes; `None` means nothing changed in time.
    pub fn capture(&mut self, wait_for_damage: bool, timeout: Duration) -> Result<Option<Frame>> {
        let manager = self.state.screencopy.clone().ok_or_else(|| anyhow!("compositor lacks wlr-screencopy"))?;
        let output = self.state.output.clone().ok_or_else(|| anyhow!("no output"))?;
        self.state.frame = FrameState::default();
        let frame = manager.capture_output(1, &output, &self.qh, ());
        let deadline = Instant::now() + Duration::from_secs(2);
        let v3 = manager.version() >= 3;
        while !(self.state.frame.buffer_done || (!v3 && !self.state.frame.formats.is_empty())) {
            if self.state.frame.failed || Instant::now() > deadline {
                frame.destroy();
                bail!("screen capture failed");
            }
            self.dispatch_timeout(Duration::from_millis(100))?;
        }
        let key = *self
            .state
            .frame
            .formats
            .iter()
            .find(|f| SUPPORTED.contains(&f.0))
            .ok_or_else(|| anyhow!("no supported capture format"))?;
        self.ensure_buffer(key)?;
        let buffer = &self.shm_buffer.as_ref().unwrap().buffer;
        if wait_for_damage { frame.copy_with_damage(buffer) } else { frame.copy(buffer) }

        let deadline = Instant::now() + if wait_for_damage { timeout } else { Duration::from_secs(2) };
        while !self.state.frame.ready {
            if self.state.frame.failed {
                frame.destroy();
                bail!("screen capture failed");
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                frame.destroy();
                let _ = self.conn.flush();
                if wait_for_damage {
                    return Ok(None);
                }
                bail!("screen capture timed out");
            }
            self.dispatch_timeout(left.min(Duration::from_millis(100)))?;
        }
        frame.destroy();
        let (format, w, h, stride) = key;
        let map = &self.shm_buffer.as_ref().unwrap().map;
        Ok(Some(to_rgba(map, format, w, h, stride, self.state.frame.y_invert)))
    }

    fn ensure_buffer(&mut self, key: (wl_shm::Format, u32, u32, u32)) -> Result<()> {
        if self.shm_buffer.as_ref().is_some_and(|b| b.key == key) {
            return Ok(());
        }
        if let Some(old) = self.shm_buffer.take() {
            old.buffer.destroy();
            old.pool.destroy();
        }
        let (format, w, h, stride) = key;
        let size = (stride * h) as usize;
        let fd = memfd(size)?;
        let map = unsafe { MmapMut::map_mut(&std::fs::File::from(fd.try_clone()?))? };
        let shm = self.state.shm.as_ref().ok_or_else(|| anyhow!("no wl_shm"))?;
        let pool = shm.create_pool(fd.as_fd(), size as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, &self.qh, ());
        self.shm_buffer = Some(ShmBuffer { _fd: fd, map, pool, buffer, key });
        Ok(())
    }

    // ------------------------------------------------------------------ input

    fn pointer(&mut self) -> Result<ZwlrVirtualPointerV1> {
        if self.pointer.is_none() {
            let mgr = self.state.pointer_mgr.as_ref().ok_or_else(|| anyhow!("compositor lacks virtual pointers"))?;
            self.pointer = Some(mgr.create_virtual_pointer(self.state.seat.as_ref(), &self.qh, ()));
        }
        Ok(self.pointer.clone().unwrap())
    }

    pub fn cursor(&self) -> (f64, f64) {
        self.cursor
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<()> {
        let (w, h) = self.size();
        let x = x.clamp(0.0, w as f64 - 1.0);
        let y = y.clamp(0.0, h as f64 - 1.0);
        let p = self.pointer()?;
        p.motion_absolute(self.time(), x.round() as u32, y.round() as u32, w, h);
        p.frame();
        self.cursor = (x, y);
        self.conn.flush()?;
        Ok(())
    }

    pub fn button(&mut self, button: u32, pressed: bool) -> Result<()> {
        use wayland_client::protocol::wl_pointer::ButtonState;
        let p = self.pointer()?;
        p.button(self.time(), button, if pressed { ButtonState::Pressed } else { ButtonState::Released });
        p.frame();
        self.conn.flush()?;
        Ok(())
    }

    /// Scroll by wheel clicks; positive `dy` scrolls down, positive `dx` right.
    pub fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        use wayland_client::protocol::wl_pointer::{Axis, AxisSource};
        let p = self.pointer()?;
        for (axis, steps) in [(Axis::VerticalScroll, dy), (Axis::HorizontalScroll, dx)] {
            if steps == 0 {
                continue;
            }
            let dir = steps.signum();
            for _ in 0..steps.abs() {
                let t = self.time();
                p.axis_source(AxisSource::Wheel);
                p.axis_discrete(t, axis, 15.0 * dir as f64, dir);
                p.frame();
                self.conn.flush()?;
                std::thread::sleep(Duration::from_millis(12));
            }
        }
        Ok(())
    }

    /// Tiny pointer wiggle: counts as user activity so the agent screen's
    /// shell never starts its screensaver or lock while the agent thinks.
    pub fn keep_awake(&mut self) -> Result<()> {
        let p = self.pointer()?;
        p.motion(self.time(), 1.0, 0.0);
        p.frame();
        p.motion(self.time(), -1.0, 0.0);
        p.frame();
        self.conn.flush()?;
        Ok(())
    }

    fn keyboard(&mut self) -> Result<()> {
        if self.keyboard.is_none() {
            let mgr = self.state.keyboard_mgr.as_ref().ok_or_else(|| anyhow!("compositor lacks virtual keyboards"))?;
            let seat = self.state.seat.as_ref().ok_or_else(|| anyhow!("no seat"))?;
            let kbd = mgr.create_virtual_keyboard(seat, &self.qh, ());
            let keymap = Keyboard::new()?;
            upload_keymap(&kbd, keymap.us_keymap())?;
            self.keyboard = Some((kbd, keymap));
            self.queue.roundtrip(&mut self.state)?;
        }
        Ok(())
    }

    /// Press and release one key with the given modifier mask held.
    fn tap(&mut self, keycode: u32, mods: u32) -> Result<()> {
        let t = self.time();
        let delay = self.key_delay;
        let (kbd, _) = self.keyboard.as_ref().unwrap();
        use wayland_client::protocol::wl_keyboard::KeyState;
        if mods != 0 {
            kbd.modifiers(mods, 0, 0, 0);
        }
        kbd.key(t, keycode, KeyState::Pressed.into());
        self.conn.flush()?;
        std::thread::sleep(delay);
        kbd.key(t + 1, keycode, KeyState::Released.into());
        if mods != 0 {
            kbd.modifiers(0, 0, 0, 0);
        }
        self.conn.flush()?;
        std::thread::sleep(delay);
        Ok(())
    }

    /// Type text exactly, including characters missing from a US layout.
    pub fn type_text(&mut self, text: &str) -> Result<()> {
        self.keyboard()?;
        let chars: Vec<char> = text.chars().filter(|c| *c != '\r').collect();
        let mut i = 0;
        while i < chars.len() {
            // Runs of characters the US keymap has are typed with real keycodes,
            // which every app understands; other runs get a custom keymap.
            let us = self.keyboard.as_ref().unwrap().1.lookup_char(chars[i]).is_some();
            let mut j = i;
            while j < chars.len() && self.keyboard.as_ref().unwrap().1.lookup_char(chars[j]).is_some() == us {
                j += 1;
            }
            let run = &chars[i..j];
            if us {
                for c in run {
                    let (code, mods) = self.keyboard.as_ref().unwrap().1.lookup_char(*c).unwrap();
                    self.tap(code, mods)?;
                }
            } else {
                let (kbd, keymap) = self.keyboard.as_ref().unwrap();
                let (text, codes) = keymap.custom_keymap(run);
                upload_keymap(kbd, &text)?;
                self.queue.roundtrip(&mut self.state)?;
                for code in codes {
                    self.tap(code, 0)?;
                }
                let (kbd, keymap) = self.keyboard.as_ref().unwrap();
                upload_keymap(kbd, keymap.us_keymap())?;
                self.queue.roundtrip(&mut self.state)?;
            }
            i = j;
        }
        self.queue.roundtrip(&mut self.state)?;
        Ok(())
    }

    /// Press a combination like "ctrl+shift+t", "Return" or "super".
    pub fn key_combo(&mut self, combo: &str) -> Result<()> {
        self.keyboard()?;
        let (code, mods) = self.keyboard.as_ref().unwrap().1.parse_combo(combo)?;
        self.tap(code, mods)?;
        self.queue.roundtrip(&mut self.state)?;
        Ok(())
    }

    /// Hold modifiers ("ctrl+shift") for following pointer actions; "" releases them.
    pub fn hold_modifiers(&mut self, spec: &str) -> Result<()> {
        self.keyboard()?;
        let (kbd, keymap) = self.keyboard.as_ref().unwrap();
        kbd.modifiers(keymap.parse_mods(spec)?, 0, 0, 0);
        self.conn.flush()?;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.queue.roundtrip(&mut self.state)?;
        Ok(())
    }
}

fn upload_keymap(kbd: &ZwpVirtualKeyboardV1, text: &str) -> Result<()> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    let fd = memfd(bytes.len())?;
    let mut map = unsafe { MmapMut::map_mut(&std::fs::File::from(fd.try_clone()?))? };
    map.copy_from_slice(&bytes);
    kbd.keymap(wayland_client::protocol::wl_keyboard::KeymapFormat::XkbV1.into(), fd.as_fd(), bytes.len() as u32);
    Ok(())
}

fn memfd(size: usize) -> Result<OwnedFd> {
    let fd = rustix::fs::memfd_create("cua", rustix::fs::MemfdFlags::CLOEXEC)?;
    rustix::fs::ftruncate(&fd, size as u64)?;
    Ok(fd)
}

const SUPPORTED: [wl_shm::Format; 4] =
    [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888, wl_shm::Format::Xbgr8888, wl_shm::Format::Abgr8888];

fn to_rgba(data: &[u8], format: wl_shm::Format, w: u32, h: u32, stride: u32, y_invert: bool) -> Frame {
    let (w_, h_, stride) = (w as usize, h as usize, stride as usize);
    let mut rgba = vec![0u8; w_ * h_ * 4];
    // Little-endian: [AX]RGB8888 is stored B,G,R,A; [AX]BGR8888 is R,G,B,A.
    let bgr = matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888);
    for y in 0..h_ {
        let src_row = if y_invert { h_ - 1 - y } else { y };
        let src = &data[src_row * stride..src_row * stride + w_ * 4];
        let dst = &mut rgba[y * w_ * 4..(y + 1) * w_ * 4];
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
            if bgr {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
            } else {
                d[0] = s[0];
                d[1] = s[1];
                d[2] = s[2];
            }
            d[3] = 255;
        }
    }
    Frame { width: w, height: h, rgba }
}

// ------------------------------------------------------------- dispatching

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        match interface.as_str() {
            "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
            "wl_seat" if state.seat.is_none() => state.seat = Some(registry.bind(name, version.min(5), qh, ())),
            "wl_output" if state.output.is_none() => {
                state.output = Some(registry.bind(name, version.min(4), qh, ()))
            }
            "zwlr_screencopy_manager_v1" => state.screencopy = Some(registry.bind(name, version.min(3), qh, ())),
            "zwlr_virtual_pointer_manager_v1" => {
                state.pointer_mgr = Some(registry.bind(name, version.min(2), qh, ()))
            }
            "zwp_virtual_keyboard_manager_v1" => state.keyboard_mgr = Some(registry.bind(name, 1, qh, ())),
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { flags: WEnum::Value(flags), width, height, .. } = event
            && flags.contains(wl_output::Mode::Current) {
                state.output_size = (width, height);
            }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer { format: WEnum::Value(format), width, height, stride } => {
                state.frame.formats.push((format, width, height, stride))
            }
            Event::BufferDone => state.frame.buffer_done = true,
            Event::Flags { flags: WEnum::Value(flags) } => {
                state.frame.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert)
            }
            Event::Ready { .. } => state.frame.ready = true,
            Event::Failed => state.frame.failed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ZwlrScreencopyManagerV1);
delegate_noop!(State: ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ZwlrVirtualPointerV1);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);
