//! rexpaper — a Rex client for the reMarkable Paper Pro.
//!
//! Rex is a server that runs terminals; this is one more client of it. It
//! lists the server's sessions and tabs, shows a tab with its split panes
//! on e-ink, and types into them. The renderer, keyboard and rotation code
//! come from PaperTerm (MaximeRivest/paperterm, MIT); the local shell is
//! replaced by a Rex connection (see rex.rs, stream.rs, wire.rs).

#[cfg(target_os = "linux")]
mod evkbd;
#[cfg(not(target_os = "linux"))]
mod evkbd {
    //! No evdev off the tablet: a keyboard set that is always empty.
    use crate::keys::KeyAction;
    use std::os::fd::RawFd;
    pub struct Press {
        pub action: KeyAction,
        pub ctrl: bool,
        pub alt: bool,
    }
    pub struct Keyboards;
    impl Keyboards {
        pub fn new(_grab: bool) -> Self {
            Keyboards
        }
        pub fn scan(&mut self) -> Vec<RawFd> {
            Vec::new()
        }
        pub fn read(&mut self, _fd: RawFd) -> (Vec<Press>, bool) {
            (Vec::new(), true)
        }
        pub fn remove(&mut self, _fd: RawFd) {}
    }
}
mod font;
mod keys;
mod orient;
mod palette;
mod pen;
mod qtfb;
mod render;
mod rex;
mod screen;
mod stream;
mod ui;
mod web;
mod wire;

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::os::fd::RawFd;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use polling::{Event as PollEvent, Events, PollMode, Poller};
use serde_json::{json, Value};

use crate::font::Psf2Font;
use crate::keys::KeyAction;
use crate::orient::Rot;
use crate::pen::PenEv;
use crate::render::{TermArea, BLACK, SCREEN_H, SCREEN_W, WHITE};
use crate::rex::{Control, Net};
use crate::screen::Screen;
use crate::stream::{Item, StreamFilter};
use crate::ui::{clip, wrap, Rect};
use crate::wire::{In, Out, Target, Wire};

// The controls are at the bottom because AppLoad shows its own title bar
// over the top when the top is touched.
/// The status bar: back, the tab's name, and what needs attention.
const BAR_H: usize = 64;
/// The row of keys above the bar, for use without a keyboard.
const KEYS_H: usize = 92;
/// The strip that names each pane when a tab shows more than one.
const TITLE_H: usize = 36;
/// What AppLoad's on-screen keyboard covers at the bottom of the screen.
const KBD_H: usize = 376;
const ITEM_H: usize = 88;
const MENU_W: usize = 620;
const KBD_SCAN: Duration = Duration::from_secs(2);
/// Poller keys at and above this belong to keyboards: key = base + fd.
const KBD_KEY_BASE: usize = 1000;
/// Marks a touch event that this program made from the pen.
const PEN_TAP: i32 = -77;
const CONFIG_DIR: &str = "/home/root/.config/rexpaper";
const TERMINAL: &str = "com.superlogical.terminal";
const SHELL: &str = "com.superlogical.terminal.shell";
/// The label of the terminal block that reads handwriting (mac/scribe.py).
const SCRIBE: &str = "rexpaper scribe";
/// How long the pen rests before the writing is sent off to be read.
const INK_PAUSE: Duration = Duration::from_millis(1300);
// How AppLoad drives the e-ink (its REFRESH_MODE values).
/// Quickest, and leaves ghosts: only while the pen is writing.
const MODE_INK: i32 = 0;
/// Quick and clean enough for text: the normal mode.
const MODE_TEXT: i32 = 1;
/// Slower and leaves no ghost: for the update that removes ink.
const MODE_CLEAN: i32 = 3;
/// How long the clean mode stays on for an update to be drawn in it.
const CLEAN_FOR: Duration = Duration::from_millis(350);

const FONTS: [(&str, &[u8]); 3] = [
    ("12x24", include_bytes!("../fonts/spleen-12x24.psfu")),
    ("16x32", include_bytes!("../fonts/spleen-16x32.psfu")),
    ("32x64", include_bytes!("../fonts/spleen-32x64.psfu")),
];

/// The server's own emulator answers the program's queries and tracks its
/// title, so everything our local copy would say back is dropped.
#[derive(Clone)]
struct Listener;

impl EventListener for Listener {
    fn send_event(&self, _event: Event) {}
}

#[derive(Copy, Clone)]
struct GridSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

struct FdSource(RawFd);
impl AsRawFd for FdSource {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Dirty rects for one frame, in logical (canvas) coordinates. Kept apart
/// rather than unioned: two small edits on different lines would otherwise
/// become one large rect, which is slow to refresh on e-ink.
#[derive(Default)]
struct DirtyRegion {
    rects: Vec<(usize, usize, usize, usize)>,
}

/// Above this many rects per frame, fall back to a single union.
const MAX_RECTS: usize = 6;

impl DirtyRegion {
    fn add(&mut self, x: usize, y: usize, w: usize, h: usize) {
        for r in &mut self.rects {
            if r.1 == y && r.3 == h {
                let x0 = r.0.min(x);
                let x1 = (r.0 + r.2).max(x + w);
                *r = (x0, y, x1 - x0, h);
                return;
            }
        }
        self.rects.push((x, y, w, h));
    }

    fn add_rect(&mut self, r: Rect) {
        self.add(r.x, r.y, r.w, r.h);
    }

    fn flush(&mut self, canvas: &[u8], screen: &mut Screen, rot: Rot) {
        let mut rects = std::mem::take(&mut self.rects);
        if rects.len() > MAX_RECTS {
            let u = rects
                .iter()
                .copied()
                .reduce(|(rx, ry, rw, rh), (x, y, w, h)| {
                    let (x0, y0) = (rx.min(x), ry.min(y));
                    (x0, y0, (rx + rw).max(x + w) - x0, (ry + rh).max(y + h) - y0)
                })
                .unwrap();
            rects = vec![u];
        }
        let (lw, lh) = (render::canvas_w(), render::canvas_h());
        for (x, y, w, h) in rects {
            if x >= lw || y >= lh {
                continue;
            }
            let (w, h) = (w.min(lw - x), h.min(lh - y));
            if w == 0 || h == 0 {
                continue;
            }
            let (px, py, pw, ph) =
                orient::blit(canvas, lw, screen.framebuffer(), SCREEN_W, SCREEN_H, rot, (x, y, w, h));
            screen.update_partial(px as i32, py as i32, pw as i32, ph as i32);
        }
    }
}

// ---------------------------------------------------------------- list

enum RowKind {
    Session { sid: String, label: String },
    /// One tab. `blocks` are its terminals, for the names of notifications.
    Window { sid: String, wid: String, name: String, blocks: Vec<String> },
    /// A block outside every tab, such as a port forward. Not tappable.
    Note,
    NewSession,
}

/// One line of the session list.
struct Row {
    kind: RowKind,
    text: String,
    status: String,
    /// Where it was drawn, for taps.
    span: Option<Rect>,
}

struct ListView {
    rows: Vec<Row>,
    stale: Option<Instant>,
    drawn: bool,
    /// How many rows are scrolled off the top.
    top: usize,
    press: Option<(i32, i32, i32)>,
}

/// The most urgent state among a terminal's program status records.
fn status_of(records: &Value) -> String {
    let states: Vec<&str> = records
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["state"].as_str()).collect())
        .unwrap_or_default();
    ["blocked", "error", "working", "done"]
        .into_iter()
        .find(|s| states.contains(s))
        .map(|s| format!("[{s}]"))
        .unwrap_or_default()
}

/// A window's label, unless it is the default the server made from its ID.
fn real_label(w: &Value) -> Option<&str> {
    w["label"].as_str().filter(|l| !l.is_empty() && !l.starts_with("Window "))
}

/// Ask the server for every session and its tabs. Also returns a name for
/// each terminal, "session / title", for notifications.
type Loaded = (Vec<Row>, HashMap<String, String>, Vec<(String, String)>);

fn load_rows(ctl: &mut Control, net: &mut Net) -> Result<Loaded, String> {
    let mut rows = Vec::new();
    let mut names = HashMap::new();
    // Handwriting readers left over from an earlier run: session, block.
    let mut scribes = Vec::new();
    let sessions = ctl.call(net, "session.list", json!({}))?;
    for s in sessions["sessions"].as_array().into_iter().flatten() {
        let Some(sid) = s["session_id"].as_str() else {
            continue;
        };
        let Ok(view) = ctl.call(net, "session.view", json!({"session_id": sid})) else {
            continue; // destroyed since the list was made
        };
        let label = view["label"].as_str().unwrap_or("session").to_string();
        rows.push(Row {
            kind: RowKind::Session { sid: sid.to_string(), label: label.clone() },
            text: label.clone(),
            status: String::new(),
            span: None,
        });
        for (i, w) in view["windows"].as_array().into_iter().flatten().enumerate() {
            let Some(wid) = w["window_id"].as_str() else {
                continue;
            };
            let focused = w["focused_block_id"].as_str();
            let (mut blocks, mut first, mut main, mut status, mut others) =
                (Vec::new(), None, None, String::new(), 0);
            for b in w["layers"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|l| l["blocks"].as_array().into_iter().flatten())
            {
                let Some(bid) = b["block_id"].as_str() else {
                    continue;
                };
                if b["creator_name"] != TERMINAL {
                    others += 1;
                    continue;
                }
                let title = ctl
                    .terminal(net, "title", sid, bid, json!({}))
                    .ok()
                    .and_then(|t| t["title"].as_str().map(str::to_string))
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| b["label"].as_str().unwrap_or("terminal").to_string());
                let st = ctl
                    .terminal(net, "program_status", sid, bid, json!({}))
                    .map(|r| status_of(&r["records"]))
                    .unwrap_or_default();
                if status.is_empty() || st == "[blocked]" {
                    status = st;
                }
                names.insert(bid.to_string(), format!("{label} / {title}"));
                if Some(bid) == focused {
                    main = Some(title.clone());
                }
                first.get_or_insert(title);
                blocks.push(bid.to_string());
            }
            let title = real_label(w)
                .map(str::to_string)
                .or(main)
                .or(first)
                .unwrap_or_else(|| "no terminal".to_string());
            let extra = blocks.len().saturating_sub(1) + others;
            let more = if extra > 0 { format!("  (+{extra})") } else { String::new() };
            rows.push(Row {
                kind: RowKind::Window {
                    sid: sid.to_string(),
                    wid: wid.to_string(),
                    name: format!("{label} / {title}"),
                    blocks,
                },
                text: format!("{:>2}  {title}{more}", i + 1),
                status,
                span: None,
            });
        }
        // Blocks that sit in no tab: port forwards and detached terminals.
        let listed = ctl.call(net, "session.list_blocks", json!({"session_id": sid}));
        for b in listed.iter().flat_map(|l| l["blocks"].as_array().into_iter().flatten()) {
            if b["placement"] == "placed" {
                continue;
            }
            if b["label"] == SCRIBE {
                scribes.push((sid.to_string(), b["block_id"].as_str().unwrap_or("").to_string()));
                continue;
            }
            let what = if b["creator_name"] == "com.superlogical.tunnel" { "[port forward]" } else { "[detached]" };
            rows.push(Row {
                kind: RowKind::Note,
                text: format!("    {}", b["label"].as_str().unwrap_or("block")),
                status: what.to_string(),
                span: None,
            });
        }
    }
    rows.push(Row {
        kind: RowKind::NewSession,
        text: "+ New session".to_string(),
        status: String::new(),
        span: None,
    });
    Ok((rows, names, scribes))
}

// ---------------------------------------------------------------- panes

/// One terminal: a local emulator fed by the block's data stream.
struct TermView {
    term: Term<Listener>,
    parser: Processor<StdSyncHandler>,
    area: TermArea,
    /// What each drawn cell looked like last (see render::cell_look).
    shadow: Vec<u64>,
    data: Wire,
    filter: Option<StreamFilter>,
    /// Whether this client owns the block, and so decides its size.
    owner: bool,
    exited: bool,
    stream_closed: Option<String>,
    /// Typed bytes not yet sent. One write is in flight at a time, because
    /// the server may run two writes out of order.
    pending: Vec<u8>,
    inflight: Option<String>,
    /// When to read the screen again. The first read races with the
    /// stream, so output from that moment can be missing or doubled; a
    /// second read once the terminal is quiet settles it.
    settle: Option<(Instant, Instant)>,
}

enum Body {
    Term(Box<TermView>),
    /// A block that isn't a terminal: a few lines about it.
    Info(Vec<String>),
    /// A rexweb browser pane: pictures of its page.
    Web(Box<web::WebView>),
}

struct Pane {
    block_id: String,
    /// Where the pane sits in its tab, as fractions: x, y, w, h.
    frac: [f64; 4],
    /// Where it is drawn. `None` while another pane is zoomed.
    rect: Option<Rect>,
    title: String,
    title_drawn: String,
    body: Body,
    reopen: Option<(Instant, u32)>,
}

/// One open tab.
struct WinView {
    session_id: String,
    window_id: String,
    name: String,
    revision: u64,
    panes: Vec<Pane>,
    focus: usize,
    zoom: bool,
    /// Dragging selects text instead of scrolling.
    select: bool,
    drag: Option<(i32, i32)>,
    /// Where the finger went down, to tell a tap from a drag.
    press: Option<(i32, i32)>,
    repaint: bool,
    /// The layout changed on the server: read it again.
    stale: bool,
    /// The pane to focus once it appears, after a split.
    want_focus: Option<String>,
    chrome: String,
    floating: usize,
}

enum View {
    List(ListView),
    Win(Box<WinView>),
    /// A message that fills the screen. A tap tries to connect again.
    Notice { lines: Vec<String>, drawn: bool },
}

// ---------------------------------------------------------------- overlays

