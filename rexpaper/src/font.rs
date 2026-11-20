//! Minimal PSF2 bitmap font parser (format verified against the vendored
//! Spleen .psfu files: 32-byte LE header, MSB-first row bitmaps, UTF-8
//! unicode table terminated per-glyph by 0xFF, 0xFE starting ignorable
//! combining sequences).

use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct Psf2Font {
    pub width: usize,
    pub height: usize,
    charsize: usize,
    bytes_per_row: usize,
    glyphs: Arc<Vec<u8>>,
    map: Arc<HashMap<char, usize>>,
    fallback: usize,
}

impl Psf2Font {
    pub fn parse(data: &'static [u8]) -> Result<Self, String> {
        if data.len() < 32 || data[0..4] != [0x72, 0xB5, 0x4A, 0x86] {
            return Err("not a PSF2 font".into());
        }
        let u32le = |off: usize| -> usize {
            u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize
        };
        let headersize = u32le(8);
        let flags = u32le(12);
        let length = u32le(16);
        let charsize = u32le(20);
        let height = u32le(24);
        let width = u32le(28);
        let bytes_per_row = width.div_ceil(8);
        if charsize != bytes_per_row * height {
            return Err(format!("charsize mismatch: {} != {}", charsize, bytes_per_row * height));
        }
        let bitmaps_end = headersize + length * charsize;
        if data.len() < bitmaps_end {
            return Err("truncated glyph table".into());
        }

        let mut map = HashMap::new();
        if flags & 1 != 0 {
            let mut pos = bitmaps_end;
            let mut glyph = 0usize;
            while pos < data.len() && glyph < length {
                match data[pos] {
                    0xFF => {
                        glyph += 1;
                        pos += 1;
                    }
                    0xFE => {
                        // Combining sequences: skip to this glyph's terminator.
                        while pos < data.len() && data[pos] != 0xFF {
                            pos += 1;
                        }
                    }
                    _ => {
                        // One UTF-8 encoded codepoint mapping to `glyph`.
                        let len = utf8_len(data[pos]);
                        if pos + len <= data.len() {
                            if let Ok(s) = std::str::from_utf8(&data[pos..pos + len]) {
                                if let Some(c) = s.chars().next() {
                                    map.entry(c).or_insert(glyph);
                                }
                            }
                        }
                        pos += len;
                    }
                }
            }
        } else {
            // No unicode table: identity map for latin-1.
            for i in 0..length.min(256) {
                map.insert(char::from_u32(i as u32).unwrap(), i);
            }
        }

        // Spleen lacks many symbols that terminal programs print. Stand a
        // close glyph in for each, and draw the regular families.
        for (chars, alts) in STAND_INS {
            let Some(&glyph) = alts.chars().find_map(|a| map.get(&a)) else {
                continue;
            };
            for c in chars.chars() {
                map.entry(c).or_insert(glyph);
            }
        }
        let mut glyphs = data[headersize..bitmaps_end].to_vec();
        let mut draw = |glyphs: &mut Vec<u8>, boxes: &[(usize, usize, usize, usize)]| -> usize {
            let mut g = vec![0u8; charsize];
            for &(x0, y0, w, h) in boxes {
                for y in y0..(y0 + h).min(height) {
                    for x in x0..(x0 + w).min(width) {
                        g[y * bytes_per_row + x / 8] |= 0x80 >> (x % 8);
                    }
                }
            }
            glyphs.extend_from_slice(&g);
            glyphs.len() / charsize - 1
        };
        // Lower blocks, one to eight eighths high, then left blocks.
        for n in 1..=8usize {
            let (h, w) = (height * n / 8, width * (8 - n).max(1) / 8);
            let low = char::from_u32(0x2580 + n as u32).unwrap();
            if !map.contains_key(&low) {
                let g = draw(&mut glyphs, &[(0, height - h, width, h)]);
                map.insert(low, g);
            }
            let left = char::from_u32(0x2588 + n as u32).unwrap();
            if n < 8 && !map.contains_key(&left) {
                let g = draw(&mut glyphs, &[(0, 0, w, height)]);
                map.insert(left, g);
            }
        }
        // Braille: bit i of the code point is dot i + 1, in two columns of
        // four. Spinners and small charts are made of these.
        let dot = (width / 5).max(2);
        for bits in 0..256u32 {
            let c = char::from_u32(0x2800 + bits).unwrap();
            if map.contains_key(&c) {
                continue;
            }
            let boxes: Vec<_> = (0..8)
                .filter(|i| bits & (1 << i) != 0)
                .map(|i| {
                    let (col, row) = match i {
                        0..=2 => (0, i),
                        3..=5 => (1, i - 3),
                        6 => (0, 3),
                        _ => (1, 3),
                    };
                    let x = width * (1 + 2 * col) / 4 - dot / 2;
                    let y = height * (1 + 2 * row) / 8 - dot / 2;
                    (x, y, dot, dot)
                })
                .collect();
            let g = draw(&mut glyphs, &boxes);
            map.insert(c, g);
        }
        // Anything else unknown is a small hollow box: a "?" would read as
        // text the program wrote.
        let t = (width / 10).max(1);
        let (x0, y0, w, h) = (width / 4, height * 3 / 8, width / 2, height / 4);
        let fallback = draw(
            &mut glyphs,
            &[(x0, y0, w, t), (x0, y0 + h - t, w, t), (x0, y0, t, h), (x0 + w - t, y0, t, h)],
        );
        Ok(Self {
            width,
            height,
            charsize,
            bytes_per_row,
            glyphs: Arc::new(glyphs),
            map: Arc::new(map),
            fallback,
        })
    }

