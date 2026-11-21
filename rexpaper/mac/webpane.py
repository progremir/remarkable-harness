#!/usr/bin/env python3
"""A browser inside a Rex terminal pane.

RexWeb (https://github.com/acoyfellow/rexweb, with rexweb.patch applied)
keeps the page in a WebKit view that is never put on screen. This program
runs in the pane, asks RexWeb for pictures of the page and draws them in the
terminal with the kitty image protocol, and sends the mouse and the keyboard
back. Because the page is drawn by the terminal itself, the pane behaves
like any other: nothing floats over Rex.

    webpane.py [url]

The first line is the bar: the address, then back, forward and reload.
Click the address, or press Ctrl-L, to type a new one. Ctrl-R reloads,
Alt-Left and Alt-Right go back and forward.
"""

import base64
import fcntl
import hashlib
import http.client
import json
import os
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time
import tty

sys.path.insert(0, os.path.dirname(os.path.realpath(__file__)))
# Clicks, text and keys are done inside the page, as for the tablet: WebKit
# takes real mouse and key events only in a window that is on screen.
from helper import EDITABLE, JS_KEY, JS_TAP, JS_TEXT  # noqa: E402

HOME = os.path.expanduser("~/.rexweb")
BLOCK = os.environ.get("REX_BLOCK", "")
SESSION = os.environ.get("REX_SESSION", "")
BUTTONS = " ←  →  ⟳ "  # back, forward, reload: 3 cells each
# Scroll what is under the pointer, or the page if nothing there scrolls.
JS_SCROLL = """(function(x, y, dy) {
  let e = document.elementFromPoint(x, y);
  while (e && e !== document.documentElement && e !== document.body) {
    const s = getComputedStyle(e);
    if (/(auto|scroll)/.test(s.overflowY) && e.scrollHeight > e.clientHeight + 1) { e.scrollBy(0, dy); return; }
    e = e.parentElement;
  }
  window.scrollBy(0, dy);
})(%f, %f, %f)"""
JS_STATE = "({w: innerWidth, h: innerHeight})"
KEYS = {
    "\x1b[A": "up", "\x1b[B": "down", "\x1b[C": "right", "\x1b[D": "left",
    "\x1bOA": "up", "\x1bOB": "down", "\x1bOC": "right", "\x1bOD": "left",
    "\x1b[5~": "pageup", "\x1b[6~": "pagedown", "\x1b[H": "home", "\x1b[F": "end",
    "\x1b[1~": "home", "\x1b[4~": "end", "\x1b[3~": "delete",
}
BACK = ("\x1b[1;3D", "\x1bb", "\x1b\x1b[D")
FORWARD = ("\x1b[1;3C", "\x1bf", "\x1b\x1b[C")


def port():
    try:
        with open(os.path.join(HOME, "config.json")) as f:
            return int(os.environ.get("REXWEB_PORT") or json.load(f).get("port") or 18802)
    except (OSError, ValueError):
        return int(os.environ.get("REXWEB_PORT") or 18802)


class RexWeb:
    """RexWeb's local API, over one connection that is reopened when lost."""

    def __init__(self):
        self.conn = None

    def call(self, route, body=None, timeout=10):
        body = dict(body or {}, id=BLOCK)
        try:
            with open(os.path.join(HOME, "token")) as f:
                token = f.read().strip()
        except OSError:
            token = ""
        for attempt in (0, 1):
            try:
                if self.conn is None:
                    self.conn = http.client.HTTPConnection("127.0.0.1", port(), timeout=timeout)
                self.conn.request("POST", route, json.dumps(body),
                                  {"content-type": "application/json", "x-rexweb-token": token})
                return json.loads(self.conn.getresponse().read() or b"{}")
            except (OSError, ValueError, http.client.HTTPException):
                self.conn = None
                if attempt:
                    return {"error": "RexWeb isn't answering"}


