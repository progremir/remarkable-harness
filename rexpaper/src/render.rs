//! Pixel-level drawing into the RGB565 shared-memory framebuffer.

use crate::font::Psf2Font;
use crate::palette::Palette;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::Rgb;

pub const SCREEN_W: usize = 1620;
pub const SCREEN_H: usize = 2160;

use std::sync::atomic::{AtomicUsize, Ordering};

// Size of the logical canvas we draw into. It equals the physical screen in
// portrait and is transposed in landscape; see orient.rs.
static CANVAS_W: AtomicUsize = AtomicUsize::new(SCREEN_W);
static CANVAS_H: AtomicUsize = AtomicUsize::new(SCREEN_H);

pub fn set_canvas_size(w: usize, h: usize) {
    CANVAS_W.store(w, Ordering::Relaxed);
    CANVAS_H.store(h, Ordering::Relaxed);
}

#[inline]
pub fn canvas_w() -> usize {
    CANVAS_W.load(Ordering::Relaxed)
}

#[inline]
pub fn canvas_h() -> usize {
    CANVAS_H.load(Ordering::Relaxed)
}

#[inline]
pub fn rgb565(c: Rgb) -> u16 {
    (((c.r as u16) >> 3) << 11) | (((c.g as u16) >> 2) << 5) | ((c.b as u16) >> 3)
}

pub const WHITE: u16 = 0xFFFF;
pub const BLACK: u16 = 0x0000;

#[inline]
fn put_px(fb: &mut [u8], x: usize, y: usize, c: u16) {
    let i = (y * canvas_w() + x) * 2;
    fb[i] = (c & 0xff) as u8;
    fb[i + 1] = (c >> 8) as u8;
}

pub fn fill_rect(fb: &mut [u8], x: usize, y: usize, w: usize, h: usize, c: u16) {
    let lo = (c & 0xff) as u8;
    let hi = (c >> 8) as u8;
    let cw = canvas_w();
    for row in y..(y + h).min(canvas_h()) {
        let start = (row * cw + x) * 2;
        let end = start + w.min(cw - x) * 2;
        let slice = &mut fb[start..end];
        for px in slice.chunks_exact_mut(2) {
            px[0] = lo;
            px[1] = hi;
        }
    }
}

pub fn invert_rect(fb: &mut [u8], x: usize, y: usize, w: usize, h: usize) {
    let cw = canvas_w();
    for row in y..(y + h).min(canvas_h()) {
        let start = (row * cw + x) * 2;
        let end = start + w.min(cw - x) * 2;
        for b in &mut fb[start..end] {
            *b = !*b;
        }
    }
}

/// Draw one glyph cell. `bold` uses classic double-strike.
pub fn draw_glyph(
    fb: &mut [u8],
    font: &Psf2Font,
    x: usize,
    y: usize,
    c: char,
    fg: u16,
    bg: u16,
    bold: bool,
) {
    let glyph = font.glyph(c);
    let bpr = font.bytes_per_row();
    for gy in 0..font.height {
        let row = &glyph[gy * bpr..(gy + 1) * bpr];
        let py = y + gy;
        let mut prev_on = false;
        for gx in 0..font.width {
            let on = row[gx / 8] & (0x80 >> (gx % 8)) != 0;
            let ink = on || (bold && prev_on);
            put_px(fb, x + gx, py, if ink { fg } else { bg });
            prev_on = on;
        }
    }
}

/// Draw a text label (no wrapping).
pub fn draw_text(fb: &mut [u8], font: &Psf2Font, x: usize, y: usize, s: &str, fg: u16, bg: u16) {
    let mut cx = x;
    for c in s.chars() {
        if cx + font.width > canvas_w() {
            break;
        }
        draw_glyph(fb, font, cx, y, c, fg, bg, false);
        cx += font.width;
    }
}

/// Terminal grid geometry on screen.
pub struct TermArea {
    pub x0: usize,
    pub y0: usize,
    pub cols: usize,
    pub rows: usize,
    pub cw: usize,
    pub ch: usize,
    /// First grid row shown. Above zero when the terminal has more rows
    /// than fit on screen: we then show its last rows.
    pub row_off: usize,
}

impl TermArea {
    pub fn cell_rect(&self, col: usize, row: usize) -> (usize, usize, usize, usize) {
        (
            self.x0 + col * self.cw,
            self.y0 + row * self.ch,
            self.cw,
            self.ch,
        )
    }
}

