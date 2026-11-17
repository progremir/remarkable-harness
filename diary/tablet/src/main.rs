//! Diary: a page that writes back.
//!
//! Write with the pen and pause. The new ink goes to the Mac (diaryd.py, over
//! `tailscale nc`), Claude reads it, and the answer comes back as pen strokes
//! that are written out under your message.
//!
//! Two ways to use it:
//! - The diary (the default): a bare page, like the diary in the film. Your
//!   message dissolves into the paper, the answer bleeds in with a quill,
//!   stays a few seconds and dissolves too. Nothing else is on screen; hold
//!   a finger in the bottom left corner for two seconds to open the notebook.
//! - The notebook: ruled paper that keeps the ink. "Diary" in its bar goes
//!   back. Its bar is at the bottom of the screen. It has pages, each as long as you like (drag
//!   with a finger to scroll), and a button to wipe the current page. Pages
//!   are kept in PAGES_PATH, so they are still there next time.
//!
//! All the thinking and layout happen on the Mac; this app only records pen
//! strokes and replays the strokes it is sent.

mod font;
mod qtfb;

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use font::Psf2Font;
use qtfb::QtfbClient;

const PW: i32 = 1620;
const PH: i32 = 2160;
const PORT: &str = "5910";
const TAILSCALE: &str = "/home/root/.vellum/bin/tailscale";
const HOST_PATH: &str = "/home/root/.config/diary/host";
const FONT: &[u8] = include_bytes!("../fonts/spleen-16x32.psfu");
const WHITE: u16 = 0xffff;
const BLACK: u16 = 0x0000;
const GRAY: u16 = 0x8410;
/// Radius of the user's pen line, in pixels.
const PEN_R: i32 = 2;
const FONT_BIG: &[u8] = include_bytes!("../fonts/spleen-32x64.psfu");
const PAGES_PATH: &str = "/home/root/.local/share/diary/pages.json";
/// Height of the notebook's bar; ink is never drawn over it. The bar is at
/// the bottom because AppLoad shows its own title bar, with a close button,
/// over the top of the screen whenever the top is touched.
const BAR_FULL: i32 = 100;
/// Bar buttons, as x ranges: previous page, next (or new) page, wipe.
const PREV: (i32, i32) = (0, 170);
const NEXT: (i32, i32) = (400, 570);
const WIPE: (i32, i32) = (PW - 520, PW);
/// The switch between fading ink and a notebook that keeps it.
const MODE: (i32, i32) = (650, 1050);
/// A finger must travel this far before it scrolls, so a resting palm does not.
const SCROLL_START: i32 = 45;
/// Ruled paper: a rule every RULE_GAP pixels of the page from RULE0 down
/// (8 mm, like a school notebook), and a red margin line. diaryd.py uses the
/// same numbers to sit its handwriting on the rules.
const RULE0: i32 = 200;
const RULE_GAP: i32 = 72;
const MARGIN_X: i32 = 150;
const RULE_C: u16 = 0xa5db; // soft blue-grey
const MARGIN_C: u16 = 0xdaeb; // faded red

// ---------------------------------------------------------------- drawing

/// Height of the bar right now: the fading diary is a bare page with no bar.
static BAR_NOW: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn bar() -> i32 {
    BAR_NOW.load(std::sync::atomic::Ordering::Relaxed)
}

/// The first screen row that belongs to the bar: the page ends here.
fn floor() -> i32 {
    PH - bar()
}

fn set_bar(vanish: bool) {
    BAR_NOW.store(if vanish { 0 } else { BAR_FULL }, std::sync::atomic::Ordering::Relaxed);
}

