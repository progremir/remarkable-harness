#!/usr/bin/env python3
"""Reads handwriting for the Rex tablet client.

The client starts this in a terminal block of its own on the Rex server's
machine and talks to it through that terminal:

  client -> scribe   one JSON object per line, typed into the terminal:
                     {"id": 7, "strokes": [[x, y, x, y, ...], ...]}
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
import shutil
import struct
import subprocess
import sys
import zlib

MODEL = os.environ.get("SCRIBE_MODEL", "sonnet")
MAX_TURNS = 25  # then a fresh conversation, so old pictures don't pile up

PROMPT = """You turn handwriting into typed text for a terminal.

Each message is a picture of what someone just wrote by hand with a pen on a tablet. Your whole reply is typed at the cursor of their terminal, exactly as you give it. The terminal may be at a shell prompt or at the prompt of a coding agent, so the writing can be a shell command, a file path, a piece of code or plain language.

Rules:
- Reply with the text they wrote and nothing else: no quotes around it, no explanation, no markdown.
- Keep their words, case, digits, punctuation and symbols such as - / . ~ | > & $ exactly. Prefer the reading that makes sense as a command or sentence: `ls -la`, not `1s -1a`.
- Several handwritten lines are one text: join them with single spaces.
- If they finish with a return arrow (a bent arrow like the Enter key) or underline the whole text twice, they want it submitted: end your reply with <enter>.
- A lone return arrow means just <enter>.
- If nothing is readable, reply with exactly <unreadable>."""


def strokes_png(strokes, scale=0.5, pad=24):
    """Pen strokes (screen pixels) -> PNG bytes, cropped to the ink."""
    xs = [x for s in strokes for x, _ in s]
    ys = [y for s in strokes for _, y in s]
    x0, y0 = min(xs) - pad, min(ys) - pad
    w = int((max(xs) + pad - x0) * scale) + 1
    h = int((max(ys) + pad - y0) * scale) + 1
    img = [bytearray(b"\xff" * w) for _ in range(h)]

    def dot(x, y):
        xi, yi = int(x), int(y)
        for yy in (yi, yi + 1):
            if 0 <= yy < h:
                for xx in (xi, xi + 1):
                    if 0 <= xx < w:
                        img[yy][xx] = 0

    for s in strokes:
        pts = [((x - x0) * scale, (y - y0) * scale) for x, y in s]
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

    def read(self, png):
        if self.proc is None or self.proc.poll() is not None or self.turns >= MAX_TURNS:
            self.start()
        self.turns += 1
        content = [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                         "data": base64.b64encode(png).decode()}},
            {"type": "text", "text": "(handwriting)"},
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

    reader = Reader()
    reader.start()  # warm up while they are still writing
    say({"ready": 1}, "ready")
    for raw in lines(fd):
        try:
            msg = json.loads(raw)
            strokes = [list(zip(s[0::2], s[1::2])) for s in msg["strokes"] if len(s) >= 2]
            rid = msg["id"]
        except (ValueError, KeyError, TypeError):
            continue
        if not strokes:
            continue
        try:
            say({"id": rid, "text": tidy(reader.read(strokes_png(strokes)))}, f"done {rid}")
        except Exception as e:  # the client shows this instead of typing
            say({"id": rid, "error": str(e)[:200]}, f"done {rid}")
    reader.stop()


if __name__ == "__main__":
    main()
