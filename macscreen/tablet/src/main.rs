//! macview: shows a Mac window on the reMarkable Paper Pro.
//!
//! On launch it lists the Macs on your tailnet; tap one to connect. It is a
//! small VNC client for rmscreen (the Mac side), reaching the Mac through
//! `tailscale nc` (the tablet's tailscaled runs in userspace mode, so ordinary
//! sockets can't reach tailnet addresses), and draws into an AppLoad qtfb
//! window.
//!
//! - Rotation: follows AppLoad's rotation, tells the Mac the orientation
//!   (custom message 250); the Mac answers with a new logical size
//!   (DesktopSize) and the logical image is rotated onto the panel.
//! - Touch: one finger is the mouse (pointer events); a second finger turns
//!   the gesture into a scroll (custom message 251, cancelling the pointer
//!   with button bit 0x80).
//! - Keyboard: physical keyboards are grabbed and sent as raw Linux keycodes
//!   (keysym 0x10f00000 | code), which rmscreen maps to Mac keys.

mod font;
mod orient;
mod qtfb;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flate2::{Decompress, FlushDecompress};
use font::Psf2Font;
use orient::Rot;

const PW: usize = 1620; // physical panel
const PH: usize = 2160;
const PORT: &str = "5900";
const TAILSCALE: &str = "/home/root/.vellum/bin/tailscale";
const LAST_PATH: &str = "/home/root/.config/macscreen/last";
const FONT_BIG: &[u8] = include_bytes!("../fonts/spleen-32x64.psfu");
const FONT_SMALL: &[u8] = include_bytes!("../fonts/spleen-16x32.psfu");
const WHITE: u16 = 0xffff;
const BLACK: u16 = 0x0000;
const GRAY: u16 = 0x8410;

type Client = Arc<Mutex<qtfb::QtfbClient>>;
type Out = Arc<Mutex<ChildStdin>>;

/// Where keyboard input goes: the current session, if any.
static KEY_SINK: Mutex<Option<Out>> = Mutex::new(None);

// ---------------------------------------------------------------- drawing

/// A logical (rotated) RGB565 image that is presented onto the panel.
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Canvas { w, h, px: vec![0xff; w * h * 2] }
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: u16) {
        for yy in y..(y + h).min(self.h) {
            for xx in x..(x + w).min(self.w) {
                let o = (yy * self.w + xx) * 2;
                self.px[o..o + 2].copy_from_slice(&c.to_le_bytes());
            }
        }
    }

    fn text(&mut self, f: &Psf2Font, x: usize, y: usize, s: &str, c: u16) {
        let bpr = f.bytes_per_row();
        let mut cx = x;
        for ch in s.chars() {
            if cx + f.width > self.w {
                break;
            }
            let g = f.glyph(ch);
            for gy in 0..f.height {
                for gx in 0..f.width {
                    if g[gy * bpr + gx / 8] & (0x80 >> (gx % 8)) != 0 && y + gy < self.h {
                        let o = ((y + gy) * self.w + cx + gx) * 2;
                        self.px[o..o + 2].copy_from_slice(&c.to_le_bytes());
                    }
                }
            }
            cx += f.width;
        }
    }
}

/// The image must have the shape the panel is showing (portrait or
/// landscape): right after a rotation the Mac may still send the old size,
/// which is skipped until the resized image follows. Returns the rect clamped
/// to the image, or None if there is nothing to draw.
fn drawable(canvas: &Canvas, rot: Rot, rect: (usize, usize, usize, usize)) -> Option<(usize, usize, usize, usize)> {
    let (w, h) = if rot.is_landscape() { (PH, PW) } else { (PW, PH) };
    if canvas.w != w || canvas.h != h {
        return None;
    }
    let (x, y, rw, rh) = rect;
    let rect = (x.min(w), y.min(h), rw.min(w - x.min(w)), rh.min(h - y.min(h)));
    (rect.2 > 0 && rect.3 > 0).then_some(rect)
}