/// A rectangle that grows to cover everything drawn since the last refresh.
#[derive(Clone, Copy)]
struct Dirty {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Dirty {
    fn new() -> Self {
        Dirty { x0: PW, y0: PH, x1: -1, y1: -1 }
    }
    fn add(&mut self, x: i32, y: i32, r: i32) {
        self.x0 = self.x0.min(x - r).max(0);
        self.y0 = self.y0.min(y - r).max(0);
        self.x1 = self.x1.max(x + r).min(PW - 1);
        self.y1 = self.y1.max(y + r).min(PH - 1);
    }
    fn flush(&mut self, c: &QtfbClient) {
        if self.x1 >= self.x0 && self.y1 >= self.y0 {
            let _ = c.update_partial(self.x0, self.y0, self.x1 - self.x0 + 1, self.y1 - self.y0 + 1);
        }
        *self = Dirty::new();
    }
}

fn put(fb: &mut [u8], x: i32, y: i32, c: u16) {
    if x >= 0 && y >= 0 && x < PW && y < PH {
        let o = ((y * PW + x) * 2) as usize;
        fb[o..o + 2].copy_from_slice(&c.to_le_bytes());
    }
}

/// A round blob of ink at a screen position.
fn dot(fb: &mut [u8], x: i32, y: i32, r: i32, c: u16, d: &mut Dirty) {
    touch(fb, x, y, Tip { r, nib: 0, color: c }, 256, 0, d);
}

/// How a stroke's ink goes down: a round tip of radius `r`, or, when `nib`
/// is set, a broad quill nib of that half-width held at 40 degrees, which
/// makes thick downstrokes and hairline upstrokes.
#[derive(Clone, Copy)]
struct Tip {
    r: i32,
    nib: i32,
    color: u16,
}

impl Tip {
    /// The same tip, lighter: the wet front of a line, or ink that is fading.
    fn thin(self) -> Tip {
        Tip { r: (self.r - 1).max(1), nib: self.nib * 3 / 5, color: self.color }
    }
}

/// One touch of the tip. `keep` < 256 leaves blotchy holes (see `dissolve`).
fn touch(fb: &mut [u8], x: i32, y: i32, t: Tip, keep: i32, salt: i32, d: &mut Dirty) {
    let end = floor();
    let reach = t.r.max(t.nib) + 1;
    if y + reach < 0 || y - reach >= end {
        return;
    }
    let mut ink = |px: i32, py: i32| {
        let hash = ((px / 3) * 7919 + (py / 3) * 104729 + (px / 3) * (py / 3) * 31 + salt * 977) & 0xff;
        if py < end && hash < keep {
            put(fb, px, py, t.color);
        }
    };
    if t.nib > 0 {
        for k in -t.nib..=t.nib {
            let (px, py) = (x + k * 77 / 100, y - k * 64 / 100);
            ink(px, py);
            ink(px + 1, py);
            ink(px, py + 1);
        }
    } else {
        for dy in -t.r..=t.r {
            for dx in -t.r..=t.r {
                if dx * dx + dy * dy <= t.r * t.r + t.r / 2 {
                    ink(x + dx, y + dy);
                }
            }
        }
    }
    d.add(x, y.min(end - 1 - reach), reach);
}

fn draw_line(fb: &mut [u8], a: (i32, i32), b: (i32, i32), t: Tip, keep: i32, salt: i32, d: &mut Dirty) {
    let n = (b.0 - a.0).abs().max((b.1 - a.1).abs()).max(1);
    for k in 0..=n {
        touch(fb, a.0 + (b.0 - a.0) * k / n, a.1 + (b.1 - a.1) * k / n, t, keep, salt, d);
    }
}

fn line(fb: &mut [u8], a: (i32, i32), b: (i32, i32), r: i32, c: u16, d: &mut Dirty) {
    let n = (b.0 - a.0).abs().max((b.1 - a.1).abs()).max(1);
    for k in 0..=n {
        dot(fb, a.0 + (b.0 - a.0) * k / n, a.1 + (b.1 - a.1) * k / n, r, c, d);
    }
}

fn fill(fb: &mut [u8], x: i32, y: i32, w: i32, h: i32, c: u16) {
    for yy in y.max(0)..(y + h).min(PH) {
        for xx in x.max(0)..(x + w).min(PW) {
            put(fb, xx, yy, c);
        }
    }
}

fn text(fb: &mut [u8], f: &Psf2Font, x: i32, y: i32, s: &str, c: u16) {
    let bpr = f.bytes_per_row();
    let mut cx = x;
    for ch in s.chars() {
        let g = f.glyph(ch);
        for gy in 0..f.height {
            for gx in 0..f.width {
                if g[gy * bpr + gx / 8] & (0x80 >> (gx % 8)) != 0 {
                    put(fb, cx + gx as i32, y + gy as i32, c);
                }
            }
        }
        cx += f.width as i32;
    }
}

fn text_width(f: &Psf2Font, s: &str) -> i32 {
    (s.chars().count() * f.width) as i32
}

/// Paints clean ruled paper over a screen rectangle (below the bar).
fn paper(fb: &mut [u8], scroll: i32, x: i32, y: i32, w: i32, h: i32) {
    for yy in y.max(0)..(y + h).min(floor()) {
        let py = yy + scroll;
        let ruled = bar() > 0; // the diary's page is bare
        let rule = ruled && py >= RULE0 && (py - RULE0) % RULE_GAP < 2;
        for xx in x.max(0)..(x + w).min(PW) {
            let c = if ruled && (MARGIN_X..MARGIN_X + 2).contains(&xx) {
                MARGIN_C
            } else if rule {
                RULE_C
            } else {
                WHITE
            };
            put(fb, xx, yy, c);
        }
    }
}

/// One line of small grey text centred at the bottom of the screen.
fn status(client: &mut QtfbClient, f: &Psf2Font, s: &str) {
    let y = floor() - 56;
    let fb = client.framebuffer();
    fill(fb, 0, y, PW, f.height as i32, WHITE);
    text(fb, f, (PW - text_width(f, s)) / 2, y, s, GRAY);
    let _ = client.update_partial(0, y, PW, f.height as i32);
}

/// Lets the ink sink into the page: a few lighter and lighter passes, then white.
fn fade(client: &mut QtfbClient) {
    let from = 0;
    let to = (PW * floor() * 2) as usize;
    for _ in 0..3 {
        let fb = client.framebuffer();
        for px in fb[from..to].chunks_exact_mut(2) {
            let v = u16::from_le_bytes([px[0], px[1]]);
            if v != WHITE {
                let (r, g, b) = ((v >> 11) & 31, (v >> 5) & 63, v & 31);
                let (r, g, b) = (r + (31 - r + 1) / 2, g + (63 - g + 1) / 2, b + (31 - b + 1) / 2);
                px.copy_from_slice(&((r << 11) | (g << 5) | b).to_le_bytes());
            }
        }
        let _ = client.update_all();
        std::thread::sleep(Duration::from_millis(220));
    }
    client.framebuffer()[from..to].fill(0xff);
    let _ = client.update_all();
    std::thread::sleep(Duration::from_millis(250));
}

/// Ink soaking into the paper: the strokes break up into blotches that thin
/// out pass by pass until the paper is clean. Works in pure black and white,
/// so it looks right in the quick screen mode too.
fn dissolve(client: &mut QtfbClient, strokes: &[Stroke], scroll: i32, passes: i32) {
    let mut area = Dirty::new();
    for s in strokes {
        for p in &s.pts {
            area.add(p.0, (p.1 - scroll).clamp(0, floor() - 1), s.r.max(s.nib) + 2);
        }
    }
    if area.x1 < area.x0 {
        return;
    }
    let (x, y, w, h) = (area.x0, area.y0, area.x1 - area.x0 + 1, area.y1 - area.y0 + 1);
    for pass in 1..=passes {
        let fb = client.framebuffer();
        paper(fb, scroll, x, y, w, h);
        if pass < passes {
            let keep = 256 - 256 * pass / passes;
            let mut d = Dirty::new();
            for s in strokes {
                let tip = if pass * 2 > passes { s.tip().thin() } else { s.tip() };
                for seg in s.pts.windows(2) {
                    draw_line(fb, (seg[0].0, seg[0].1 - scroll), (seg[1].0, seg[1].1 - scroll), tip, keep, pass, &mut d);
                }
            }
        }
        let _ = client.update_partial(x, y, w, h);
        std::thread::sleep(Duration::from_millis(150));
    }
}

// ---------------------------------------------------------------- the pen

enum PenEv {
    Down(i32, i32),
    Move(i32, i32),
    Up,
}

/// The pen digitizer, read straight from the kernel. AppLoad also forwards
/// pen events, but thinned out and late, which makes handwriting laggy and
/// angular; here every sample arrives (several hundred a second).
struct Pen {
    fd: i32,
    raw: (i32, i32),
    touching: bool,
    was_touching: bool,
    last: (i32, i32),
}

/// Range of the digitizer's ABS_X / ABS_Y; it maps linearly onto the screen.
const PEN_MAX: (i32, i32) = (11180, 15340);

impl Pen {
    fn open() -> Option<Pen> {
        for e in std::fs::read_dir("/sys/class/input").ok()?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("event") {
                continue;
            }
            let dev = std::fs::read_to_string(e.path().join("device/name")).unwrap_or_default();
            if !dev.to_lowercase().contains("marker") {
                continue;
            }
            let path = format!("/dev/input/{name}\0");
            let fd = unsafe { libc::open(path.as_ptr() as *const libc::c_char, libc::O_RDONLY | libc::O_NONBLOCK) };
            if fd >= 0 {
                return Some(Pen { fd, raw: (0, 0), touching: false, was_touching: false, last: (-1, -1) });
            }
        }
        None
    }

