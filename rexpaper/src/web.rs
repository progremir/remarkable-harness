//! Browser panes. A rexweb pane (https://github.com/acoyfellow/rexweb) is a
//! terminal block labelled "browser" with a WebKit view drawn over it on the
//! Mac. The tablet has no browser engine, so it shows pictures of the page,
//! which the helper on the Mac takes, and sends taps and typing back.

use std::time::Instant;

use crate::font::Psf2Font;
use crate::render::{self, BLACK, WHITE};
use crate::ui::{self, Rect};

/// The label rexweb gives its panes.
pub const LABEL: &str = "browser";

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

    pub fn draw(&mut self, fb: &mut [u8], font: &Psf2Font, r: Rect) {
        self.drawn = true;
        render::fill_rect(fb, r.x, r.y, r.w, r.h, WHITE);
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
    }
}
