//! Pixel mapping between the rotated logical canvas and the physical
//! framebuffer. The orientation itself comes from AppLoad, which follows
//! the tablet's own rotation (see `supportsRotation` in the manifest).

/// Clockwise quarter turns applied to content. 0 = native portrait.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Rot(pub u8);

impl Rot {
    pub fn is_landscape(self) -> bool {
        self.0 % 2 == 1
    }

    /// Logical canvas size for a physical `pw` x `ph` screen.
    pub fn logical_size(self, pw: usize, ph: usize) -> (usize, usize) {
        if self.is_landscape() {
            (ph, pw)
        } else {
            (pw, ph)
        }
    }

    /// Logical (lx, ly) -> physical (px, py).
    #[inline]
    pub fn to_phys(self, lx: usize, ly: usize, pw: usize, ph: usize) -> (usize, usize) {
        match self.0 {
            1 => (pw - 1 - ly, lx),
            2 => (pw - 1 - lx, ph - 1 - ly),
            3 => (ly, ph - 1 - lx),
            _ => (lx, ly),
        }
    }

    /// Physical (px, py) -> logical (lx, ly). Inputs are clamped on-screen.
    pub fn to_logical(self, px: i32, py: i32, pw: usize, ph: usize) -> (i32, i32) {
        let px = px.clamp(0, pw as i32 - 1);
        let py = py.clamp(0, ph as i32 - 1);
        let (pw, ph) = (pw as i32, ph as i32);
        match self.0 {
            1 => (py, pw - 1 - px),
            2 => (pw - 1 - px, ph - 1 - py),
            3 => (ph - 1 - py, px),
            _ => (px, py),
        }
    }
}

/// Copy a logical rect from `canvas` (logical width `lw`) into the physical
/// framebuffer `fb` (`pw` x `ph`), rotated. Returns the physical rect.
pub fn blit(
    canvas: &[u8],
    lw: usize,
    fb: &mut [u8],
    pw: usize,
    ph: usize,
    rot: Rot,
    (x, y, w, h): (usize, usize, usize, usize),
) -> (usize, usize, usize, usize) {
    if rot.0 == 0 {
        for row in y..y + h {
            let s = (row * lw + x) * 2;
            fb[s..s + w * 2].copy_from_slice(&canvas[s..s + w * 2]);
        }
        return (x, y, w, h);
    }
    for ly in y..y + h {
        for lx in x..x + w {
            let s = (ly * lw + lx) * 2;
            let (px, py) = rot.to_phys(lx, ly, pw, ph);
            let d = (py * pw + px) * 2;
            fb[d] = canvas[s];
            fb[d + 1] = canvas[s + 1];
        }
    }
    let (ax, ay) = rot.to_phys(x, y, pw, ph);
    let (bx, by) = rot.to_phys(x + w - 1, y + h - 1, pw, ph);
    let (x0, y0) = (ax.min(bx), ay.min(by));
    (x0, y0, ax.max(bx) - x0 + 1, ay.max(by) - y0 + 1)
}

impl Rot {
    /// Map AppLoad's framebuffer rotation (how it turns our image on screen
    /// to follow the tablet) to the pre-rotation we apply so text reads
    /// upright. `swap` flips the two landscape directions.
    pub fn from_appload(r: i32, swap: bool) -> Rot {
        match (r, swap) {
            (1, false) | (2, true) => Rot(1),
            (2, false) | (1, true) => Rot(3),
            (3, _) => Rot(2),
            _ => Rot(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let (pw, ph) = (1620, 2160);
        for r in 0..4 {
            let rot = Rot(r);
            let (lw, lh) = rot.logical_size(pw, ph);
            for &(lx, ly) in &[
                (0, 0),
                (lw - 1, 0),
                (0, lh - 1),
                (lw - 1, lh - 1),
                (123, 456),
            ] {
                let (px, py) = rot.to_phys(lx, ly, pw, ph);
                assert!(px < pw && py < ph);
                assert_eq!(
                    rot.to_logical(px as i32, py as i32, pw, ph),
                    (lx as i32, ly as i32)
                );
            }
        }
    }

    #[test]
    fn blit_rect_matches_pixels() {
        let (pw, ph) = (8, 12);
        for r in 0..4 {
            let rot = Rot(r);
            let (lw, lh) = rot.logical_size(pw, ph);
            let mut canvas = vec![0u8; lw * lh * 2];
            let mut fb = vec![0u8; pw * ph * 2];
            let rect = (1, 2, 3, 4);
            for ly in 2..6 {
                for lx in 1..4 {
                    canvas[(ly * lw + lx) * 2] = 7;
                }
            }
            let (x, y, w, h) = blit(&canvas, lw, &mut fb, pw, ph, rot, rect);
            for py in 0..ph {
                for px in 0..pw {
                    let inside = px >= x && px < x + w && py >= y && py < y + h;
                    assert_eq!(fb[(py * pw + px) * 2] == 7, inside, "rot {r} at {px},{py}");
                }
            }
        }
    }
}