    /// Everything the pen did since the last call, in screen pixels.
    fn read(&mut self) -> Vec<PenEv> {
        let mut out = Vec::new();
        let mut buf = [0u8; 24 * 64];
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 24 {
                return out;
            }
            for ev in buf[..n as usize].chunks_exact(24) {
                let kind = u16::from_le_bytes([ev[16], ev[17]]);
                let code = u16::from_le_bytes([ev[18], ev[19]]);
                let value = i32::from_le_bytes([ev[20], ev[21], ev[22], ev[23]]);
                match (kind, code) {
                    (3, 0) => self.raw.0 = value,
                    (3, 1) => self.raw.1 = value,
                    (1, 0x14a) => self.touching = value != 0, // BTN_TOUCH
                    (0, 0) => {
                        let p = (self.raw.0 * (PW - 1) / PEN_MAX.0, self.raw.1 * (PH - 1) / PEN_MAX.1);
                        match (self.was_touching, self.touching) {
                            (false, true) => out.push(PenEv::Down(p.0, p.1)),
                            (true, true) if p != self.last => out.push(PenEv::Move(p.0, p.1)),
                            (true, false) => out.push(PenEv::Up),
                            _ => {}
                        }
                        self.last = p;
                        self.was_touching = self.touching;
                    }
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------- pages

/// A pen stroke in page coordinates (the page scrolls under the screen).
struct Stroke {
    color: u16,
    r: i32,
    /// Half-width of a quill nib, or 0 for a round pen.
    nib: i32,
    pts: Vec<(i32, i32)>,
}

impl Stroke {
    fn tip(&self) -> Tip {
        Tip { r: self.r, nib: self.nib, color: self.color }
    }
}

#[derive(Default)]
struct Page {
    /// Page y shown at the top of the screen.
    scroll: i32,
    strokes: Vec<Stroke>,
}

impl Page {
    fn bottom(&self) -> i32 {
        self.strokes.iter().flat_map(|s| s.pts.iter().map(|p| p.1)).max().unwrap_or(0)
    }
    /// You can always scroll far enough to get clean paper under the last ink.
    fn max_scroll(&self) -> i32 {
        (self.bottom() - PH / 3).max(0)
    }
}

struct Book {
    pages: Vec<Page>,
    cur: usize,
    /// Fading ink: nothing is kept, the screen shows only `ghost`.
    vanish: bool,
    /// The ink on screen in fading mode. Never saved.
    ghost: Vec<Stroke>,
}

impl Book {
    fn load() -> Book {
        let mut book = Book { pages: Vec::new(), cur: 0, vanish: true, ghost: Vec::new() };
        let v: serde_json::Value = std::fs::read(PAGES_PATH).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for p in v["pages"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            let mut page = Page { scroll: p["scroll"].as_i64().unwrap_or(0) as i32, strokes: Vec::new() };
            for s in p["strokes"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                let flat: Vec<i32> = s["p"].as_array().map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0) as i32).collect()).unwrap_or_default();
                page.strokes.push(Stroke {
                    color: s["c"].as_u64().unwrap_or(0) as u16,
                    r: s["r"].as_i64().unwrap_or(2) as i32,
                    nib: s["n"].as_i64().unwrap_or(0) as i32,
                    pts: flat.chunks_exact(2).map(|p| (p[0], p[1])).collect(),
                });
            }
            book.pages.push(page);
        }
        if book.pages.is_empty() {
            book.pages.push(Page::default());
        }
        book.vanish = v["vanish"].as_bool().unwrap_or(true);
        set_bar(book.vanish);
        book.cur = (v["current"].as_u64().unwrap_or(0) as usize).min(book.pages.len() - 1);
        book
    }

    fn save(&self) {
        let pages: Vec<serde_json::Value> = self
            .pages
            .iter()
            .map(|p| {
                let strokes: Vec<serde_json::Value> = p
                    .strokes
                    .iter()
                    .map(|s| {
                        let flat: Vec<i32> = s.pts.iter().flat_map(|p| [p.0, p.1]).collect();
                        serde_json::json!({ "c": s.color, "r": s.r, "n": s.nib, "p": flat })
                    })
                    .collect();
                serde_json::json!({ "scroll": p.scroll, "strokes": strokes })
            })
            .collect();
        let doc = serde_json::json!({ "current": self.cur, "vanish": self.vanish, "pages": pages }).to_string();
        let tmp = format!("{PAGES_PATH}.tmp");
        if let Some(dir) = std::path::Path::new(PAGES_PATH).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&tmp, doc).is_ok() {
            let _ = std::fs::rename(&tmp, PAGES_PATH);
        }
    }

