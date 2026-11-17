#!/usr/bin/env python3
"""Diary server: the tablet sends the pen strokes of a new message, Claude
reads them and the reply goes back as pen strokes to write under it.

Runs on the Mac and uses the Claude Code login (`claude -p`), so it needs no
API key. One Claude conversation lives for as long as the tablet app is open.

  diaryd.py [tablet-ip]      listen on port 5910 for that tablet (and localhost).
                             Without the argument the address is read from
                             ~/.config/rmdiary/tablet (the tablet's Tailscale IP).

Wire format, one JSON object per line:
  tablet -> Mac   {"strokes": [[x, y, x, y, ...], ...]}        page pixels
  Mac -> tablet   {"clear": bool, "speed": px_per_s,
                   "strokes": [{"c": [r, g, b], "w": px, "n": nib, "p": [x, y, ...]}, ...]}
                  ("n" > 0: drawn with a broad quill nib of that half-width)
With "endless": true in the tablet's message the page has no bottom: y may
exceed the screen and "clear" is never set (the tablet scrolls instead).
"at": y in the tablet's message starts the reply under y instead of under the
ink (fading mode: the message is gone by then). "hold" in the reply is how
many seconds the tablet should leave it up in fading mode.
"""
import base64
import json
import os
import random
import re
import shutil
import socketserver
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ink  # noqa: E402

PORT = 5910
PAGE_W, PAGE_H = 1620, 2160
LEFT, RIGHT, TOP, BOTTOM = 130, 110, 130, 90
# The tablet's ruled paper (see RULE0, RULE_GAP, MARGIN_X in the app): handwriting sits on the rules.
RULE0, RULE_GAP, RULED_LEFT = 200, 72, 185
MODEL = os.environ.get("DIARY_MODEL", "opus")
DEBUG = os.environ.get("DIARY_DEBUG")
HOME = os.path.expanduser("~/.config/rmdiary")
MAX_TURNS = 60
# Size of the handwriting: pixels per font unit (a lower-case letter is about 9 units tall).
SCALE = float(os.environ.get("DIARY_SCALE", "2.4"))
# The hand: "cursive" is round and joined-up, "scripts" is finer and more formal.
FACE = os.environ.get("DIARY_FACE", "cursive")
INK_W = 4  # width of the written line, the same as the user's pen
# The quill: half-width of the nib in pixels (0 writes with a round pen) and how far the hand leans.
NIB = int(os.environ.get("DIARY_NIB", "5"))
SLANT = float(os.environ.get("DIARY_SLANT", "0.3"))

PERSONA = """You are the mind that lives inside an enchanted diary. Someone writes on your page with a pen; their ink sinks in, and your answer writes itself underneath in flowing handwriting. You are warm, curious and a little mysterious, and you are genuinely helpful: you answer questions properly, you remember what was written earlier, and you keep the writer's confidence.

Each message you receive is a picture of what they just wrote or sketched by hand. Read the handwriting generously. If a word is unclear, go with the most sensible reading; if the whole thing is illegible, say so lightly and ask them to write it again.

How to answer:
- Reply only to this newest message.
- Keep it very short, because it is written out by hand: one or two sentences, at most 25 words. Go longer only when they ask for something that truly needs it, and never beyond 60 words.
- Plain text only: ASCII letters, digits and basic punctuation. No markdown, no emoji, no lists.
- Reply in the language they wrote in if you can do so in ASCII letters; otherwise in English.

You can also draw, but only when they ask for a drawing, sketch or picture. Then add ONE drawing after your text as an SVG:
- <svg viewBox="0 0 400 300"> ... </svg>, drawn as pen line art.
- Use only path, line, polyline, polygon, circle, ellipse and rect, with a stroke and fill="none". No text, no transform, no gradients, no <use>.
- Colours: black, red, blue, green, yellow, orange, purple, pink, brown, gray. Use colour with intent; black outlines with a few coloured accents look best on this paper.
- Make it beautiful: confident flowing curves (C and Q commands), good proportions, real detail and a little character, the way a skilled illustrator sketches with a pen. Shade with hatching lines if it helps. Use up to about 70 elements.
- With a drawing, keep the text to a sentence.

Output format: your handwritten text first, then the optional SVG. Nothing else: no preamble, no notes about what you read."""


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def find_claude():
    return shutil.which("claude") or os.path.expanduser("~/.local/bin/claude")


class Mind:
    """A long-lived `claude -p` conversation fed with stream-json turns."""

    def __init__(self):
        self.proc = None
        self.turns = 0
        self.lock = threading.Lock()

    def warm_up(self):
        with self.lock:
            if self.proc is None:
                self.start()

    def start(self):
        self.stop()
        work = os.path.join(HOME, "work")
        os.makedirs(work, exist_ok=True)
        self.proc = subprocess.Popen(
            [find_claude(), "-p", "--model", MODEL,
             "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
             "--tools", "", "--strict-mcp-config", "--disable-slash-commands",
             "--no-session-persistence", "--setting-sources", "",
             "--system-prompt", PERSONA],
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

    def ask(self, png):
        with self.lock:
            return self._ask(png)

    def _ask(self, png):
        if self.proc is None or self.proc.poll() is not None or self.turns >= MAX_TURNS:
            self.start()
        self.turns += 1
        content = [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                         "data": base64.b64encode(png).decode()}},
            {"type": "text", "text": "(new handwriting on the page)"},
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