/// Something a menu item, a key or a dialog's answer makes happen.
#[derive(Clone)]
enum Act {
    NewSession,
    NewTab(String),
    Split(&'static str),
    Zoom,
    ClosePane,
    RenameTab,
    RenameSession(String, String),
    EndSession(String, String),
    Select,
    Paste,
    Font(i32),
    Refresh,
    Keyboard,
    /// Go to an address in the browser pane; back, forward or reload.
    WebGo,
    WebNav(&'static str),
    WebZoom(i32),
    /// Open a browser pane beside this pane.
    Browser(bool),
    /// Answer a prompt that an action on the server raised.
    Reply(String),
}

struct PickItem {
    id: String,
    text: String,
    on: bool,
}

enum AskKind {
    Text { value: String, hint: String },
    Confirm { detail: String, yes: String },
    Pick { items: Vec<PickItem>, multiple: bool },
}

struct Ask {
    title: String,
    kind: AskKind,
    act: Act,
    /// Tap targets, set when drawn: cancel, confirm, and the pick items.
    cancel: Rect,
    ok: Rect,
    items: Vec<Rect>,
}

enum Answer {
    Text(String),
    Yes,
    Ids(Vec<String>),
}

enum Overlay {
    Menu { items: Vec<(String, Act)>, spans: Vec<Rect>, area: Rect },
    Ask(Ask),
}

#[derive(Clone, Copy, PartialEq)]
enum KeyBtn {
    Key(KeyAction),
    Ctrl,
    CtrlC,
    Menu,
}

const KEY_ROW: [(&str, KeyBtn); 11] = [
    ("Esc", KeyBtn::Key(KeyAction::Esc)),
    ("Tab", KeyBtn::Key(KeyAction::Tab)),
    ("Ctrl", KeyBtn::Ctrl),
    ("^C", KeyBtn::CtrlC),
    ("\u{2190}", KeyBtn::Key(KeyAction::Left)),
    ("\u{2193}", KeyBtn::Key(KeyAction::Down)),
    ("\u{2191}", KeyBtn::Key(KeyAction::Up)),
    ("\u{2192}", KeyBtn::Key(KeyAction::Right)),
    ("Space", KeyBtn::Key(KeyAction::Char(' '))),
    ("Enter", KeyBtn::Key(KeyAction::Enter)),
    ("Menu", KeyBtn::Menu),
];

/// What the pen has written and not yet turned into text.
#[derive(Default)]
struct Ink {
    /// Strokes still being written, in canvas pixels.
    strokes: Vec<Vec<(i32, i32)>>,
    /// Strokes sent to be read: they stay on screen until the text comes.
    sent: Vec<Vec<(i32, i32)>>,
    down: bool,
    last_up: Option<Instant>,
    /// The request waiting for its text: id, and when it was sent.
    waiting: Option<(u64, Instant)>,
    /// The pen is pressing a button, not writing.
    tapping: Option<(i32, i32)>,
    /// The eraser is down: where it was last, the left and right ends of
    /// its rub, how many typed characters the rub has deleted, and whether
    /// it has rubbed out any ink.
    erasing: Option<((i32, i32), i32, i32, usize, bool)>,
    /// When the pen last came near or left.
    away: Option<Instant>,
    /// The pen is within reach of the screen.
    near: bool,
    /// A slow refresh that clears ghosts is due, with the next full one.
    /// Only **Refresh screen** asks for it.
    deghost_now: bool,
}

/// The terminal block on the server's machine that reads handwriting.
struct Scribe {
    sid: String,
    bid: String,
    /// The request that is creating the block, and the one reading its
    /// screen. They are answered in `control_msg`: nothing here waits for
    /// the server, because waiting would stop the ink.
    creating: Option<String>,
    polling: Option<String>,
    /// It has said it is ready for requests.
    ready: bool,
    /// Requests not yet written to it, and the write that is under way.
    /// One write at a time, because the server may run two out of order.
    queued: Vec<String>,
    writing: Option<String>,
    polled: Instant,
    /// Its title changed while its screen was being read: the answer on
    /// the way may be from before the change, so read again after it.
    stale: bool,
}

/// What a request to the helper was for.
enum Want {
    Ink,
    /// A picture of this browser pane's page.
    Shot(String),
    /// Something done to this browser pane: look at it again afterwards.
    Act(String),
    Open,
    /// A browser pane opened in a new tab of this session: show that tab.
    OpenTab(String),
}

/// A picture of a page being downloaded.
struct Fetch {
    bid: String,
    hash: String,
    /// The request for the download's ticket, then the connection.
    req: Option<String>,
    wire: Option<Wire>,
    welcomed: bool,
    buf: Vec<u8>,
}

struct Retry {
    at: Instant,
    tries: u32,
    /// The tab to open again: session, window, focused pane, zoomed.
    resume: Option<(String, String, Option<String>, bool)>,
}

struct App {
    screen: Screen,
    canvas: Vec<u8>,
    rot: Rot,
    fonts: Vec<Psf2Font>,
    font_ix: usize,
    /// The size of a page's text, in percent of what fits the pane.
    web_zoom: u32,
    ui: Psf2Font,
    big: Psf2Font,
    palette: palette::Palette,
    net: Net,
    ctl: Option<Control>,
    /// "other", or "app" to receive prompts.
    kind: String,
    view: View,
    overlay: Option<Overlay>,
    overlay_drawn: bool,
    /// Draw into the canvas only: one refresh follows for all of it. The
    /// screen drops a full refresh that a partial one follows at once.
    hold: bool,
    /// When to refresh the whole screen once more. AppLoad drops a full
    /// refresh when a partial one arrives while it is still pending, so
    /// after each full refresh the partial ones wait and one more full
    /// refresh shows what they would have.
    settle_at: Option<Instant>,
    dirty: DirtyRegion,
    /// Leave room at the bottom for AppLoad's on-screen keyboard.
    kbd: bool,
    /// Ctrl and Alt pressed on their own, on the key row or on AppLoad's
    /// keyboard: each applies to the next key.
    ctrl: bool,
    alt: bool,
    clip: String,
    banner: Option<(String, Instant)>,
    names: HashMap<String, String>,
    retry: Option<Retry>,
    last_load: Instant,
    ink: Ink,
    scribe: Option<Scribe>,
    /// The pen is read from the digitizer, so AppLoad's copies are skipped.
    raw_pen: bool,
    /// Requests the helper has not answered, by id, and what each is for.
    wants: HashMap<u64, Want>,
    /// Answers read since the last request: the next one tells the helper.
    acks: Vec<u64>,
    next_req: u64,
    /// Pictures of pages on their way from the Mac.
    fetches: Vec<Fetch>,
    /// Keys typed into a browser pane and not sent yet: its block, the bytes.
    web_keys: (String, Vec<u8>),
    /// Where ink was removed: this part of the screen is drawn again.
    repair: Option<Rect>,
    /// Where ink was removed and its ghost is still to be cleared: that
    /// part is drawn again in the clean mode once the pen is up.
    flash: Option<Rect>,
    /// What the eraser has cleared in the rub that is under way.
    /// Log pen timings (the `trace` setting).
    trace: bool,
    trace_down: Option<Instant>,
    /// The screen's mode now, and when a spell of the clean mode ends.
    mode: i32,
    clean_until: Option<Instant>,
}

/// Where the parts of the screen are.
struct Geo {
    lw: usize,
    lh: usize,
    /// Top of the key row: the terminals fill everything above it.
    keys_y: usize,
    bar_y: usize,
}

/// Collect the blocks of a layout tree with their places.
fn flatten(node: &Value, out: &mut Vec<(String, [f64; 4])>) {
    if let Some(id) = node["block_id"].as_str() {
        let mut r = [0.0, 0.0, 1.0, 1.0];
        for (i, v) in node["rect"].as_array().into_iter().flatten().take(4).enumerate() {
            r[i] = v.as_f64().unwrap_or(r[i]);
        }
        out.push((id.to_string(), r));
    } else if node["split"].is_object() {
        flatten(&node["split"]["before"], out);
        flatten(&node["split"]["after"], out);
    }
}

struct WinInfo {
    /// Block, place, creator, label.
    panes: Vec<(String, [f64; 4], String, String)>,
    focused: Option<String>,
    zoomed: Option<String>,
    /// The tab's label, if someone gave it one.
    label: Option<String>,
    floating: usize,
}

/// What a session view says about one window. Only the tiled layer is
/// shown; floating layers are counted.
fn window_info(view: &Value, wid: &str) -> Option<WinInfo> {
    let w = view["windows"]
        .as_array()?
        .iter()
        .find(|w| w["window_id"].as_str() == Some(wid))?;
    let mut info = WinInfo {
        panes: Vec::new(),
        focused: w["focused_block_id"].as_str().map(str::to_string),
        zoomed: w["zoomed_block_id"].as_str().map(str::to_string),
        label: real_label(w).map(str::to_string),
        floating: 0,
    };
    for layer in w["layers"].as_array().into_iter().flatten() {
        if layer["kind"] != "tiled" {
            if layer["state"] != "minimized" {
                info.floating += 1;
            }
            continue;
        }
        let mut places = Vec::new();
        flatten(&layer["layout"], &mut places);
        for (id, frac) in places {
            let b = layer["blocks"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|b| b["block_id"].as_str() == Some(id.as_str()));
            let creator = b.and_then(|b| b["creator_name"].as_str()).unwrap_or("").to_string();
            let label = b.and_then(|b| b["label"].as_str()).unwrap_or("").to_string();
            info.panes.push((id, frac, creator, label));
        }
    }
    Some(info)
}

/// A pane's place on a `w` x `h` canvas.
fn frac_rect(f: [f64; 4], w: usize, h: usize) -> Rect {
    let x0 = (f[0] * w as f64).round() as usize;
    let y0 = (f[1] * h as f64).round() as usize;
    let x1 = (((f[0] + f[2]) * w as f64).round() as usize).min(w);
    let y1 = (((f[1] + f[3]) * h as f64).round() as usize).min(h);
    Rect::new(x0.min(w), y0.min(h), x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1))
}

/// The part of a pane that holds its content: all of it when it is the
/// only one shown, otherwise what the title strip and the dividing lines
/// leave.
fn inner(r: Rect, multi: bool) -> Rect {
    if multi {
        Rect::new(r.x + 2, r.y + TITLE_H, r.w.saturating_sub(6), r.h.saturating_sub(TITLE_H + 4))
    } else {
        r
    }
}

/// Columns and rows that fit in `r`.
fn fit(font: &Psf2Font, r: Rect) -> (usize, usize) {
    ((r.w / font.width).max(1), (r.h / font.height).max(1))
}

/// Where a `cols` x `rows` terminal is drawn inside `r`. One larger than
/// `r` shows its left columns and its last rows.
fn make_area(font: &Psf2Font, r: Rect, cols: usize, rows: usize) -> TermArea {
    let (fc, fr) = fit(font, r);
    let shown = rows.min(fr);
    TermArea {
        cols: cols.min(fc),
        rows: shown,
        cw: font.width,
        ch: font.height,
        x0: r.x + r.w.saturating_sub(fc * font.width) / 2,
        y0: r.y,
        row_off: rows - shown,
    }
}

fn new_term(cols: usize, rows: usize, content: &str) -> (Term<Listener>, Processor<StdSyncHandler>) {
    let mut term = Term::new(
        Config {
            scrolling_history: 5000,
            ..Config::default()
        },
        &GridSize { cols, lines: rows },
        Listener,
    );
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    parser.advance(&mut term, content.as_bytes());
    (term, parser)
}

/// The terminal's screen and scrollback as escape sequences that redraw it.
fn fetch_content(ctl: &mut Control, net: &mut Net, sid: &str, bid: &str) -> Result<String, String> {
    let extras = json!({"format": "vt", "extra_cursor": true, "extra_modes": true,
                        "extra_style": true, "extra_scrolling_region": true,
                        "max_bytes": 900_000});
    let first = ctl.terminal(net, "format", sid, bid, extras)?;
    let content = first["content"].as_str().unwrap_or("").to_string();
    let (Some(id), Some(total)) = (first["id"].as_u64(), first["total"].as_u64()) else {
        return Ok(content);
    };
    // More scrollback than one reply holds, and `content` is its oldest
    // part. Read the newest part instead, from a line start.
    let offset = total.saturating_sub(600_000);
    let tail = ctl.terminal(
        net,
        "blob_read",
        sid,
        bid,
        json!({"id": id, "offset": offset, "max_bytes": 900_000}),
    )?;
    let text = tail["data"].as_str().unwrap_or("");
    let start = text.find("\r\n").map_or(0, |i| i + 2);
    Ok(format!("\x1b[0m{}", &text[start..]))
}

impl TermView {
    /// Take one message from the data stream. `mute` drops the program's
    /// output, for bytes the screen we just read already includes. Returns
    /// Rex's own messages, such as a new size.
    fn on_data(&mut self, msg: In, mute: bool) -> Vec<Value> {
        let mut out = Vec::new();
        match msg {
            In::Binary(b) => match &mut self.filter {
                None => {
                    // The handshake answer: WELCOME carries the channel nonce.
                    if b.len() >= 32 && b[0] == 0x81 {
                        let nonce = u64::from_le_bytes(b[24..32].try_into().unwrap());
                        self.filter = Some(StreamFilter::new(nonce));
                    } else {
                        let code = b.get(1).copied().unwrap_or(0);
                        self.stream_closed = Some(format!("refused, reason {code}"));
                    }
                }
                Some(filter) => {
                    for item in filter.feed(&b) {
                        match item {
                            Item::Bytes(bytes) if !mute => {
                                self.parser.advance(&mut self.term, &bytes);
                                if let Some((at, limit)) = &mut self.settle {
                                    *at = (Instant::now() + Duration::from_millis(700)).min(*limit);
                                }
                            }
                            Item::Bytes(_) => {}
                            Item::Msg(m) => out.push(m),
                        }
                    }
                }
            },
            In::Closed(reason) => self.stream_closed = Some(reason),
            In::Text(_) => {}
        }
        out
    }

    fn size(&self) -> (usize, usize) {
        (self.term.columns(), self.term.screen_lines())
    }

    /// The grid point under a canvas position, for selecting text.
    fn point_at(&self, x: i32, y: i32) -> Point {
        let a = &self.area;
        let col = ((x - a.x0 as i32).max(0) as usize / a.cw).min(a.cols.saturating_sub(1));
        let row = ((y - a.y0 as i32).max(0) as usize / a.ch).min(a.rows.saturating_sub(1));
        let line = (row + a.row_off) as i32 - self.term.grid().display_offset() as i32;
        Point::new(Line(line), Column(col))
    }
}

/// Claim nothing, but follow one terminal: size it, start its stream and
/// read its screen. The caller has attached to the session and owns the
/// block.
fn open_term(
    ctl: &mut Control,
    net: &mut Net,
    font: &Psf2Font,
    sid: &str,
    bid: &str,
    r: Rect,
) -> Result<TermView, String> {
    let (cols, rows) = fit(font, r);
    ctl.terminal(
        net,
        "resize",
        sid,
        bid,
        json!({"columns": cols, "rows": rows,
               "cell_width_px": font.width, "cell_height_px": font.height}),
    )?;
    let ticket = ctl.call(
        net,
        "data.connect",
        json!({"session_id": sid, "block_id": bid,
               "data_ref_id": "terminal", "subprotocol": "rex.data.terminal.v1"}),
    )?;
    let ticket = ticket["ticket"]
        .as_str()
        .and_then(wire::b64url_decode)
        .filter(|t| t.len() == 16)
        .ok_or("the server sent no data ticket")?;
    let data = net.open("/api/data", "rex.data.terminal.v1");
    let mut hello = vec![0x01];
    hello.extend_from_slice(&ticket);
    data.send(Out::Binary(hello));

    let (term, parser) = new_term(cols, rows, "");
    let now = Instant::now();
    let mut tv = TermView {
        term,
        parser,
        area: make_area(font, r, cols, rows),
        shadow: Vec::new(),
        data,
        filter: None,
        owner: true,
        exited: false,
        stream_closed: None,
        pending: Vec::new(),
        inflight: None,
        settle: Some((now + Duration::from_millis(700), now + Duration::from_secs(5))),
    };
    // Wait for the stream to start before reading the screen: the stream
    // then covers everything after the read, and what it carried before
    // the read is dropped as already included.
    match net.wait_wire(tv.data.id, Duration::from_secs(20)) {
        Some(first) => {
            tv.on_data(first, true);
        }
        None => return Err("the terminal's stream did not start".into()),
    }
    if let Some(why) = &tv.stream_closed {
        return Err(format!("the terminal's stream was {why}"));
    }
    let content = fetch_content(ctl, net, sid, bid)?;
    tv.parser.advance(&mut tv.term, content.as_bytes());
    for m in net.take_wire(tv.data.id) {
        for msg in tv.on_data(m, true) {
            // A size from before our resize took effect.
            let (c, r2) = (msg["cols"].as_u64().unwrap_or(0), msg["rows"].as_u64().unwrap_or(0));
            if msg["type"] == "child_exited" {
                tv.exited = true;
            } else if msg["type"] == "resize" && c > 0 && r2 > 0 && (c as usize, r2 as usize) != tv.size() {
                tv.term.resize(GridSize { cols: c as usize, lines: r2 as usize });
                tv.area = make_area(font, r, c as usize, r2 as usize);
            }
        }
    }
    Ok(tv)
}

/// What to show for a block that isn't a terminal.
fn info_lines(ctl: &mut Control, net: &mut Net, sid: &str, bid: &str, creator: &str, label: &str) -> Vec<String> {
    let call = |ctl: &mut Control, net: &mut Net, m: &str| {
        ctl.call(net, &format!("{creator}.{m}"), json!({"session_id": sid, "block_id": bid, "args": {}}))
    };
    let mut lines = Vec::new();
    match creator {
        "com.superlogical.tunnel" => {
            lines.push("Port forward".to_string());
            if let Ok(i) = call(ctl, net, "info") {
                let s = |k: &str| i[k].as_str().unwrap_or("").to_string();
                if i["flavor"] == "dial" {
                    lines.push(format!("connects to {}", s("target")));
                    if !s("client_address").is_empty() {
                        lines.push(format!("from {}", s("client_address")));
                    }
                } else {
                    lines.push(format!("listens on {}", s("address")));
                    if !s("client_target").is_empty() {
                        lines.push(format!("for {}", s("client_target")));
                    }
                }
                lines.push(format!("{} connections now", i["connections"].as_u64().unwrap_or(0)));
            }
        }
        "com.superlogical.onboarding" => {
            lines.push("Rex walkthrough".to_string());
            if let Ok(i) = call(ctl, net, "state") {
                let step = i["step"].as_str().filter(|s| !s.is_empty()).unwrap_or("first step");
                lines.push(format!("at: {step}"));
            }
            lines.push("Open it in Rex on the Mac.".to_string());
        }
        _ => {
            lines.push(if label.is_empty() { "Block".to_string() } else { label.to_string() });
            lines.push(format!("made by {creator}"));
            lines.push("This client can't show it.".to_string());
        }
    }
    lines
}

impl App {
    fn geo(&self) -> Geo {
        let (lw, lh) = (render::canvas_w(), render::canvas_h());
        let bottom = lh - if self.kbd { KBD_H.min(lh / 2) } else { 0 };
        let bar_y = bottom - BAR_H;
        Geo { lw, lh, keys_y: bar_y - KEYS_H, bar_y }
    }

    /// Copy the whole canvas to the screen and refresh all of it.
    fn present(&mut self) {
        if self.hold {
            return;
        }
        self.show_all();
        self.settle_at = Some(Instant::now() + Duration::from_millis(450));
    }

    fn show_all(&mut self) {
        let (lw, lh) = (render::canvas_w(), render::canvas_h());
        self.dirty = DirtyRegion::default();
        orient::blit(
            &self.canvas,
            lw,
            self.screen.framebuffer(),
            SCREEN_W,
            SCREEN_H,
            self.rot,
            (0, 0, lw, lh),
        );
        self.screen.update_all();
    }

    /// Refresh the parts of the screen that changed.
    fn flush(&mut self) {
        if let Some(t) = self.trace_down.filter(|_| !self.dirty.rects.is_empty()) {
            eprintln!("rexpaper: trace first ink sent {} us after the pen landed, held {}", t.elapsed().as_micros(),
                !(self.settle_at.is_none() || self.ink.down || !self.ink.strokes.is_empty()));
            self.trace_down = None;
        }
        // Ink is never held back: a pen that leaves no line feels broken.
        if self.settle_at.is_none() || self.ink.down || !self.ink.strokes.is_empty() {
            self.dirty.flush(&self.canvas, &mut self.screen, self.rot);
        }
    }

    fn notice(&mut self, text: &str) {
        eprintln!("rexpaper: {text}");
        self.overlay = None;
        self.view = View::Notice {
            lines: wrap(text, 44),
            drawn: false,
        };
    }

    fn say(&mut self, text: &str) {
        eprintln!("rexpaper: {text}");
        self.banner = Some((text.to_string(), Instant::now()));
        self.touch_chrome();
    }

    /// Make the bar, or the list, draw again.
    fn touch_chrome(&mut self) {
        match &mut self.view {
            View::Win(wv) => wv.chrome.clear(),
            View::List(lv) if self.overlay.is_none() => lv.drawn = false,
            _ => {}
        }
    }

    fn call(&mut self, method: &str, payload: Value) -> Result<Value, String> {
        let ctl = self.ctl.as_mut().ok_or("not connected")?;
        ctl.call(&mut self.net, method, payload)
    }

    fn send(&mut self, method: &str, payload: Value) {
        if let Some(ctl) = self.ctl.as_mut() {
            ctl.send(method, payload);
        }
    }

    // ------------------------------------------------------ connection

    fn connect(&mut self) {
        self.ctl = None;
        let kind = self.kind.clone();
        let result = Control::connect(&mut self.net, &kind).and_then(|mut ctl| {
            // Lets this connection use every session, and sends the events
            // that keep the list current and carry notifications.
            ctl.call(&mut self.net, "session.attach_all", json!({"event_scope": "all"}))?;
            let loaded = load_rows(&mut ctl, &mut self.net)?;
            Ok((ctl, loaded))
        });
        let resume = self.retry.as_ref().and_then(|r| r.resume.clone());
        match result {
            Ok((ctl, (rows, names, scribes))) => {
                self.ctl = Some(ctl);
                // Keep one reader from an earlier run and close the rest.
                self.scribe = None;
                for (i, (sid, bid)) in scribes.into_iter().enumerate() {
                    if i == 0 {
                        self.scribe = Some(Scribe { sid, bid, creating: None, polling: None, ready: true, queued: Vec::new(), writing: None, polled: Instant::now(), stale: false });
                    } else {
                        self.send("block.close", json!({"session_id": sid, "block_id": bid}));
                    }
                }
                self.retry = None;
                self.names = names;
                self.last_load = Instant::now();
                self.view = View::List(ListView { rows, stale: None, drawn: false, top: 0, press: None });
                if let Some((sid, wid, focus, zoom)) = resume {
                    self.open(&sid, &wid, None, focus.as_deref());
                    if matches!(&self.view, View::Win(wv) if zoom && wv.panes.len() > 1) {
                        self.perform(Act::Zoom, None);
                    }
                }
            }
            Err(e) => {
                let host = self.net.target.host().to_string();
                let tries = self.retry.as_ref().map_or(0, |r| r.tries) + 1;
                let wait = Duration::from_secs((1u64 << tries.min(4)).min(15));
                self.retry = Some(Retry { at: Instant::now() + wait, tries, resume });
                self.notice(&format!(
                    "Can't reach Rex on {host}: {e}. Trying again shortly; tap to try now."
                ));
            }
        }
    }