    /// Glyph bitmap: `height` rows of `bytes_per_row` bytes, MSB = leftmost.
    pub fn glyph(&self, c: char) -> &[u8] {
        let idx = *self.map.get(&c).unwrap_or(&self.fallback);
        &self.glyphs[idx * self.charsize..(idx + 1) * self.charsize]
    }

    pub fn bytes_per_row(&self) -> usize {
        self.bytes_per_row
    }
}

/// Symbols the fonts lack, each group with the glyphs to use instead, best
/// first. The last choice of every group is ASCII, so one always exists.
const STAND_INS: &[(&str, &str)] = &[
    ("\u{273b}\u{273d}\u{2722}\u{2733}\u{2734}\u{2735}\u{2736}\u{2737}\u{2738}\u{2739}\u{273a}\u{274b}\u{2731}\u{2732}\u{203b}\u{2055}\u{2605}\u{2606}\u{2217}", "*"),
    ("\u{276f}\u{279c}\u{2794}\u{27a4}\u{276d}", "\u{203a}>"),
    ("\u{276e}\u{276c}", "\u{2039}<"),
    ("\u{23fa}\u{2b24}\u{25c9}\u{26ab}\u{1f534}\u{1f7e2}", "\u{25cf}\u{2022}*"),
    ("\u{25e6}\u{25ef}\u{26aa}\u{25cc}", "\u{25cb}o"),
    ("\u{23bf}\u{2514}\u{2570}\u{2517}", "\u{2514}`"),
    ("\u{256d}\u{250f}", "\u{250c}+"),
    ("\u{256e}\u{2513}", "\u{2510}+"),
    ("\u{256f}\u{251b}", "\u{2518}+"),
    ("\u{2501}\u{254c}\u{2504}\u{2508}\u{2574}\u{2576}", "\u{2500}-"),
    ("\u{2503}\u{254e}\u{2506}\u{250a}\u{2575}\u{2577}", "\u{2502}|"),
    ("\u{23f5}\u{25b6}\u{25ba}\u{25b8}\u{25b9}\u{25b7}\u{23e9}", "\u{25b6}\u{25ba}>"),
    ("\u{23f4}\u{25c0}\u{25c4}\u{25c2}\u{25c3}\u{25c1}", "\u{25c0}\u{25c4}<"),
    ("\u{25b4}\u{25b5}\u{25b3}\u{23f6}", "\u{25b2}^"),
    ("\u{25be}\u{25bf}\u{25bd}\u{23f7}", "\u{25bc}v"),
    ("\u{23f8}", "\u{2016}\""),
    ("\u{23f9}\u{25aa}\u{25fc}\u{25fe}\u{2b1b}", "\u{25a0}#"),
    ("\u{25c7}\u{2666}\u{2b25}", "\u{25c6}*"),
    ("\u{2713}\u{2714}\u{2611}\u{2705}", "\u{221a}v"),
    ("\u{2717}\u{2718}\u{2715}\u{2716}\u{2612}\u{274c}", "\u{d7}x"),
    ("\u{26a0}\u{2757}\u{2755}", "!"),
    ("\u{2192}\u{27f6}\u{21d2}\u{21e2}", "\u{2192}>"),
    ("\u{2190}\u{27f5}\u{21d0}", "\u{2190}<"),
    ("\u{2191}\u{21d1}", "\u{2191}^"),
    ("\u{2193}\u{21d3}", "\u{2193}v"),
    ("\u{23ce}\u{21b5}\u{21a9}", "\u{2190}<"),
    ("\u{2026}\u{22ef}", "\u{2026}~"),
    ("\u{2219}\u{22c5}\u{30fb}", "\u{b7}."),
    ("\u{a0}\u{2009}\u{200a}\u{202f}\u{2003}\u{2002}", " "),
];

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}
