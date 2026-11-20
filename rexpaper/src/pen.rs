//! The pen digitizer, read straight from the kernel. AppLoad also forwards
//! pen events, but thinned out and late, which makes handwriting laggy and
//! angular; here every sample arrives (several hundred a second).

use std::os::fd::RawFd;

use crate::render::{SCREEN_H, SCREEN_W};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PenEv {
    /// Screen pixels, before rotation. True for the eraser end.
    Down(i32, i32, bool),
    Move(i32, i32),
    Up,
    /// The pen came within reach of the screen, or left it.
    Near(bool),
}

pub struct Pen {
    fd: RawFd,
    raw: (i32, i32),
    touching: bool,
    was_touching: bool,
    rubber: bool,
    near: bool,
    was_near: bool,
    last: (i32, i32),
}

/// Range of the digitizer's ABS_X / ABS_Y; it maps linearly onto the screen.
const PEN_MAX: (i32, i32) = (11180, 15340);

impl Pen {
    pub fn open() -> Option<Pen> {
        for e in std::fs::read_dir("/sys/class/input").ok()?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("event") {
                continue;
            }
            let dev = std::fs::read_to_string(e.path().join("device/name")).unwrap_or_default();
            if !dev.to_lowercase().contains("marker") {
                continue;
            }
            let path = std::ffi::CString::new(format!("/dev/input/{name}")).ok()?;
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
            if fd >= 0 {
                return Some(Pen { fd, raw: (0, 0), touching: false, was_touching: false, rubber: false, near: false, was_near: false, last: (-1, -1) });
            }
        }
        None
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Everything the pen did since the last call.
    pub fn read(&mut self) -> Vec<PenEv> {
        let mut out = Vec::new();
        let mut buf = [0u8; 24 * 64];
        let (pw, ph) = (SCREEN_W as i32, SCREEN_H as i32);
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
                    (1, 0x141) => {
                        // BTN_TOOL_RUBBER
                        self.rubber = value != 0;
                        self.near = value != 0;
                    }
                    (1, 0x140) => self.near = value != 0, // BTN_TOOL_PEN
                    (0, 0) => {
                        if self.near != self.was_near {
                            self.was_near = self.near;
                            out.push(PenEv::Near(self.near));
                        }
                        let p = (self.raw.0 * (pw - 1) / PEN_MAX.0, self.raw.1 * (ph - 1) / PEN_MAX.1);
                        match (self.was_touching, self.touching) {
                            (false, true) => out.push(PenEv::Down(p.0, p.1, self.rubber)),
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