class Pane:
    def __init__(self, url):
        self.web = RexWeb()
        self.url = url
        self.shown_url = None
        self.title = ""
        self.editing = None  # the address being typed, or None
        self.png = os.path.join(HOME, "pane-%s.png" % re.sub(r"\W", "", BLOCK)[-12:])
        self.last = None     # hash of the picture on screen
        self.geom = None     # (rows, cols, xpixel, ypixel)
        self.scale = 2.0     # the terminal's pixels to a point (a CSS pixel)
        self.image = None    # where the picture is: (cols, rows, css w, css h)
        self.busy_until = 0  # look often until then: something just happened
        self.resized = True
        self.down = None     # the cell the mouse button went down on
        self.lost = 0

    # ---------------------------------------------------------- terminal

    def out(self, data):
        if isinstance(data, str):
            data = data.encode()
        while data:
            try:
                n = os.write(1, data)
                data = data[n:]
            except BlockingIOError:
                select.select([], [1], [], 1)

    def size(self):
        rows, cols, xp, yp = struct.unpack("HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))
        if not xp or not yp:
            xp, yp = cols * 8, rows * 16
        return rows, cols, xp, yp

    def draw_bar(self):
        rows, cols, _, _ = self.geom
        if self.editing is not None:
            text = "go to: " + self.editing
            line = text[-(cols - 1):]
            self.out("\x1b[H\x1b[0;7m" + line.ljust(cols) + "\x1b[0m\x1b[1;%dH\x1b[?25h" % (len(line) + 1))
            return
        # "rexweb · " comes first: the reMarkable client knows a browser
        # pane by it.
        room = max(8, cols - len(BUTTONS))
        text = ("rexweb · " + (self.url or "blank"))[:room]
        self.out("\x1b[?25l\x1b[H\x1b[0;2m" + text.ljust(room) + "\x1b[0m" + BUTTONS[:cols - room])

    def draw_image(self, data, pw, ph):
        """Put the page's picture under the bar, as large as fits."""
        rows, cols, xp, yp = self.geom
        cw, ch = xp / cols, yp / rows
        area_w, area_h = cols * cw, (rows - 1) * ch
        k = min(area_w / pw, area_h / ph)
        c = max(1, min(cols, round(pw * k / cw)))
        r = max(1, min(rows - 1, round(ph * k / ch)))
        if self.image is None or self.image[:2] != (c, r):
            # A picture of another shape: clear what the old one covered.
            self.out("\x1b_Ga=d,d=A,q=2\x1b\\\x1b[2;1H\x1b[J")
        b64 = base64.b64encode(data)
        head = "a=T,f=100,i=1,p=1,q=2,C=1,c=%d,r=%d," % (c, r)
        parts = ["\x1b[2;1H"]
        for i in range(0, len(b64), 4096):
            more = 1 if i + 4096 < len(b64) else 0
            parts.append("\x1b_G%sm=%d;%s\x1b\\" % (head if i == 0 else "", more, b64[i:i + 4096].decode()))
        self.out("".join(parts))
        return c, r

    # -------------------------------------------------------------- page

    def register(self):
        got = self.web.call("/register", {"session": SESSION, "url": self.url or "", "inline": True})
        if got.get("error") == "RexWeb isn't answering":
            for app in (os.environ.get("REXWEB_APP"), os.path.expanduser("~/Applications/RexWeb.app"), "/Applications/RexWeb.app"):
                if app and os.path.isdir(app):
                    subprocess.run(["open", "-g", app])
                    break
            for _ in range(50):
                time.sleep(0.1)
                if not self.web.call("/health", timeout=1).get("error"):
                    break
            got = self.web.call("/register", {"session": SESSION, "url": self.url or "", "inline": True})
        self.resized = True
        return got

    def refresh(self):
        """Ask for the page's picture and draw it if it changed."""
        if self.resized or self.geom is None:
            self.geom = self.size()
            rows, cols, xp, yp = self.geom
            # The terminal gives its size in screen pixels; the page is laid
            # out in points. RexWeb says how many pixels a point is.
            for _ in (0, 1):
                got = self.web.call("/size", {"width": max(100, round(xp / self.scale)), "base": True,
                                              "height": max(100, round(yp * (rows - 1) / rows / self.scale))})
                if not got.get("scale") or got["scale"] == self.scale:
                    break
                self.scale = float(got["scale"])
            self.resized = False
            self.last = None
            self.image = None
            self.out("\x1b_Ga=d,d=A,q=2\x1b\\\x1b[2J")
            self.shown_url = None
            time.sleep(0.05)
        info = self.web.call("/info")
        if info.get("error"):
            self.lost += 1
            if self.lost >= 3:
                self.lost = 0
                self.register()
            return
        self.lost = 0
        url = info.get("url", "")
        if url:
            self.url = url
        if (self.url, self.editing) != self.shown_url:
            self.draw_bar()
            self.shown_url = (self.url, self.editing)
        title = info.get("title") or self.url or "browser"
        if title != self.title:
            self.title = title
            self.out("\x1b]2;%s\x07" % re.sub(r"[\x00-\x1f\x7f]", " ", title)[:120])
        if info.get("loading"):
            self.busy_until = max(self.busy_until, time.time() + 1)
        state = self.web.call("/eval", {"js": JS_STATE}).get("value") or {}
        shot = self.web.call("/shot", {"path": self.png, "width": round(self.geom[2] / self.scale)})
        if shot.get("error"):
            return
        try:
            with open(self.png, "rb") as f:
                data = f.read()
        except OSError:
            return
        digest = hashlib.sha1(data).digest()
        pw, ph = shot.get("width") or 1, shot.get("height") or 1
        if digest != self.last:
            c, r = self.draw_image(data, pw, ph)
            self.last = digest
            self.image = (c, r, state.get("w") or pw / 2, state.get("h") or ph / 2)
            if self.editing is not None:
                self.draw_bar()

    def act(self, route, body=None):
        self.web.call(route, body)
        self.busy_until = time.time() + 1.5

    def js(self, code):
        self.act("/eval", {"js": code})

    def press(self, key):
        self.js(JS_KEY % (json.dumps(key), EDITABLE))

    def type(self, text):
        self.js(JS_TEXT % json.dumps(text))

    def page_point(self, col, row):
        """The page's CSS point under a terminal cell (1-based), or None."""
        if not self.image or row < 2:
            return None
        c, r, w, h = self.image
        if col > c or row - 1 > r:
            return None
        return (col - 0.5) * w / c, (row - 1.5) * h / r

    # ------------------------------------------------------------- input

    def go(self, text):
        text = text.strip()
        if not text:
            return
        if " " in text or ("." not in text and ":" not in text and not text.startswith("localhost")):
            from urllib.parse import quote_plus
            text = "https://duckduckgo.com/?q=" + quote_plus(text)
        self.url = text
        self.act("/nav", {"url": text})

    def mouse(self, b, col, row, press):
        if b & 64:  # the wheel
            at = self.page_point(col, row)
            if at:
                dy = 120 if b & 1 else -120
                self.act("/eval", {"js": JS_SCROLL % (at[0], at[1], dy)})
            return
        if (b & 3) != 0:
            return
        moving = bool(b & 32)
        if row == 1 and press and not moving:
            rows, cols, _, _ = self.geom
            first = cols - len(BUTTONS)
            if col > first:
                self.act(("/back", "/forward", "/reload")[min(2, (col - first - 1) // 4)])
            else:
                self.editing = ""
                self.shown_url = None
            return
        at = self.page_point(col, row)
        if not at:
            return
        if self.editing is not None:
            self.editing = None
            self.shown_url = None
        # A click is a press and a release in the same place.
        if press and not moving:
            self.down = (col, row)
        elif not press and not moving:
            if self.down == (col, row):
                self.js(JS_TAP % (at[0], at[1]))
            self.down = None

    def edit_key(self, text):
        for ch in text:
            if ch in "\r\n":
                url, self.editing = self.editing, None
                self.go(url)
                break
            elif ch == "\x1b":
                self.editing = None
                break
            elif ch in "\x7f\x08":
                self.editing = self.editing[:-1]
            elif ch == "\x15":
                self.editing = ""
            elif ch >= " ":
                self.editing += ch
        self.shown_url = None

    def keys(self, text):
        """Typed text and keys, with the mouse reports already taken out."""
        while text:
            if text.startswith("\x1b[200~"):  # a paste
                body, _, text = text[6:].partition("\x1b[201~")
                if self.editing is not None:
                    self.edit_key(body.replace("\n", "").replace("\r", ""))
                else:
                    self.type(body)
                continue
            if self.editing is not None:
                if text[0] == "\x1b" and len(text) > 1:
                    m = re.match(r"\x1b(\[[0-9;]*[A-Za-z~]|O.|.)", text)
                    text = text[m.end():] if m else text[1:]
                    continue
                self.edit_key(text[0])
                text = text[1:]
                continue
            if text[0] == "\x1b":
                m = re.match(r"\x1b(\x1b\[[A-D]|\[[0-9;]*[A-Za-z~]|O.|[a-z])", text)
                seq = m.group(0) if m else "\x1b"
                text = text[len(seq):]
                if seq in BACK:
                    self.act("/back")
                elif seq in FORWARD:
                    self.act("/forward")
                elif seq in KEYS:
                    self.press(KEYS[seq])
                elif seq == "\x1b":
                    self.press("escape")
                continue
            ch = text[0]
            if ch == "\x0c":  # Ctrl-L
                self.editing = ""
                self.shown_url = None
            elif ch == "\x12":  # Ctrl-R
                self.act("/reload")
            elif ch in "\r\n":
                self.press("enter")
            elif ch in "\x7f\x08":
                self.press("backspace")
            elif ch == "\t":
                self.press("tab")
            elif ch >= " ":
                run = re.match(r"[^\x00-\x1f\x7f]+", text).group(0)
                self.type(run)
                text = text[len(run):]
                continue
            text = text[1:]

    def feed(self, text):
        pos = 0
        for m in re.finditer(r"\x1b\[<(\d+);(\d+);(\d+)([Mm])", text):
            self.keys(text[pos:m.start()])
            self.mouse(int(m.group(1)), int(m.group(2)), int(m.group(3)), m.group(4) == "M")
            pos = m.end()
        self.keys(text[pos:])

    # --------------------------------------------------------------- run

    def run(self):
        signal.signal(signal.SIGWINCH, lambda *_: setattr(self, "resized", True))
        self.register()
        self.busy_until = time.time() + 3
        pending = b""
        while True:
            self.refresh()
            wait = 0.04 if time.time() < self.busy_until else 0.6
            try:
                ready = select.select([0], [], [], wait)[0]
            except InterruptedError:
                continue
            if not ready:
                continue
            data = os.read(0, 65536)
            if not data:
                return
            pending += data
            # Keep back a sequence or a character that is cut in two.
            text = pending.decode("utf-8", errors="ignore")
            cut = re.search(r"\x1b(\[<?[0-9;]*)?$", text)
            if cut and len(pending) < 64:
                continue
            pending = b""
            self.feed(text)


def main():
    if not BLOCK or not SESSION:
        sys.exit("webpane.py must run inside a Rex pane")
    pane = Pane(sys.argv[1] if len(sys.argv) > 1 else "")
    old = termios.tcgetattr(0)
    tty.setraw(0)
    # The other screen, no cursor, mouse presses, drags and wheel as SGR
    # reports, and pastes marked.
    os.write(1, b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[2J")

    def leave(*_):
        os.write(1, b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[?2004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[?1049l")
        termios.tcsetattr(0, termios.TCSADRAIN, old)
        pane.web.call("/close", timeout=1)
        try:
            os.remove(pane.png)
        except OSError:
            pass
        os._exit(0)

    for sig in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, leave)
    try:
        pane.run()
    finally:
        leave()


if __name__ == "__main__":
    main()
