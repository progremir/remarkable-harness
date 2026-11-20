//! Physical keyboards read straight from evdev (/dev/input/event*).
//!
//! AppLoad does not forward hardware key events to qtfb apps, so Bluetooth
//! or USB keyboards (e.g. one paired by MoveWriter) are read here. While the
//! terminal runs, each keyboard is grabbed (EVIOCGRAB) so keystrokes don't
//! also reach xochitl underneath; the kernel drops the grab when we exit.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::keys::KeyAction;

const EV_KEY: u16 = 0x01;
const EVENT_SIZE: usize = 24; // struct input_event on aarch64

const KEY_A: usize = 30;
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
const KEY_CAPSLOCK: u16 = 58;

const fn ioc(dir: u64, ty: u8, nr: u8, size: u64) -> u64 {
    (dir << 30) | (size << 16) | ((ty as u64) << 8) | nr as u64
}
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;
const EVIOCGRAB: u64 = ioc(IOC_WRITE, b'E', 0x90, 4);
const KEY_BITS_LEN: usize = 96; // covers KEY_MAX (0x2ff) bits
const EVIOCGBIT_KEY: u64 = ioc(IOC_READ, b'E', 0x20 + EV_KEY as u8, KEY_BITS_LEN as u64);

struct Device {
    path: PathBuf,
    file: File,
}

#[derive(Default)]
struct Mods {
    shift: bool,
    ctrl: bool,
    alt: bool,
    caps: bool,
}

pub struct Keyboards {
    devices: Vec<Device>,
    mods: Mods,
    grab: bool,
    /// Devices already looked at and found not to be keyboards. Opening
    /// them again on every scan stalled the program for a tenth of a
    /// second each time, which showed as lag in pen strokes.
    others: Vec<std::path::PathBuf>,
}

/// A key press decoded to a terminal action plus modifier state.
pub struct Press {
    pub action: KeyAction,
    pub ctrl: bool,
    pub alt: bool,
}

impl Keyboards {
    pub fn new(grab: bool) -> Self {
        Keyboards {
            devices: Vec::new(),
            mods: Mods::default(),
            grab,
            others: Vec::new(),
        }
    }

    /// Open any keyboards that appeared since the last scan. Returns the fds
    /// of newly opened devices so the caller can add them to its poller.
    pub fn scan(&mut self) -> Vec<RawFd> {
        let mut added = Vec::new();
        let Ok(entries) = fs::read_dir("/dev/input") else {
            return added;
        };
        let present: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
        // A device that went away may come back as something else.
        self.others.retain(|p| present.contains(p));
        for path in present {
            let is_event = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"));
            if !is_event || self.others.contains(&path) || self.devices.iter().any(|d| d.path == path) {
                continue;
            }
            let Ok(file) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)
            else {
                continue;
            };
            if !has_letter_keys(file.as_raw_fd()) {
                self.others.push(path); // power button, touch panel, pen, ...
                continue;
            }
            if self.grab {
                unsafe { libc::ioctl(file.as_raw_fd(), EVIOCGRAB as _, 1 as libc::c_int) };
            }
            eprintln!("rexpaper: keyboard {}", path.display());
            added.push(file.as_raw_fd());
            self.devices.push(Device { path, file });
        }
        added
    }

    /// Drain pending events from the device behind `fd`. Returns the decoded
    /// key presses, and whether the device went away (the caller should stop
    /// polling it, then call `remove`).
    pub fn read(&mut self, fd: RawFd) -> (Vec<Press>, bool) {
        let mut out = Vec::new();
        let Some(idx) = self.devices.iter().position(|d| d.file.as_raw_fd() == fd) else {
            return (out, true);
        };
        let mut buf = [0u8; EVENT_SIZE * 64];
        loop {
            match self.devices[idx].file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for ev in buf[..n].chunks_exact(EVENT_SIZE) {
                        let ty = u16::from_ne_bytes([ev[16], ev[17]]);
                        let code = u16::from_ne_bytes([ev[18], ev[19]]);
                        let value = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
                        if ty == EV_KEY {
                            if let Some(p) = self.key(code, value) {
                                out.push(p);
                            }
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    // ENODEV: keyboard disconnected.
                    return (out, true);
                }
            }
        }
        (out, false)
    }

    /// Close a device that `read` reported gone, forgetting held modifiers.
    pub fn remove(&mut self, fd: RawFd) {
        if let Some(idx) = self.devices.iter().position(|d| d.file.as_raw_fd() == fd) {
            eprintln!(
                "rexpaper: keyboard {} gone",
                self.devices[idx].path.display()
            );
            self.devices.remove(idx);
            self.mods = Mods::default();
        }
    }

    /// value: 1 = press, 2 = autorepeat, 0 = release.
    fn key(&mut self, code: u16, value: i32) -> Option<Press> {
        let down = value != 0;
        match code {
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => self.mods.shift = down,
            KEY_LEFTCTRL | KEY_RIGHTCTRL => self.mods.ctrl = down,
            KEY_LEFTALT | KEY_RIGHTALT => self.mods.alt = down,
            KEY_CAPSLOCK if value == 1 => self.mods.caps = !self.mods.caps,
            _ if down => {
                let action = action_for(code, &self.mods)?;
                return Some(Press {
                    action,
                    ctrl: self.mods.ctrl,
                    alt: self.mods.alt,
                });
            }
            _ => {}
        }
        None
    }
}

