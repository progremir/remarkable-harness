//! Browser panes. A rexweb pane (https://github.com/acoyfellow/rexweb) is a
//! terminal block labelled "browser" with a WebKit view drawn over it on the
//! Mac. The tablet has no browser engine, so it shows pictures of the page,
//! which the helper on the Mac takes, and sends taps and typing back.

use std::time::Instant;

use crate::font::Psf2Font;
use crate::render::{self, BLACK, WHITE};
use crate::ui::{self, Rect};

/// The label rexweb gives a pane until its page has a title.
pub const LABEL: &str = "browser";
/// What the terminal under a rexweb pane shows: "rexweb · " and the address.
pub const SCREEN: &str = "rexweb \u{b7}";

/// Height of the bar above the page: back, forward, reload and the address.
pub const BAR_H: usize = 60;
const BTN_W: usize = 84;
const RELOAD_W: usize = 124;
const ZOOM_W: usize = 76;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BarHit {
    Back,
    Forward,
    Reload,
    Address,
    Smaller,
    Bigger,
}

/// The part of a browser pane that shows the page.
pub fn page_rect(r: Rect) -> Rect {
    Rect::new(r.x, r.y + BAR_H, r.w, r.h.saturating_sub(BAR_H))
}

fn bar_parts(r: Rect) -> [(BarHit, Rect); 6] {
    let h = BAR_H - 2;
    let left = 2 * BTN_W + RELOAD_W;
    [
        (BarHit::Back, Rect::new(r.x, r.y, BTN_W, h)),
        (BarHit::Forward, Rect::new(r.x + BTN_W, r.y, BTN_W, h)),
        (BarHit::Reload, Rect::new(r.x + 2 * BTN_W, r.y, RELOAD_W, h)),
        (BarHit::Address, Rect::new(r.x + left, r.y, r.w.saturating_sub(left + 2 * ZOOM_W), h)),
        (BarHit::Smaller, Rect::new(r.x + r.w.saturating_sub(2 * ZOOM_W), r.y, ZOOM_W, h)),
        (BarHit::Bigger, Rect::new(r.x + r.w.saturating_sub(ZOOM_W), r.y, ZOOM_W, h)),
    ]
}

/// What part of the bar a point of the canvas is on.
pub fn bar_hit(x: i32, y: i32, r: Rect) -> Option<BarHit> {
    bar_parts(r).iter().find(|(_, b)| b.has(x, y)).map(|(hit, _)| *hit)
}

/// A pen stroke left on a page as a note. Its points are in the page's
/// CSS pixels, from the top of the whole page, so it stays with what it
/// was drawn on when the page scrolls. `vw` is how wide the page was laid
/// out then: at another width the points are scaled to match.
#[derive(Clone)]
pub struct Note {
    pub vw: f64,
    pub pts: Vec<(f64, f64)>,
}

/// The name a page's notes are kept under: its address without the part
/// after `#`, which only says where on the page to look.
pub fn note_key(url: &str) -> &str {
    url.split('#').next().unwrap_or(url)
}

pub struct WebView {
    /// The page as last seen: RGB565, `iw` x `ih`.
    img: Vec<u8>,
    pub iw: usize,
    pub ih: usize,
    /// Names the picture, so the helper can say "the same" instead of
    /// sending it again.
    pub hash: String,
    /// The page's own size in CSS pixels: the picture shows exactly this.
    pub vw: f64,
    pub vh: f64,
    /// How far down the page is scrolled and how tall it is, in CSS pixels.
    pub y: f64,
    pub full: f64,
    /// The button just pressed, shown dark until the page answers.
    pub held: Option<BarHit>,
    pub url: String,
    pub title: String,
    /// The page's focus is in a text field.
    pub edit: bool,
    pub drawn: bool,
    /// A picture has been asked for and not arrived.
    pub busy: bool,
    pub next_shot: Instant,
    pub error: Option<String>,
}

impl WebView {
    pub fn new() -> WebView {
        WebView {
            img: Vec::new(),
            iw: 0,
            ih: 0,
            hash: String::new(),
            vw: 0.0,
            vh: 0.0,
            y: 0.0,
            full: 0.0,
            held: None,
            url: String::new(),
            title: String::new(),
            edit: false,
            drawn: false,
            busy: false,
            next_shot: Instant::now(),
            error: None,
        }
    }