    fn page(&mut self) -> &mut Page {
        &mut self.pages[self.cur]
    }

    /// Page y at the top of the screen. The fading page does not scroll.
    fn scroll(&self) -> i32 {
        if self.vanish { 0 } else { self.pages[self.cur].scroll }
    }

    /// The ink that is on screen.
    fn strokes(&self) -> &Vec<Stroke> {
        if self.vanish { &self.ghost } else { &self.pages[self.cur].strokes }
    }

    fn strokes_mut(&mut self) -> &mut Vec<Stroke> {
        if self.vanish { &mut self.ghost } else { &mut self.pages[self.cur].strokes }
    }
}

/// Draws the first `upto` points of a stroke at its place on the screen.
fn draw_stroke(fb: &mut [u8], s: &Stroke, upto: usize, scroll: i32, d: &mut Dirty) {
    let n = upto.min(s.pts.len());
    if n == 0 {
        return;
    }
    let (lo, hi) = s.pts[..n].iter().fold((i32::MAX, i32::MIN), |(lo, hi), p| (lo.min(p.1), hi.max(p.1)));
    if hi - scroll < -8 || lo - scroll > PH + 8 {
        return;
    }
    touch(fb, s.pts[0].0, s.pts[0].1 - scroll, s.tip(), 256, 0, d);
    for w in s.pts[..n].windows(2) {
        draw_line(fb, (w[0].0, w[0].1 - scroll), (w[1].0, w[1].1 - scroll), s.tip(), 256, 0, d);
    }
}

/// The notebook bar: page turning on the left, back to the diary in the middle, wipe on the right.
fn draw_bar(fb: &mut [u8], book: &Book, armed: bool, big: &Psf2Font) {
    if book.vanish {
        return; // a bare page
    }
    fill(fb, 0, floor(), PW, bar(), WHITE);
    fill(fb, 0, floor() + 1, PW, 2, GRAY);
    let y = floor() + 3 + (bar() - 3 - big.height as i32) / 2;
    let centre = |fb: &mut [u8], (x0, x1): (i32, i32), s: &str, c: u16| {
        text(fb, big, (x0 + x1 - text_width(big, s)) / 2, y, s, c);
    };
    centre(fb, MODE, "Diary", BLACK);
    centre(fb, PREV, "<", if book.cur > 0 { BLACK } else { 0xd69a });
    centre(fb, (PREV.1, NEXT.0), &format!("{}/{}", book.cur + 1, book.pages.len()), BLACK);
    centre(fb, NEXT, if book.cur + 1 < book.pages.len() { ">" } else { "+" }, BLACK);
    let label = if armed { "Tap again to wipe" } else { "Wipe" };
    text(fb, big, PW - 50 - text_width(big, label), y, label, BLACK);
}