fn has_letter_keys(fd: RawFd) -> bool {
    let mut bits = [0u8; KEY_BITS_LEN];
    let n = unsafe { libc::ioctl(fd, EVIOCGBIT_KEY as _, bits.as_mut_ptr()) };
    n > 0 && bits[KEY_A / 8] & (1 << (KEY_A % 8)) != 0
}

/// US layout. Letters honour caps lock; everything else only shift.
fn action_for(code: u16, m: &Mods) -> Option<KeyAction> {
    use KeyAction::*;
    let ch = |lower: char, upper: char| Some(Char(if m.shift { upper } else { lower }));
    let letter = |c: char| {
        let upper = m.shift != m.caps;
        Some(Char(if upper { c.to_ascii_uppercase() } else { c }))
    };
    match code {
        1 => Some(Esc),
        2 => ch('1', '!'),
        3 => ch('2', '@'),
        4 => ch('3', '#'),
        5 => ch('4', '$'),
        6 => ch('5', '%'),
        7 => ch('6', '^'),
        8 => ch('7', '&'),
        9 => ch('8', '*'),
        10 => ch('9', '('),
        11 => ch('0', ')'),
        12 => ch('-', '_'),
        13 => ch('=', '+'),
        14 => Some(Backspace),
        15 => Some(Tab),
        16 => letter('q'),
        17 => letter('w'),
        18 => letter('e'),
        19 => letter('r'),
        20 => letter('t'),
        21 => letter('y'),
        22 => letter('u'),
        23 => letter('i'),
        24 => letter('o'),
        25 => letter('p'),
        26 => ch('[', '{'),
        27 => ch(']', '}'),
        28 | 96 => Some(Enter),
        30 => letter('a'),
        31 => letter('s'),
        32 => letter('d'),
        33 => letter('f'),
        34 => letter('g'),
        35 => letter('h'),
        36 => letter('j'),
        37 => letter('k'),
        38 => letter('l'),
        39 => ch(';', ':'),
        40 => ch('\'', '"'),
        41 => ch('`', '~'),
        43 => ch('\\', '|'),
        44 => letter('z'),
        45 => letter('x'),
        46 => letter('c'),
        47 => letter('v'),
        48 => letter('b'),
        49 => letter('n'),
        50 => letter('m'),
        51 => ch(',', '<'),
        52 => ch('.', '>'),
        53 => ch('/', '?'),
        57 => Some(Char(' ')),
        86 => ch('`', '~'), // ISO key next to left shift on non-US boards
        102 => Some(Home),
        103 => Some(Up),
        104 => Some(PgUp),
        105 => Some(Left),
        106 => Some(Right),
        107 => Some(End),
        108 => Some(Down),
        109 => Some(PgDn),
        111 => Some(Delete),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_caps_and_modifiers() {
        let mut k = Keyboards::new(false);
        let c = |p: Option<Press>| match p.map(|p| p.action) {
            Some(KeyAction::Char(c)) => Some(c),
            _ => None,
        };
        assert_eq!(c(k.key(30, 1)), Some('a'));
        k.key(KEY_LEFTSHIFT, 1);
        assert_eq!(c(k.key(30, 1)), Some('A'));
        assert_eq!(c(k.key(2, 1)), Some('!'));
        k.key(KEY_LEFTSHIFT, 0);
        k.key(KEY_CAPSLOCK, 1);
        k.key(KEY_CAPSLOCK, 0);
        assert_eq!(c(k.key(30, 1)), Some('A'));
        assert_eq!(c(k.key(2, 1)), Some('1'));
        k.key(KEY_LEFTCTRL, 1);
        let p = k.key(46, 2).unwrap(); // autorepeat counts as a press
        assert!(p.ctrl && !p.alt);
        assert!(k.key(30, 0).is_none()); // releases produce nothing
    }
}