    /// Take a new picture of the page, as the JPEG file the helper made.
    pub fn set_jpeg(&mut self, data: &[u8]) -> Result<(), String> {
        let mut dec = jpeg_decoder::Decoder::new(data);
        let px = dec.decode().map_err(|e| e.to_string())?;
        let info = dec.info().ok_or("no picture")?;
        let (w, h) = (info.width as usize, info.height as usize);
        let mut img = Vec::with_capacity(w * h * 2);
        let mut push = |r: u8, g: u8, b: u8| {
            let v = ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3);
            img.extend_from_slice(&v.to_le_bytes());
        };
        match info.pixel_format {
            jpeg_decoder::PixelFormat::RGB24 => px.chunks_exact(3).for_each(|p| push(p[0], p[1], p[2])),
            jpeg_decoder::PixelFormat::L8 => px.iter().for_each(|&l| push(l, l, l)),
            _ => return Err("a kind of picture this can't show".into()),
        }
        if img.len() != w * h * 2 {
            return Err("the picture is cut short".into());
        }
        (self.img, self.iw, self.ih) = (img, w, h);
        self.drawn = false;
        Ok(())
    }

    /// Move the picture by a drag of `dy` canvas pixels (up is positive),
    /// as far as the page can scroll, so the drag shows before the Mac's
    /// picture of the scrolled page arrives. The uncovered strip is blank.
    pub fn shift(&mut self, dy: i32) {
        if self.iw == 0 || self.vw <= 0.0 {
            return;
        }
        let px = self.iw as f64 / self.vw;
        let (up, down) = (self.y * px, ((self.full - self.vh - self.y) * px).max(0.0));
        let dy = (dy as f64).clamp(-up, down).round() as i64;
        let (row, rows) = (self.iw * 2, self.ih);
        let n = dy.unsigned_abs() as usize;
        if n == 0 {
            return;
        }
        if n >= rows {
            self.img.fill(0xff);
        } else if dy > 0 {
            self.img.copy_within(n * row.., 0);
            self.img[(rows - n) * row..].fill(0xff);
        } else {
            self.img.copy_within(..(rows - n) * row, n * row);
            self.img[..n * row].fill(0xff);
        }
        self.y += dy as f64 / px;
        self.hash.clear();
        self.drawn = false;
    }

    /// The point of the page, in CSS pixels, under a point of the canvas.
    pub fn page_point(&self, x: i32, y: i32, r: Rect) -> Option<(f64, f64)> {
        let (px, py) = (x - r.x as i32, y - r.y as i32);
        if self.iw == 0 || px < 0 || py < 0 || px as usize >= self.iw || py as usize >= self.ih {
            return None;
        }
        Some((px as f64 * self.vw / self.iw as f64, py as f64 * self.vh / self.ih as f64))
    }

    /// How many CSS pixels one canvas pixel is.
    pub fn scale(&self) -> f64 {
        if self.iw == 0 {
            1.0
        } else {
            self.vw / self.iw as f64
        }
    }

    /// Where a point of the canvas is on the whole page, in CSS pixels.
    pub fn to_page(&self, x: i32, y: i32, r: Rect) -> Option<(f64, f64)> {
        if self.iw == 0 || self.vw <= 0.0 {
            return None;
        }
        let k = self.vw / self.iw as f64;
        Some(((x - r.x as i32) as f64 * k, (y - r.y as i32) as f64 * k + self.y))
    }

    /// Where a note's point is on the canvas now. It may be off the pane.
    pub fn to_canvas(&self, note: &Note, p: (f64, f64), r: Rect) -> (i32, i32) {
        let fit = if note.vw > 0.0 { self.vw / note.vw } else { 1.0 };
        let k = self.iw as f64 / self.vw.max(1.0);
        (r.x as i32 + (p.0 * fit * k).round() as i32, r.y as i32 + ((p.1 * fit - self.y) * k).round() as i32)
    }

    /// Draw the notes left on this page over its picture.
    fn draw_notes(&self, fb: &mut [u8], notes: &[Note], r: Rect) {
        if self.iw == 0 || self.vw <= 0.0 {
            return;
        }
        let (x0, y0, x1, y1) = (r.x as i32 + 2, r.y as i32 + 2, (r.x + r.w) as i32 - 3, (r.y + r.h) as i32 - 3);
        let mut dot = |x: i32, y: i32| {
            if x >= x0 && y >= y0 && x < x1 && y < y1 {
                render::fill_rect(fb, x as usize - 1, y as usize - 1, 4, 4, BLACK);
            }
        };
        for note in notes {
            let pts: Vec<(i32, i32)> = note.pts.iter().map(|&p| self.to_canvas(note, p, r)).collect();
            if pts.iter().all(|p| p.1 < y0 - 4) || pts.iter().all(|p| p.1 > y1 + 4) {
                continue;
            }
            if let [one] = pts[..] {
                dot(one.0, one.1);
            }
            for w in pts.windows(2) {
                let (a, b) = (w[0], w[1]);
                let n = (b.0 - a.0).abs().max((b.1 - a.1).abs()).max(1);
                for k in 0..=n {
                    dot(a.0 + (b.0 - a.0) * k / n, a.1 + (b.1 - a.1) * k / n);
                }
            }
        }
    }

    pub fn draw(&mut self, fb: &mut [u8], font: &Psf2Font, r: Rect, notes: &[Note]) {
        self.drawn = true;
        render::fill_rect(fb, r.x, r.y, r.w, r.h, WHITE);
        for (hit, b) in bar_parts(r) {
            match hit {
                BarHit::Back => ui::button(fb, font, b, "\u{2190}", self.held == Some(hit)),
                BarHit::Forward => ui::button(fb, font, b, "\u{2192}", self.held == Some(hit)),
                BarHit::Reload => ui::button(fb, font, b, "Reload", self.held == Some(hit)),
                BarHit::Smaller => ui::button(fb, font, b, "A-", false),
                BarHit::Bigger => ui::button(fb, font, b, "A+", false),
                BarHit::Address => {
                    let text = if self.url.is_empty() { "Tap to enter an address" } else { &self.url };
                    ui::line(fb, font, b, text, false);
                    ui::outline(fb, b, 2, BLACK);
                }
            }
        }
        render::fill_rect(fb, r.x, r.y + BAR_H - 2, r.w, 2, BLACK);
        let r = page_rect(r);
        if self.img.is_empty() {
            let text = match &self.error {
                Some(e) => e.clone(),
                None => "Loading the page...".to_string(),
            };
            for (i, line) in ui::wrap(&text, (r.w.saturating_sub(48) / font.width).max(8)).iter().enumerate() {
                render::draw_text(fb, font, r.x + 24, r.y + 40 + i * 44, line, BLACK, WHITE);
            }
            return;
        }
        let cw = render::canvas_w();
        let (w, h) = (self.iw.min(r.w), self.ih.min(r.h));
        for row in 0..h {
            let src = &self.img[row * self.iw * 2..(row * self.iw + w) * 2];
            let at = ((r.y + row) * cw + r.x) * 2;
            fb[at..at + w * 2].copy_from_slice(src);
        }
        self.draw_notes(fb, notes, r);
    }
}
