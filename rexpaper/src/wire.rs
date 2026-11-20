//! One WebSocket connection to a Rex server, run on its own thread.
//!
//! The tablet's tailscaled runs in userspace mode, so a connection to the
//! tailnet is a `tailscale nc` child process: we speak TLS and WebSocket
//! over its stdin and stdout. On the Mac (tests) the same code runs over
//! the server's Unix socket, or a plain TLS socket.

use std::io::{self, Read, Write};
use std::os::fd::{IntoRawFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};

use polling::Poller;

pub const TAILSCALE: &str = "/home/root/.vellum/bin/tailscale";

#[derive(Clone, Debug)]
pub enum Target {
    /// The server's local socket.
    Unix(String),
    /// TLS to `host:port`. With `tailscale`, through `tailscale nc`.
    Tls {
        host: String,
        port: u16,
        tailscale: Option<String>,
    },
}

impl Target {
    /// `unix:/path`, `tls://host[:port]` (direct), or `host[:port]`
    /// (through `tailscale nc`). The port defaults to 443.
    pub fn parse(s: &str) -> Result<Target, String> {
        let s = s.trim();
        if let Some(path) = s.strip_prefix("unix:") {
            return Ok(Target::Unix(path.to_string()));
        }
        let (rest, tailscale) = match s.strip_prefix("tls://") {
            Some(rest) => (rest, None),
            None => (
                s.strip_prefix("https://").unwrap_or(s),
                Some(std::env::var("REXPAPER_TAILSCALE").unwrap_or_else(|_| TAILSCALE.into())),
            ),
        };
        let rest = rest.trim_end_matches('/');
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| format!("bad port in {s:?}"))?),
            None => (rest, 443),
        };
        if host.is_empty() {
            return Err("no server address".into());
        }
        Ok(Target::Tls {
            host: host.to_string(),
            port,
            tailscale,
        })
    }

    pub fn host(&self) -> &str {
        match self {
            Target::Unix(_) => "this machine",
            Target::Tls { host, .. } => host,
        }
    }
}

/// A message from the server, or the end of the connection.
pub enum In {
    Text(String),
    Binary(Vec<u8>),
    Closed(String),
}

