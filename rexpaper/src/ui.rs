//! Small drawing helpers for the parts of the screen that aren't terminal:
//! buttons, boxes and text that has to fit.

use crate::font::Psf2Font;
use crate::render::{self, BLACK, WHITE};

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub fn new(x: usize, y: usize, w: usize, h: usize) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn has(&self, x: i32, y: i32) -> bool {
        x >= self.x as i32
            && y >= self.y as i32
            && x < (self.x + self.w) as i32
            && y < (self.y + self.h) as i32
    }
}

/// Shorten `s` to `max` characters, marking the cut with `~`.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('~');
        out
    }
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let last = lines.last_mut().unwrap();
        if !last.is_empty() && last.chars().count() + 1 + word.chars().count() > width {
            lines.push(word.to_string());
        } else {
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(word);
        }
    }
    lines
}

pub fn outline(fb: &mut [u8], r: Rect, t: usize, c: u16) {
    render::fill_rect(fb, r.x, r.y, r.w, t, c);
    render::fill_rect(fb, r.x, r.y + r.h - t, r.w, t, c);
    render::fill_rect(fb, r.x, r.y, t, r.h, c);
    render::fill_rect(fb, r.x + r.w - t, r.y, t, r.h, c);
}

/// A boxed label with its text centred. `on` draws it white on black.
pub fn button(fb: &mut [u8], font: &Psf2Font, r: Rect, text: &str, on: bool) {
    let (fg, bg) = if on { (WHITE, BLACK) } else { (BLACK, WHITE) };
    render::fill_rect(fb, r.x, r.y, r.w, r.h, bg);
    outline(fb, r, 2, BLACK);
    let text = clip(text, (r.w.saturating_sub(8) / font.width).max(1));
    let tw = text.chars().count() * font.width;
    let x = r.x + r.w.saturating_sub(tw) / 2;
    let y = r.y + r.h.saturating_sub(font.height) / 2;
    render::draw_text(fb, font, x, y, &text, fg, bg);
}

/// Left-aligned text on one line of a box, clipped to its width.
pub fn line(fb: &mut [u8], font: &Psf2Font, r: Rect, text: &str, on: bool) {
    let (fg, bg) = if on { (WHITE, BLACK) } else { (BLACK, WHITE) };
    render::fill_rect(fb, r.x, r.y, r.w, r.h, bg);
    let text = clip(text, (r.w.saturating_sub(16) / font.width).max(1));
    let y = r.y + r.h.saturating_sub(font.height) / 2;
    render::draw_text(fb, font, r.x + 8, y, &text, fg, bg);
}