/// Render one damaged viewport line from the grid into the framebuffer.
pub fn draw_line<T: EventListener>(
    fb: &mut [u8],
    font: &Psf2Font,
    palette: &Palette,
    term: &Term<T>,
    area: &TermArea,
    row: usize,
    left: usize,
    right: usize,
    sel: Option<&SelectionRange>,
) {
    let display_offset = term.grid().display_offset();
    let line = Line((row + area.row_off) as i32 - display_offset as i32);
    let colors = term.colors();
    let right = right.min(area.cols - 1);

    for col in left..=right {
        let cell = &term.grid()[line][Column(col)];
        let (px, py, _, _) = area.cell_rect(col, row);
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            let bg = rgb565(palette.resolve(cell.bg, colors));
            fill_rect(fb, px, py, area.cw, area.ch, bg);
            continue;
        }
        let mut fg = palette.resolve(cell.fg, colors);
        let mut bg = palette.resolve(cell.bg, colors);
        let picked = sel.is_some_and(|s| s.contains(Point::new(line, Column(col))));
        if cell.flags.contains(Flags::INVERSE) != picked {
            std::mem::swap(&mut fg, &mut bg);
        }
        let fg565 = rgb565(fg);
        let bg565 = rgb565(bg);
        // A tab leaves '\t' in its first cell: draw control characters blank.
        let glyph_char = if cell.flags.contains(Flags::HIDDEN) || cell.c.is_control() {
            ' '
        } else {
            cell.c
        };
        let bold = cell.flags.intersects(Flags::BOLD | Flags::DIM_BOLD);
        draw_glyph(fb, font, px, py, glyph_char, fg565, bg565, bold);
        if cell.flags.intersects(Flags::ALL_UNDERLINES) {
            fill_rect(fb, px, py + area.ch - 2, area.cw, 2, fg565);
        }
        if cell.flags.contains(Flags::STRIKEOUT) {
            fill_rect(fb, px, py + area.ch / 2, area.cw, 1, fg565);
        }
    }

    // Cursor: block-invert its cell when it sits on this row.
    if term.mode().contains(TermMode::SHOW_CURSOR) && display_offset == 0 {
        let cursor: Point = term.grid().cursor.point;
        let crow = cursor.line.0 as usize;
        let ccol = cursor.column.0;
        if crow == row + area.row_off && ccol >= left && ccol <= right {
            let (px, py, w, h) = area.cell_rect(ccol, row);
            invert_rect(fb, px, py, w, h);
        }
    }
}

/// Fingerprint of how one cell currently looks on screen: glyph, colours,
/// style and whether the cursor sits on it. Lets the caller skip cells that
/// are already drawn correctly, keeping e-ink updates as small as possible.
/// `row` is a grid row (screen row plus `TermArea::row_off`).
pub fn cell_look<T: EventListener>(
    term: &Term<T>,
    palette: &Palette,
    row: usize,
    col: usize,
    sel: Option<&SelectionRange>,
) -> u64 {
    let display_offset = term.grid().display_offset();
    let line = Line(row as i32 - display_offset as i32);
    let colors = term.colors();
    let cell = &term.grid()[line][Column(col)];
    let mut fg = palette.resolve(cell.fg, colors);
    let mut bg = palette.resolve(cell.bg, colors);
    let picked = sel.is_some_and(|s| s.contains(Point::new(line, Column(col))));
    if cell.flags.contains(Flags::INVERSE) != picked {
        std::mem::swap(&mut fg, &mut bg);
    }
    let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
    let c = if spacer || cell.flags.contains(Flags::HIDDEN) || cell.c.is_control() {
        ' '
    } else {
        cell.c
    };
    let style = (cell.flags.intersects(Flags::BOLD | Flags::DIM_BOLD) as u64)
        | (cell.flags.intersects(Flags::ALL_UNDERLINES) as u64) << 1
        | (cell.flags.contains(Flags::STRIKEOUT) as u64) << 2
        | (spacer as u64) << 3;
    let cursor = term.mode().contains(TermMode::SHOW_CURSOR)
        && display_offset == 0
        && term.grid().cursor.point.line.0 as usize == row
        && term.grid().cursor.point.column.0 == col;
    (c as u64)
        | (rgb565(fg) as u64) << 21
        | (rgb565(bg) as u64) << 37
        | style << 53
        | (cursor as u64) << 60
}