pub enum Out {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

/// Handle to a connection thread. Dropping it closes the connection.
pub struct Wire {
    pub id: usize,
    tx: mpsc::Sender<Out>,
    wake: RawFd,
}

impl Wire {
    pub fn send(&self, m: Out) {
        let _ = self.tx.send(m);
        unsafe { libc::write(self.wake, b"x".as_ptr() as *const libc::c_void, 1) };
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        self.send(Out::Close);
        unsafe { libc::close(self.wake) };
    }
}

/// Open `route` on `target`. Messages arrive on `sink` tagged with `id`,
/// and `poller` is notified for each one.
pub fn spawn(
    id: usize,
    target: Target,
    route: &str,
    subprotocol: &str,
    sink: mpsc::Sender<(usize, In)>,
    poller: Arc<Poller>,
) -> Wire {
    let (tx, rx) = mpsc::channel();
    let mut pipe = [0 as RawFd; 2];
    unsafe { libc::pipe(pipe.as_mut_ptr()) };
    set_nonblocking(pipe[0]);
    let (route, subprotocol) = (route.to_string(), subprotocol.to_string());
    std::thread::spawn(move || {
        let reason = match run(&target, &route, &subprotocol, &rx, pipe[0], id, &sink, &poller) {
            Ok(reason) => reason,
            Err(e) => e,
        };
        unsafe { libc::close(pipe[0]) };
        let _ = sink.send((id, In::Closed(reason)));
        let _ = poller.notify();
    });
    Wire {
        id,
        tx,
        wake: pipe[1],
    }
}

struct Fd(RawFd);

impl Read for Fd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { libc::read(self.0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

impl Write for Fd {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { libc::write(self.0, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

fn wait(fd: RawFd, events: libc::c_short, timeout_ms: i32) {
    let mut p = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    unsafe { libc::poll(&mut p, 1, timeout_ms) };
}

fn write_all(w: &mut Fd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        match w.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait(w.0, libc::POLLOUT, 1000),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The byte channel under the WebSocket: two descriptors and optional TLS.
struct Chan {
    r: Fd,
    w: Fd,
    tls: Option<rustls::ClientConnection>,
    child: Option<Child>,
}

impl Drop for Chan {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.r.0);
            if self.w.0 != self.r.0 {
                libc::close(self.w.0);
            }
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Chan {
    fn connect(target: &Target) -> Result<Chan, String> {
        let (r, w, child, tls_host) = match target {
            Target::Unix(path) => {
                let s = std::os::unix::net::UnixStream::connect(path)
                    .map_err(|e| format!("{path}: {e}"))?;
                let fd = s.into_raw_fd();
                (fd, fd, None, None)
            }
            Target::Tls {
                host,
                port,
                tailscale: None,
            } => {
                let s = std::net::TcpStream::connect((host.as_str(), *port))
                    .map_err(|e| format!("{host}:{port}: {e}"))?;
                let _ = s.set_nodelay(true);
                let fd = s.into_raw_fd();
                (fd, fd, None, Some(host.clone()))
            }
            Target::Tls {
                host,
                port,
                tailscale: Some(bin),
            } => {
                let mut child = Command::new(bin)
                    .args(["nc", host, &port.to_string()])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| format!("{bin}: {e}"))?;
                let w = child.stdin.take().unwrap().into_raw_fd();
                let r = child.stdout.take().unwrap().into_raw_fd();
                (r, w, Some(child), Some(host.clone()))
            }
        };
        set_nonblocking(r);
        set_nonblocking(w);
        let tls = match tls_host {
            None => None,
            Some(host) => {
                let roots = rustls::RootCertStore {
                    roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                };
                let config = rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                let name = rustls::pki_types::ServerName::try_from(host).map_err(|e| e.to_string())?;
                Some(
                    rustls::ClientConnection::new(Arc::new(config), name)
                        .map_err(|e| e.to_string())?,
                )
            }
        };
        Ok(Chan {
            r: Fd(r),
            w: Fd(w),
            tls,
            child,
        })
    }

    fn flush_tls(&mut self) -> io::Result<()> {
        let Chan { w, tls, .. } = self;
        if let Some(t) = tls {
            while t.wants_write() {
                match t.write_tls(w) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait(w.0, libc::POLLOUT, 1000),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }

    fn send(&mut self, data: &[u8]) -> io::Result<()> {
        match &mut self.tls {
            Some(t) => {
                t.writer().write_all(data)?;
                self.flush_tls()
            }
            None => write_all(&mut self.w, data),
        }
    }

    /// Append whatever has arrived to `out`. `Ok(false)` at end of stream.
    fn recv(&mut self, out: &mut Vec<u8>) -> io::Result<bool> {
        let mut buf = [0u8; 16384];
        let mut open = true;
        let Chan { r, tls, .. } = self;
        match tls {
            None => loop {
                match r.read(&mut buf) {
                    Ok(0) => return Ok(false),
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            },
            Some(t) => {
                loop {
                    match t.read_tls(r) {
                        Ok(0) => {
                            open = false;
                            break;
                        }
                        Ok(_) => {
                            t.process_new_packets().map_err(io::Error::other)?;
                            loop {
                                match t.reader().read(&mut buf) {
                                    Ok(0) => break,
                                    Ok(n) => out.extend_from_slice(&buf[..n]),
                                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                                    Err(e) => return Err(e),
                                }
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
                self.flush_tls()?;
                Ok(open)
            }
        }
    }
}

fn random(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode base64url (or base64), padded or not.
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = random(4);
    let mut out = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => out.push(0x80 | n as u8),
        n if n < 65536 => {
            out.push(0x80 | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(0x80 | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// Take one complete frame off the front of `buf`: (fin, opcode, payload).
fn take_frame(buf: &mut Vec<u8>) -> Option<(bool, u8, Vec<u8>)> {
    if buf.len() < 2 {
        return None;
    }
    let (fin, op) = (buf[0] & 0x80 != 0, buf[0] & 0x0f);
    let masked = buf[1] & 0x80 != 0;
    let (len, mut at) = match buf[1] & 0x7f {
        126 => {
            if buf.len() < 4 {
                return None;
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4)
        }
        127 => {
            if buf.len() < 10 {
                return None;
            }
            (u64::from_be_bytes(buf[2..10].try_into().unwrap()) as usize, 10)
        }
        n => (n as usize, 2),
    };
    let mask = if masked {
        if buf.len() < at + 4 {
            return None;
        }
        at += 4;
        Some([buf[at - 4], buf[at - 3], buf[at - 2], buf[at - 1]])
    } else {
        None
    };
    if buf.len() < at + len {
        return None;
    }
    let mut payload: Vec<u8> = buf[at..at + len].to_vec();
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    buf.drain(..at + len);
    Some((fin, op, payload))
}

const PING_SECS: u64 = 20;
const QUIET_SECS: u64 = 50;

#[allow(clippy::too_many_arguments)]
fn run(
    target: &Target,
    route: &str,
    subprotocol: &str,
    rx: &mpsc::Receiver<Out>,
    wake: RawFd,
    id: usize,
    sink: &mpsc::Sender<(usize, In)>,
    poller: &Poller,
) -> Result<String, String> {
    let io = |e: io::Error| e.to_string();
    let mut chan = Chan::connect(target)?;
    let key = b64_encode(&random(16));
    let request = format!(
        "GET {route} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: {subprotocol}\r\n\r\n",
        match target {
            Target::Unix(_) => "rex",
            Target::Tls { host, .. } => host,
        }
    );
    chan.send(request.as_bytes()).map_err(io)?;

    let mut inbuf: Vec<u8> = Vec::new();
    let mut upgraded = false;
    let mut partial: Option<(u8, Vec<u8>)> = None;
    let started = std::time::Instant::now();
    // Our own keepalive: the server's pings are two minutes apart, too slow
    // to notice a connection that died while the tablet slept.
    let (mut last_in, mut last_ping) = (started, started);
    loop {
        if upgraded {
            if last_in.elapsed().as_secs() >= QUIET_SECS {
                return Err("the connection went quiet".into());
            }
            if last_ping.elapsed().as_secs() >= PING_SECS {
                last_ping = std::time::Instant::now();
                chan.send(&frame(9, b"")).map_err(io)?;
            }
            while let Ok(m) = rx.try_recv() {
                match m {
                    Out::Text(t) => chan.send(&frame(1, t.as_bytes())).map_err(io)?,
                    Out::Binary(b) => chan.send(&frame(2, &b)).map_err(io)?,
                    Out::Close => {
                        let _ = chan.send(&frame(8, &1000u16.to_be_bytes()));
                        return Ok("closed".into());
                    }
                }
            }
        } else if started.elapsed().as_secs() > 20 {
            return Err("the server did not answer".into());
        }

        let mut fds = [
            libc::pollfd {
                fd: chan.r.0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        unsafe { libc::poll(fds.as_mut_ptr(), 2, 1000) };
        if fds[1].revents != 0 {
            let mut scratch = [0u8; 64];
            while unsafe { libc::read(wake, scratch.as_mut_ptr() as *mut libc::c_void, 64) } > 0 {}
        }
        let open = if fds[0].revents != 0 {
            last_in = std::time::Instant::now();
            chan.recv(&mut inbuf).map_err(io)?
        } else {
            true
        };

        if !upgraded {
            if let Some(end) = inbuf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&inbuf[..end]).to_string();
                let status = head.lines().next().unwrap_or("").to_string();
                if !status.contains(" 101") {
                    return Err(format!("the server refused the connection ({status})"));
                }
                inbuf.drain(..end + 4);
                upgraded = true;
            }
        }
        if upgraded {
            let mut got = false;
            while let Some((fin, op, payload)) = take_frame(&mut inbuf) {
                match op {
                    9 => chan.send(&frame(10, &payload)).map_err(io)?,
                    10 => {}
                    8 => {
                        let reason = String::from_utf8_lossy(payload.get(2..).unwrap_or(&[])).to_string();
                        let code = payload
                            .get(..2)
                            .map(|c| u16::from_be_bytes([c[0], c[1]]))
                            .unwrap_or(1005);
                        return Ok(format!("{code} {reason}").trim().to_string());
                    }
                    _ => {
                        let (kind, mut data) = match (op, partial.take()) {
                            (0, Some((kind, mut data))) => {
                                data.extend_from_slice(&payload);
                                (kind, data)
                            }
                            (op, _) => (op, payload),
                        };
                        if !fin {
                            partial = Some((kind, std::mem::take(&mut data)));
                            continue;
                        }
                        let msg = if kind == 1 {
                            In::Text(String::from_utf8_lossy(&data).to_string())
                        } else {
                            In::Binary(data)
                        };
                        let _ = sink.send((id, msg));
                        got = true;
                    }
                }
            }
            if got {
                let _ = poller.notify();
            }
        }
        if !open {
            return Err("the connection dropped".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64() {
        assert_eq!(b64_encode(b"hello!!"), "aGVsbG8hIQ==");
        assert_eq!(b64url_decode("aGVsbG8hIQ").unwrap(), b"hello!!");
        assert_eq!(b64url_decode("_-8").unwrap(), vec![0xff, 0xef]);
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = frame(1, b"hi");
        buf.extend(frame(2, &vec![7u8; 70000]));
        let (fin, op, p) = take_frame(&mut buf).unwrap();
        assert!(fin && op == 1 && p == b"hi");
        let (_, op, p) = take_frame(&mut buf).unwrap();
        assert!(op == 2 && p.len() == 70000 && p[69999] == 7);
        assert!(buf.is_empty());
    }

    #[test]
    fn targets() {
        assert!(matches!(Target::parse("unix:/tmp/x").unwrap(), Target::Unix(p) if p == "/tmp/x"));
        assert!(matches!(Target::parse("tls://a.ts.net").unwrap(),
            Target::Tls { host, port: 443, tailscale: None } if host == "a.ts.net"));
        assert!(matches!(Target::parse("https://a.ts.net:7297/").unwrap(),
            Target::Tls { host, port: 7297, tailscale: Some(_) } if host == "a.ts.net"));
    }
}
