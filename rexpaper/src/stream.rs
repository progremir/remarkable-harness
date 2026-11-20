//! Reads a Rex terminal data stream (`rex.data.terminal.v1`).
//!
//! The stream opens with a snapshot in Ghostty's binary format, which only a
//! Ghostty-based emulator can load. We skip it: the caller gets the starting
//! screen from `com.superlogical.terminal.format` instead. The snapshot ends
//! at Rex's first own message, a `resize`.
//!
//! After that the stream is the program's output, with Rex's messages mixed
//! in as `ESC _ rex1;<nonce>;<json> ESC \`. The nonce comes from the data
//! handshake, so a look-alike that a program printed never matches.

use serde_json::Value;

pub enum Item {
    /// Program output, for the terminal emulator.
    Bytes(Vec<u8>),
    /// One of Rex's messages: `resize`, `theme`, `child_exited`.
    Msg(Value),
}

pub struct StreamFilter {
    prefix: Vec<u8>,
    buf: Vec<u8>,
    in_snapshot: bool,
}

/// Longest message we wait for before giving up on its terminator.
const MAX_MSG: usize = 1 << 20;

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl StreamFilter {
    pub fn new(nonce: u64) -> Self {
        Self {
            prefix: format!("\x1b_rex1;{nonce};").into_bytes(),
            buf: Vec::new(),
            in_snapshot: true,
        }
    }

    /// How many trailing bytes of `buf` could be the start of the prefix.
    fn partial_tail(&self) -> usize {
        let max = self.prefix.len().min(self.buf.len());
        (1..=max)
            .rev()
            .find(|&n| self.buf[self.buf.len() - n..] == self.prefix[..n])
            .unwrap_or(0)
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<Item> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            match find(&self.buf, &self.prefix) {
                Some(at) => {
                    let body = at + self.prefix.len();
                    let Some(len) = find(&self.buf[body..], b"\x1b\\") else {
                        if self.buf.len() - at > MAX_MSG {
                            // Never terminated: drop the marker and move on.
                            self.buf.drain(..body);
                            continue;
                        }
                        // Wait for the rest, but hand over what came before.
                        if at > 0 && !self.in_snapshot {
                            out.push(Item::Bytes(self.buf.drain(..at).collect()));
                        } else {
                            self.buf.drain(..at);
                        }
                        return out;
                    };
                    if at > 0 && !self.in_snapshot {
                        out.push(Item::Bytes(self.buf[..at].to_vec()));
                    }
                    self.in_snapshot = false;
                    if let Ok(v) = serde_json::from_slice(&self.buf[body..body + len]) {
                        out.push(Item::Msg(v));
                    }
                    self.buf.drain(..body + len + 2);
                }
                None => {
                    let keep = self.partial_tail();
                    let ready = self.buf.len() - keep;
                    if ready > 0 {
                        let bytes: Vec<u8> = self.buf.drain(..ready).collect();
                        if !self.in_snapshot {
                            out.push(Item::Bytes(bytes));
                        }
                    }
                    return out;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Value>) {
        let mut f = StreamFilter::new(42);
        let (mut bytes, mut msgs) = (Vec::new(), Vec::new());
        for c in chunks {
            for item in f.feed(c) {
                match item {
                    Item::Bytes(b) => bytes.extend(b),
                    Item::Msg(m) => msgs.push(m),
                }
            }
        }
        (bytes, msgs)
    }

    const STREAM: &[u8] = b"GHOSTSNP\x01\x00binary\x1b_rex1;42;{\"type\":\"resize\",\"cols\":80,\"rows\":24}\x1b\\\
        hello \x1b[1mworld\x1b_rex1;42;{\"type\":\"child_exited\",\"exit_code\":0}\x1b\\tail";

    #[test]
    fn whole() {
        let (bytes, msgs) = run(&[STREAM]);
        assert_eq!(bytes, b"hello \x1b[1mworldtail");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["cols"], 80);
        assert_eq!(msgs[1]["type"], "child_exited");
    }

    #[test]
    fn every_split_point() {
        let whole = run(&[STREAM]);
        for i in 1..STREAM.len() {
            let got = run(&[&STREAM[..i], &STREAM[i..]]);
            assert_eq!(got, whole, "split at {i}");
        }
        let bytes: Vec<&[u8]> = STREAM.chunks(1).collect();
        assert_eq!(run(&bytes), whole);
    }

    #[test]
    fn foreign_nonce_is_program_output() {
        let s = b"snap\x1b_rex1;42;{\"type\":\"resize\"}\x1b\\a\x1b_rex1;7;{\"type\":\"resize\"}\x1b\\b";
        let (bytes, msgs) = run(&[s]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(bytes, b"a\x1b_rex1;7;{\"type\":\"resize\"}\x1b\\b");
    }
}