/// Repaints the whole screen: the bar, the page, and the part of the reply
/// that has been written so far.
fn render(client: &mut QtfbClient, book: &Book, hand: &Hand, armed: bool, big: &Psf2Font) {
    let fb = client.framebuffer();
    let scroll = book.scroll();
    paper(fb, scroll, 0, 0, PW, PH);
    let mut d = Dirty::new();
    for s in book.strokes() {
        draw_stroke(fb, s, usize::MAX, scroll, &mut d);
    }
    if let Some(s) = hand.queue.front() {
        draw_stroke(fb, s, hand.next, scroll, &mut d);
    }
    draw_bar(fb, book, armed, big);
    let _ = client.update_all();
}

fn refresh_bar(client: &mut QtfbClient, book: &Book, armed: bool, big: &Psf2Font) {
    if book.vanish {
        return;
    }
    draw_bar(client.framebuffer(), book, armed, big);
    let _ = client.update_partial(0, floor(), PW, bar());
}

// ---------------------------------------------------------------- the Mac

enum Net {
    Up,
    Down,
    Reply(serde_json::Value),
}

type Out = Arc<Mutex<Option<ChildStdin>>>;

fn host() -> String {
    // The Mac's Tailscale address, written there once when the app is set up.
    std::fs::read_to_string(HOST_PATH).unwrap_or_default().trim().to_string()
}

/// Keeps a connection to the Mac open, forever. The server greets with a
/// line; only then is the writer end handed to the main loop, so a message is
/// never written into a connection that did not reach the server.
fn network(tx: Sender<Net>, out: Out) {
    loop {
        let host = host();
        if host.is_empty() {
            eprintln!("diary: no Mac address in {HOST_PATH}");
        }
        let child = Command::new(TAILSCALE)
            .args(["nc", &host, PORT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            let mut stdin = child.stdin.take();
            let stdout = child.stdout.take().unwrap();
            for l in BufReader::new(stdout).lines() {
                let Ok(l) = l else { break };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) else { continue };
                if v.get("hello").is_some() {
                    if let Some(w) = stdin.take() {
                        *out.lock().unwrap() = Some(w);
                    }
                    let _ = tx.send(Net::Up);
                } else if tx.send(Net::Reply(v)).is_err() {
                    return;
                }
            }
            *out.lock().unwrap() = None;
            let _ = child.kill();
            let _ = child.wait();
        }
        if tx.send(Net::Down).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

fn send_line(out: &Out, line: &str) -> bool {
    let mut g = out.lock().unwrap();
    let Some(w) = g.as_mut() else { return false };
    let ok = w.write_all(line.as_bytes()).and_then(|_| w.write_all(b"\n")).and_then(|_| w.flush()).is_ok();
    if !ok {
        *g = None;
    }
    ok
}

// ---------------------------------------------------------------- replaying a reply

fn parse_strokes(v: &serde_json::Value) -> VecDeque<Stroke> {
    let mut out = VecDeque::new();
    for s in v["strokes"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let c: Vec<u16> = s["c"].as_array().map(|a| a.iter().map(|x| x.as_u64().unwrap_or(0) as u16).collect()).unwrap_or_default();
        let color = if c.len() == 3 { ((c[0] >> 3) << 11) | ((c[1] >> 2) << 5) | (c[2] >> 3) } else { BLACK };
        let flat: Vec<i32> = s["p"].as_array().map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0) as i32).collect()).unwrap_or_default();
        let pts: Vec<(i32, i32)> = flat.chunks_exact(2).map(|p| (p[0], p[1])).collect();
        if !pts.is_empty() {
            out.push_back(Stroke { color, r: (s["w"].as_i64().unwrap_or(3) as i32 / 2).clamp(1, 4), nib: (s["n"].as_i64().unwrap_or(0) as i32).clamp(0, 12), pts });
        }
    }
    out
}

/// The hand that writes the reply: where it is in the queue of strokes.
struct Hand {
    queue: VecDeque<Stroke>,
    /// Index of the next point of the front stroke.
    next: usize,
    /// Pixels the pen may still travel (negative while it is lifted).
    budget: f32,
    speed: f32,
}

impl Hand {
    fn idle(&self) -> bool {
        self.queue.is_empty()
    }

