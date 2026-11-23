#!/usr/bin/env python3
"""The Rex tablet client's helper on the Rex server's machine.

The client starts this in a terminal block of its own and talks to it
through that terminal:

  client -> helper   one JSON object per line, typed into the terminal
  helper -> client   a line on the screen for each one:
                     @@scribe {"id": 7, ...}@@
                     and the terminal's title set to "scribe done 7", which
                     tells the client to read the screen.

Each request names the replies the client has read, in "acks". The screen
only ever holds the replies that are not acknowledged yet.

Handwriting: {"id": 7, "strokes": [[x, y, x, y, ...], ...], "before": "the line up to the cursor"}
  -> {"id": 7, "text": "ls -la"}. Claude reads the strokes, through the
  Claude Code login on this machine (`claude -p`), so it needs no API key.
  SCRIBE_MODEL picks the model.

Browser panes (rexweb, https://github.com/acoyfellow/rexweb): {"id": 8, "web": OP, "block": ID, ...}
  shot    {"w": pixels, "last": hash}  a picture of the page as a JPEG file,
          for the client to download: {"path", "hash", "vw", "vh", ...}, or
          {"same": true} when the page looks as it did.
  tap     {"x", "y"}                   click at a point of the page, in CSS pixels
  scroll  {"dy"}                       scroll the page
  text    {"text"}                     type into the focused field
  key     {"key"}                      enter, backspace, tab, escape, up, down, pageup, pagedown
  nav     {"url"}, back, forward, reload
  open    {"session", "anchor", "url", "split"}   open a browser pane beside a pane

  helper.py < requests.jsonl      also works without a terminal, for testing.
"""
import base64
import hashlib
import json
import os
import queue
import re
import shutil
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import zlib

MODEL = os.environ.get("SCRIBE_MODEL", "sonnet")
MAX_TURNS = 25  # then a fresh conversation, so old pictures don't pile up

PROMPT = """You turn handwriting into typed text for a terminal.

Each message is a picture of what someone just wrote by hand with a pen on a tablet. Your whole reply is typed at the cursor of their terminal, exactly as you give it. The terminal may be at a shell prompt or at the prompt of a coding agent, so the writing can be a shell command, a file path, a piece of code or plain language.

Rules:
- Reply with the text they wrote and nothing else: no quotes around it, no explanation, no markdown.
- Keep their words, case, digits, punctuation and symbols such as - / . ~ | > & $ exactly. Prefer the reading that makes sense as a command or sentence: `ls -la`, not `1s -1a`.
- Several handwritten lines are one text: join them with single spaces.
- Spaces at the ends matter, and you write one as <space>. The note with each picture quotes what is already on the line, up to the cursor. Start your reply with <space> when the new writing is a new word after a word that is already there: `git` on the line and `status` written gives `<space>status`. Give no <space> when the line is empty or already ends in a space (a shell prompt such as `% ` or `$ ` ends in one), or when the writing carries on what is there: punctuation after a word, a file extension, the rest of a path, the rest of a word.
- If they draw the space sign, a flat line with both ends turned up like a tray, reply with exactly <space>.
- If they finish with a return arrow (a bent arrow like the Enter key) or underline the whole text twice, they want it submitted: end your reply with <enter>.
- A lone return arrow means just <enter>.
- A small mark on its own is punctuation or a symbol, and they mean it: a dot is `.`, a dot with a tail is `,`, a short flat line is `-`, a long flat line is `_`, a leaning line is `/` or `\\`, an upright line as tall as a letter is `|`, an upright line over a dot is `!`, a hook over a dot is `?`, two dots are `:`, a dot over a tail is `;`, one or two short ticks are `'` or `"`, and so on for `* ~ = + < > ( ) [ ] { } # @ $ % & ^`. Give your best reading of a single mark. Never call a clear mark unreadable.
- Every picture is drawn at the same scale, and the note with it says how tall their letters usually are. Use that to tell a dot from a line, and a comma from a slash.
- If the picture really holds nothing you can read, such as a scribble that is no character at all, reply with exactly <unreadable>."""


SCALE = 0.5          # picture pixels per screen pixel, the same for every picture
MIN_W, MIN_H = 320, 200   # so a lone dot is a dot on a page, not a blob that fills the frame