def compose(answer, message_bottom, rng, endless=False):
    """Claude's answer -> the reply message for the tablet."""
    svg = None
    m = re.search(r"<svg.*?</svg>", answer, re.S)
    if m:
        svg = m.group(0)
        answer = answer[:m.start()] + answer[m.end():]
    text = re.sub(r"```\w*", "", answer).strip()

    art, art_size = [], (0, 0)
    if svg:
        try:
            art, art_size = ink.svg_to_strokes(svg)
        except Exception as e:  # a broken drawing should not lose the words
            log("svg failed:", e)

    left = RULED_LEFT if endless else LEFT
    width = PAGE_W - left - RIGHT

    def layout(top, scale):
        strokes, y = [], top
        if text and endless:
            # Put the first baseline on the next rule under the message, one line per rule.
            base = RULE0 + (max(0, top + 20 * scale - RULE0) // RULE_GAP + 1) * RULE_GAP
            lines, y = ink.write_text(text, left, base - 31 * scale, width, scale=scale,
                                      line_gap=RULE_GAP / scale, rng=rng, wobble=0.6, face=FACE, slant=SLANT)
            strokes += [{"c": [0, 0, 0], "w": INK_W, "n": NIB, "p": s} for s in lines]
        elif text:
            lines, y = ink.write_text(text, left, top, width, scale=scale, rng=rng, face=FACE)
            strokes += [{"c": [0, 0, 0], "w": INK_W, "p": s} for s in lines]
        if art:
            k = min(760 / art_size[0], 580 / art_size[1])
            ox, oy = left + (width - art_size[0] * k) / 2, y + 30
            for s in art:
                strokes.append({"c": list(s["c"]), "w": max(3, min(8, round(s["w"] * k * 0.7))),
                                "p": [(ox + x * k, oy + py * k) for x, py in s["p"]]})
            y = oy + art_size[1] * k + 20
        return strokes, y

    start = message_bottom + (10 if endless else 70)
    strokes, bottom = layout(start, SCALE)
    clear = False
    if bottom > PAGE_H - BOTTOM and not endless:
        # A fixed page: start again at the top of a clean one.
        clear = True
        strokes, bottom = layout(TOP, SCALE)
    out = []
    for s in strokes:
        flat = []
        for x, y in s["p"]:
            flat += [int(round(max(0, min(PAGE_W - 1, x)))), int(round(max(0, y if endless else min(PAGE_H - 1, y))))]
        out.append({"c": s["c"], "w": s["w"], "n": s.get("n", 0), "p": flat})
    # How long the reply stays in fading mode: time to read it once more.
    hold = 4 + 0.3 * len(text.split()) + (5 if art else 0)
    return {"clear": clear, "speed": 1700, "hold": hold, "bottom": int(bottom), "strokes": out}, text


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        peer = self.client_address[0]
        if peer not in self.server.allowed:
            log("refused", peer)
            return
        log("tablet connected")
        self.wfile.write(b'{"hello":1}\n')
        self.wfile.flush()
        mind = Mind()
        threading.Thread(target=mind.warm_up, daemon=True).start()  # warm up while they write
        rng = random.Random()
        try:
            for raw in self.rfile:
                try:
                    msg = json.loads(raw)
                except ValueError:
                    continue
                strokes = [list(zip(s[0::2], s[1::2])) for s in msg.get("strokes", []) if len(s) >= 2]
                if not strokes:
                    continue
                t = time.time()
                bottom = msg.get("at") or max(y for s in strokes for _, y in s)
                png = ink.strokes_png(strokes)
                if DEBUG:
                    with open(os.path.join(HOME, "last-message.png"), "wb") as f:
                        f.write(png)
                try:
                    answer = mind.ask(png)
                except Exception as e:
                    log("claude failed:", e)
                    answer = "The ink will not come... something is wrong on the other side. Try again in a moment."
                reply, text = compose(answer, bottom, rng, endless=bool(msg.get("endless")))
                log(f"replied in {time.time() - t:.1f}s, {len(reply['strokes'])} strokes"
                    + (f": {text!r}" if DEBUG else ""))
                self.wfile.write((json.dumps(reply, separators=(",", ":")) + "\n").encode())
                self.wfile.flush()
        except OSError:
            pass
        finally:
            mind.stop()
            log("tablet disconnected")


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def main():
    os.makedirs(HOME, exist_ok=True)
    tablet = sys.argv[1] if len(sys.argv) > 1 else ""
    if not tablet:
        try:
            with open(os.path.join(HOME, "tablet")) as f:
                tablet = f.read().strip()
        except OSError:
            pass
    if not tablet:
        log("no tablet address: pass it as an argument or put it in ~/.config/rmdiary/tablet")
    srv = Server(("0.0.0.0", PORT), Handler)
    srv.allowed = {"127.0.0.1", tablet} - {""}
    log(f"diary server on port {PORT} for {tablet}, model {MODEL}")
    srv.serve_forever()


if __name__ == "__main__":
    main()