    /// Writes for `dt` seconds. Finished strokes move to `done`. Returns
    /// true when the next point is below the screen and the page must scroll.
    ///
    /// The line goes down thin at the tip and fills out a little behind it,
    /// the way wet ink spreads into paper.
    fn step(&mut self, dt: f32, fb: &mut [u8], scroll: i32, done: &mut Vec<Stroke>, d: &mut Dirty) -> bool {
        const TRAIL: usize = 14;
        self.budget = (self.budget + self.speed * dt).min(self.speed * 0.1);
        while self.budget > 0.0 {
            let Some(s) = self.queue.front() else { return false };
            let at = |i: usize| (s.pts[i].0, s.pts[i].1 - scroll);
            if self.next >= s.pts.len() {
                // The stroke is finished: let the rest of it fill out.
                for i in self.next.saturating_sub(TRAIL + 1).max(1)..s.pts.len() {
                    draw_line(fb, at(i - 1), at(i), s.tip(), 256, 0, d);
                }
                // Lifting the pen and moving to the next stroke takes a moment.
                let from = *s.pts.last().unwrap();
                done.push(self.queue.pop_front().unwrap());
                self.next = 0;
                if let Some(n) = self.queue.front() {
                    let (dx, dy) = ((n.pts[0].0 - from.0) as f32, (n.pts[0].1 - from.1) as f32);
                    self.budget -= 40.0 + (dx * dx + dy * dy).sqrt() * 0.35;
                }
                continue;
            }
            let b = at(self.next);
            if b.1 > floor() - 110 {
                return true;
            }
            let tip = s.tip().thin();
            if self.next == 0 {
                touch(fb, b.0, b.1, tip, 256, 0, d);
                self.next = 1;
                continue;
            }
            let a = at(self.next - 1);
            draw_line(fb, a, b, tip, 256, 0, d);
            if self.next > TRAIL {
                draw_line(fb, at(self.next - TRAIL - 1), at(self.next - TRAIL), s.tip(), 256, 0, d);
            }
            let (dx, dy) = ((b.0 - a.0) as f32, (b.1 - a.1) as f32);
            self.budget -= (dx * dx + dy * dy).sqrt().max(0.5);
            self.next += 1;
        }
        false
    }
}

// ---------------------------------------------------------------- main

/// Waits up to `ms` for AppLoad events or pen samples. Err when the window was closed.
fn wait_events(client: &QtfbClient, pen: Option<&Pen>, ms: i32) -> io::Result<qtfb::Drained> {
    let mut fds = [
        libc::pollfd { fd: client.raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: pen.map_or(-1, |p| p.fd), events: libc::POLLIN, revents: 0 },
    ];
    unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
    client.drain_events()
}

/// A finger on the page: it scrolls once it has moved far enough.
struct Finger {
    id: i32,
    start_y: i32,
    start_scroll: i32,
    scrolling: bool,
    drawn: Instant,
}

const DOTS_X: i32 = 180;

fn erase_dots(client: &mut QtfbClient, y: i32, scroll: i32) {
    if y - 12 >= 0 && y + 12 < floor() {
        paper(client.framebuffer(), scroll, DOTS_X, y - 12, 180, 24);
        let _ = client.update_partial(DOTS_X, y - 12, 180, 24);
    }
}

