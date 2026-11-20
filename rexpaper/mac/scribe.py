#!/usr/bin/env python3
"""Reads handwriting for the Rex tablet client.

The client starts this in a terminal block of its own on the Rex server's
machine and talks to it through that terminal:

  client -> scribe   one JSON object per line, typed into the terminal:
                     {"id": 7, "strokes": [[x, y, x, y, ...], ...],
                      "before": "the line up to the cursor"}
  scribe -> client   a line on the screen for each one:
                     @@scribe {"id": 7, "text": "ls -la"}@@
                     and the terminal's title set to "scribe done 7", which
                     tells the client to read the screen.

Claude reads the strokes, through the Claude Code login on this machine
(`claude -p`), so it needs no API key. SCRIBE_MODEL picks the model.

  scribe.py < requests.jsonl      also works without a terminal, for testing.
"""
import base64
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import time
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

    def say(obj, title):
        out = "@@scribe " + json.dumps(obj) + "@@\r\n"
        if term:
            out += f"\x1b]2;scribe {title}\x07"
        sys.stdout.write(out)
        sys.stdout.flush()

    home = os.path.expanduser("~/.config/rexpaper/scribe")
    os.makedirs(home, exist_ok=True)
    reader = Reader()
    reader.start()  # warm up while they are still writing
    say({"ready": 1}, "ready")
    # How tall this person writes: taken from their words, used for lone marks.
    letter = 40.0
    for raw in lines(fd):
        try:
            msg = json.loads(raw)
            strokes = [list(zip(s[0::2], s[1::2])) for s in msg["strokes"] if len(s) >= 2]
            rid = msg["id"]
        except (ValueError, KeyError, TypeError):
            continue
        if not strokes:
            continue
        w, h = ink_size(strokes)
        if w > 2.5 * h and h > 12:  # a word or more: its height is the height of a line of writing
            letter = 0.7 * letter + 0.3 * (h * 0.6)
        note = (f"(handwriting; the ink is {w:.0f} x {h:.0f} pixels, "
                f"and their lower-case letters are about {letter:.0f} pixels tall; "
                f"the line up to the cursor is {json.dumps(str(msg.get('before', '')))})")
        png = strokes_png(strokes)
        try:
            answer = reader.read(png, note)
            say({"id": rid, "text": tidy(answer)}, f"done {rid}")
        except Exception as e:  # the client shows this instead of typing
            answer = f"error: {e}"
            say({"id": rid, "error": str(e)[:200]}, f"done {rid}")
        # The last pictures and what was made of them, to see why a reading went wrong.
        try:
            with open(os.path.join(home, f"ink-{rid % 20:02d}.png"), "wb") as f:
                f.write(png)
            with open(os.path.join(home, "log"), "a") as f:
                f.write(f"{time.strftime('%F %T')} ink-{rid % 20:02d}.png {note} -> {answer!r}\n")
        except OSError:
            pass
    reader.stop()


if __name__ == "__main__":
    main()