def strokes_png(strokes, pad=24):
    """Pen strokes (screen pixels) -> PNG bytes at a fixed scale, the ink in the middle."""
    xs = [x for s in strokes for x, _ in s]
    ys = [y for s in strokes for _, y in s]
    w = max(MIN_W, int((max(xs) - min(xs) + 2 * pad) * SCALE) + 1)
    h = max(MIN_H, int((max(ys) - min(ys) + 2 * pad) * SCALE) + 1)
    x0 = (min(xs) + max(xs)) / 2 - w / SCALE / 2
    y0 = (min(ys) + max(ys)) / 2 - h / SCALE / 2
    img = [bytearray(b"\xff" * w) for _ in range(h)]

    def dot(x, y):
        xi, yi = int(x), int(y)
        for yy in (yi - 1, yi, yi + 1):
            if 0 <= yy < h:
                for xx in (xi - 1, xi, xi + 1):
                    if 0 <= xx < w:
                        img[yy][xx] = 0

    for s in strokes:
        pts = [((x - x0) * SCALE, (y - y0) * SCALE) for x, y in s]
        dot(*pts[0])
        for (ax, ay), (bx, by) in zip(pts, pts[1:]):
            n = int(max(abs(bx - ax), abs(by - ay))) + 1
            for k in range(1, n + 1):
                dot(ax + (bx - ax) * k / n, ay + (by - ay) * k / n)

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    raw = b"".join(b"\x00" + bytes(r) for r in img)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 0, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))


def ink_size(strokes):
    """Width and height of the ink, in picture pixels."""
    xs = [x for s in strokes for x, _ in s]
    ys = [y for s in strokes for _, y in s]
    return (max(xs) - min(xs)) * SCALE, (max(ys) - min(ys)) * SCALE