    /// The control connection is gone. Remember the open tab and start
    /// connecting again.
    fn lost(&mut self, why: &str) {
        let resume = match &self.view {
            View::Win(wv) => Some((
                wv.session_id.clone(),
                wv.window_id.clone(),
                wv.panes.get(wv.focus).map(|p| p.block_id.clone()),
                wv.zoom,
            )),
            _ => self.retry.as_ref().and_then(|r| r.resume.clone()),
        };
        self.ctl = None;
        self.retry = Some(Retry { at: Instant::now() + Duration::from_millis(500), tries: 0, resume });
        self.notice(&format!("Lost the connection to Rex ({why}). Connecting again..."));
    }

    fn show_list(&mut self) {
        self.clean_until = None;
        self.set_mode(MODE_TEXT);
        let Some(ctl) = self.ctl.as_mut() else {
            return self.connect();
        };
        let top = match &self.view {
            View::List(lv) => lv.top,
            _ => 0,
        };
        match load_rows(ctl, &mut self.net) {
            Ok((rows, names, _)) => {
                self.names = names;
                self.last_load = Instant::now();
                let top = top.min(rows.len().saturating_sub(1));
                self.view = View::List(ListView { rows, stale: None, drawn: false, top, press: None });
            }
            Err(e) => self.lost(&e),
        }
    }

    // ------------------------------------------------------ tabs