/// Copies a logical rect to the panel (rotated) and refreshes it.
fn present(client: &Client, canvas: &Canvas, rot: Rot, rect: (usize, usize, usize, usize), full: bool) {
    let Some(rect) = drawable(canvas, rot, rect) else { return };
    let mut c = client.lock().unwrap();
    let fb = c.framebuffer();
    let (x, y, w, h) = orient::blit(&canvas.px, canvas.w, fb, PW, PH, rot, rect);
    let _ = if full { c.update_all() } else { c.update_partial(x as i32, y as i32, w as i32, h as i32) };
}

// ---------------------------------------------------------------- Macs on the tailnet

#[derive(Clone)]
struct Mac {
    name: String,
    ip: String,
    online: bool,
}

fn list_macs() -> Vec<Mac> {
    let Ok(out) = Command::new(TAILSCALE).args(["status", "--json"]).output() else { return vec![] };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return vec![] };
    let mut macs = Vec::new();
    if let Some(peers) = v["Peer"].as_object() {
        for p in peers.values() {
            if p["OS"].as_str() != Some("macOS") {
                continue;
            }
            let Some(ip) = p["TailscaleIPs"].as_array().and_then(|a| a.iter().filter_map(|i| i.as_str()).find(|i| i.contains('.')))
            else {
                continue;
            };
            macs.push(Mac {
                name: p["HostName"].as_str().unwrap_or("Mac").to_string(),
                ip: ip.to_string(),
                online: p["Online"].as_bool().unwrap_or(false),
            });
        }
    }
    macs.sort_by(|a, b| b.online.cmp(&a.online).then(a.name.cmp(&b.name)));
    macs
}

fn last_mac() -> String {
    std::fs::read_to_string(LAST_PATH).unwrap_or_default().trim().to_string()
}

fn remember_mac(name: &str) {
    let _ = std::fs::create_dir_all("/home/root/.config/macscreen");
    let _ = std::fs::write(LAST_PATH, name);
}

// ---------------------------------------------------------------- AppLoad events

/// Waits up to `ms` for events from AppLoad. Err when the window was closed.
fn wait_events(client: &Client, ms: i32) -> io::Result<qtfb::Drained> {
    let fd = client.lock().unwrap().raw_fd();
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, ms) };
    client.lock().unwrap().drain_events()
}