class Reader:
    """A long-lived `claude -p` conversation: one picture in, one text out."""

    def __init__(self):
        self.proc = None
        self.turns = 0

    def start(self):
        self.stop()
        claude = shutil.which("claude") or os.path.expanduser("~/.local/bin/claude")
        work = os.path.expanduser("~/.config/rexpaper/scribe")
        os.makedirs(work, exist_ok=True)
        self.proc = subprocess.Popen(
            [claude, "-p", "--model", MODEL,
             "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
             "--tools", "", "--strict-mcp-config", "--disable-slash-commands",
             "--no-session-persistence", "--setting-sources", "",
             "--system-prompt", PROMPT],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, cwd=work)
        self.turns = 0

    def stop(self):
        if self.proc:
            try:
                self.proc.stdin.close()
                self.proc.terminate()
            except OSError:
                pass
            self.proc = None

    def read(self, png, note):
        if self.proc is None or self.proc.poll() is not None or self.turns >= MAX_TURNS:
            self.start()
        self.turns += 1
        content = [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                         "data": base64.b64encode(png).decode()}},
            {"type": "text", "text": note},
        ]
        self.proc.stdin.write(json.dumps({"type": "user", "message": {"role": "user", "content": content}}) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                self.proc = None
                raise RuntimeError("Claude stopped answering")
            try:
                m = json.loads(line)
            except ValueError:
                continue
            if m.get("type") == "result":
                if m.get("is_error"):
                    raise RuntimeError(str(m.get("result"))[:200])
                return m.get("result") or ""


# ---------------------------------------------------------------- browser panes

HOME = os.path.expanduser("~/.config/rexpaper/scribe")


def rexweb(route, body, timeout=20):
    """Call RexWeb's local API. Returns the reply, or {"error": ...}."""
    port = os.environ.get("REXWEB_PORT")
    if not port:
        try:
            with open(os.path.expanduser("~/.rexweb/config.json")) as f:
                port = json.load(f).get("port")
        except (OSError, ValueError):
            pass
    try:
        with open(os.path.expanduser("~/.rexweb/token")) as f:
            token = f.read().strip()
    except OSError:
        token = ""
    req = urllib.request.Request(
        f"http://127.0.0.1:{port or 18802}{route}", data=json.dumps(body).encode(),
        headers={"content-type": "application/json", "x-rexweb-token": token})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        try:
            return {"error": json.load(e).get("error", f"RexWeb said {e.code}")}
        except ValueError:
            return {"error": f"RexWeb said {e.code}"}
    except (OSError, ValueError):
        return {"error": "RexWeb isn't running on the Mac. Install rexweb, or open a browser pane there."}


EDITABLE = """((el) => !!el && (el.isContentEditable || el.tagName === 'TEXTAREA' ||
  (el.tagName === 'INPUT' && !['button','submit','checkbox','radio','range','file','color','image','reset'].includes(el.type))))"""

# A page is laid out for the device looking at it, as a terminal is sized: the
# helper asks rexweb (its /size route, see README) for the tablet pane's size
# in CSS pixels, and rexweb holds it while the tablet keeps asking. The
# tablet's text size makes that size smaller or bigger. A rexweb without the
# route shows the page as the Mac has it. An earlier helper set a zoom on the
# page itself; this clears it.
JS_STATE = f"""(() => {{
  const root = document.documentElement;
  if (root && root.style.zoom) root.style.zoom = '';
  return {{vw: innerWidth, vh: innerHeight, y: Math.round(scrollY),
  full: Math.max(document.documentElement.scrollHeight, document.body ? document.body.scrollHeight : 0),
  edit: {EDITABLE}(document.activeElement)}};
}})()"""

JS_TAP = """(() => {
  const x = %s, y = %s;
  const el = document.elementFromPoint(x, y);
  if (!el) return false;
  const t = el.closest('a,button,input,textarea,select,summary,label,[role=button],[role=link],[role=tab],[onclick],[contenteditable=""],[contenteditable=true]') || el;
  const ev = (type) => t.dispatchEvent(new MouseEvent(type, {bubbles: true, cancelable: true, view: window, clientX: x, clientY: y}));
  ev('mousedown');
  if (t.focus) t.focus();
  ev('mouseup');
  t.click();
  return true;
})()"""

JS_TEXT = """(() => {
  const text = %s, el = document.activeElement;
  if (!el) return false;
  if (document.execCommand('insertText', false, text)) return true;
  if ('value' in el) { el.value += text; el.dispatchEvent(new Event('input', {bubbles: true})); return true; }
  return false;
})()"""

JS_KEY = """(() => {
  const key = %s, el = document.activeElement || document.body;
  const names = {enter: 'Enter', backspace: 'Backspace', tab: 'Tab', escape: 'Escape', up: 'ArrowUp', down: 'ArrowDown',
                 left: 'ArrowLeft', right: 'ArrowRight', pageup: 'PageUp', pagedown: 'PageDown'};
  const name = names[key];
  if (!name) return false;
  const send = (type) => el.dispatchEvent(new KeyboardEvent(type, {key: name, code: name, bubbles: true, cancelable: true}));
  const went = send('keydown');
  send('keyup');
  if (!went) return true;  // the page handled the key itself
  const edit = %s(el);
  if (key === 'backspace') document.execCommand('delete');
  else if (key === 'enter' && el.tagName === 'TEXTAREA') document.execCommand('insertText', false, '\\n');
  else if (key === 'enter' && el.isContentEditable) document.execCommand('insertParagraph');
  else if (key === 'enter' && el.form) { el.form.requestSubmit ? el.form.requestSubmit() : el.form.submit(); }
  else if (key === 'escape' && el.blur) el.blur();
  else if (!edit && key === 'up') scrollBy(0, -80);
  else if (!edit && key === 'down') scrollBy(0, 80);
  else if (key === 'pageup') scrollBy(0, -innerHeight * 0.85);
  else if (key === 'pagedown') scrollBy(0, innerHeight * 0.85);
  return true;
})()"""


# The size each pane was last given, by block ID.
SIZES = {}


def find_rexweb():
    return shutil.which("rexweb") or os.path.expanduser("~/.local/bin/rexweb")


# ---- kb (KittyBrowser, https://github.com/graiz/kittybrowser) panes. kb draws
# the page inside the terminal pane and answers `kb ctl` from outside it.

KB_PANES = {}     # block ID -> kb's id for the browser in that pane
KB_LISTED = 0.0   # when `kb list` was last read
KB_USED = {}      # block ID -> when the tablet last asked about it
KB_HOLD = 20      # seconds a pane keeps the tablet's size after its last request


def find_kb():
    return shutil.which("kb") or os.path.expanduser("~/.local/bin/kb")


def kb_id(block):
    """kb's id for the browser running in a pane, or None when the pane isn't a kb."""
    global KB_LISTED
    if block not in KB_PANES and time.time() - KB_LISTED > 3:
        KB_LISTED = time.time()
        KB_PANES.clear()
        try:
            listed = subprocess.run([find_kb(), "list"], capture_output=True, text=True, timeout=5).stdout
        except (OSError, subprocess.TimeoutExpired):
            listed = ""
        for line in listed.splitlines():
            cols = line.split("\t")
            if len(cols) >= 4 and cols[2].startswith("block:"):
                KB_PANES[cols[2]] = cols[3].strip()
    return KB_PANES.get(block)


def kb(block, *args, timeout=20):
    """Run one `kb ctl` command on a pane's browser -> (worked, what it printed)."""
    try:
        done = subprocess.run([find_kb(), "ctl", "-i", KB_PANES.get(block, ""), *args],
                              capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired):
        return False, "kb didn't answer"
    if done.returncode != 0:
        KB_PANES.pop(block, None)  # it may have quit: look again next time
        return False, (done.stderr.strip() or "kb ctl failed")[:200]
    return True, done.stdout.strip()


def kb_eval(block, js):
    ok, out = kb(block, "eval", js)
    if not ok:
        return {"error": out}
    try:
        return {"value": json.loads(out)}
    except ValueError:
        return {"value": out}


def kb_release():
    """Give a pane back its own size once the tablet has stopped looking at it."""
    while True:
        time.sleep(5)
        for block, used in list(KB_USED.items()):
            if time.time() - used > KB_HOLD:
                KB_USED.pop(block, None)
                if SIZES.pop(block, None):
                    kb(block, "viewport", "reset")


def kb_shot(block, msg):
    """The picture of a kb pane's page, at the size the tablet shows it."""
    if not getattr(kb_shot, "started", False):
        kb_shot.started = True
        threading.Thread(target=kb_release, daemon=True).start()
    KB_USED[block] = time.time()
    tag = "".join(c for c in block[-8:] if c.isalnum())
    png, jpg = os.path.join(HOME, f"web-{tag}.png"), os.path.join(HOME, f"web-{tag}.jpg")
    zoom = max(25, min(400, int(msg.get("zoom") or 100)))
    w, h = int(msg.get("w") or 0), int(msg.get("h") or 0)
    want = (round(w * 50 / zoom), round(h * 50 / zoom))
    if min(want) >= 100 and SIZES.get(block) != want:
        ok, _ = kb(block, "viewport", f"{want[0]}x{want[1]}")
        SIZES[block] = want if ok else None
        time.sleep(0.3)  # the page lays itself out again
    state = kb_eval(block, JS_STATE).get("value")
    if not isinstance(state, dict):
        state = {}
    width = max(64, min(w or 1200, 1620))
    ok, out = kb(block, "screenshot", "--size", str(width), "--out", png)
    if not ok:
        return {"error": out}
    ok, status = kb(block, "status")
    info = dict(line.split(": ", 1) for line in status.splitlines() if ": " in line) if ok else {}
    done = subprocess.run(["sips", "-s", "format", "jpeg", "-s", "formatOptions", "50", png, "--out", jpg],
                          capture_output=True)
    if done.returncode != 0:
        return {"error": "couldn't make the picture: " + done.stderr.decode(errors="replace")[:120]}
    with open(jpg, "rb") as f:
        digest = hashlib.sha1(f.read()).hexdigest()[:16]
    out = {"hash": digest, "url": info.get("url", ""), "title": info.get("title", ""),
           "loading": info.get("loading", "no") != "no", "vw": state.get("vw", 0), "vh": state.get("vh", 0),
           "y": state.get("y", 0), "full": state.get("full", 0), "edit": bool(state.get("edit"))}
    if digest == msg.get("last"):
        out["same"] = True
    else:
        out["path"] = jpg
    return out


KB_KEYS = {"enter": "Enter", "backspace": "Backspace", "tab": "Tab", "escape": "Escape", "up": "ArrowUp",
           "down": "ArrowDown", "left": "ArrowLeft", "right": "ArrowRight", "pageup": "PageUp", "pagedown": "PageDown"}


def kb_act(op, block, msg):
    """Do something to a kb pane's page -> {"ok": 1} or {"error": ...}."""
    KB_USED[block] = time.time()

    def settle(reply):
        # Give a click or a key that loads a page time to start, then to finish.
        if reply.get("error"):
            return {"error": reply["error"]}
        time.sleep(0.15)
        until = time.time() + 6
        while time.time() < until:
            ok, status = kb(block, "status")
            if not ok or "loading: yes" not in status:
                break
            time.sleep(0.2)
        return {"ok": 1}

    def did(ok_out):
        return {} if ok_out[0] else {"error": ok_out[1]}

    def done(reply):
        # kb ctl waits for the page itself after these.
        return {"error": reply["error"]} if reply.get("error") else {"ok": 1}

    if op == "tap":
        return settle(kb_eval(block, JS_TAP % (float(msg.get("x", 0)), float(msg.get("y", 0)))))
    if op == "scroll":
        reply = kb_eval(block, f"scrollBy({{top: {float(msg.get('dy', 0))}, behavior: 'instant'}})")
        time.sleep(0.03)
        return {"error": reply["error"]} if reply.get("error") else {"ok": 1}
    if op == "text":
        return done(did(kb(block, "type", ":focus", str(msg.get("text", "")))))
    if op == "key":
        name = KB_KEYS.get(str(msg.get("key", "")))
        return done(did(kb(block, "press", name))) if name else {"error": "unknown key"}
    if op == "nav":
        return done(did(kb(block, "open", str(msg.get("url", "")), timeout=40)))
    if op in ("back", "forward", "reload"):
        return done(did(kb(block, op, timeout=40)))
    return {"error": f"unknown browser request {op}"}


def kb_open(msg):
    """Open a kb pane beside a pane, or in a new tab -> {"block": its ID}."""
    cmd = ["rex", "run"] if msg.get("tab") else ["rex", "split", "--split", str(msg.get("split", "right"))]
    cmd += ["-s", str(msg.get("session", "")), "--label", "browser", "--focus=false", "--json"]
    if not msg.get("tab"):
        cmd += ["-b", str(msg.get("anchor", ""))]
    cmd += ["--", find_kb()] + ([str(msg["url"])] if msg.get("url") else [])
    try:
        done = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        return {"block": json.loads(done.stdout)["block_ids"][0]}
    except (OSError, subprocess.TimeoutExpired, ValueError, KeyError, IndexError):
        return {"error": "couldn't open a kb pane on the Mac"}


def web(msg):
    """One browser-pane request -> the reply's fields."""
    op, block = msg.get("web"), msg.get("block", "")
    pane = {"id": block}

    if op == "open" and os.path.exists(find_kb()):
        return kb_open(msg)
    if op != "open" and kb_id(block):
        if op != "shot":
            reply = kb_act(op, block, msg)
            if reply.get("error") or not msg.get("w"):
                return reply
        return kb_shot(block, msg)

    if op != "shot":
        reply = web_act(op, pane, msg)
        # A request that says how big the pane is wants the picture of what
        # the act did as its answer.
        if reply.get("error") or not msg.get("w"):
            return reply
    return web_shot(block, pane, msg)


def web_shot(block, pane, msg):
    """Take the picture of a pane's page, at the size the tablet shows it."""
    tag = "".join(c for c in block[-8:] if c.isalnum())
    png, jpg = os.path.join(HOME, f"web-{tag}.png"), os.path.join(HOME, f"web-{tag}.jpg")
    zoom = max(25, min(400, int(msg.get("zoom") or 100)))
    w, h = int(msg.get("w") or 0), int(msg.get("h") or 0)
    # Two tablet pixels to a CSS pixel at 100%: text is then about the
    # size it is on paper.
    want = (round(w * 50 / zoom), round(h * 50 / zoom))
    sized = False
    if min(want) >= 100:
        sized = bool(rexweb("/size", dict(pane, width=want[0], height=want[1], hold=20)).get("sized"))
        if sized and SIZES.get(block) != want:
            time.sleep(0.4)  # the page lays itself out again
        SIZES[block] = want if sized else None
    state = rexweb("/eval", dict(pane, js=JS_STATE)).get("value") or {}
    # Ask for the picture at the tablet's own width, so it isn't stretched.
    shot = rexweb("/shot", dict(pane, path=png, width=w / 2) if sized else dict(pane, path=png))
    if shot.get("error"):
        return {"error": shot["error"]}
    info = rexweb("/info", pane)
    width = max(64, min(int(msg.get("w") or 1200), int(shot.get("width") or 1200)))
    done = subprocess.run(["sips", "--resampleWidth", str(width), "-s", "format", "jpeg",
                           "-s", "formatOptions", "50", png, "--out", jpg], capture_output=True)
    if done.returncode != 0:
        return {"error": "couldn't make the picture: " + done.stderr.decode(errors="replace")[:120]}
    with open(jpg, "rb") as f:
        digest = hashlib.sha1(f.read()).hexdigest()[:16]
    out = {"hash": digest, "url": info.get("url", ""), "title": info.get("title", ""),
           "loading": bool(info.get("loading")), "vw": state.get("vw", 0), "vh": state.get("vh", 0),
           "y": state.get("y", 0), "full": state.get("full", 0), "edit": bool(state.get("edit"))}
    if digest == msg.get("last"):
        out["same"] = True
    else:
        out["path"] = jpg
    return out


def web_act(op, pane, msg):
    """Do something to the page -> {"ok": 1} or {"error": ...}."""
    def settle(reply):
        # Give a click or a key that loads a page time to do so.
        if not reply.get("error"):
            time.sleep(0.15)
            rexweb("/loaded", dict(pane, timeout=6), timeout=8)
        return {"error": reply["error"]} if reply.get("error") else {"ok": 1}

    if op == "tap":
        return settle(rexweb("/eval", dict(pane, js=JS_TAP % (float(msg.get("x", 0)), float(msg.get("y", 0))))))
    if op == "scroll":
        # Scrolling loads nothing: no need to wait for it.
        reply = rexweb("/eval", dict(pane, js=f"scrollBy({{top: {float(msg.get('dy', 0))}, behavior: 'instant'}})"))
        time.sleep(0.03)
        return {"error": reply["error"]} if reply.get("error") else {"ok": 1}
    if op == "text":
        return settle(rexweb("/eval", dict(pane, js=JS_TEXT % json.dumps(str(msg.get("text", ""))))))
    if op == "key":
        return settle(rexweb("/eval", dict(pane, js=JS_KEY % (json.dumps(str(msg.get("key", ""))), EDITABLE))))
    if op == "nav":
        reply = rexweb("/nav", dict(pane, url=str(msg.get("url", ""))), timeout=40)
        return {"error": reply["error"]} if reply.get("error") else {"ok": 1}
    if op in ("back", "forward", "reload"):
        reply = rexweb("/" + op, pane, timeout=40)
        return {"error": reply["error"]} if reply.get("error") else {"ok": 1}
    if op == "open":
        where = ["--tab"] if msg.get("tab") else ["--split", str(msg.get("split", "right"))]
        cmd = [find_rexweb(), "open", "--no-wait", "--focus=false", *where,
               "-s", str(msg.get("session", "")), "-b", str(msg.get("anchor", ""))]
        if msg.get("url"):
            cmd.append(str(msg["url"]))
        try:
            done = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired):
            return {"error": "rexweb isn't installed on the Mac. See https://github.com/acoyfellow/rexweb"}
        new = done.stdout.strip().splitlines()
        if not new:
            return {"error": (done.stderr.strip() or "rexweb open failed")[:200]}
        return {"block": new[0]}
    return {"error": f"unknown browser request {op}"}


