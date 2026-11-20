//! Where pixels go: the tablet's qtfb framebuffer, or a file when testing
//! on the Mac (`REXPAPER_HEADLESS=frame.raw`, readable by tools/fb2png.py).

use std::io;
use std::os::fd::RawFd;

use crate::qtfb::{self, Drained};
use crate::render::{SCREEN_H, SCREEN_W};

pub enum Screen {
    Qtfb(qtfb::QtfbClient),
    Headless { fb: Vec<u8>, path: String },
}

impl Screen {
    pub fn open() -> io::Result<Screen> {
        if let Ok(path) = std::env::var("REXPAPER_HEADLESS") {
            return Ok(Screen::Headless {
                fb: vec![0xff; SCREEN_W * SCREEN_H * 2],
                path,
            });
        }
        let key: i32 = std::env::var("QTFB_KEY")
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| io::Error::other("QTFB_KEY not set (start it from AppLoad)"))?;
        let client = qtfb::QtfbClient::connect(key, qtfb::FBFMT_RMPP_RGB565, SCREEN_W, SCREEN_H, 2)?;
        // Low-latency waveform for typing.
        let _ = client.set_refresh_mode(qtfb::REFRESH_MODE_FAST);
        Ok(Screen::Qtfb(client))
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        match self {
            Screen::Qtfb(c) => Some(c.raw_fd()),
            Screen::Headless { .. } => None,
        }
    }

    pub fn framebuffer(&mut self) -> &mut [u8] {
        match self {
            Screen::Qtfb(c) => c.framebuffer(),
            Screen::Headless { fb, .. } => fb,
        }
    }

    pub fn update_all(&self) {
        if let Screen::Qtfb(c) = self {
            let _ = c.update_all();
        }
    }

    pub fn update_partial(&self, x: i32, y: i32, w: i32, h: i32) {
        if let Screen::Qtfb(c) = self {
            let _ = c.update_partial(x, y, w, h);
        }
    }

    /// A slow refresh that leaves no ghosts, such as of erased ink.
    pub fn deghost(&self) {
        if let Screen::Qtfb(c) = self {
            let _ = c.request_full_refresh();
        }
    }

    /// Choose how the e-ink is driven: see the `MODE_` constants.
    pub fn set_mode(&self, mode: i32) {
        if let Screen::Qtfb(c) = self {
            let _ = c.set_refresh_mode(mode);
        }
    }

    pub fn drain_events(&self) -> io::Result<Drained> {
        match self {
            Screen::Qtfb(c) => c.drain_events(),
            Screen::Headless { .. } => Ok(Drained::default()),
        }
    }

    /// Close the window. Headless: write the last frame to the file.
    pub fn finish(&self) {
        match self {
            Screen::Qtfb(c) => c.terminate(),
            Screen::Headless { fb, path } => {
                if let Err(e) = std::fs::write(path, fb) {
                    eprintln!("rexpaper: {path}: {e}");
                }
            }
        }
    }
}