    /// Lay the panes of `info` out on the canvas: which are shown, and where.
    fn place(&self, fracs: &[[f64; 4]], focus: usize, zoom: bool) -> Vec<Option<Rect>> {
        let g = self.geo();
        let single = zoom || fracs.len() == 1;
        fracs
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if single {
                    (i == focus).then(|| Rect::new(0, 0, g.lw, g.keys_y))
                } else {
                    Some(frac_rect(*f, g.lw, g.keys_y))
                }
            })
            .collect()
    }

    fn make_pane(&mut self, sid: &str, p: &(String, [f64; 4], String, String), rect: Option<Rect>, multi: bool) -> Pane {
        let (bid, frac, creator, label) = p;
        let font = self.fonts[self.font_ix].clone();
        let g = self.geo();
        // A pane hidden by zoom is sized as if it filled the screen.
        let r = inner(rect.unwrap_or(Rect::new(0, 0, g.lw, g.keys_y)), multi && rect.is_some());
        let (mut title, mut reopen) = (label.clone(), None);
        // A rexweb pane is a terminal whose screen says so. Its label can't
        // tell: rexweb sets it to the page's title.
        let browser = creator == TERMINAL && self.is_browser(sid, bid, &label);
        let body = match (creator.as_str(), self.ctl.as_mut()) {
            (TERMINAL, _) if browser => Body::Web(Box::new(web::WebView::new())),
            (TERMINAL, Some(ctl)) => {
                if let Some(t) = ctl
                    .terminal(&mut self.net, "title", sid, bid, json!({}))
                    .ok()
                    .and_then(|t| t["title"].as_str().map(str::to_string))
                    .filter(|t| !t.trim().is_empty())
                {
                    title = t;
                }
                match open_term(ctl, &mut self.net, &font, sid, bid, r) {
                    Ok(tv) => Body::Term(Box::new(tv)),
                    Err(e) => {
                        eprintln!("rexpaper: {bid}: {e}");
                        reopen = Some((Instant::now() + Duration::from_secs(2), 1));
                        Body::Info(vec!["Couldn't open this terminal:".to_string(), e])
                    }
                }
            }
            (_, Some(ctl)) => Body::Info(info_lines(ctl, &mut self.net, sid, bid, creator, label)),
            _ => Body::Info(vec!["Not connected.".to_string()]),
        };
        Pane {
            block_id: bid.clone(),
            frac: *frac,
            rect,
            title,
            title_drawn: String::new(),
            body,
            reopen,
        }
    }

    /// Attach to the tab's session, claim its terminals, size them to this
    /// screen and start following their output.
    /// Whether a terminal block is a rexweb pane.
    fn is_browser(&mut self, sid: &str, bid: &str, label: &str) -> bool {
        label == web::LABEL
            || self.ctl.as_mut().is_some_and(|ctl| {
                let args = json!({"format": "text", "trim": true, "max_bytes": 400});
                ctl.terminal(&mut self.net, "format", sid, bid, args)
                    .is_ok_and(|v| v["content"].as_str().is_some_and(|t| t.trim_start().starts_with(web::SCREEN)))
            })
    }

    /// The terminals among `panes` to claim. Browser panes are left alone:
    /// rexweb draws a pane on the Mac when any device has claimed or sized it.
    fn claimable(&mut self, sid: &str, panes: &[&(String, [f64; 4], String, String)]) -> Vec<String> {
        panes
            .iter()
            .filter(|p| p.2 == TERMINAL && !self.is_browser(sid, &p.0, &p.3))
            .map(|p| p.0.clone())
            .collect()
    }

    fn open_window(&mut self, sid: &str, wid: &str, name: Option<&str>, focus: Option<&str>) -> Result<WinView, String> {
        self.call("session.attach", json!({"session_id": sid}))?;
        let view = self.call("session.view", json!({"session_id": sid}))?;
        let info = window_info(&view, wid).ok_or("that tab is gone")?;
        let terms = self.claimable(sid, &info.panes.iter().collect::<Vec<_>>());
        if !terms.is_empty() {
            self.call(
                "session.own_block",
                json!({"session_id": sid, "block_ids": terms, "mode": "take"}),
            )?;
        }
        let pick = |id: &Option<String>| id.as_ref().and_then(|id| info.panes.iter().position(|p| &p.0 == id));
        let wanted = focus.and_then(|f| info.panes.iter().position(|p| p.0 == f));
        let zoom = info.zoomed.is_some() && wanted.is_none();
        let focus = wanted.or(pick(&info.zoomed)).or(pick(&info.focused)).unwrap_or(0);
        let fracs: Vec<[f64; 4]> = info.panes.iter().map(|p| p.1).collect();
        let rects = self.place(&fracs, focus, zoom);
        let multi = rects.iter().flatten().count() > 1;
        let mut panes = Vec::new();
        for (p, rect) in info.panes.iter().zip(rects) {
            panes.push(self.make_pane(sid, p, rect, multi));
        }
        let name = match name {
            Some(n) => n.to_string(),
            None => {
                let label = view["label"].as_str().unwrap_or("session");
                let title = panes.get(focus).map_or("", |p| p.title.as_str());
                format!("{label} / {}", info.label.as_deref().unwrap_or(title))
            }
        };
        Ok(WinView {
            session_id: sid.to_string(),
            window_id: wid.to_string(),
            name,
            revision: view["revision"].as_u64().unwrap_or(0),
            panes,
            focus,
            zoom,
            select: false,
            drag: None,
            press: None,
            repaint: true,
            stale: false,
            want_focus: None,
            chrome: String::new(),
            floating: info.floating,
        })
    }

    fn open(&mut self, sid: &str, wid: &str, name: Option<&str>, focus: Option<&str>) {
        let old = match &self.view {
            View::Win(wv) if wv.session_id != sid => Some(wv.session_id.clone()),
            _ => None,
        };
        match self.open_window(sid, wid, name, focus) {
            Ok(wv) => {
                self.view = View::Win(Box::new(wv));

                if let Some(old) = old {
                    self.send("session.detach", json!({"session_id": old}));
                }
            }
            Err(e) => {
                if self.ctl.is_some() {
                    self.send("session.detach", json!({"session_id": sid}));
                    self.show_list();
                    self.say(&format!("Couldn't open that tab: {e}"));
                }
            }
        }
    }

    /// Leave the tab. Detaching hands its terminals back to the next
    /// client in line for them, usually the Mac app.
    fn close_window(&mut self) {
        if let View::Win(wv) = &self.view {
            let sid = wv.session_id.clone();
            self.send("session.detach", json!({"session_id": sid}));
        }
        self.overlay = None;
        self.show_list();
    }

    /// The tab's layout changed on the server: read it again, keep the
    /// panes that are still there and open the new ones.
    fn refresh_window(&mut self) {
        let View::Win(wv) = &mut self.view else {
            return;
        };
        wv.stale = false;
        let (sid, wid) = (wv.session_id.clone(), wv.window_id.clone());
        let view = match self.call("session.view", json!({"session_id": sid})) {
            Ok(v) => v,
            Err(_) => return self.close_window(),
        };
        let Some(info) = window_info(&view, &wid) else {
            return self.close_window();
        };
        let View::Win(wv) = &mut self.view else {
            return;
        };
        wv.revision = view["revision"].as_u64().unwrap_or(wv.revision);
        wv.floating = info.floating;
        let focused_id = wv.panes.get(wv.focus).map(|p| p.block_id.clone());
        let want = wv.want_focus.take();
        let zoom = wv.zoom;
        let mut old: HashMap<String, Pane> =
            std::mem::take(&mut wv.panes).into_iter().map(|p| (p.block_id.clone(), p)).collect();
        let new_terms = self.claimable(&sid, &info.panes.iter().filter(|p| !old.contains_key(&p.0)).collect::<Vec<_>>());
        if !new_terms.is_empty() {
            let _ = self.call(
                "session.own_block",
                json!({"session_id": sid, "block_ids": new_terms, "mode": "take"}),
            );
        }
        let at = |id: &Option<String>| id.as_ref().and_then(|id| info.panes.iter().position(|p| &p.0 == id));
        let focus = at(&want).or(at(&focused_id)).or(at(&info.focused)).unwrap_or(0);
        let fracs: Vec<[f64; 4]> = info.panes.iter().map(|p| p.1).collect();
        let zoom = zoom && fracs.len() > 1;
        let rects = self.place(&fracs, focus, zoom);
        let multi = rects.iter().flatten().count() > 1;
        let mut panes = Vec::new();
        for (p, rect) in info.panes.iter().zip(rects) {
            match old.remove(&p.0) {
                Some(mut pane) => {
                    pane.frac = p.1;
                    panes.push(pane);
                }
                None => panes.push(self.make_pane(&sid, p, rect, multi)),
            }
        }
        let View::Win(wv) = &mut self.view else {
            return;
        };
        wv.panes = panes;
        wv.focus = focus;
        wv.zoom = zoom;
        self.relayout();
    }

    /// Place every pane again and fit the terminals this client owns to
    /// their new places. Called when anything about the screen changes.
    fn relayout(&mut self) {
        let font = self.fonts[self.font_ix].clone();
        let View::Win(wv) = &self.view else {
            self.touch_chrome();
            return;
        };
        let fracs: Vec<[f64; 4]> = wv.panes.iter().map(|p| p.frac).collect();
        let rects = self.place(&fracs, wv.focus, wv.zoom);
        let multi = rects.iter().flatten().count() > 1;
        let (View::Win(wv), ctl) = (&mut self.view, self.ctl.as_mut()) else {
            return;
        };
        let mut resizes = Vec::new();
        for (pane, rect) in wv.panes.iter_mut().zip(rects) {
            pane.rect = rect;
            pane.title_drawn.clear();
            if let Body::Web(wb) = &mut pane.body {
                wb.drawn = false;
                wb.hash.clear();
                wb.next_shot = Instant::now();
            }
            let (Some(rect), Body::Term(tv)) = (rect, &mut pane.body) else {
                continue;
            };
            let r = inner(rect, multi);
            let want = fit(&font, r);
            if tv.owner && want != tv.size() {
                tv.term.resize(GridSize { cols: want.0, lines: want.1 });
                resizes.push((pane.block_id.clone(), want));
            }
            tv.area = make_area(&font, r, tv.term.columns(), tv.term.screen_lines());
        }
        wv.repaint = true;
        if let Some(ctl) = ctl {
            for (bid, (cols, rows)) in resizes {
                ctl.send(
                    &format!("{TERMINAL}.resize"),
                    json!({"session_id": wv.session_id, "block_id": bid,
                           "args": {"columns": cols, "rows": rows,
                                    "cell_width_px": font.width, "cell_height_px": font.height}}),
                );
            }
        }
    }

    /// Claim the tab's terminals again and size them to this screen.
    fn take_over(&mut self) {
        let (View::Win(wv), Some(ctl)) = (&mut self.view, self.ctl.as_mut()) else {
            return;
        };
        let ids: Vec<String> = wv
            .panes
            .iter_mut()
            .filter_map(|p| match &mut p.body {
                Body::Term(tv) if !tv.owner => {
                    tv.owner = true;
                    Some(p.block_id.clone())
                }
                _ => None,
            })
            .collect();
        ctl.send(
            "session.own_block",
            json!({"session_id": wv.session_id, "block_ids": ids, "mode": "take"}),
        );
        // Sizes are compared with what each terminal has now, so make the
        // ones just claimed differ.
        for p in wv.panes.iter_mut() {
            if let Body::Term(tv) = &mut p.body {
                if ids.contains(&p.block_id) {
                    tv.term.resize(GridSize { cols: 1, lines: 1 });
                }
            }
        }
        self.relayout();
    }

    fn focused_term(&mut self) -> Option<(&mut TermView, String, String)> {
        let View::Win(wv) = &mut self.view else {
            return None;
        };
        let sid = wv.session_id.clone();
        let pane = wv.panes.get_mut(wv.focus)?;
        match &mut pane.body {
            Body::Term(tv) => Some((tv, sid, pane.block_id.clone())),
            Body::Info(_) | Body::Web(_) => None,
        }
    }

    fn flush_input(&mut self) {
        let Some(ctl) = self.ctl.as_mut() else {
            return;
        };
        let View::Win(wv) = &mut self.view else {
            return;
        };
        for pane in wv.panes.iter_mut() {
            let Body::Term(tv) = &mut pane.body else {
                continue;
            };
            if tv.inflight.is_some() || tv.pending.is_empty() {
                continue;
            }
            let data = String::from_utf8_lossy(&tv.pending).to_string();
            tv.pending.clear();
            tv.inflight = Some(ctl.send(
                &format!("{TERMINAL}.write"),
                json!({"session_id": wv.session_id, "block_id": pane.block_id, "args": {"data": data}}),
            ));
        }
    }

    fn type_bytes(&mut self, bytes: Vec<u8>) {
        if let Some((bid, _)) = self.focused_web() {
            // Sent together a moment later, as one request for a word
            // rather than one for each letter.
            if self.web_keys.0 != bid {
                self.flush_web_keys();
                self.web_keys.0 = bid;
            }
            self.web_keys.1.extend(bytes);
            return;
        }
        if let Some((tv, _, _)) = self.focused_term() {
            tv.pending.extend(bytes);
            tv.term.selection = None;
            if tv.term.grid().display_offset() != 0 {
                tv.term.scroll_display(Scroll::Bottom);
            }
        }
        self.flush_input();
    }

    fn key(&mut self, action: KeyAction, ctrl: bool, alt: bool) {
        let ctrl = ctrl || std::mem::take(&mut self.ctrl);
        let alt = alt || std::mem::take(&mut self.alt);
        if let Some(Overlay::Ask(ask)) = &mut self.overlay {
            match (&mut ask.kind, action) {
                (_, KeyAction::Esc) => return self.close_overlay(),
                (AskKind::Text { value, .. }, KeyAction::Char(c)) if !ctrl => value.push(c),
                (AskKind::Text { value, .. }, KeyAction::Backspace) => {
                    value.pop();
                }
                (_, KeyAction::Enter) => return self.answer(),
                _ => return,
            }
            self.overlay_drawn = false;
            return;
        }
        if self.overlay.is_some() {
            if action == KeyAction::Esc {
                self.close_overlay();
            }
            return;
        }
        match &self.view {
            View::Win(_) => {
                let mode = self.focused_term().map(|(tv, _, _)| *tv.term.mode());
                let mode = match mode {
                    Some(mode) => mode,
                    None if self.focused_web().is_some() => TermMode::empty(),
                    None => return,
                };
                self.type_bytes(keys::encode(action, ctrl, alt, mode));
                self.touch_chrome();
            }
            View::Notice { .. } if action == KeyAction::Enter => self.connect(),
            _ => {}
        }
    }

    // ------------------------------------------------------ actions

    fn close_overlay(&mut self) {
        if let Some(Overlay::Ask(Ask { act: Act::Reply(call_id), .. })) = self.overlay.take() {
            self.send("client.reply", json!({"call_id": call_id, "result": {"cancelled": true}}));
        }
        self.overlay = None;
        match &mut self.view {
            View::Win(wv) => wv.repaint = true,
            View::List(lv) => lv.drawn = false,
            View::Notice { drawn, .. } => *drawn = false,
        }
    }

    fn ask(&mut self, title: &str, kind: AskKind, act: Act) {
        self.overlay = Some(Overlay::Ask(Ask {
            title: title.to_string(),
            kind,
            act,
            cancel: Rect::default(),
            ok: Rect::default(),
            items: Vec::new(),
        }));
        self.overlay_drawn = false;
    }

    fn menu(&mut self, items: Vec<(String, Act)>) {
        self.overlay = Some(Overlay::Menu { items, spans: Vec::new(), area: Rect::default() });
        self.overlay_drawn = false;
    }

    fn window_menu(&mut self) {
        let View::Win(wv) = &self.view else {
            return;
        };
        let it = |s: &str, a: Act| (s.to_string(), a);
        let mut items = Vec::new();
        if matches!(wv.panes.get(wv.focus).map(|p| &p.body), Some(Body::Web(_))) {
            items.extend([
                it("Go to address", Act::WebGo),
                it("Page back", Act::WebNav("back")),
                it("Page forward", Act::WebNav("forward")),
                it("Reload page", Act::WebNav("reload")),
                it("Page text bigger", Act::WebZoom(1)),
                it("Page text smaller", Act::WebZoom(-1)),
            ]);
        }
        items.extend([
            it("New tab", Act::NewTab(wv.session_id.clone())),
            it("Split right", Act::Split("horizontal")),
            it("Split down", Act::Split("vertical")),
            it("Open browser pane", Act::Browser(false)),
            it("Open browser tab", Act::Browser(true)),
        ]);
        if wv.panes.len() > 1 {
            items.push(it(if wv.zoom { "Show all panes" } else { "Zoom this pane" }, Act::Zoom));
        }
        items.extend([
            it("Close this pane", Act::ClosePane),
            it("Rename tab", Act::RenameTab),
            it("Select text", Act::Select),
            it("Paste", Act::Paste),
            it("Smaller text", Act::Font(-1)),
            it("Larger text", Act::Font(1)),
            it("Refresh screen", Act::Refresh),
            it(
                if self.kbd { "Use the whole screen" } else { "Make room for keyboard" },
                Act::Keyboard,
            ),
        ]);
        self.menu(items);
    }

    /// The dialog's answer, from its confirm button or the Enter key.
    fn answer(&mut self) {
        let Some(Overlay::Ask(ask)) = self.overlay.take() else {
            return;
        };
        let answer = match ask.kind {
            AskKind::Text { value, .. } => Answer::Text(value),
            AskKind::Confirm { .. } => Answer::Yes,
            AskKind::Pick { items, .. } => {
                Answer::Ids(items.into_iter().filter(|i| i.on).map(|i| i.id).collect())
            }
        };
        self.close_overlay();
        self.perform(ask.act, Some(answer));
    }

    /// Do `act`. One that needs a name or a yes first opens a dialog and
    /// comes back here with the answer.
    fn perform(&mut self, act: Act, answer: Option<Answer>) {
        let shell = json!({"block": {"flavor": SHELL}});
        let (win_sid, win_wid, focus_bid) = match &self.view {
            View::Win(wv) => (
                wv.session_id.clone(),
                wv.window_id.clone(),
                wv.panes.get(wv.focus).map(|p| p.block_id.clone()),
            ),
            _ => Default::default(),
        };
        let result: Result<(), String> = (|| {
            match (act, answer) {
                (Act::NewSession, _) => {
                    let made = self.call("session.create", json!({"initial_windows": [{"layout": shell}]}))?;
                    let sid = made["session_id"].as_str().ok_or("no session came back")?.to_string();
                    let wid = made["initial_windows"][0]["window_id"].as_str().unwrap_or("").to_string();
                    self.open(&sid, &wid, None, None);
                }
                (Act::NewTab(sid), _) => {
                    // `focus: false` leaves the Mac app on the tab it shows.
                    let made = self.call(
                        "session.new_window",
                        json!({"session_id": sid, "layout": shell, "focus": false}),
                    )?;
                    let wid = made["window_id"].as_str().ok_or("no tab came back")?.to_string();
                    self.open(&sid, &wid, None, None);
                }
                (Act::Split(direction), _) => {
                    let anchor = focus_bid.ok_or("no pane to split")?;
                    let made = self.call(
                        "session.new_split",
                        json!({"session_id": win_sid, "anchor_id": anchor, "direction": direction,
                               "side": "after", "layout": shell, "focus": false}),
                    )?;
                    if let View::Win(wv) = &mut self.view {
                        wv.want_focus = made["block_ids"][0].as_str().map(str::to_string);
                        wv.zoom = false;
                        wv.stale = true;
                    }
                }
                (Act::Zoom, _) => {
                    if let View::Win(wv) = &mut self.view {
                        wv.zoom = !wv.zoom;
                    }
                    self.relayout();
                }
                (Act::ClosePane, None) => self.ask(
                    "Close this pane?",
                    AskKind::Confirm {
                        detail: "The program running in it ends.".to_string(),
                        yes: "Close".to_string(),
                    },
                    Act::ClosePane,
                ),
                (Act::ClosePane, Some(_)) => {
                    let bid = focus_bid.ok_or("no pane to close")?;
                    self.call("block.close", json!({"session_id": win_sid, "block_id": bid}))?;
                }
                (Act::RenameTab, None) => self.ask(
                    "Name this tab",
                    AskKind::Text { value: String::new(), hint: "empty for the default".to_string() },
                    Act::RenameTab,
                ),
                (Act::RenameTab, Some(Answer::Text(label))) => {
                    self.call(
                        "session.set_window_label",
                        json!({"session_id": win_sid, "window_id": win_wid, "label": label}),
                    )?;
                    if let (View::Win(wv), false) = (&mut self.view, label.trim().is_empty()) {
                        let session = wv.name.split(" / ").next().unwrap_or("").to_string();
                        wv.name = format!("{session} / {}", label.trim());
                    }
                }
                (Act::RenameSession(sid, old), None) => self.ask(
                    "Name this session",
                    AskKind::Text { value: old.clone(), hint: "empty for the default".to_string() },
                    Act::RenameSession(sid, old),
                ),
                (Act::RenameSession(sid, _), Some(Answer::Text(label))) => {
                    self.call("session.set_label", json!({"session_id": sid, "label": label}))?;
                }
                (Act::EndSession(sid, label), None) => self.ask(
                    &format!("End session {label}?"),
                    AskKind::Confirm {
                        detail: "Every terminal in it closes, on every device.".to_string(),
                        yes: "End session".to_string(),
                    },
                    Act::EndSession(sid, label),
                ),
                (Act::EndSession(sid, _), Some(_)) => {
                    self.call("session.destroy", json!({"session_id": sid}))?;
                }
                (Act::Select, _) => {
                    if let View::Win(wv) = &mut self.view {
                        wv.select = true;
                    }
                    self.say("Drag over the text to copy it");
                }
                (Act::Paste, _) => {
                    if self.clip.is_empty() {
                        return Err("nothing copied yet".into());
                    }
                    let text = self.clip.replace("\r\n", "\r").replace('\n', "\r");
                    let bracketed = self
                        .focused_term()
                        .is_some_and(|(tv, _, _)| tv.term.mode().contains(TermMode::BRACKETED_PASTE));
                    let bytes = if bracketed {
                        format!("\x1b[200~{text}\x1b[201~").into_bytes()
                    } else {
                        text.into_bytes()
                    };
                    self.type_bytes(bytes);
                }
                (Act::Font(step), _) => {
                    let ix = (self.font_ix as i32 + step).clamp(0, FONTS.len() as i32 - 1) as usize;
                    if ix != self.font_ix {
                        self.font_ix = ix;
                        let _ = std::fs::write(format!("{CONFIG_DIR}/font"), FONTS[ix].0);
                        // Cell sizes changed: nothing drawn so far is right.
                        if let View::Win(wv) = &mut self.view {
                            for p in wv.panes.iter_mut() {
                                if let Body::Term(tv) = &mut p.body {
                                    if tv.owner {
                                        tv.term.resize(GridSize { cols: 1, lines: 1 });
                                    }
                                }
                            }
                        }
                        self.relayout();
                    }
                }
                (Act::Refresh, _) => {
                    if let View::Win(wv) = &mut self.view {
                        for p in wv.panes.iter_mut() {
                            if let Body::Term(tv) = &mut p.body {
                                let now = Instant::now();
                                tv.settle = Some((now, now));
                            }
                        }
                        wv.repaint = true;
                        self.ink.deghost_now = true;
                    }
                }
                (Act::Keyboard, _) => {
                    self.kbd = !self.kbd;
                    self.relayout();
                }
                (Act::WebGo, None) => {
                    // The field starts empty: rubbing out an address on the
                    // tablet is slower than writing a new one.
                    let url = self.focused_web().map(|(_, wb)| wb.url.clone()).unwrap_or_default();
                    self.ask(
                        &format!("Go to (now at {})", clip(&url, 50)),
                        AskKind::Text { value: String::new(), hint: "an address, or words to search for".to_string() },
                        Act::WebGo,
                    );
                }
                (Act::WebGo, Some(Answer::Text(url))) => {
                    let bid = focus_bid.ok_or("no pane")?;
                    self.web_act(&bid, json!({"web": "nav", "url": url}));
                }
                (Act::WebZoom(step), _) => {
                    let zoom = (self.web_zoom as i32 + step * 25).clamp(50, 300) as u32;
                    if zoom != self.web_zoom {
                        self.web_zoom = zoom;
                        let _ = std::fs::write(format!("{CONFIG_DIR}/webzoom"), zoom.to_string());
                        if let View::Win(wv) = &mut self.view {
                            for p in wv.panes.iter_mut() {
                                if let Body::Web(wb) = &mut p.body {
                                    wb.next_shot = Instant::now();
                                }
                            }
                        }
                        self.say(&format!("Page text at {zoom}%"));
                    }
                }
                (Act::WebNav(op), _) => {
                    let bid = focus_bid.ok_or("no pane")?;
                    self.web_act(&bid, json!({"web": op}));
                }
                (Act::Browser(tab), None) => self.ask(
                    if tab { "Open a browser tab at" } else { "Open a browser pane at" },
                    AskKind::Text { value: String::new(), hint: "an address, or words to search for".to_string() },
                    Act::Browser(tab),
                ),
                (Act::Browser(true), Some(Answer::Text(url))) => {
                    let anchor = focus_bid.ok_or("no pane to start from")?;
                    self.ask_helper(
                        json!({"web": "open", "session": win_sid, "anchor": anchor, "url": url, "tab": true}),
                        Want::OpenTab(win_sid.clone()),
                    )?;
                    self.say("Opening the browser tab...");
                }
                (Act::Browser(false), Some(Answer::Text(url))) => {
                    let anchor = focus_bid.ok_or("no pane to put it beside")?;
                    self.ask_helper(
                        json!({"web": "open", "session": win_sid, "anchor": anchor, "url": url, "split": "right"}),
                        Want::Open,
                    )?;
                    if let View::Win(wv) = &mut self.view {
                        wv.zoom = false;
                    }
                }
                (Act::Reply(call_id), Some(answer)) => {
                    let result = match answer {
                        Answer::Text(text) => json!({"text": text}),
                        Answer::Ids(ids) => json!({"ids": ids}),
                        Answer::Yes => json!({"cancelled": true}),
                    };
                    self.send("client.reply", json!({"call_id": call_id, "result": result}));
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(e) = result {
            if self.ctl.is_some() {
                self.say(&format!("That didn't work: {e}"));
            }
        }
    }

    /// An action on the server asks the person something.
    fn prompt(&mut self, p: &Value) {
        let call_id = p["call_id"].as_str().unwrap_or("").to_string();
        let q = &p["prompt"];
        let title = match p["action"].as_str() {
            Some(a) if !a.is_empty() => format!("{} ({a})", q["title"].as_str().unwrap_or("Question")),
            _ => q["title"].as_str().unwrap_or("Question").to_string(),
        };
        let kind = match q["kind"].as_str() {
            Some("input") => AskKind::Text {
                value: q["value"].as_str().unwrap_or("").to_string(),
                hint: q["placeholder"].as_str().unwrap_or("").to_string(),
            },
            Some("pick") => AskKind::Pick {
                items: q["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|i| PickItem {
                        id: i["id"].as_str().unwrap_or("").to_string(),
                        text: match i["subtitle"].as_str() {
                            Some(s) if !s.is_empty() => format!("{}  -  {s}", i["title"].as_str().unwrap_or("")),
                            _ => i["title"].as_str().unwrap_or("").to_string(),
                        },
                        on: i["selected"].as_bool().unwrap_or(false),
                    })
                    .collect(),
                multiple: q["multiple"].as_bool().unwrap_or(false),
            },
            _ => {
                return self.send(
                    "client.reply",
                    json!({"call_id": call_id,
                           "error": {"code": "not_supported", "message": "rexpaper can't show this prompt"}}),
                );
            }
        };
        // A prompt already open is dismissed, not answered.
        self.close_overlay();
        self.ask(&title, kind, Act::Reply(call_id));
    }

    // ------------------------------------------------------ input

    fn touch(&mut self, ev: qtfb::InputEvent) {
        if ev.input_type == qtfb::INPUT_VKB_PRESS {
            if !self.kbd {
                // AppLoad's keyboard is up, and it covers the bottom rows.
                self.kbd = true;
                self.relayout();
                if self.overlay.is_some() {
                    self.overlay_drawn = false;
                }
            }
            // AppLoad sends its Ctrl and Alt keys as presses of their own,
            // not as a modifier on the key that follows.
            match ev.x & !(keys::MOD_SHIFT | keys::MOD_CTRL | keys::MOD_ALT) {
                0x0100_0021 => {
                    self.ctrl = !self.ctrl;
                    self.touch_chrome();
                }
                0x0100_0023 => self.alt = !self.alt,
                0 | 0x0100_0020 | 0x0100_0022 | 0x0100_0024 => {} // Shift, Meta, Caps Lock
                _ => match keys::decode_vkb(ev.x) {
                    Some((action, ctrl, alt)) => self.key(action, ctrl, alt),
                    None => eprintln!("rexpaper: unmapped virtual key {:#x}", ev.x),
                },
            }
            return;
        }
        let from_pen = matches!(ev.input_type, qtfb::INPUT_PEN_PRESS | qtfb::INPUT_PEN_RELEASE | qtfb::INPUT_PEN_UPDATE);
        if from_pen && self.raw_pen && ev.dev_id != PEN_TAP {
            return;
        }
        let g = self.geo();
        let (x, y) = self.rot.to_logical(ev.x, ev.y, SCREEN_W, SCREEN_H);
        // A touch at the very top is a swipe for AppLoad's title bar.
        let press = matches!(ev.input_type, qtfb::INPUT_TOUCH_PRESS | qtfb::INPUT_PEN_PRESS) && ev.y >= 30;
        let release = matches!(ev.input_type, qtfb::INPUT_TOUCH_RELEASE | qtfb::INPUT_PEN_RELEASE);
        if self.overlay.is_some() {
            if press {
                self.overlay_touch(x, y);
            }
            return;
        }
        match &mut self.view {
            View::Notice { .. } if press => {
                if self.ctl.is_some() {
                    self.show_list();
                } else {
                    self.connect();
                }
            }
            View::List(lv) => {
                if press {
                    lv.press = Some((ev.dev_id, x, y));
                } else if let (true, Some((id, x0, y0))) = (release, lv.press) {
                    if id != ev.dev_id {
                        return;
                    }
                    lv.press = None;
                    let dy = y - y0;
                    if dy.abs() > 80 {
                        // A swipe scrolls by the rows it covered.
                        let step = (dy.unsigned_abs() as usize / ITEM_H).max(1);
                        lv.top = if dy < 0 {
                            (lv.top + step).min(lv.rows.len().saturating_sub(1))
                        } else {
                            lv.top.saturating_sub(step)
                        };
                        lv.drawn = false;
                    } else {
                        self.list_tap(x0, y0);
                    }
                }
            }
            View::Win(wv) => {
                if press && y as usize >= g.bar_y {
                    if (x as usize) < 330 {
                        self.close_window();
                    } else if self.banner.is_some() {
                        self.banner = None;
                        self.touch_chrome();
                    } else {
                        self.take_over();
                    }
                } else if press && y as usize >= g.keys_y {
                    let i = (x.max(0) as usize * KEY_ROW.len() / g.lw).min(KEY_ROW.len() - 1);
                    match KEY_ROW[i].1 {
                        KeyBtn::Key(k) => self.key(k, false, false),
                        KeyBtn::Ctrl => {
                            self.ctrl = !self.ctrl;
                            self.touch_chrome();
                        }
                        KeyBtn::CtrlC => self.key(KeyAction::Char('c'), true, false),
                        KeyBtn::Menu => self.window_menu(),
                    }
                } else if press {
                    if let Some(i) = wv.panes.iter().position(|p| p.rect.is_some_and(|r| r.has(x, y))) {
                        if i != wv.focus {
                            wv.focus = i;
                            for p in wv.panes.iter_mut() {
                                p.title_drawn.clear();
                            }
                        }
                    }
                    wv.drag = Some((ev.dev_id, y));
                    wv.press = Some((x, y));
                    let select = wv.select;
                    if let Some(Pane { body: Body::Term(tv), .. }) = wv.panes.get_mut(wv.focus) {
                        tv.term.selection = select
                            .then(|| Selection::new(SelectionType::Simple, tv.point_at(x, y), Side::Left));
                    }
                } else if wv.drag.is_some_and(|d| d.0 == ev.dev_id) {
                    let (id, last_y) = wv.drag.unwrap();
                    let multi = wv.panes.iter().filter(|p| p.rect.is_some()).count() > 1;
                    if let Some(Pane { body: Body::Web(wb), rect: Some(rect), block_id, .. }) = wv.panes.get_mut(wv.focus) {
                        // On a page a tap clicks, and a drag scrolls when the
                        // finger lifts: e-ink can't follow a moving page.
                        if release {
                            let (x0, y0) = wv.press.unwrap_or((x, y));
                            let bid = block_id.clone();
                            let area = inner(*rect, multi);
                            let tap = (x - x0).abs() + (y - y0).abs() < 30;
                            let bar = if tap { web::bar_hit(x0, y0, area) } else { None };
                            let act = match bar {
                                Some(web::BarHit::Back) => Some(json!({"web": "back"})),
                                Some(web::BarHit::Forward) => Some(json!({"web": "forward"})),
                                Some(web::BarHit::Reload) => Some(json!({"web": "reload"})),
                                Some(web::BarHit::Address | web::BarHit::Smaller | web::BarHit::Bigger) => None,
                                None if tap => wb
                                    .page_point(x0, y0, web::page_rect(area))
                                    .map(|(px, py)| json!({"web": "tap", "x": px, "y": py})),
                                None => Some(json!({"web": "scroll", "dy": (y0 - y) as f64 * wb.scale()})),
                            };
                            wv.drag = None;
                            // Show at once that the touch was taken: the
                            // real picture is a trip to the Mac away.
                            match bar {
                                Some(hit) if act.is_some() => {
                                    wb.held = Some(hit);
                                    wb.drawn = false;
                                }
                                None if !tap => wb.shift(y0 - y),
                                _ => {}
                            }
                            if let Some(act) = act {
                                self.web_act(&bid, act);
                            } else {
                                match bar {
                                    Some(web::BarHit::Address) => self.perform(Act::WebGo, None),
                                    Some(web::BarHit::Smaller) => self.perform(Act::WebZoom(-1), None),
                                    Some(web::BarHit::Bigger) => self.perform(Act::WebZoom(1), None),
                                    _ => {}
                                }
                            }
                        }
                        return;
                    }
                    let select = wv.select;
                    let Some(Pane { body: Body::Term(tv), .. }) = wv.panes.get_mut(wv.focus) else {
                        return;
                    };
                    if select {
                        let at = tv.point_at(x, y);
                        if let Some(sel) = &mut tv.term.selection {
                            sel.update(at, Side::Right);
                        }
                        if release {
                            wv.drag = None;
                            wv.select = false;
                            let text = tv.term.selection_to_string().unwrap_or_default();
                            let n = text.chars().count();
                            if n > 0 {
                                self.clip = text;
                                self.say(&format!("Copied {n} characters. Menu > Paste puts them in a terminal."));
                            } else {
                                self.banner = None;
                                self.touch_chrome();
                            }
                        }
                    } else if release {
                        wv.drag = None;
                    } else {
                        let lines = (y - last_y) / tv.area.ch as i32;
                        if lines != 0 {
                            // Drag down shows earlier history.
                            tv.term.scroll_display(Scroll::Delta(lines));
                            wv.drag = Some((id, last_y + lines * tv.area.ch as i32));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn list_tap(&mut self, x: i32, y: i32) {
        let View::List(lv) = &self.view else {
            return;
        };
        let Some(row) = lv.rows.iter().find(|r| r.span.is_some_and(|s| s.has(x, y))) else {
            return;
        };
        match &row.kind {
            RowKind::Window { sid, wid, name, .. } => {
                let (sid, wid, name) = (sid.clone(), wid.clone(), name.clone());
                self.open(&sid, &wid, Some(&name), None);
            }
            RowKind::Session { sid, label } => {
                let (sid, label) = (sid.clone(), label.clone());
                self.menu(vec![
                    ("New tab".to_string(), Act::NewTab(sid.clone())),
                    ("Rename session".to_string(), Act::RenameSession(sid.clone(), label.clone())),
                    ("End session".to_string(), Act::EndSession(sid, label)),
                ]);
            }
            RowKind::NewSession => self.perform(Act::NewSession, None),
            RowKind::Note => {}
        }
    }

    fn overlay_touch(&mut self, x: i32, y: i32) {
        match &mut self.overlay {
            Some(Overlay::Menu { items, spans, .. }) => {
                let hit = spans.iter().position(|s| s.has(x, y)).map(|i| items[i].1.clone());
                self.close_overlay();
                if let Some(act) = hit {
                    self.perform(act, None);
                }
            }
            Some(Overlay::Ask(ask)) => {
                if ask.ok.has(x, y) {
                    self.answer();
                } else if ask.cancel.has(x, y) {
                    self.close_overlay();
                } else if let Some(i) = ask.items.iter().position(|s| s.has(x, y)) {
                    if let AskKind::Pick { items, multiple } = &mut ask.kind {
                        if *multiple {
                            items[i].on = !items[i].on;
                            self.overlay_drawn = false;
                        } else {
                            for (j, it) in items.iter_mut().enumerate() {
                                it.on = j == i;
                            }
                            self.answer();
                        }
                    }
                }
            }
            None => {}
        }
    }

    // ------------------------------------------------------ pen

    /// Draw one piece of a pen stroke on the canvas.
    fn ink_line(&mut self, a: (i32, i32), b: (i32, i32)) {
        let (lw, lh) = (render::canvas_w() as i32, render::canvas_h() as i32);
        let n = (b.0 - a.0).abs().max((b.1 - a.1).abs()).max(1);
        for k in 0..=n {
            let (x, y) = (a.0 + (b.0 - a.0) * k / n, a.1 + (b.1 - a.1) * k / n);
            if x >= 2 && y >= 2 && x < lw - 2 && y < lh - 2 {
                render::fill_rect(&mut self.canvas, x as usize - 1, y as usize - 1, 4, 4, BLACK);
            }
        }
        let (x0, y0) = ((a.0.min(b.0) - 2).max(0), (a.1.min(b.1) - 2).max(0));
        let (x1, y1) = ((a.0.max(b.0) + 3).min(lw), (a.1.max(b.1) + 3).min(lh));
        if x1 > x0 && y1 > y0 {
            self.dirty.rects.push((x0 as usize, y0 as usize, (x1 - x0) as usize, (y1 - y0) as usize));
        }
    }

    /// Put the ink back after the screen under it was drawn again.
    fn redraw_ink(&mut self) {
        let strokes: Vec<Vec<(i32, i32)>> = self.ink.sent.iter().chain(&self.ink.strokes).cloned().collect();
        for s in strokes {
            for w in s.windows(2) {
                self.ink_line(w[0], w[1]);
            }
            if s.len() == 1 {
                self.ink_line(s[0], s[0]);
            }
        }
    }

    /// The pen writes on a tab's terminals and taps everywhere else.
    fn pen(&mut self, ev: PenEv) {
        let g = self.geo();
        let at = |x: i32, y: i32| self.rot.to_logical(x, y, SCREEN_W, SCREEN_H);
        let tap = |input_type: i32, (x, y): (i32, i32)| {
            let (px, py) = self.rot.to_phys(x.max(0) as usize, y.max(0) as usize, SCREEN_W, SCREEN_H);
            qtfb::InputEvent { input_type, dev_id: PEN_TAP, x: px as i32, y: py as i32, d: 0 }
        };
        match ev {
            PenEv::Down(px, py, eraser) => {
                let p = at(px, py);
                let writes = self.overlay.is_none()
                    && matches!(&self.view, View::Win(wv) if !wv.select)
                    && (p.1 as usize) < g.keys_y;
                if writes && eraser {
                    // What the eraser rubs out is drawn away in the clean
                    // mode, so it leaves no ghost. Speed doesn't matter here.
                    self.clean_until = None;
                    self.set_mode(MODE_CLEAN);
                    self.ink.erasing = Some((p, p.0, p.0, 0, false));
                    self.erase(p);
                } else if writes {
                    self.clean_until = None;
                    if self.trace {
                        eprintln!("rexpaper: trace pen down, mode was {}, near {}", self.mode, self.ink.near);
                        self.trace_down = Some(Instant::now());
                    }
                    self.set_mode(MODE_INK);
                    self.ink.down = true;
                    self.ink.strokes.push(vec![p]);
                    self.ink_line(p, p);
                } else {
                    self.ink.tapping = Some(p);
                    let e = tap(qtfb::INPUT_PEN_PRESS, p);
                    self.touch(e);
                }
            }
            PenEv::Move(px, py) => {
                let p = at(px, py);
                if self.ink.tapping.is_some() {
                    self.ink.tapping = Some(p);
                    let e = tap(qtfb::INPUT_PEN_UPDATE, p);
                    self.touch(e);
                } else if self.ink.erasing.is_some() {
                    self.erase(p);
                } else if self.ink.down {
                    let Some(last) = self.ink.strokes.last().and_then(|s| s.last().copied()) else {
                        return;
                    };
                    if (p.0 - last.0).pow(2) + (p.1 - last.1).pow(2) >= 4 {
                        self.ink.strokes.last_mut().unwrap().push(p);
                        self.ink_line(last, p);
                    }
                }
            }
            PenEv::Near(near) => {
                // Get ready for ink as the pen approaches, not when it lands.
                self.ink.near = near;
                self.ink.away = Some(Instant::now());
                if self.trace {
                    eprintln!("rexpaper: trace pen near {near}");
                }
                if near && matches!(self.view, View::Win(_)) && self.clean_until.is_none() {
                    self.set_mode(MODE_INK);
                }
            }
            PenEv::Up => {
                if let Some(p) = self.ink.tapping.take() {
                    let e = tap(qtfb::INPUT_PEN_RELEASE, p);
                    self.touch(e);
                } else if let Some((_, _, _, deleted, inked)) = self.ink.erasing.take() {
                    self.clean_until = Some(Instant::now() + CLEAN_FOR);
                    // A dab with the eraser deletes one character.
                    if deleted == 0 && !inked && self.ink.strokes.is_empty() && self.ink.sent.is_empty() {
                        self.type_bytes(vec![0x7f]);
                    }
                    self.ink.last_up = Some(Instant::now());
                } else if self.ink.down {
                    self.ink.down = false;
                    self.ink.last_up = Some(Instant::now());
                }
            }
        }
    }

    /// The eraser moved to `p`. While there are scribbles it rubs them
    /// out, a stroke at a time. With none, it deletes typed text from the
    /// cursor backwards: one character for each cell's width it rubs over.
    fn erase(&mut self, p: (i32, i32)) {
        let Some((last, lo, hi, deleted, inked)) = self.ink.erasing else {
            return;
        };
        let (lo, hi) = (lo.min(p.0), hi.max(p.0));
        self.ink.erasing = Some((p, lo, hi, deleted, inked));
        if self.ink.strokes.is_empty() && self.ink.sent.is_empty() {
            if inked {
                return; // this rub was for ink, and has just finished it off
            }
            let cw = self.fonts[self.font_ix].width as i32;
            let want = ((hi - lo) / cw) as usize;
            if want > deleted {
                self.type_bytes(vec![0x7f; want - deleted]);
                self.ink.erasing = Some((p, lo, hi, want, false));
            }
            return;
        }
        // A stroke goes when the eraser's path comes near any of its points.
        const REACH: i32 = 30;
        let n = ((p.0 - last.0).abs().max((p.1 - last.1).abs()) / 10).max(1);
        let path: Vec<(i32, i32)> =
            (0..=n).map(|k| (last.0 + (p.0 - last.0) * k / n, last.1 + (p.1 - last.1) * k / n)).collect();
        let hit = |s: &Vec<(i32, i32)>| {
            s.iter().any(|q| path.iter().any(|e| (q.0 - e.0).pow(2) + (q.1 - e.1).pow(2) <= REACH * REACH))
        };
        let (mut gone, mut kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.ink.strokes).into_iter().partition(&hit);
        self.ink.strokes = std::mem::take(&mut kept);
        let (gone_sent, kept_sent): (Vec<_>, Vec<_>) = std::mem::take(&mut self.ink.sent).into_iter().partition(&hit);
        self.ink.sent = kept_sent;
        gone.extend(gone_sent);
        if gone.is_empty() {
            return;
        }
        self.ink.erasing = Some((p, lo, hi, deleted, true));
        self.unink(&gone);
        if self.ink.sent.is_empty() && self.ink.waiting.take().is_some() {
            // Everything sent off was rubbed out: its text is not wanted.
            self.banner = None;
        }
    }

    fn set_mode(&mut self, mode: i32) {
        if mode != self.mode {
            self.mode = mode;
            self.screen.set_mode(mode);
        }
    }

    /// Mark where these strokes were, to draw that part of the screen again
    /// without them. No full refresh: the pen may already be writing more.
    fn unink(&mut self, gone: &[Vec<(i32, i32)>]) {
        let pts = || gone.iter().flatten();
        let (Some(x0), Some(y0)) = (pts().map(|p| p.0).min(), pts().map(|p| p.1).min()) else {
            return;
        };
        let (x1, y1) = (pts().map(|p| p.0).max().unwrap(), pts().map(|p| p.1).max().unwrap());
        let (lw, lh) = (render::canvas_w() as i32, render::canvas_h() as i32);
        let (x0, y0) = ((x0 - 6).max(0), (y0 - 6).max(0));
        let (x1, y1) = ((x1 + 7).min(lw), (y1 + 7).min(lh));
        let r = Rect::new(x0 as usize, y0 as usize, (x1 - x0).max(1) as usize, (y1 - y0).max(1) as usize);
        if self.ink.erasing.is_some() {
            // Under the eraser the ink goes at once, so the rub shows.
            self.repair = Some(self.repair.map_or(r, |o| o.union(r)));
        } else {
            self.flash = Some(self.flash.map_or(r, |o| o.union(r)));
        }
    }

    /// Throw the ink away and show the screen without it.
    fn clear_ink(&mut self, all: bool) {
        let mut gone = std::mem::take(&mut self.ink.sent);
        self.ink.waiting = None;
        if all {
            gone.append(&mut self.ink.strokes);
        }
        self.unink(&gone);
    }

    /// Start the handwriting reader in a terminal block of its own, in the
    /// open tab's session. No window shows it.
    fn start_scribe(&mut self) -> Result<(), String> {
        let path = setting("helper").ok_or("no helper is set up: run rexpaper/install.sh again")?;
        let View::Win(wv) = &self.view else {
            return Err("open a tab first".into());
        };
        let sid = wv.session_id.clone();
        let ctl = self.ctl.as_mut().ok_or("not connected")?;
        let id = ctl.send(
            "session.new_block",
            json!({"session_id": sid, "flavor": SHELL, "label": SCRIBE,
                   "options": {"command": ["python3", path], "size": {"columns": 500, "rows": 60}}}),
        );
        self.scribe = Some(Scribe {
            sid,
            bid: String::new(),
            creating: Some(id),
            polling: None,
            ready: false,
            queued: Vec::new(), writing: None,
            polled: Instant::now(),
            stale: false,
        });
        Ok(())
    }

    /// Send a request to the helper on the Mac, starting it if need be.
    /// Its answer comes to `helper_reply`.
    fn ask_helper(&mut self, mut body: Value, want: Want) -> Result<u64, String> {
        let here = match &self.view {
            View::Win(wv) => wv.session_id.clone(),
            _ => return Err("open a tab first".into()),
        };
        // The helper has to live in the open tab's session: only there can
        // this client download the files it makes.
        if let Some(sc) = self.scribe.take_if(|sc| sc.sid != here) {
            if !sc.bid.is_empty() {
                self.send("block.close", json!({"session_id": sc.sid, "block_id": sc.bid}));
            }
            self.wants.clear();
        }
        if self.scribe.is_none() {
            self.start_scribe()?;
        }
        self.next_req += 1;
        let id = self.next_req;
        body["id"] = json!(id);
        body["acks"] = json!(std::mem::take(&mut self.acks));
        let line = format!("{body}\n");
        self.wants.insert(id, want);
        let Some(sc) = self.scribe.as_mut() else {
            return Err("no helper".into());
        };
        sc.queued.push(line);
        self.pump_helper();
        Ok(id)
    }

    /// Write the waiting requests to the helper, if it is free to take them.
    fn pump_helper(&mut self) {
        let (Some(sc), Some(ctl)) = (self.scribe.as_mut(), self.ctl.as_mut()) else {
            return;
        };
        if !sc.ready || sc.writing.is_some() || sc.queued.is_empty() {
            return;
        }
        let data: String = std::mem::take(&mut sc.queued).concat();
        trace(&format!("write {}", data.chars().take(60).collect::<String>().trim()));
        sc.writing = Some(ctl.send(
            &format!("{TERMINAL}.write"),
            json!({"session_id": sc.sid, "block_id": sc.bid, "args": {"data": data}}),
        ));
    }

    /// The pen has rested: send what it wrote to be read.
    fn send_ink(&mut self) {
        let strokes: Vec<Vec<i32>> = self
            .ink
            .strokes
            .iter()
            .map(|s| s.iter().flat_map(|p| [p.0, p.1]).collect())
            .collect();
        // What is on the line already, so the reader can tell whether the
        // writing starts a new word and needs a space before it.
        let before = match self.focused_term() {
            Some((tv, _, _)) => {
                let at = tv.term.grid().cursor.point;
                let text: String = (0..at.column.0).map(|c| tv.term.grid()[at.line][Column(c)].c).collect();
                let n = text.chars().count();
                text.chars().skip(n.saturating_sub(160)).collect()
            }
            None => String::new(),
        };
        match self.ask_helper(json!({"strokes": strokes, "before": before}), Want::Ink) {
            Ok(id) => {
                self.ink.sent = std::mem::take(&mut self.ink.strokes);
                self.ink.waiting = Some((id, Instant::now()));
                self.banner = Some(("Reading your handwriting...".to_string(), Instant::now()));
                self.touch_chrome();
            }
            Err(e) => {
                self.clear_ink(true);
                self.say(&format!("Can't read handwriting: {e}"));
            }
        }
    }

    /// Ask for the reader's screen. The answer comes to `scribe_screen`.
    fn poll_scribe(&mut self) {
        let (Some(sc), Some(ctl)) = (self.scribe.as_mut(), self.ctl.as_mut()) else {
            return;
        };
        let stuck = sc.polled.elapsed() >= Duration::from_secs(8);
        if sc.bid.is_empty() || (sc.polling.is_some() && !stuck) {
            sc.stale = sc.polling.is_some();
            return;
        }
        sc.stale = false;
        sc.polled = Instant::now();
        trace("poll");
        sc.polling = Some(ctl.send(
            &format!("{TERMINAL}.format"),
            json!({"session_id": sc.sid, "block_id": sc.bid,
                   "args": {"format": "text", "unwrap": true, "trim": true}}),
        ));
    }

    /// The helper could not be started or reached.
    fn scribe_lost(&mut self, why: &str) {
        self.scribe = None;
        self.wants.clear();
        self.fetches.clear();
        if let View::Win(wv) = &mut self.view {
            for p in wv.panes.iter_mut() {
                if let Body::Web(wb) = &mut p.body {
                    wb.busy = false;
                    wb.error = Some(format!("The helper on the Mac stopped ({why})."));
                    wb.drawn = false;
                }
            }
        }
        if self.ink.waiting.is_some() {
            self.banner = None;
            self.clear_ink(false);
            self.say(&format!("The handwriting reader stopped ({why}). Write again to restart it."));
        }
    }

    /// What the helper has printed: its answers to requests.
    fn scribe_screen(&mut self, screen: &str) {
        let Some(sc) = self.scribe.as_mut() else {
            return;
        };
        let mut replies = Vec::new();
        for part in screen.split("@@scribe ").skip(1) {
            if let Some(v) = part.split("@@").next().and_then(|j| serde_json::from_str::<Value>(j).ok()) {
                replies.push(v);
            }
        }
        if !sc.ready && replies.iter().any(|r| r["ready"] == 1) {
            sc.ready = true;
            self.acks.push(0);
            self.pump_helper();
        }
        for reply in replies {
            let Some(id) = reply["id"].as_u64() else {
                continue;
            };
            if let Some(want) = self.wants.remove(&id) {
                trace(&format!("reply {id}"));
                self.acks.push(id);
                self.helper_reply(id, want, &reply);
            }
        }
    }

    fn helper_reply(&mut self, id: u64, want: Want, reply: &Value) {
        let error = reply["error"].as_str().map(str::to_string);
        match want {
            Want::Ink => {
                if self.ink.waiting.map(|w| w.0) != Some(id) {
                    return; // rubbed out while it was being read
                }
                self.banner = None;
                self.clear_ink(false);
                match (reply["text"].as_str(), error) {
                    (Some(text), _) if !text.is_empty() => self.type_bytes(text.as_bytes().to_vec()),
                    (_, Some(e)) => self.say(&format!("Couldn't read the handwriting: {e}")),
                    _ => self.say("Couldn't read that. Try writing it again."),
                }
                self.touch_chrome();
            }
            Want::Shot(bid) => {
                let View::Win(wv) = &mut self.view else {
                    return;
                };
                let Some(pane) = wv.panes.iter_mut().find(|p| p.block_id == bid) else {
                    return;
                };
                let Body::Web(wb) = &mut pane.body else {
                    return;
                };
                wb.busy = false;
                if let Some(e) = error {
                    if wb.error.as_deref() != Some(e.as_str()) {
                        wb.error = Some(e);
                        wb.drawn = false;
                    }
                    wb.next_shot = Instant::now() + Duration::from_secs(5);
                    return;
                }
                wb.error = None;
                wb.vw = reply["vw"].as_f64().unwrap_or(0.0);
                wb.vh = reply["vh"].as_f64().unwrap_or(0.0);
                wb.y = reply["y"].as_f64().unwrap_or(0.0);
                wb.full = reply["full"].as_f64().unwrap_or(0.0);
                if wb.held.take().is_some() {
                    wb.drawn = false;
                }
                wb.edit = reply["edit"].as_bool().unwrap_or(false);
                let url = reply["url"].as_str().unwrap_or("").to_string();
                if url != wb.url {
                    wb.url = url;
                    wb.drawn = false;
                }
                let title = reply["title"].as_str().filter(|t| !t.trim().is_empty()).unwrap_or(&wb.url).to_string();
                if title != pane.title {
                    wb.title = title.clone();
                    pane.title = title;
                    wv.chrome.clear();
                }
                // Look again soon while the page is still loading, and now
                // and then otherwise: pages change by themselves.
                let loading = reply["loading"].as_bool().unwrap_or(false);
                wb.next_shot = Instant::now() + Duration::from_secs(if loading { 1 } else { 6 });
                let (Some(path), Some(hash)) = (reply["path"].as_str(), reply["hash"].as_str()) else {
                    return; // the same picture as before
                };
                wb.busy = true;
                trace("fetch");
                let (path, hash) = (path.to_string(), hash.to_string());
                self.fetch(bid, path, hash);
            }
            Want::Act(bid) => {
                // The answer to an act is the picture of what it did.
                if error.is_none() && reply["hash"].is_string() {
                    return self.helper_reply(id, Want::Shot(bid), reply);
                }
                if let Some(e) = error {
                    self.say(&format!("The page didn't take that: {e}"));
                }
                if let View::Win(wv) = &mut self.view {
                    if let Some(Pane { body: Body::Web(wb), .. }) = wv.panes.iter_mut().find(|p| p.block_id == bid) {
                        wb.busy = false;
                        wb.held = None;
                        wb.drawn = false;
                        wb.next_shot = Instant::now();
                    }
                }
            }
            Want::Open => {
                if let Some(e) = error {
                    self.say(&format!("Couldn't open a browser pane: {e}"));
                }
            }
            Want::OpenTab(sid) => {
                if let Some(e) = error {
                    return self.say(&format!("Couldn't open a browser tab: {e}"));
                }
                let block = reply["block"].as_str().unwrap_or("").to_string();
                let found = self.call("session.view", json!({"session_id": sid})).ok().and_then(|view| {
                    let wids: Vec<String> = view["windows"]
                        .as_array()?
                        .iter()
                        .filter_map(|w| w["window_id"].as_str().map(str::to_string))
                        .collect();
                    wids.into_iter().find_map(|wid| {
                        let info = window_info(&view, &wid)?;
                        let pane = info.panes.iter().find(|p| !block.is_empty() && p.0.ends_with(&block))?;
                        Some((wid, pane.0.clone()))
                    })
                });
                match found {
                    Some((wid, bid)) => {
                        self.banner = None;
                        self.open(&sid, &wid, None, Some(&bid));
                    }
                    None => self.say("The browser tab opened, but I can't find it. Look in Sessions."),
                }
            }
        }
    }

    // ------------------------------------------------------ browser panes

    /// Start downloading a picture of a page from the helper's machine.
    fn fetch(&mut self, bid: String, path: String, hash: String) {
        let (Some(sc), Some(ctl)) = (self.scribe.as_ref(), self.ctl.as_mut()) else {
            return;
        };
        let mut r = [0u8; 16];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = std::io::Read::read_exact(&mut f, &mut r);
        }
        r[6] = (r[6] & 0x0f) | 0x40;
        r[8] = (r[8] & 0x3f) | 0x80;
        let hex: String = r.iter().map(|b| format!("{b:02x}")).collect();
        let uuid = format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]);
        let req = ctl.send(
            &format!("{TERMINAL}.download_begin"),
            json!({"session_id": sc.sid, "block_id": sc.bid,
                   "args": {"ref": format!("download:{uuid}"), "path": path}}),
        );
        self.fetches.retain(|f| f.bid != bid);
        self.fetches.push(Fetch { bid, hash, req: Some(req), wire: None, welcomed: false, buf: Vec::new() });
    }

    /// The download finished, or failed: show the picture.
    fn fetched(&mut self, f: Fetch, failed: Option<String>) {
        let View::Win(wv) = &mut self.view else {
            return;
        };
        let Some(Pane { body: Body::Web(wb), .. }) = wv.panes.iter_mut().find(|p| p.block_id == f.bid) else {
            return;
        };
        wb.busy = false;
        trace(&format!("got {} bytes", f.buf.len()));
        let done = match failed {
            Some(e) => Err(e),
            None => wb.set_jpeg(&f.buf),
        };
        trace("decoded");
        match done {
            Ok(()) => wb.hash = f.hash,
            Err(e) => {
                eprintln!("rexpaper: picture of {}: {e}", f.bid);
                wb.next_shot = Instant::now() + Duration::from_secs(3);
            }
        }
    }

    fn web_pane(&mut self, bid: &str) -> Option<(usize, &mut web::WebView)> {
        let View::Win(wv) = &mut self.view else {
            return None;
        };
        let i = wv.panes.iter().position(|p| p.block_id == bid)?;
        match &mut wv.panes[i].body {
            Body::Web(wb) => Some((i, wb)),
            _ => None,
        }
    }

    /// The focused pane, if it is a browser pane: its block and view.
    fn focused_web(&mut self) -> Option<(String, &mut web::WebView)> {
        let View::Win(wv) = &mut self.view else {
            return None;
        };
        let pane = wv.panes.get_mut(wv.focus)?;
        match &mut pane.body {
            Body::Web(wb) => Some((pane.block_id.clone(), wb)),
            _ => None,
        }
    }

    fn flush_web_keys(&mut self) {
        let (bid, bytes) = std::mem::take(&mut self.web_keys);
        if !bytes.is_empty() {
            self.web_type(&bid, &bytes);
        }
    }

    /// Do something to a browser pane's page.
    fn web_act(&mut self, bid: &str, mut body: Value) {
        trace(&format!("act {}", body["web"]));
        body["block"] = json!(bid);
        // Ask for the picture of the result in the same breath: one trip to
        // the Mac and back instead of two.
        let zoom = self.web_zoom;
        if let View::Win(wv) = &mut self.view {
            let multi = wv.panes.iter().filter(|p| p.rect.is_some()).count() > 1;
            if let Some(Pane { rect: Some(rect), body: Body::Web(wb), .. }) = wv.panes.iter_mut().find(|p| p.block_id == bid) {
                let page = web::page_rect(inner(*rect, multi));
                (body["w"], body["h"], body["last"], body["zoom"]) = (json!(page.w), json!(page.h), json!(wb.hash), json!(zoom));
                wb.busy = true;
                wb.next_shot = Instant::now() + Duration::from_secs(20);
            }
        }
        if let Err(e) = self.ask_helper(body, Want::Act(bid.to_string())) {
            self.say(&format!("Can't reach the page: {e}"));
        }
    }

    /// Text and keys for a browser pane, as the bytes a terminal would get.
    fn web_type(&mut self, bid: &str, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).to_string();
        let mut run = String::new();
        let send = |app: &mut App, run: &mut String| {
            if !run.is_empty() {
                app.web_act(bid, json!({"web": "text", "text": std::mem::take(run)}));
            }
        };
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            let key = match c {
                '\r' | '\n' => "enter",
                '\x7f' | '\x08' => "backspace",
                '\t' => "tab",
                '\x1b' => {
                    // An arrow or paging key, as a terminal escape sequence.
                    let mut seq = String::new();
                    while let Some(&n) = chars.peek() {
                        seq.push(n);
                        chars.next();
                        if n.is_ascii_alphabetic() || n == '~' {
                            break;
                        }
                    }
                    match seq.as_str() {
                        "" => "escape",
                        "[A" | "OA" => "up",
                        "[B" | "OB" => "down",
                        "[C" | "OC" => "right",
                        "[D" | "OD" => "left",
                        "[5~" => "pageup",
                        "[6~" => "pagedown",
                        _ => continue,
                    }
                }
                c if c.is_control() => continue,
                c => {
                    run.push(c);
                    continue;
                }
            };
            send(self, &mut run);
            self.web_act(bid, json!({"web": "key", "key": key}));
        }
        send(self, &mut run);
    }

    /// The screen turned: lay everything out for the new shape.
    fn rotate(&mut self, rot: Rot) {
        self.rot = rot;
        let (lw, lh) = rot.logical_size(SCREEN_W, SCREEN_H);
        render::set_canvas_size(lw, lh);
        self.overlay_drawn = false;
        match &mut self.view {
            View::List(lv) => lv.drawn = false,
            View::Notice { drawn, .. } => *drawn = false,
            View::Win(_) => self.relayout(),
        }
    }

    // ------------------------------------------------------ messages

    /// One of Rex's messages from a terminal's stream.
    fn stream_msg(&mut self, pane: usize, msg: Value) {
        let font = self.fonts[self.font_ix].clone();
        let me = self.ctl.as_ref().map(|c| c.client_id.clone()).unwrap_or_default();
        let View::Win(wv) = &mut self.view else {
            return;
        };
        let multi = wv.panes.iter().filter(|p| p.rect.is_some()).count() > 1;
        let Some(Pane { body: Body::Term(tv), rect, .. }) = wv.panes.get_mut(pane) else {
            return;
        };
        match msg["type"].as_str() {
            Some("resize") => {
                let cols = msg["cols"].as_u64().unwrap_or(0) as usize;
                let rows = msg["rows"].as_u64().unwrap_or(0) as usize;
                tv.owner = msg["owner"].as_str() == Some(me.as_str());
                wv.chrome.clear();
                if cols > 0 && rows > 0 && (cols, rows) != tv.size() {
                    tv.term.resize(GridSize { cols, lines: rows });
                    if let Some(rect) = rect {
                        tv.area = make_area(&font, inner(*rect, multi), cols, rows);
                        wv.repaint = true;
                    }
                }
            }
            Some("child_exited") => {
                tv.exited = true;
                wv.chrome.clear();
            }
            _ => {}
        }
    }

    fn block_event(&mut self, bid: &str, name: &str, p: &Value) {
        if self.scribe.as_ref().is_some_and(|sc| sc.bid == bid && !bid.is_empty()) {
            // The reader sets its title when it has something to say.
            if name == "title_changed" {
                trace("title");
                self.poll_scribe();
            } else if name == "child_exited" {
                self.scribe = None;
            }
            return;
        }
        let who = self.names.get(bid).cloned().unwrap_or_else(|| "A terminal".to_string());
        match name {
            "program_status_changed" => {
                let r = &p["record"];
                let state = r["state"].as_str().unwrap_or("");
                if matches!(state, "blocked" | "done" | "error") {
                    let what = match (state, r["kind"].as_str()) {
                        ("blocked", Some(k)) => format!("needs you ({k})"),
                        ("blocked", None) => "needs you".to_string(),
                        (s, _) => s.to_string(),
                    };
                    let msg = r["msg"].as_str().or(r["title"].as_str()).unwrap_or("");
                    let sep = if msg.is_empty() { "" } else { " - " };
                    self.say(&format!("{who}: {what}{sep}{msg}"));
                }
                self.list_stale();
            }
            "program_status_removed" | "child_exited" => self.list_stale(),
            "desktop_notification" => {
                let (t, b) = (p["title"].as_str().unwrap_or(""), p["body"].as_str().unwrap_or(""));
                self.say(&format!("{who}: {t} {b}"));
            }
            "clipboard_written" | "selection_copied" => {
                if let Some(text) = p["text"].as_str().filter(|t| !t.is_empty()) {
                    self.clip = text.to_string();
                }
            }
            "title_changed" => {
                if let (View::Win(wv), Some(t)) = (&mut self.view, p["title"].as_str()) {
                    if let Some(pane) = wv.panes.iter_mut().find(|pn| pn.block_id == bid) {
                        if !t.trim().is_empty() {
                            pane.title = t.to_string();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn list_stale(&mut self) {
        if let View::List(lv) = &mut self.view {
            lv.stale.get_or_insert_with(Instant::now);
        }
    }

    fn control_msg(&mut self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if v["type"] == "response" {
            let id = v["id"].as_str();
            let failed = v.get("error").filter(|e| !e.is_null()).map(|e| e["message"].as_str().unwrap_or("failed").to_string());
            if let Some(i) = self.fetches.iter().position(|f| f.req.as_deref() == id && id.is_some()) {
                let ticket = v["payload"]["ticket"].as_str().and_then(wire::b64url_decode).filter(|t| t.len() == 16);
                match ticket {
                    Some(ticket) => {
                        let data = self.net.open("/api/data", "rex.data.bytes.v1");
                        let mut hello = vec![0x01];
                        hello.extend_from_slice(&ticket);
                        data.send(Out::Binary(hello));
                        trace("ticket");
                        self.fetches[i].req = None;
                        self.fetches[i].wire = Some(data);
                    }
                    None => {
                        let f = self.fetches.remove(i);
                        self.fetched(f, Some(failed.unwrap_or_else(|| "no download ticket".to_string())));
                    }
                }
                return;
            }
            if let Some(sc) = self.scribe.as_mut() {
                if sc.writing.as_deref() == id && id.is_some() {
                    sc.writing = None;
                    trace("written");
                    self.pump_helper();
                    return;
                }
                if sc.creating.as_deref() == id && id.is_some() {
                    sc.creating = None;
                    match (failed, v["payload"]["block_id"].as_str()) {
                        (None, Some(bid)) => sc.bid = bid.to_string(),
                        (why, _) => self.scribe_lost(&why.unwrap_or_else(|| "no reader came back".to_string())),
                    }
                    return;
                }
                if sc.polling.as_deref() == id && id.is_some() {
                    sc.polling = None;
                    let stale = sc.stale;
                    match failed {
                        None => {
                            let screen = v["payload"]["content"].as_str().unwrap_or("").to_string();
                            self.scribe_screen(&screen);
                            if stale {
                                self.poll_scribe();
                            }
                        }
                        Some(why) => self.scribe_lost(&why),
                    }
                    return;
                }
            }
            if let View::Win(wv) = &mut self.view {
                for p in wv.panes.iter_mut() {
                    if let Body::Term(tv) = &mut p.body {
                        if tv.inflight.as_deref() == v["id"].as_str() {
                            tv.inflight = None;
                        }
                    }
                }
                self.flush_input();
            }
            return;
        }
        if v["type"] != "event" {
            return;
        }
        let event = &v["payload"];
        let kind = event["type"].as_str().unwrap_or("");
        if std::env::var("REXPAPER_DEBUG").is_ok() {
            eprintln!("rexpaper: event {}", &text[..text.len().min(300)]);
        }
        let p = &event["payload"];
        match kind {
            "client_prompt_requested" => return self.prompt(p),
            "client_prompt_ended" => {
                let open = matches!(&self.overlay,
                    Some(Overlay::Ask(Ask { act: Act::Reply(id), .. })) if Some(id.as_str()) == p["call_id"].as_str());
                if open {
                    self.overlay = None;
                    self.close_overlay();
                }
                return;
            }
            "client_perform_requested" | "client_call_requested" => {
                return self.send(
                    "client.reply",
                    json!({"call_id": p["call_id"],
                           "error": {"code": "not_supported", "message": "rexpaper has no actions"}}),
                );
            }
            "session_created" | "session_destroyed" | "session_moved" => self.list_stale(),
            _ => {}
        }
        let sid = p["session_id"].as_str().unwrap_or("");
        let inner_ev = &p["event"];
        let name = inner_ev["type"].as_str().unwrap_or("");
        let ip = &inner_ev["payload"];
        if kind == "session_event" {
            match name {
                "block_event" => {
                    let bid = ip["block_id"].as_str().unwrap_or("").to_string();
                    let ev = ip["name"].as_str().unwrap_or("").to_string();
                    self.block_event(&bid, &ev, &ip["payload"]);
                }
                "client_connected" | "client_disconnected" | "client_connection_changed" => {}
                _ => self.list_stale(),
            }
        }
        let me = self.ctl.as_ref().map(|c| c.client_id.clone()).unwrap_or_default();
        let mut leave = false;
        if let View::Win(wv) = &mut self.view {
            if kind == "session_destroyed" && sid == wv.session_id {
                leave = true;
            } else if kind == "session_event" && sid == wv.session_id {
                let bid = ip["block_id"].as_str().unwrap_or("");
                match name {
                    "window_closed" if ip["window_id"].as_str() == Some(wv.window_id.as_str()) => leave = true,
                    "session_view_changed" => {
                        if ip["revision"].as_u64().unwrap_or(0) > wv.revision {
                            wv.stale = true;
                        }
                    }
                    // A block that joins a tab also changes the view, which is
                    // handled above. One that is in no tab, like the
                    // handwriting reader, needs nothing drawn again.
                    "block_closed" if wv.panes.iter().any(|pn| pn.block_id == bid) => wv.stale = true,
                    "block_owner_changed" => {
                        if let Some(Pane { body: Body::Term(tv), .. }) =
                            wv.panes.iter_mut().find(|pn| pn.block_id == bid)
                        {
                            tv.owner = ip["owner"].as_str() == Some(me.as_str());
                            wv.chrome.clear();
                        }
                    }
                    _ => {}
                }
            }
        }
        if leave {
            self.overlay = None;
            self.show_list();
        }
    }

    fn net_msg(&mut self, id: usize, msg: In) {
        if self.ctl.as_ref().map(Control::wire_id) == Some(id) {
            match msg {
                In::Text(t) => self.control_msg(&t),
                In::Closed(reason) => self.lost(&reason),
                In::Binary(_) => {}
            }
            return;
        }
        if let Some(i) = self.fetches.iter().position(|f| f.wire.as_ref().is_some_and(|w| w.id == id)) {
            match msg {
                In::Binary(b) if !self.fetches[i].welcomed => {
                    if b.first() == Some(&0x81) {
                        trace("welcomed");
                        self.fetches[i].welcomed = true;
                    } else {
                        let f = self.fetches.remove(i);
                        self.fetched(f, Some("the download was refused".to_string()));
                    }
                }
                In::Binary(b) => self.fetches[i].buf.extend_from_slice(&b),
                In::Closed(_) => {
                    // The stream closes when the whole file has come.
                    let f = self.fetches.remove(i);
                    let failed = f.buf.is_empty().then(|| "nothing came".to_string());
                    self.fetched(f, failed);
                }
                In::Text(_) => {}
            }
            return;
        }
        let View::Win(wv) = &mut self.view else {
            return;
        };
        let Some(i) = wv
            .panes
            .iter()
            .position(|p| matches!(&p.body, Body::Term(tv) if tv.data.id == id))
        else {
            return; // a stream we already left
        };
        let Body::Term(tv) = &mut wv.panes[i].body else {
            return;
        };
        let msgs = tv.on_data(msg, false);
        if let Some(why) = &tv.stream_closed {
            eprintln!("rexpaper: stream {id} ended: {why}");
            wv.chrome.clear();
            if wv.panes[i].reopen.is_none() {
                wv.panes[i].reopen = Some((Instant::now() + Duration::from_secs(2), 0));
            }
        }
        for m in msgs {
            self.stream_msg(i, m);
        }
    }

    /// Work that waits for its moment: re-reading a terminal that has gone
    /// quiet, reopening a stream that dropped, following a layout change.
    fn tick(&mut self) {
        if self.ctl.is_none() {
            return;
        }
        let rested = self.ink.last_up.is_some_and(|t| t.elapsed() >= INK_PAUSE);
        if rested && !self.ink.down && !self.ink.strokes.is_empty() && self.ink.waiting.is_none() {
            if matches!(self.view, View::Win(_)) {
                self.send_ink();
            } else {
                self.clear_ink(true);
            }
        }
        self.flush_web_keys();
        // Pictures of the browser panes on screen that are due for one.
        let mut shots = Vec::new();
        if let (View::Win(wv), None) = (&mut self.view, &self.overlay) {
            let multi = wv.panes.iter().filter(|p| p.rect.is_some()).count() > 1;
            for p in wv.panes.iter_mut() {
                if let (Some(rect), Body::Web(wb)) = (p.rect, &mut p.body) {
                    if !wb.busy && Instant::now() >= wb.next_shot {
                        wb.busy = true;
                        wb.next_shot = Instant::now() + Duration::from_secs(20);
                        let page = web::page_rect(inner(rect, multi));
                        shots.push((p.block_id.clone(), page.w, page.h, wb.hash.clone()));
                    }
                }
            }
        }
        for (bid, w, h, last) in shots {
            let asked = self.ask_helper(json!({"web": "shot", "block": bid, "w": w, "h": h, "last": last, "zoom": self.web_zoom}), Want::Shot(bid.clone()));
            if let (Err(e), Some((_, wb))) = (asked, self.web_pane(&bid)) {
                wb.busy = false;
                wb.error = Some(format!("Can't show this page: {e}"));
                wb.drawn = false;
            }
        }
        // The title change that announces a reply can be missed: look anyway.
        let due = self.scribe.as_ref().is_some_and(|sc| {
            (!self.wants.is_empty() || !sc.ready) && sc.polled.elapsed() >= Duration::from_millis(2500)
        });
        if due {
            self.poll_scribe();
        }
        if self.ink.waiting.is_some_and(|w| w.1.elapsed() >= Duration::from_secs(60)) {
            self.clear_ink(false);
            self.say("Reading the handwriting took too long. Try again.");
        }
        if matches!(&self.view, View::Win(wv) if wv.stale) {
            self.refresh_window();
        }
        let font = self.fonts[self.font_ix].clone();
        let now = Instant::now();
        let (View::Win(wv), Some(ctl)) = (&mut self.view, self.ctl.as_mut()) else {
            return;
        };
        let sid = wv.session_id.clone();
        let multi = wv.panes.iter().filter(|p| p.rect.is_some()).count() > 1;
        for pane in wv.panes.iter_mut() {
            let g_rect = pane.rect;
            match &mut pane.body {
                Body::Term(tv) if tv.settle.is_some_and(|(at, _)| now >= at) => {
                    tv.settle = None;
                    if tv.stream_closed.is_some() {
                        continue;
                    }
                    let Ok(content) = fetch_content(ctl, &mut self.net, &sid, &pane.block_id) else {
                        continue;
                    };
                    let (cols, rows) = tv.size();
                    let (term, parser) = new_term(cols, rows, &content);
                    tv.term = term;
                    tv.parser = parser;
                    for m in self.net.take_wire(tv.data.id) {
                        tv.on_data(m, true);
                    }
                }
                body => {
                    let dropped = match body {
                        // "4000 block closed": the terminal is gone for good.
                        Body::Term(tv) => tv.stream_closed.as_ref().is_some_and(|w| !w.starts_with("4000")),
                        Body::Info(_) => true,
                        Body::Web(_) => false,
                    };
                    let Some((at, tries)) = pane.reopen.filter(|(at, _)| dropped && now >= *at) else {
                        continue;
                    };
                    let full = Rect::new(0, 0, render::canvas_w(), render::canvas_h() - BAR_H - KEYS_H);
                    let r = inner(g_rect.unwrap_or(full), multi && g_rect.is_some());
                    let _ = at;
                    match open_term(ctl, &mut self.net, &font, &sid, &pane.block_id, r) {
                        Ok(tv) => {
                            *body = Body::Term(Box::new(tv));
                            pane.reopen = None;
                            wv.repaint = true;
                        }
                        Err(e) => {
                            eprintln!("rexpaper: {}: {e}", pane.block_id);
                            pane.reopen = (tries < 4).then(|| (now + Duration::from_secs(4), tries + 1));
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------ drawing

    fn draw_list(&mut self) {
        let g = self.geo();
        let host = self.net.target.host().to_string();
        let banner = self.banner.as_ref().map(|b| b.0.clone());
        let View::List(lv) = &mut self.view else {
            return;
        };
        lv.drawn = true;
        let (lw, lh) = (g.lw, g.lh);
        let fb = &mut self.canvas;
        render::fill_rect(fb, 0, 0, lw, lh, WHITE);
        render::draw_text(fb, &self.big, 60, 110, "Rex", BLACK, WHITE);
        render::draw_text(fb, &self.ui, 190, 138, &clip(&host, (lw - 250) / 16), BLACK, WHITE);
        let mut y = 230;
        let floor = lh - 110;
        let top = lv.top;
        if top > 0 {
            render::draw_text(fb, &self.ui, 90, y - 44, "... swipe down for earlier", BLACK, WHITE);
        }
        let mut hidden = 0;
        for (i, row) in lv.rows.iter_mut().enumerate() {
            row.span = None;
            if i < top {
                continue;
            }
            let h = match row.kind {
                RowKind::Session { .. } => 120,
                _ => 96,
            };
            if y + h > floor {
                hidden += 1;
                continue;
            }
            row.span = Some(Rect::new(0, y, lw, h));
            match row.kind {
                RowKind::Session { .. } => {
                    render::draw_text(fb, &self.big, 60, y + 36, &clip(&row.text, (lw - 300) / 32), BLACK, WHITE);
                    render::draw_text(fb, &self.ui, lw - 60 - 3 * 16, y + 60, "...", BLACK, WHITE);
                    render::fill_rect(fb, 60, y + h - 4, lw - 120, 4, BLACK);
                }
                RowKind::Window { .. } | RowKind::Note => {
                    let room = (lw - 180) / 16 - row.status.len();
                    render::draw_text(fb, &self.ui, 90, y + 32, &clip(&row.text, room), BLACK, WHITE);
                    let sx = lw - 60 - row.status.len() * 16;
                    render::draw_text(fb, &self.ui, sx, y + 32, &row.status, BLACK, WHITE);
                    render::fill_rect(fb, 90, y + h - 1, lw - 150, 1, BLACK);
                }
                RowKind::NewSession => {
                    ui::button(fb, &self.ui, Rect::new(60, y + 20, 360, 68), &row.text, false);
                }
            }
            y += h;
        }
        if hidden > 0 {
            render::draw_text(fb, &self.ui, 90, floor + 4, "... swipe up for more", BLACK, WHITE);
        }
        if let Some(b) = banner {
            render::fill_rect(fb, 0, lh - 60, lw, 2, BLACK);
            render::draw_text(fb, &self.ui, 16, lh - 44, &clip(&b, lw / 16 - 2), BLACK, WHITE);
        }
        self.present();
    }

    fn draw_notice(&mut self) {
        let g = self.geo();
        let View::Notice { lines, drawn } = &mut self.view else {
            return;
        };
        *drawn = true;
        render::fill_rect(&mut self.canvas, 0, 0, g.lw, g.lh, WHITE);
        let x = g.lw.saturating_sub(44 * 32) / 2;
        for (i, line) in lines.iter().enumerate() {
            render::draw_text(&mut self.canvas, &self.big, x, g.lh / 3 + i * 80, line, BLACK, WHITE);
        }
        self.present();
    }

    fn draw_win(&mut self) {
        let g = self.geo();
        let banner = self.banner.as_ref().map(|b| b.0.clone());
        let ctrl = self.ctrl;
        let font = &self.fonts[self.font_ix];
        let View::Win(wv) = &mut self.view else {
            return;
        };
        let full = wv.repaint;
        let multi = wv.panes.iter().filter(|p| p.rect.is_some()).count() > 1;
        // Where read ink is to go: take it away between two strokes, in
        // the clean mode, so that no ghost of it stays.
        if let Some(r) = self.flash.filter(|_| !full && !self.ink.down && self.ink.erasing.is_none()) {
            self.flash = None;
            self.repair = Some(self.repair.map_or(r, |o| o.union(r)));
            self.clean_until = Some(Instant::now() + CLEAN_FOR);
            if self.mode != MODE_CLEAN {
                self.mode = MODE_CLEAN;
                self.screen.set_mode(MODE_CLEAN);
            }
        }
        if full {
            self.flash = None;
        }
        // Then wipe it, and draw what was under it again.
        let repair = self.repair.take().filter(|_| !full);
        if full {
            wv.repaint = false;
            wv.chrome.clear();
            render::fill_rect(&mut self.canvas, 0, 0, g.lw, g.lh, WHITE);
        } else if let Some(r) = repair {
            wv.chrome.clear();
            render::fill_rect(&mut self.canvas, r.x, r.y, r.w, r.h, WHITE);
        }
        let redo = full || repair.is_some();
        let focus = wv.focus;
        for (i, pane) in wv.panes.iter_mut().enumerate() {
            let Some(rect) = pane.rect else {
                continue;
            };
            if multi {
                if redo {
                    // Dividing lines on the pane's right and bottom edges.
                    render::fill_rect(&mut self.canvas, rect.x + rect.w - 3, rect.y, 3, rect.h, BLACK);
                    render::fill_rect(&mut self.canvas, rect.x, rect.y + rect.h - 3, rect.w, 3, BLACK);
                }
                let key = format!("{}{}", if i == focus { '*' } else { ' ' }, pane.title);
                if redo || key != pane.title_drawn {
                    let strip = Rect::new(rect.x, rect.y, rect.w.saturating_sub(3), TITLE_H);
                    ui::line(&mut self.canvas, &self.ui, strip, &pane.title, i == focus);
                    self.dirty.add_rect(strip);
                    pane.title_drawn = key;
                }
            }
            match &mut pane.body {
                Body::Term(tv) => {
                    if full || tv.shadow.len() != tv.area.rows * tv.area.cols {
                        tv.shadow = vec![u64::MAX; tv.area.rows * tv.area.cols];
                    }
                    if let Some(r) = repair {
                        // Forget what the cells under the wiped part showed.
                        let a = &tv.area;
                        for row in 0..a.rows {
                            let y = a.y0 + row * a.ch;
                            if y + a.ch <= r.y || y >= r.y + r.h {
                                continue;
                            }
                            for col in 0..a.cols {
                                let x = a.x0 + col * a.cw;
                                if x + a.cw > r.x && x < r.x + r.w {
                                    tv.shadow[row * a.cols + col] = u64::MAX;
                                }
                            }
                        }
                    }
                    let sel = tv.term.selection.as_ref().and_then(|s| s.to_range(&tv.term));
                    // Redraw only the cells whose look changed, and per row
                    // only the span between the first and last of them:
                    // small e-ink updates.
                    for row in 0..tv.area.rows {
                        let mut changed: Option<(usize, usize)> = None;
                        for col in 0..tv.area.cols {
                            let look = render::cell_look(
                                &tv.term,
                                &self.palette,
                                row + tv.area.row_off,
                                col,
                                sel.as_ref(),
                            );
                            let slot = &mut tv.shadow[row * tv.area.cols + col];
                            if *slot != look {
                                *slot = look;
                                changed = Some(changed.map_or((col, col), |(a, _)| (a, col)));
                            }
                        }
                        if let Some((a, b)) = changed {
                            render::draw_line(
                                &mut self.canvas,
                                font,
                                &self.palette,
                                &tv.term,
                                &tv.area,
                                row,
                                a,
                                b,
                                sel.as_ref(),
                            );
                            let (x, y, _, _) = tv.area.cell_rect(a, row);
                            self.dirty.add(x, y, (b - a + 1) * tv.area.cw, tv.area.ch);
                        }
                    }
                }
                Body::Info(lines) if redo => {
                    let r = inner(rect, multi);
                    for (n, text) in lines.iter().enumerate() {
                        let y = r.y + 40 + n * 48;
                        if y + 32 < r.y + r.h {
                            let t = clip(text, (r.w.saturating_sub(48) / 16).max(1));
                            render::draw_text(&mut self.canvas, &self.ui, r.x + 24, y, &t, BLACK, WHITE);
                        }
                    }
                }
                Body::Info(_) => {}
                Body::Web(wb) => {
                    if redo || !wb.drawn {
                        let r = inner(rect, multi);
                        wb.draw(&mut self.canvas, &self.ui, r);
                        trace("drawn");
                        self.dirty.add_rect(r);
                    }
                }
            }
        }

        let state = match wv.panes.get(focus).map(|p| &p.body) {
            _ if banner.is_some() => banner.unwrap(),
            Some(Body::Term(tv)) if tv.stream_closed.is_some() => {
                format!("stream ended: {}", tv.stream_closed.as_deref().unwrap_or(""))
            }
            Some(Body::Term(tv)) if tv.exited => "program exited".to_string(),
            _ if wv.panes.iter().any(|p| matches!(&p.body, Body::Term(tv) if !tv.owner)) => {
                "sized by another device - tap here to fit".to_string()
            }
            _ if wv.select => "drag over the text to copy it".to_string(),
            _ if wv.floating > 0 => format!("+{} floating not shown", wv.floating),
            _ => String::new(),
        };
        let cols = g.lw / 16;
        let name_room = (cols / 3).max(12);
        let left = format!("< Sessions   {}", clip(&wv.name, name_room));
        let state = clip(&state, cols.saturating_sub(left.chars().count() + 5));
        let gap = cols.saturating_sub(left.chars().count() + state.chars().count() + 2);
        let chrome = format!(" {left}{}{state}|{ctrl}", " ".repeat(gap));
        if chrome != wv.chrome {
            let bar = chrome.rsplit_once('|').map_or("", |c| c.0);
            render::fill_rect(&mut self.canvas, 0, g.keys_y, g.lw, g.lh - g.keys_y, WHITE);
            let n = KEY_ROW.len();
            for (i, (label, btn)) in KEY_ROW.iter().enumerate() {
                let x0 = i * g.lw / n;
                let r = Rect::new(x0 + 4, g.keys_y + 6, (i + 1) * g.lw / n - x0 - 8, KEYS_H - 12);
                ui::button(&mut self.canvas, &self.ui, r, label, *btn == KeyBtn::Ctrl && ctrl);
            }
            render::fill_rect(&mut self.canvas, 0, g.bar_y, g.lw, 2, BLACK);
            render::draw_text(&mut self.canvas, &self.ui, 0, g.bar_y + 18, bar, BLACK, WHITE);
            self.dirty.add(0, g.keys_y, g.lw, g.lh - g.keys_y);
            wv.chrome = chrome;
        }

        if redo {
            self.redraw_ink();
        }
        if full {
            self.present();
        } else {
            if let Some(r) = repair {
                self.dirty.add_rect(r);
                self.dirty.add(0, g.keys_y, g.lw, g.lh - g.keys_y);
            }
            self.flush();
        }
    }

    /// `full` when the view under the overlay was drawn again too.
    fn draw_overlay(&mut self, full: bool) {
        let g = self.geo();
        self.overlay_drawn = true;
        let fb = &mut self.canvas;
        let area = match &mut self.overlay {
            Some(Overlay::Menu { items, spans, area }) => {
                let h = items.len() * ITEM_H + 8;
                let w = MENU_W.min(g.lw - 40);
                // Over the list it sits in the middle; over a tab, above
                // the Menu key.
                let (x, y) = match self.view {
                    View::Win(_) => (g.lw - w - 12, g.keys_y.saturating_sub(h + 12)),
                    _ => ((g.lw - w) / 2, (g.lh.saturating_sub(h)) / 3),
                };
                *area = Rect::new(x, y, w, h);
                render::fill_rect(fb, x, y, w, h, WHITE);
                ui::outline(fb, *area, 4, BLACK);
                spans.clear();
                for (i, (text, _)) in items.iter().enumerate() {
                    let r = Rect::new(x + 4, y + 4 + i * ITEM_H, w - 8, ITEM_H);
                    ui::line(fb, &self.ui, Rect::new(r.x + 16, r.y + 2, r.w - 32, r.h - 4), text, false);
                    if i > 0 {
                        render::fill_rect(fb, r.x + 16, r.y, r.w - 32, 1, BLACK);
                    }
                    spans.push(r);
                }
                *area
            }
            Some(Overlay::Ask(ask)) => {
                let w = (g.lw - 160).min(1300);
                let x = (g.lw - w) / 2;
                let rows = match &ask.kind {
                    AskKind::Pick { items, .. } => items.len().min(12),
                    _ => 1,
                };
                let h = 110 + rows * ITEM_H + 150;
                let y = 180.min(g.bar_y.saturating_sub(h) / 2);
                let area = Rect::new(x, y, w, h);
                render::fill_rect(fb, x, y, w, h, WHITE);
                ui::outline(fb, area, 4, BLACK);
                let chars = (w - 80) / 16;
                render::draw_text(fb, &self.ui, x + 40, y + 40, &clip(&ask.title, chars), BLACK, WHITE);
                let body = Rect::new(x + 40, y + 110, w - 80, ITEM_H - 12);
                ask.items.clear();
                let mut yes = "OK".to_string();
                match &ask.kind {
                    AskKind::Text { value, hint } => {
                        ui::outline(fb, body, 2, BLACK);
                        let shown: String = if value.is_empty() {
                            format!("_   ({hint})")
                        } else {
                            // Keep the end in view, where the typing is.
                            let n = value.chars().count();
                            let tail: String = value.chars().skip(n.saturating_sub(chars - 4)).collect();
                            format!("{tail}_")
                        };
                        render::draw_text(fb, &self.ui, body.x + 16, body.y + 22, &clip(&shown, chars - 2), BLACK, WHITE);
                    }
                    AskKind::Confirm { detail, yes: label } => {
                        render::draw_text(fb, &self.ui, body.x, body.y + 22, &clip(detail, chars), BLACK, WHITE);
                        yes = label.clone();
                    }
                    AskKind::Pick { items, multiple } => {
                        for (i, it) in items.iter().take(12).enumerate() {
                            let r = Rect::new(body.x, body.y + i * ITEM_H, body.w, ITEM_H - 12);
                            let mark = match (*multiple, it.on) {
                                (true, true) => "[x] ",
                                (true, false) => "[ ] ",
                                _ => "",
                            };
                            ui::line(fb, &self.ui, r, &format!("{mark}{}", it.text), false);
                            ui::outline(fb, r, 1, BLACK);
                            ask.items.push(r);
                        }
                        if !multiple {
                            yes.clear();
                        }
                    }
                }
                let by = y + h - 110;
                ask.cancel = Rect::new(x + 40, by, 300, 76);
                ui::button(fb, &self.ui, ask.cancel, "Cancel", false);
                ask.ok = Rect::default();
                if !yes.is_empty() {
                    let bw = (yes.len() * 16 + 80).max(300);
                    ask.ok = Rect::new(x + w - 40 - bw, by, bw, 76);
                    ui::button(fb, &self.ui, ask.ok, &yes, true);
                }
                area
            }
            None => return,
        };
        if full {
            return self.present();
        }
        self.dirty.add_rect(area);
        self.flush();
    }

    fn draw(&mut self) {
        if self.overlay.is_some() {
            // The view under a dialog waits: drawing it would paint over
            // the dialog.
            if !self.overlay_drawn {
                // What a dialog replaced, such as a menu, is cleared first.
                self.hold = true;
                let under = match &self.view {
                    View::Win(wv) if wv.repaint => {
                        self.draw_win();
                        true
                    }
                    View::List(ListView { drawn: false, .. }) => {
                        self.draw_list();
                        true
                    }
                    View::Notice { drawn: false, .. } => {
                        self.draw_notice();
                        true
                    }
                    _ => false,
                };
                self.hold = false;
                self.draw_overlay(under);
            }
            return;
        }
        match &self.view {
            View::List(ListView { drawn: false, .. }) => self.draw_list(),
            View::Notice { drawn: false, .. } => self.draw_notice(),
            View::Win(_) => self.draw_win(),
            _ => {}
        }
    }

    /// The focused terminal's text, for checking a headless run.
    fn dump_text(&self) -> String {
        let View::Win(wv) = &self.view else {
            return String::new();
        };
        let Some(Pane { body: Body::Term(tv), .. }) = wv.panes.get(wv.focus) else {
            return String::new();
        };
        let mut out = String::new();
        for row in 0..tv.term.screen_lines() {
            let line: String = (0..tv.term.columns())
                .map(|col| tv.term.grid()[Line(row as i32)][Column(col)].c)
                .collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

/// Timings for finding slowness: written to /tmp/rexpaper.trace when the
/// file `trace` is in the config folder.
fn trace(what: &str) {
    use std::io::Write;
    use std::sync::OnceLock;
    static ON: OnceLock<Option<Instant>> = OnceLock::new();
    let Some(t0) = ON.get_or_init(|| (std::env::var("REXPAPER_TRACE").is_ok() || std::path::Path::new(&format!("{CONFIG_DIR}/trace")).exists()).then(Instant::now)) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/rexpaper.trace") {
        let _ = writeln!(f, "{:8} {what}", t0.elapsed().as_millis());
    }
}

fn setting(name: &str) -> Option<String> {
    std::env::var(format!("REXPAPER_{}", name.to_uppercase()))
        .ok()
        .or_else(|| std::fs::read_to_string(format!("{CONFIG_DIR}/{name}")).ok())
        .map(|s| s.lines().next().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
}

fn server_target() -> Result<Target, String> {
    let spec = setting("server")
        .ok_or_else(|| format!("No Rex server set. Put its Tailscale name in {CONFIG_DIR}/server."))?;
    Target::parse(&spec)
}

fn single_instance_lock() -> bool {
    let path = std::ffi::CString::new("/tmp/rexpaper.lock").unwrap();
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
        // The fd is leaked on purpose: the lock lives as long as the process.
        fd < 0 || libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0
    }
}

fn env_secs(name: &str) -> Option<Duration> {
    std::env::var(name).ok()?.parse::<f64>().ok().map(Duration::from_secs_f64)
}

/// `REXPAPER_TAPS="2:810,300 3.5:100,80-400,80"`: taps and drags at
/// times, for driving a headless run. `x,y` are canvas pixels.
fn scripted_taps() -> Vec<(Duration, [i32; 4])> {
    let xy = |s: &str| -> Option<(i32, i32)> {
        let (x, y) = s.split_once(',')?;
        Some((x.parse().ok()?, y.parse().ok()?))
    };
    let mut taps: Vec<(Duration, [i32; 4])> = std::env::var("REXPAPER_TAPS")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|t| {
            let (at, path) = t.split_once(':')?;
            let (from, to) = path.split_once('-').unwrap_or((path, path));
            let ((x0, y0), (x1, y1)) = (xy(from)?, xy(to)?);
            Some((Duration::from_secs_f64(at.parse().ok()?), [x0, y0, x1, y1]))
        })
        .collect();
    taps.reverse();
    taps
}

fn main() {
    if let Err(e) = run() {
        eprintln!("rexpaper: fatal: {e}");
        std::process::exit(1);
    }
}

fn run() -> std::io::Result<()> {
    let headless = std::env::var("REXPAPER_HEADLESS").ok();
    if headless.is_none() && !single_instance_lock() {
        eprintln!("rexpaper: another instance is running, exiting");
        return Ok(());
    }
    let parse = |d: &'static [u8]| Psf2Font::parse(d).map_err(std::io::Error::other);
    let fonts = FONTS.iter().map(|f| parse(f.1)).collect::<Result<Vec<_>, _>>()?;
    let wanted = setting("font");
    let font_ix = FONTS.iter().position(|f| Some(f.0) == wanted.as_deref()).unwrap_or(1);

    let screen = Screen::open()?;
    let poller = Arc::new(Poller::new()?);
    if let Some(fd) = screen.raw_fd() {
        unsafe { poller.add_with_mode(&FdSource(fd), PollEvent::readable(2), PollMode::Level)? };
    }
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;

    let target = server_target();
    let mut app = App {
        screen,
        canvas: vec![0xff; SCREEN_W * SCREEN_H * 2],
        rot: Rot(0),
        fonts,
        font_ix,
        web_zoom: setting("webzoom").and_then(|z| z.parse().ok()).unwrap_or(100).clamp(50, 300),
        ui: parse(FONTS[1].1)?,
        big: parse(FONTS[2].1)?,
        palette: palette::Palette::new(),
        net: Net::new(
            target.clone().unwrap_or(Target::Unix(String::new())),
            Arc::clone(&poller),
        ),
        ctl: None,
        kind: if setting("kind").as_deref() == Some("app") { "app" } else { "other" }.to_string(),
        view: View::Notice {
            lines: Vec::new(),
            drawn: true,
        },
        overlay: None,
        overlay_drawn: false,
        hold: false,
        settle_at: None,
        dirty: DirtyRegion::default(),
        kbd: std::env::var("REXPAPER_KBD").is_ok(),
        ctrl: false,
        alt: false,
        clip: String::new(),
        banner: None,
        names: HashMap::new(),
        retry: None,
        last_load: Instant::now(),
        ink: Ink::default(),
        scribe: None,
        raw_pen: false,
        wants: HashMap::new(),
        acks: Vec::new(),
        next_req: 0,
        fetches: Vec::new(),
        web_keys: (String::new(), Vec::new()),
        repair: None,
        flash: None,
        trace: setting("trace").is_some(),
        trace_down: None,
        mode: MODE_TEXT,
        clean_until: None,
    };
    render::set_canvas_size(SCREEN_W, SCREEN_H);
    if let Some(r) = std::env::var("REXPAPER_ROT").ok().and_then(|r| r.parse().ok()) {
        app.rotate(Rot(r));
    }
    match &target {
        Ok(t) => {
            app.notice(&format!("Connecting to Rex on {}...", t.host()));
            app.draw();
            app.connect();
        }
        Err(e) => app.notice(e),
    }

    // Test hooks for headless runs on the Mac.
    if let (Ok(want), View::List(lv)) = (std::env::var("REXPAPER_OPEN"), &app.view) {
        let hit = lv.rows.iter().find_map(|r| match &r.kind {
            RowKind::Window { sid, wid, blocks, .. } => blocks
                .iter()
                .find(|b| b.ends_with(&want))
                .map(|b| (sid.clone(), wid.clone(), b.clone())),
            _ => None,
        });
        match hit {
            Some((s, w, b)) => app.open(&s, &w, None, Some(&b)),
            None => eprintln!("rexpaper: no terminal with an ID ending in {want}"),
        }
    }
    let mut type_text = std::env::var("REXPAPER_TYPE").ok();
    let mut taps = scripted_taps();
    let mut inject = std::env::var("REXPAPER_INJECT").ok();
    let type_at = env_secs("REXPAPER_TYPE_AT").unwrap_or(Duration::from_secs(2));
    let mut drop_at = env_secs("REXPAPER_DROP_AT");
    let exit_after = env_secs("REXPAPER_EXIT_AFTER");
    let started = Instant::now();

    let mut pen = pen::Pen::open();
    if let Some(p) = &pen {
        unsafe { poller.add_with_mode(&FdSource(p.raw_fd()), PollEvent::readable(3), PollMode::Level)? };
        app.raw_pen = true;
    }
    // `REXPAPER_INK=strokes.json` writes those strokes with the pen, at
    // `REXPAPER_INK_AT` seconds: [[[x, y], ...], ...] in screen pixels.
    let mut ink_test: Option<Vec<Vec<(i32, i32)>>> = std::env::var("REXPAPER_INK")
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok());
    let erase_test = std::env::var("REXPAPER_INK_ERASER").is_ok();
    let ink_at = env_secs("REXPAPER_INK_AT").unwrap_or(Duration::from_secs(3));
    let mut keyboards = evkbd::Keyboards::new(true);
    let mut last_kbd_scan: Option<Instant> = None;
    let mut events = Events::with_capacity(NonZeroUsize::new(32).unwrap());
    let mut clock = (Instant::now(), SystemTime::now());

    'main: loop {
        if stop.load(Ordering::Relaxed) || exit_after.is_some_and(|d| started.elapsed() >= d) {
            break;
        }
        events.clear();
        let nap = [app.settle_at, app.clean_until]
            .into_iter()
            .flatten()
            .map(|at| at.saturating_duration_since(Instant::now()))
            .fold(Duration::from_millis(300), Duration::min);
        poller.wait(&mut events, Some(nap))?;
        let woke = Instant::now();
        if app.clean_until.is_some_and(|at| Instant::now() >= at) {
            app.clean_until = None;
            let near = app.ink.near && matches!(app.view, View::Win(_));
            app.set_mode(if near { MODE_INK } else { MODE_TEXT });
        }
        // The pen has gone: back to the mode that draws text cleanly.
        let away = !app.ink.near && !app.ink.down && app.ink.away.is_none_or(|t| t.elapsed() >= Duration::from_secs(2));
        if away && app.mode == MODE_INK && app.clean_until.is_none() {
            app.set_mode(MODE_TEXT);
        }
        if app.settle_at.is_some_and(|at| Instant::now() >= at) {
            app.settle_at = None;
            app.show_all();
            if std::mem::take(&mut app.ink.deghost_now) {
                app.screen.deghost();
            }
        }

        // While the tablet sleeps the wall clock runs and this one doesn't.
        // A gap between them means the connections are dead: don't wait
        // for the keepalive to find out.
        let now = (Instant::now(), SystemTime::now());
        let wall = now.1.duration_since(clock.1).unwrap_or_default();
        let slept = wall.saturating_sub(now.0 - clock.0) > Duration::from_secs(8);
        clock = now;
        if (slept || drop_at.is_some_and(|d| started.elapsed() >= d)) && app.ctl.is_some() {
            drop_at = None;
            app.lost("the tablet slept");
        }

        let mut rotation = None;
        for ev in events.iter() {
            match ev.key {
                2 => {
                    let Ok(drained) = app.screen.drain_events() else {
                        break 'main; // the window was closed
                    };
                    rotation = drained.rotation.or(rotation);
                    for input in drained.inputs {
                        app.touch(input);
                    }
                }
                3 => {
                    for e in pen.as_mut().map(|p| p.read()).unwrap_or_default() {
                        if app.trace && matches!(e, PenEv::Down(..)) {
                            let age = pen.as_ref().map_or(0, |p| p.down_age_us);
                            eprintln!("rexpaper: trace {:.3} pen landed {} us before it was read", started.elapsed().as_secs_f64(), age);
                        }
                        app.pen(e);
                    }
                }
                k if k >= KBD_KEY_BASE => {
                    let fd = (k - KBD_KEY_BASE) as RawFd;
                    let (presses, gone) = keyboards.read(fd);
                    for p in presses {
                        app.key(p.action, p.ctrl, p.alt);
                    }
                    if gone {
                        let _ = poller.delete(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) });
                        keyboards.remove(fd);
                    }
                }
                _ => {}
            }
        }

        // Keyboards come and go (Type Folio, Bluetooth): look again now and then.
        if last_kbd_scan.is_none_or(|t| t.elapsed() >= KBD_SCAN) {
            last_kbd_scan = Some(Instant::now());
            for fd in keyboards.scan() {
                unsafe {
                    poller.add_with_mode(
                        &FdSource(fd),
                        PollEvent::readable(KBD_KEY_BASE + fd as usize),
                        PollMode::Level,
                    )?;
                }
            }
        }

        let t_net = Instant::now();
        while let Some((id, msg)) = app.net.pop() {
            app.net_msg(id, msg);
        }
        let net_ms = t_net.elapsed().as_millis();

        if let Some(r) = rotation {
            let rot = Rot::from_appload(r, false);
            if rot != app.rot {
                app.rotate(rot);
            }
        }

        if app.ctl.is_none() && app.retry.as_ref().is_some_and(|r| Instant::now() >= r.at) {
            app.draw();
            app.connect();
        }

        // Reload the list a moment after the server says something changed,
        // but not so often that the screen keeps flashing.
        if app.overlay.is_none()
            && app.last_load.elapsed() >= Duration::from_secs(3)
            && matches!(&app.view, View::List(ListView { stale: Some(t), .. }) if t.elapsed() >= Duration::from_secs(1))
        {
            app.show_list();
        }
        let t_tick = Instant::now();
        app.tick();
        let tick_ms = t_tick.elapsed().as_millis();
        if app.banner.as_ref().is_some_and(|b| b.1.elapsed() >= Duration::from_secs(30)) {
            app.banner = None;
            app.touch_chrome();
        }

        while taps.last().is_some_and(|t| started.elapsed() >= t.0) {
            let (_, [x0, y0, x1, y1]) = taps.pop().unwrap();
            let steps = [
                (qtfb::INPUT_TOUCH_PRESS, x0, y0),
                (qtfb::INPUT_TOUCH_UPDATE, x1, y1),
                (qtfb::INPUT_TOUCH_RELEASE, x1, y1),
            ];
            for (input_type, x, y) in steps {
                let (px, py) = app.rot.to_phys(x as usize, y as usize, SCREEN_W, SCREEN_H);
                app.touch(qtfb::InputEvent { input_type, dev_id: 0, x: px as i32, y: py as i32, d: 0 });
            }
        }
        if started.elapsed() >= ink_at {
            for stroke in ink_test.take().unwrap_or_default() {
                for (i, p) in stroke.iter().enumerate() {
                    app.pen(if i == 0 { PenEv::Down(p.0, p.1, erase_test) } else { PenEv::Move(p.0, p.1) });
                }
                app.pen(PenEv::Up);
            }
        }
        if started.elapsed() >= Duration::from_secs(2) {
            // A control message as if the server had sent it.
            if let Some(text) = inject.take() {
                app.control_msg(&text);
            }
        }
        if started.elapsed() >= type_at && (matches!(app.view, View::Win(_)) || app.overlay.is_some()) {
            if let Some(text) = type_text.take() {
                for c in text.chars() {
                    match c {
                        '\n' => app.key(KeyAction::Enter, false, false),
                        c => app.key(KeyAction::Char(c), false, false),
                    }
                }
            }
        }

        let t_draw = Instant::now();
        app.draw();
        if app.trace && woke.elapsed() >= Duration::from_millis(25) {
            eprintln!(
                "rexpaper: trace {:.3} slow loop {} ms (net {net_ms}, tick {tick_ms}, draw {})",
                started.elapsed().as_secs_f64(),
                woke.elapsed().as_millis(),
                t_draw.elapsed().as_millis()
            );
        }
    }

    eprintln!("rexpaper: exiting");
    if let Some(path) = &headless {
        let _ = std::fs::write(format!("{path}.txt"), app.dump_text());
    }
    if let Some(sc) = app.scribe.take().filter(|sc| !sc.bid.is_empty()) {
        let _ = app.call("block.close", json!({"session_id": sc.sid, "block_id": sc.bid}));
    }
    // Hand the terminals back before the connection goes.
    if let View::Win(wv) = &app.view {
        let sid = wv.session_id.clone();
        let _ = app.call("session.detach", json!({"session_id": sid}));
    }
    app.screen.finish();
    // Dropping the connections sends their close frames: give the threads
    // a moment, so the server sees a client that left, not one that vanished.
    app.view = View::Notice { lines: Vec::new(), drawn: true };
    app.ctl = None;
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> Value {
        json!({"windows": [{
            "window_id": "window:1",
            "focused_block_id": "block:b",
            "layers": [
                {"kind": "tiled", "layout": {"rect": [0, 0, 1, 1], "split": {
                    "direction": "horizontal", "ratio": 0.5,
                    "before": {"rect": [0, 0, 0.5, 1], "block_id": "block:a"},
                    "after": {"rect": [0.5, 0, 0.5, 1], "split": {
                        "direction": "vertical", "ratio": 0.5,
                        "before": {"rect": [0.5, 0, 0.5, 0.5], "block_id": "block:b"},
                        "after": {"rect": [0.5, 0.5, 0.5, 0.5], "block_id": "block:c"}}}}},
                 "blocks": [
                    {"block_id": "block:a", "creator_name": TERMINAL, "label": "shell"},
                    {"block_id": "block:b", "creator_name": TERMINAL},
                    {"block_id": "block:c", "creator_name": "com.superlogical.onboarding"}]},
                {"kind": "floating", "layout": null, "blocks": []},
                {"kind": "floating", "state": "minimized", "layout": null, "blocks": []}]}]})
    }

    #[test]
    fn window_layout() {
        let info = window_info(&view(), "window:1").unwrap();
        let ids: Vec<&str> = info.panes.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(ids, ["block:a", "block:b", "block:c"]);
        assert_eq!(info.panes[2].1, [0.5, 0.5, 0.5, 0.5]);
        assert_eq!(info.panes[2].2, "com.superlogical.onboarding");
        assert_eq!(info.focused.as_deref(), Some("block:b"));
        assert_eq!(info.floating, 1);
        assert!(window_info(&view(), "window:2").is_none());
    }

    #[test]
    fn panes_tile_the_canvas() {
        let info = window_info(&view(), "window:1").unwrap();
        let r: Vec<Rect> = info.panes.iter().map(|p| frac_rect(p.1, 1620, 2004)).collect();
        assert_eq!(r[0], Rect::new(0, 0, 810, 2004));
        assert_eq!(r[1], Rect::new(810, 0, 810, 1002));
        assert_eq!(r[2], Rect::new(810, 1002, 810, 1002));
        // Inside a shared tab a pane gives up its title strip and borders.
        assert_eq!(inner(r[1], true), Rect::new(812, TITLE_H, 804, 1002 - TITLE_H - 4));
        assert_eq!(inner(r[1], false), r[1]);
    }

    #[test]
    fn status_picks_the_most_urgent() {
        let recs = json!([{"state": "working"}, {"state": "blocked"}, {"state": "done"}]);
        assert_eq!(status_of(&recs), "[blocked]");
        assert_eq!(status_of(&json!([])), "");
    }
}