fn main() -> io::Result<()> {
    let key: i32 = std::env::var("QTFB_KEY")
        .ok()
        .and_then(|k| k.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "QTFB_KEY not set (start from AppLoad)"))?;
    let pause = Duration::from_millis(std::env::var("DIARY_PAUSE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2800));
    let small = Psf2Font::parse(FONT).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let big = Psf2Font::parse(FONT_BIG).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut client = QtfbClient::connect(key, qtfb::FBFMT_RMPP_RGB565, PW as usize, PH as usize, 2)?;

    // The quickest screen mode suits pen strokes. DIARY_REFRESH=fast or =default tries the others.
    match std::env::var("DIARY_REFRESH").as_deref() {
        Ok("default") => {}
        Ok("fast") => drop(client.set_refresh_mode(qtfb::REFRESH_MODE_FAST)),
        _ => drop(client.set_refresh_mode(qtfb::REFRESH_MODE_UFAST)),
    }
    let mut pen = Pen::open();
    eprintln!("diary: pen read {}", if pen.is_some() { "directly" } else { "through AppLoad" });

    let mut book = Book::load();
    let mut hand = Hand { queue: VecDeque::new(), next: 0, budget: 0.0, speed: 2600.0 };
    render(&mut client, &book, &hand, false, &big);

    let out: Out = Arc::default();
    let (tx, rx): (Sender<Net>, Receiver<Net>) = channel();
    {
        let out = out.clone();
        std::thread::spawn(move || network(tx, out));
    }

    let mut said_down = false;
    // The message being written: finished strokes (page coordinates), and the one under the pen.
    let mut pending: Vec<Vec<i32>> = Vec::new();
    let mut cur: Option<Vec<i32>> = None;
    let mut last_pen = Instant::now() - Duration::from_secs(10);
    // The message that was sent and not answered yet.
    let mut outbox: Option<String> = None;
    let mut waiting: Option<(Instant, i32)> = None; // since, page y of the dots
    let mut dots = 0;
    let mut msg_top = 0;
    let mut finger: Option<Finger> = None;
    let mut armed: Option<Instant> = None; // wipe was tapped once
    let mut tick = Instant::now();
    let mut was_writing = false;
    let mut coloured = false;
    // Fading mode: how long a finished reply stays, and when it goes.
    let mut hold = Duration::from_secs(6);
    let mut fade_at: Option<Instant> = None;
    // A finger resting in the bottom left corner of the diary page: (finger, since).
    let mut corner: Option<(i32, Instant)> = None;

    loop {
        let d = match wait_events(&client, pen.as_ref(), if hand.idle() { 60 } else { 8 }) {
            Ok(d) => d,
            Err(_) => break, // window closed
        };
        let mut dirty = Dirty::new();
        let mut bar_tap: Option<i32> = None;

        // Pen: straight from the digitizer when possible, else what AppLoad forwards.
        let mut strokes_in: Vec<PenEv> = pen.as_mut().map(|p| p.read()).unwrap_or_default();
        if pen.is_none() {
            for ev in &d.inputs {
                match ev.input_type {
                    qtfb::INPUT_PEN_PRESS => strokes_in.push(PenEv::Down(ev.x, ev.y)),
                    qtfb::INPUT_PEN_UPDATE => strokes_in.push(PenEv::Move(ev.x, ev.y)),
                    qtfb::INPUT_PEN_RELEASE => strokes_in.push(PenEv::Up),
                    _ => {}
                }
            }
        }
        for ev in strokes_in {
            let scroll = book.scroll();
            last_pen = Instant::now();
            match ev {
                PenEv::Down(x, y) if y >= floor() => bar_tap = Some(x),
                PenEv::Down(x, y) => {
                    finger = None;
                    if fade_at.take().is_some() {
                        // Writing again: the old answer makes way at once.
                        dissolve(&mut client, &book.ghost, 0, 3);
                        book.ghost.clear();
                    }
                    dot(client.framebuffer(), x, y, PEN_R, BLACK, &mut dirty);
                    cur = Some(vec![x, y + scroll]);
                }
                PenEv::Move(x, y) => {
                    if let Some(s) = cur.as_mut() {
                        let n = s.len();
                        let a = (s[n - 2], s[n - 1] - scroll);
                        line(client.framebuffer(), a, (x, y), PEN_R, BLACK, &mut dirty);
                        s.extend([x, y + scroll]);
                    }
                }
                PenEv::Up => {
                    if let Some(s) = cur.take() {
                        book.strokes_mut().push(Stroke { color: BLACK, r: PEN_R, nib: 0, pts: s.chunks_exact(2).map(|p| (p[0], p[1])).collect() });
                        pending.push(s);
                    }
                }
            }
        }

        for ev in d.inputs {
            let scroll = book.scroll();
            match ev.input_type {
                qtfb::INPUT_TOUCH_PRESS if ev.y >= floor() => bar_tap = Some(ev.x),
                qtfb::INPUT_TOUCH_PRESS if book.vanish => {
                    if ev.x < 220 && ev.y > PH - 220 {
                        corner = Some((ev.dev_id, Instant::now()));
                    }
                }
                qtfb::INPUT_TOUCH_RELEASE if corner.is_some_and(|c| c.0 == ev.dev_id) => corner = None,
                qtfb::INPUT_TOUCH_PRESS => {
                    // A touch right after pen activity is the writing hand, not a scroll.
                    if !book.vanish && finger.is_none() && cur.is_none() && last_pen.elapsed() > Duration::from_millis(700) {
                        finger = Some(Finger { id: ev.dev_id, start_y: ev.y, start_scroll: scroll, scrolling: false, drawn: Instant::now() });
                    }
                }
                qtfb::INPUT_TOUCH_UPDATE | qtfb::INPUT_TOUCH_RELEASE => {
                    let Some(f) = finger.as_mut().filter(|f| f.id == ev.dev_id) else { continue };
                    let dy = ev.y - f.start_y;
                    if dy.abs() > SCROLL_START {
                        f.scrolling = true;
                    }
                    let release = ev.input_type == qtfb::INPUT_TOUCH_RELEASE;
                    if f.scrolling && (release || f.drawn.elapsed() > Duration::from_millis(200)) {
                        let to = (f.start_scroll - dy).clamp(0, book.pages[book.cur].max_scroll().max(f.start_scroll));
                        f.drawn = Instant::now();
                        if to != scroll {
                            book.page().scroll = to;
                            render(&mut client, &book, &hand, armed.is_some(), &big);
                            dirty = Dirty::new();
                        }
                    }
                    if release {
                        finger = None;
                    }
                }
                _ => {}
            }
        }

        if corner.is_some_and(|c| c.1.elapsed() > Duration::from_secs(2)) {
            corner = None;
            bar_tap = Some((MODE.0 + MODE.1) / 2); // the same as tapping the mode switch
        }

        // The bar: turn the page, add a page, or wipe this one. Not while a reply is on its way.
        if let Some(x) = bar_tap {
            let busy = outbox.is_some() || !hand.idle() || cur.is_some();
            let within = |r: (i32, i32)| x >= r.0 && x < r.1;
            if busy {
            } else if within(MODE) {
                book.vanish = !book.vanish;
                set_bar(book.vanish);
                book.ghost.clear();
                pending.clear();
                fade_at = None;
                armed = None;
                render(&mut client, &book, &hand, false, &big);
                book.save();
            } else if book.vanish {
            } else if within(WIPE) {
                if armed.take().is_some() {
                    fade(&mut client);
                    let p = book.page();
                    p.strokes.clear();
                    p.scroll = 0;
                    pending.clear();
                    render(&mut client, &book, &hand, false, &big);
                    book.save();
                } else {
                    armed = Some(Instant::now());
                    refresh_bar(&mut client, &book, true, &big);
                }
            } else if within(PREV) || within(NEXT) {
                let before = book.cur;
                if within(PREV) && book.cur > 0 {
                    book.cur -= 1;
                } else if within(NEXT) && book.cur + 1 < book.pages.len() {
                    book.cur += 1;
                } else if within(NEXT) && !book.page().strokes.is_empty() {
                    book.pages.push(Page::default());
                    book.cur += 1;
                }
                if book.cur != before {
                    // Ink that was not sent yet stays on its page, but is no longer a message.
                    pending.clear();
                    armed = None;
                    render(&mut client, &book, &hand, false, &big);
                    book.save();
                }
            }
        }
        if armed.is_some_and(|t| t.elapsed() > Duration::from_secs(5)) {
            armed = None;
            refresh_bar(&mut client, &book, false, &big);
        }

        while let Ok(m) = rx.try_recv() {
            match m {
                Net::Up => {
                    if said_down {
                        status(&mut client, &small, "");
                        said_down = false;
                    }
                    if let Some(line) = &outbox {
                        send_line(&out, line);
                    }
                }
                Net::Down => {
                    if (outbox.is_some() || !pending.is_empty()) && !said_down {
                        status(&mut client, &small, "The diary can't reach the Mac. Still trying...");
                        said_down = true;
                    }
                }
                Net::Reply(v) => {
                    outbox = None;
                    if let Some((_, y)) = waiting.take() {
                        let scroll = book.scroll();
                        erase_dots(&mut client, y - scroll, scroll);
                    }
                    hand.queue = parse_strokes(&v);
                    coloured = hand.queue.iter().any(|s| s.color != BLACK);
                    hand.next = 0;
                    hand.budget = 0.0;
                    hand.speed = v["speed"].as_f64().unwrap_or(2600.0) as f32;
                    // Make room: bring the message to the top if the reply would run off the screen.
                    let bottom = v["bottom"].as_i64().unwrap_or(0) as i32;
                    hold = Duration::from_secs_f64(v["hold"].as_f64().unwrap_or(6.0));
                    let scroll = book.scroll();
                    if !book.vanish && bottom - scroll > PH - 80 {
                        let to = (msg_top - 60).max(scroll);
                        if to != scroll {
                            book.page().scroll = to;
                            render(&mut client, &book, &hand, armed.is_some(), &big);
                        }
                    }
                    tick = Instant::now();
                }
            }
        }

        // Send the message once the pen has rested.
        if !pending.is_empty() && cur.is_none() && outbox.is_none() && hand.idle() && last_pen.elapsed() > pause {
            let ys = || pending.iter().flat_map(|s| s.chunks(2).map(|p| p[1]));
            let bottom = ys().max().unwrap_or(0);
            msg_top = ys().min().unwrap_or(0);
            let mut msg = serde_json::json!({ "strokes": pending, "endless": true });
            let mut dots_under = bottom;
            if book.vanish {
                // The message sinks into the paper and the answer starts at the top of the clean page.
                msg["at"] = serde_json::json!(150);
                dots_under = 150;
                dissolve(&mut client, &book.ghost, 0, 6);
                book.ghost.clear();
                fade_at = None;
            }
            let line = msg.to_string();
            pending.clear();
            if !send_line(&out, &line) && !said_down {
                status(&mut client, &small, "The diary can't reach the Mac. Still trying...");
                said_down = true;
            }
            outbox = Some(line);
            // The dots sit on the first free rule under the message.
            let rule = RULE0 + ((dots_under + 50 - RULE0).max(0) / RULE_GAP + 1) * RULE_GAP;
            waiting = Some((Instant::now(), rule - 18));
            dots = 0;
            book.save();
        }

        // Three slow dots while the answer is on its way.
        if let Some((since, y)) = waiting {
            let want = (since.elapsed().as_millis() / 450) as i32;
            if want > dots {
                dots = want;
                let k = (dots - 1) % 4;
                let sy = y - book.scroll();
                if k == 3 {
                    erase_dots(&mut client, sy, book.scroll());
                } else if sy - 12 >= 0 && sy + 12 < floor() {
                    dot(client.framebuffer(), DOTS_X + 20 + k * 46, sy, 5, BLACK, &mut dirty);
                }
            }
        }

        let now = Instant::now();
        if !hand.idle() {
            was_writing = true;
            let dt = now.duration_since(tick).as_secs_f32().min(0.05);
            let scroll = book.scroll();
            if hand.step(dt, client.framebuffer(), scroll, book.strokes_mut(), &mut dirty) {
                if book.vanish {
                    // The fading page does not scroll: what does not fit is left unwritten.
                    hand.queue.clear();
                } else {
                    // The reply ran off the bottom: move the page up and keep writing.
                    book.page().scroll += PH / 2;
                    render(&mut client, &book, &hand, armed.is_some(), &big);
                    dirty = Dirty::new();
                }
            }
        } else if was_writing {
            was_writing = false;
            book.save();
            if coloured {
                // The quick screen mode shows colour poorly; one full refresh brings it out.
                let _ = client.request_full_refresh();
            }
            if book.vanish {
                fade_at = Some(Instant::now() + hold);
            }
        }
        // Fading mode: the answer has been read; it sinks back into the page.
        if fade_at.is_some_and(|t| Instant::now() >= t) && cur.is_none() && pending.is_empty() {
            fade_at = None;
            dissolve(&mut client, &book.ghost, 0, 7);
            book.ghost.clear();
        }
        tick = now;
        dirty.flush(&client);
    }
    book.save();
    Ok(())
}