fn update_rot(rot: &mut Rot, d: &qtfb::Drained) -> bool {
    match d.rotation.map(|r| Rot::from_appload(r, false)) {
        Some(r) if r != *rot => {
            *rot = r;
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------- choosing a Mac

const ROW_TOP: usize = 260;
const ROW_H: usize = 170;

fn draw_chooser(rot: Rot, macs: &[Mac], last: &str, msg: &str, big: &Psf2Font, small: &Psf2Font) -> Canvas {
    let (w, h) = if rot.is_landscape() { (PH, PW) } else { (PW, PH) };
    let mut c = Canvas::new(w, h);
    c.text(big, 60, 70, "Connect to a Mac", BLACK);
    c.text(small, 60, 160, msg, BLACK);
    c.fill(0, ROW_TOP - 6, w, 4, BLACK);
    if macs.is_empty() {
        c.text(small, 60, ROW_TOP + 40, "No Macs on your Tailscale network", BLACK);
    }
    for (i, m) in macs.iter().enumerate() {
        let top = ROW_TOP + i * ROW_H;
        if top + ROW_H > h {
            break;
        }
        c.text(big, 60, top + 30, &m.name, if m.online { BLACK } else { GRAY });
        let mut sub = if m.online { "online".to_string() } else { "offline".to_string() };
        if m.name == last {
            sub.push_str("  \u{b7}  last used");
        }
        c.text(small, 60, top + 104, &sub, GRAY);
        c.fill(60, top + ROW_H - 2, w - 120, 2, GRAY);
    }
    c
}

/// Shows the Mac list until one is tapped. Err when the window was closed.
fn choose(client: &Client, rot: &mut Rot, msg: &str, big: &Psf2Font, small: &Psf2Font) -> io::Result<Mac> {
    let last = last_mac();
    let mut macs = list_macs();
    let mut refreshed = Instant::now();
    let mut dirty = true;
    let mut press: Option<(i32, i32)> = None;
    loop {
        if dirty {
            let c = draw_chooser(*rot, &macs, &last, msg, big, small);
            present(client, &c, *rot, (0, 0, c.w, c.h), true);
            dirty = false;
        }
        let d = wait_events(client, 1000)?;
        if update_rot(rot, &d) {
            dirty = true;
        }
        if refreshed.elapsed() > Duration::from_secs(5) {
            let fresh = list_macs();
            dirty |= fresh.iter().map(|m| (&m.name, m.online)).ne(macs.iter().map(|m| (&m.name, m.online)));
            macs = fresh;
            refreshed = Instant::now();
        }
        for ev in d.inputs {
            let (x, y) = rot.to_logical(ev.x, ev.y, PW, PH);
            match ev.input_type {
                qtfb::INPUT_TOUCH_PRESS | qtfb::INPUT_PEN_PRESS => press = Some((x, y)),
                qtfb::INPUT_TOUCH_RELEASE | qtfb::INPUT_PEN_RELEASE => {
                    if let Some((px, py)) = press.take() {
                        if (x - px).abs() < 40 && (y - py).abs() < 40 && py >= ROW_TOP as i32 {
                            let i = (py as usize - ROW_TOP) / ROW_H;
                            if let Some(m) = macs.get(i).filter(|m| m.online) {
                                return Ok(m.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------- VNC session

fn send(out: &Out, bytes: &[u8]) {
    let mut w = out.lock().unwrap();
    let _ = w.write_all(bytes).and_then(|_| w.flush());
}

fn request_update(out: &Out, incremental: bool, w: usize, h: usize) {
    let mut m = vec![3u8, incremental as u8];
    for v in [0u16, 0, w as u16, h as u16] {
        m.extend_from_slice(&v.to_be_bytes());
    }
    send(out, &m);
}

fn send_orientation(out: &Out, rot: Rot) {
    send(out, &[250, rot.is_landscape() as u8]);
}

fn send_pointer(out: &Out, mask: u8, x: i32, y: i32) {
    let mut m = vec![5u8, mask];
    m.extend_from_slice(&(x.max(0) as u16).to_be_bytes());
    m.extend_from_slice(&(y.max(0) as u16).to_be_bytes());
    send(out, &m);
}

fn send_scroll(out: &Out, x: i32, y: i32, dx: i32, dy: i32) {
    let mut m = vec![251u8, 0];
    m.extend_from_slice(&(x.max(0) as u16).to_be_bytes());
    m.extend_from_slice(&(y.max(0) as u16).to_be_bytes());
    m.extend_from_slice(&(dx.clamp(-32000, 32000) as i16).to_be_bytes());
    m.extend_from_slice(&(dy.clamp(-32000, 32000) as i16).to_be_bytes());
    send(out, &m);
}

fn send_key(out: &Out, down: bool, code: u16) {
    let mut m = vec![4u8, down as u8, 0, 0];
    m.extend_from_slice(&(0x10f0_0000u32 | code as u32).to_be_bytes());
    send(out, &m);
}

fn read_n(r: &mut impl Read, n: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn be16(b: &[u8]) -> usize {
    u16::from_be_bytes([b[0], b[1]]) as usize
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Starts `tailscale nc` to the Mac and does the RFB handshake (a few tries,
/// in case the Mac app is just starting). Returns the server's logical size.
fn connect(ip: &str) -> io::Result<(Child, ChildStdin, ChildStdout, usize, usize)> {
    let mut last = io::Error::new(io::ErrorKind::Other, "no attempt");
    for _ in 0..3 {
        let mut child = Command::new(TAILSCALE)
            .args(["nc", ip, PORT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let attempt = (|| -> io::Result<(usize, usize)> {
            let ver = read_n(&mut stdout, 12)?;
            if &ver[..3] != b"RFB" {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a VNC server"));
            }
            stdin.write_all(b"RFB 003.008\n")?;
            let n = read_n(&mut stdout, 1)?[0] as usize;
            let types = read_n(&mut stdout, n)?;
            if !types.contains(&1) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "server wants a password"));
            }
            stdin.write_all(&[1])?;
            if be32(&read_n(&mut stdout, 4)?) != 0 {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused"));
            }
            stdin.write_all(&[1])?; // shared
            let init = read_n(&mut stdout, 24)?;
            let (w, h) = (be16(&init[0..2]), be16(&init[2..4]));
            read_n(&mut stdout, be32(&init[20..24]) as usize)?; // name
            // RGB565 little-endian, like the panel.
            stdin.write_all(&[0, 0, 0, 0, 16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0])?;
            // Encodings: Zlib, DesktopSize, Raw.
            let mut enc = vec![2u8, 0, 0, 3];
            for e in [6i32, -223, 0] {
                enc.extend_from_slice(&e.to_be_bytes());
            }
            stdin.write_all(&enc)?;
            stdin.flush()?;
            Ok((w, h))
        })();
        match attempt {
            Ok((w, h)) => return Ok((child, stdin, stdout, w, h)),
            Err(e) => {
                last = e;
                let _ = child.kill();
                let _ = child.wait();
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    Err(last)
}

/// The image the Mac sends, plus how it is shown.
struct View {
    canvas: Canvas,
    rot: Rot,
}

fn reader(mut input: ChildStdout, out: Out, view: Arc<Mutex<View>>, client: Option<Client>) -> io::Result<()> {
    let mut z = Decompress::new(true);
    let mut pixels = Vec::new();
    loop {
        let t = read_n(&mut input, 1)?[0];
        match t {
            0 => {
                let hdr = read_n(&mut input, 3)?;
                let n = be16(&hdr[1..3]);
                let mut dirty: Option<(usize, usize, usize, usize)> = None;
                let mut resized = false;
                for _ in 0..n {
                    let r = read_n(&mut input, 12)?;
                    let (x, y, w, h) = (be16(&r[0..]), be16(&r[2..]), be16(&r[4..]), be16(&r[6..]));
                    match be32(&r[8..]) as i32 {
                        -223 => {
                            view.lock().unwrap().canvas = Canvas::new(w, h);
                            resized = true;
                            continue;
                        }
                        6 => {
                            let len = be32(&read_n(&mut input, 4)?) as usize;
                            let data = read_n(&mut input, len)?;
                            pixels.clear();
                            pixels.reserve(w * h * 2);
                            let before = z.total_out();
                            z.decompress_vec(&data, &mut pixels, FlushDecompress::Sync)
                                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                            if (z.total_out() - before) as usize != w * h * 2 {
                                return Err(io::Error::new(io::ErrorKind::InvalidData, "short zlib rect"));
                            }
                        }
                        0 => pixels = read_n(&mut input, w * h * 2)?,
                        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected encoding")),
                    }
                    let mut v = view.lock().unwrap();
                    let cw = v.canvas.w;
                    if x + w > cw || y + h > v.canvas.h {
                        continue;
                    }
                    for row in 0..h {
                        let o = ((y + row) * cw + x) * 2;
                        v.canvas.px[o..o + w * 2].copy_from_slice(&pixels[row * w * 2..(row + 1) * w * 2]);
                    }
                    dirty = Some(match dirty {
                        None => (x, y, w, h),
                        Some((dx, dy, dw, dh)) => {
                            let (x0, y0) = (dx.min(x), dy.min(y));
                            let (x1, y1) = ((dx + dw).max(x + w), (dy + dh).max(y + h));
                            (x0, y0, x1 - x0, y1 - y0)
                        }
                    });
                }
                let v = view.lock().unwrap();
                let rect = if resized { Some((0, 0, v.canvas.w, v.canvas.h)) } else { dirty };
                if let Some(rect) = rect {
                    match &client {
                        Some(client) => present(client, &v.canvas, v.rot, rect, resized),
                        None => {
                            // Test mode: same drawing, into a stand-in panel buffer.
                            if let Some(r) = drawable(&v.canvas, v.rot, rect) {
                                let mut fb = vec![0u8; PW * PH * 2];
                                orient::blit(&v.canvas.px, v.canvas.w, &mut fb, PW, PH, v.rot, r);
                            }
                        }
                    }
                }
                let (w, h) = (v.canvas.w, v.canvas.h);
                drop(v);
                request_update(&out, true, w, h);
            }
            1 => {
                let h = read_n(&mut input, 5)?;
                read_n(&mut input, be16(&h[3..5]) * 6)?;
            }
            2 => {}
            3 => {
                let h = read_n(&mut input, 7)?;
                read_n(&mut input, be32(&h[3..7]) as usize)?;
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown server message")),
        }
    }
}

enum Gesture {
    None,
    Pointer,
    Scroll { cx: i32, cy: i32 },
}

/// Runs one connection. Ok(message) when it ends (back to the Mac list);
/// Err when the AppLoad window was closed.
fn session(client: &Client, rot: &mut Rot, mac: &Mac) -> io::Result<String> {
    let (mut child, stdin, stdout, w, h) = match connect(&mac.ip) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("macview: cannot reach {}: {e}", mac.name);
            return Ok(format!("Couldn't reach {}. Is Mac for reMarkable running?", mac.name));
        }
    };
    eprintln!("macview: connected to {} ({w}x{h})", mac.name);
    remember_mac(&mac.name);
    let out: Out = Arc::new(Mutex::new(stdin));
    let view = Arc::new(Mutex::new(View { canvas: Canvas::new(w, h), rot: *rot }));
    let ended = Arc::new(AtomicBool::new(false));
    {
        let (out, view, client, ended) = (out.clone(), view.clone(), Some(client.clone()), ended.clone());
        std::thread::spawn(move || {
            if let Err(e) = reader(stdout, out, view, client) {
                eprintln!("macview: connection ended: {e}");
            }
            ended.store(true, Ordering::SeqCst);
        });
    }
    *KEY_SINK.lock().unwrap() = Some(out.clone());
    send_orientation(&out, *rot);
    request_update(&out, false, w, h);

    let mut fingers: HashMap<i32, (i32, i32)> = HashMap::new();
    let mut gesture = Gesture::None;
    let result = loop {
        if ended.load(Ordering::SeqCst) {
            break Ok(format!("Disconnected from {}", mac.name));
        }
        let d = match wait_events(client, 200) {
            Ok(d) => d,
            Err(e) => break Err(e), // window closed
        };
        if update_rot(rot, &d) {
            let mut v = view.lock().unwrap();
            let shape_changed = v.rot.is_landscape() != rot.is_landscape();
            v.rot = *rot;
            if shape_changed {
                drop(v);
                send_orientation(&out, *rot); // the Mac answers with the new size
            } else {
                let rect = (0, 0, v.canvas.w, v.canvas.h);
                present(client, &v.canvas, v.rot, rect, true);
            }
        }
        for ev in d.inputs {
            let (lw, lh) = {
                let v = view.lock().unwrap();
                (v.canvas.w as i32, v.canvas.h as i32)
            };
            let (x, y) = rot.to_logical(ev.x, ev.y, PW, PH);
            let (x, y) = (x.min(lw - 1), y.min(lh - 1));
            match ev.input_type {
                qtfb::INPUT_TOUCH_PRESS | qtfb::INPUT_PEN_PRESS => {
                    fingers.insert(ev.dev_id, (x, y));
                    match (&gesture, fingers.len()) {
                        (Gesture::None, 1) => {
                            gesture = Gesture::Pointer;
                            send_pointer(&out, 1, x, y);
                        }
                        (Gesture::Pointer, 2) => {
                            send_pointer(&out, 0x80, x, y); // cancel the click/drag: it's a scroll
                            let (cx, cy) = centroid(&fingers);
                            gesture = Gesture::Scroll { cx, cy };
                        }
                        _ => {}
                    }
                }
                qtfb::INPUT_TOUCH_UPDATE | qtfb::INPUT_PEN_UPDATE => {
                    if !fingers.contains_key(&ev.dev_id) {
                        continue;
                    }
                    fingers.insert(ev.dev_id, (x, y));
                    match &mut gesture {
                        Gesture::Pointer if fingers.len() == 1 => send_pointer(&out, 1, x, y),
                        Gesture::Scroll { cx, cy } if fingers.len() >= 2 => {
                            let (nx, ny) = centroid(&fingers);
                            if (nx - *cx).abs() + (ny - *cy).abs() >= 4 {
                                send_scroll(&out, nx, ny, nx - *cx, ny - *cy);
                                *cx = nx;
                                *cy = ny;
                            }
                        }
                        _ => {}
                    }
                }
                qtfb::INPUT_TOUCH_RELEASE | qtfb::INPUT_PEN_RELEASE => {
                    fingers.remove(&ev.dev_id);
                    if let Gesture::Pointer = gesture {
                        if fingers.is_empty() {
                            send_pointer(&out, 0, x, y);
                        }
                    }
                    if fingers.is_empty() {
                        gesture = Gesture::None;
                    }
                }
                _ => {}
            }
        }
    };
    *KEY_SINK.lock().unwrap() = None;
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn centroid(f: &HashMap<i32, (i32, i32)>) -> (i32, i32) {
    let n = f.len().max(1) as i32;
    let (sx, sy) = f.values().fold((0, 0), |(a, b), (x, y)| (a + x, b + y));
    (sx / n, sy / n)
}

// ---------------------------------------------------------------- keyboards

const EV_KEY: u16 = 1;
const KEY_A: usize = 30;
const BTN_STYLUS: usize = 0x14b;
const KEY_BITS: usize = 96;

fn ioc(dir: u64, ty: u8, nr: u8, size: u64) -> u64 {
    (dir << 30) | (size << 16) | ((ty as u64) << 8) | nr as u64
}

fn is_keyboard(fd: i32) -> bool {
    let mut bits = [0u8; KEY_BITS];
    let req = ioc(2, b'E', 0x20 + EV_KEY as u8, KEY_BITS as u64);
    let n = unsafe { libc::ioctl(fd, req as _, bits.as_mut_ptr()) };
    let has = |b: usize| bits[b / 8] & (1 << (b % 8)) != 0;
    n > 0 && has(KEY_A) && !has(BTN_STYLUS)
}

/// Grabs every keyboard (rescanning for new ones) and sends its keys to the
/// current session, if there is one.
fn keyboards() {
    use std::collections::HashSet;
    use std::os::fd::AsRawFd;
    let open: Arc<Mutex<HashSet<String>>> = Arc::default();
    loop {
        if let Ok(dir) = std::fs::read_dir("/dev/input") {
            for e in dir.flatten() {
                let path = e.path().to_string_lossy().into_owned();
                if !e.file_name().to_string_lossy().starts_with("event") || open.lock().unwrap().contains(&path) {
                    continue;
                }
                let Ok(mut f) = std::fs::File::open(&path) else { continue };
                if !is_keyboard(f.as_raw_fd()) {
                    continue;
                }
                unsafe { libc::ioctl(f.as_raw_fd(), ioc(1, b'E', 0x90, 4) as _, 1 as libc::c_int) };
                eprintln!("macview: keyboard {path}");
                open.lock().unwrap().insert(path.clone());
                let open = open.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 24 * 64];
                    while let Ok(n) = f.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        for ev in buf[..n].chunks_exact(24) {
                            let ty = u16::from_ne_bytes([ev[16], ev[17]]);
                            let code = u16::from_ne_bytes([ev[18], ev[19]]);
                            let value = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
                            if ty == EV_KEY && (0..=2).contains(&value) {
                                if let Some(out) = KEY_SINK.lock().unwrap().as_ref() {
                                    send_key(out, value != 0, code);
                                }
                            }
                        }
                    }
                    eprintln!("macview: keyboard {path} gone");
                    open.lock().unwrap().remove(&path);
                });
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------- test mode

/// MACVIEW_TEST=<dir> MACVIEW_IP=<ip>: no screen. Lists Macs, connects,
/// flips orientation, scrolls the list, and saves what arrives as raw RGB565.
fn test_mode(dir: &str) -> io::Result<()> {
    for m in list_macs() {
        eprintln!("macview test: mac {} {} online={}", m.name, m.ip, m.online);
    }
    let ip = std::env::var("MACVIEW_IP").map_err(|_| io::Error::new(io::ErrorKind::Other, "set MACVIEW_IP to the Mac's address"))?;
    let (mut child, stdin, stdout, w, h) = connect(&ip)?;
    eprintln!("macview test: connected, {w}x{h}");
    let out: Out = Arc::new(Mutex::new(stdin));
    // Start sideways, as a tablet held in landscape does: the Mac's first image
    // is still portrait-sized and must be skipped, not drawn.
    let view = Arc::new(Mutex::new(View { canvas: Canvas::new(w, h), rot: Rot(1) }));
    let failed = Arc::new(AtomicBool::new(false));
    {
        let (out, view, failed) = (out.clone(), view.clone(), failed.clone());
        std::thread::spawn(move || {
            if let Err(e) = reader(stdout, out, view, None) {
                eprintln!("macview test: connection ended: {e}");
            }
            failed.store(true, Ordering::SeqCst);
        });
    }
    // Reproduce the crash condition: the Mac in portrait while the panel is
    // landscape, with a full portrait image arriving before the switch.
    send_orientation(&out, Rot(0));
    std::thread::sleep(Duration::from_secs(1));
    request_update(&out, false, w, h);
    std::thread::sleep(Duration::from_secs(1));
    send_orientation(&out, Rot(1));
    let save = |name: &str| {
        let v = view.lock().unwrap();
        let path = format!("{dir}/{name}-{}x{}.raw", v.canvas.w, v.canvas.h);
        std::fs::write(&path, &v.canvas.px).ok();
        eprintln!("macview test: saved {path}");
    };
    std::thread::sleep(Duration::from_secs(2));
    save("list");
    send_scroll(&out, 800, 1000, 0, -400); // two-finger swipe up: list scrolls down
    std::thread::sleep(Duration::from_secs(2));
    save("list-scrolled");
    view.lock().unwrap().rot = Rot(0);
    send_orientation(&out, Rot(0));
    std::thread::sleep(Duration::from_secs(3));
    save("portrait");
    view.lock().unwrap().rot = Rot(3);
    send_orientation(&out, Rot(3));
    std::thread::sleep(Duration::from_secs(2));
    let _ = child.kill();
    if failed.load(Ordering::SeqCst) {
        return Err(io::Error::new(io::ErrorKind::Other, "reader stopped early"));
    }
    eprintln!("macview test: passed");
    Ok(())
}

// ---------------------------------------------------------------- main

fn main() -> io::Result<()> {
    if let Ok(dir) = std::env::var("MACVIEW_TEST") {
        return test_mode(&dir);
    }
    let key: i32 = std::env::var("QTFB_KEY")
        .ok()
        .and_then(|k| k.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "QTFB_KEY not set (start from AppLoad)"))?;
    let big = Psf2Font::parse(FONT_BIG).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let small = Psf2Font::parse(FONT_SMALL).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let client: Client = Arc::new(Mutex::new(qtfb::QtfbClient::connect(key, qtfb::FBFMT_RMPP_RGB565, PW, PH, 2)?));
    std::thread::spawn(keyboards);

    let mut rot = Rot(0);
    // Pick up AppLoad's initial rotation before drawing anything.
    if let Ok(d) = wait_events(&client, 300) {
        update_rot(&mut rot, &d);
    }
    let mut msg = "Tap a Mac to show its windows".to_string();
    loop {
        let mac = match choose(&client, &mut rot, &msg, &big, &small) {
            Ok(m) => m,
            Err(_) => break, // window closed
        };
        {
            let (w, h) = if rot.is_landscape() { (PH, PW) } else { (PW, PH) };
            let mut c = Canvas::new(w, h);
            c.text(&big, 60, 70, &format!("Connecting to {}\u{2026}", mac.name), BLACK);
            present(&client, &c, rot, (0, 0, w, h), true);
        }
        match session(&client, &mut rot, &mac) {
            Ok(m) => msg = m,
            Err(_) => break, // window closed
        }
    }
    Ok(())
}