def tidy(answer):
    """Claude's reply -> the bytes to type."""
    text = " ".join(answer.replace("```", "").split())
    if text == "<unreadable>":
        return ""
    enter = text.endswith("<enter>")
    text = text.removesuffix("<enter>").rstrip()
    # <space> stands for a space that the collapsing above would have lost.
    text = re.sub(r" ?<space> ?", " ", text)
    return text + ("\r" if enter else "")


def lines(fd):
    """Lines from `fd`, read unbuffered so a terminal in raw mode works."""
    buf = b""
    while True:
        chunk = os.read(fd, 65536)
        if not chunk:
            return
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            yield line


def main():
    fd = sys.stdin.fileno()
    term = os.isatty(fd)
    if term:
        import tty
        tty.setraw(fd)  # no echo, and no limit on the length of a line

    lock = threading.Lock()
    unread = {}  # replies the client has not acknowledged, by id

    def show(title):
        # The screen holds exactly the replies still to be read.
        out = "\x1b[3J\x1b[2J\x1b[H" if term else ""
        for obj in list(unread.values())[-1:] if not term else unread.values():
            out += "@@scribe " + json.dumps(obj) + "@@\r\n"
        if term and title:
            out += f"\x1b]2;scribe {title}\x07"
        sys.stdout.write(out)
        sys.stdout.flush()

    def say(obj, title):
        with lock:
            unread[obj.get("id", 0)] = obj
            show(title)

    os.makedirs(HOME, exist_ok=True)
    reader = Reader()
    reader_lock = threading.Lock()
    reader.start()  # warm up while they are still writing
    say({"ready": 1, "id": 0}, "ready")
    # How tall this person writes: taken from their words, used for lone marks.
    size = {"letter": 40.0}

    def ink(rid, msg, strokes):
        w, h = ink_size(strokes)
        if w > 2.5 * h and h > 12:  # a word or more: its height is the height of a line of writing
            size["letter"] = 0.7 * size["letter"] + 0.3 * (h * 0.6)
        note = (f"(handwriting; the ink is {w:.0f} x {h:.0f} pixels, "
                f"and their lower-case letters are about {size['letter']:.0f} pixels tall; "
                f"the line up to the cursor is {json.dumps(str(msg.get('before', '')))})")
        png = strokes_png(strokes)
        try:
            with reader_lock:
                answer = reader.read(png, note)
            say({"id": rid, "text": tidy(answer)}, f"done {rid}")
        except Exception as e:  # the client shows this instead of typing
            answer = f"error: {e}"
            say({"id": rid, "error": str(e)[:200]}, f"done {rid}")
        # The last pictures and what was made of them, to see why a reading went wrong.
        try:
            with open(os.path.join(HOME, f"ink-{rid % 20:02d}.png"), "wb") as f:
                f.write(png)
            with open(os.path.join(HOME, "log"), "a") as f:
                f.write(f"{time.strftime('%F %T')} ink-{rid % 20:02d}.png {note} -> {answer!r}\n")
        except OSError:
            pass

    # Browser requests run one at a time and in the order they came: typed
    # letters must not overtake each other.
    web_queue = queue.Queue()

    def browse():
        while True:
            rid, msg = web_queue.get()
            try:
                say(dict(web(msg), id=rid), f"done {rid}")
            except Exception as e:
                say({"id": rid, "error": str(e)[:200]}, f"done {rid}")
            web_queue.task_done()

    threading.Thread(target=browse, daemon=True).start()

    threads = []
    for raw in lines(fd):
        try:
            msg = json.loads(raw)
            rid = msg["id"]
        except (ValueError, KeyError, TypeError):
            continue
        with lock:
            for a in msg.get("acks") or []:
                unread.pop(a, None)
            if msg.get("acks"):
                # No new title: it would send the client to read a screen
                # that has nothing new, just as the real answer is due.
                show(None)
        if "web" in msg:
            web_queue.put((rid, msg))
            continue
        else:
            try:
                strokes = [list(zip(s[0::2], s[1::2])) for s in msg["strokes"] if len(s) >= 2]
            except (KeyError, TypeError):
                continue
            if not strokes:
                continue
            t = threading.Thread(target=ink, args=(rid, msg, strokes), daemon=True)
        t.start()
        threads.append(t)
    web_queue.join()
    for t in threads:
        t.join(60)
    reader.stop()


if __name__ == "__main__":
    main()
