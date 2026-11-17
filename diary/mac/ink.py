"""Ink helpers for the diary: handwriting from stroke fonts, SVG line art to
polylines, and a small PNG writer for the pen strokes Claude has to read.

Everything is a polyline: a list of (x, y) points in page pixels.
"""
import math
import os
import random
import re
import struct
import unicodedata
import xml.etree.ElementTree as ET
import zlib

FONT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fonts")

# ---------------------------------------------------------------- stroke font


def load_jhf(name):
    """Hershey .jhf file -> list of (left, right, strokes), one per ASCII 32.."""
    glyphs = []
    with open(os.path.join(FONT_DIR, name + ".jhf")) as f:
        lines = f.read().split("\n")
    i = 0
    while i < len(lines):
        line = lines[i]
        i += 1
        if len(line) < 10:
            continue
        pairs = int(line[5:8])
        data = line[8:]
        while len(data) < pairs * 2 and i < len(lines):
            data += lines[i]
            i += 1
        left, right = ord(data[0]) - 82, ord(data[1]) - 82
        strokes, cur = [], []
        for k in range(1, pairs):
            a, b = data[2 * k], data[2 * k + 1]
            if a == " " and b == "R":
                if cur:
                    strokes.append(cur)
                cur = []
            else:
                cur.append((ord(a) - 82, ord(b) - 82))
        if cur:
            strokes.append(cur)
        glyphs.append((left, right, strokes))
    return glyphs


_FONTS = {}


def font(name):
    if name not in _FONTS:
        _FONTS[name] = load_jhf(name)
    return _FONTS[name]


REPLACE = {
    "‘": "'", "’": "'", "“": '"', "”": '"', "—": " - ",
    "–": "-", "…": "...", " ": " ", "•": "-", "×": "x",
}


def to_ascii(text):
    for a, b in REPLACE.items():
        text = text.replace(a, b)
    text = unicodedata.normalize("NFKD", text)
    return "".join(c for c in text if 32 <= ord(c) < 127 or c == "\n")


def chaikin(pts, rounds=2):
    """Round the corners of a polyline."""
    for _ in range(rounds):
        if len(pts) < 3:
            return pts
        out = [pts[0]]
        for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
            out.append((0.75 * x0 + 0.25 * x1, 0.75 * y0 + 0.25 * y1))
            out.append((0.25 * x0 + 0.75 * x1, 0.25 * y0 + 0.75 * y1))
        out.append(pts[-1])
        pts = out
    return pts


def write_text(text, x0, y0, width, scale=2.7, face="scripts", line_gap=36, rng=None, wobble=1.0, slant=0.0):
    """Lay text out as handwriting. Returns (strokes, bottom_y).

    y0 is the top of the first line; its baseline is 31 * scale lower.
    wobble scales how far words drift off the line; slant leans the letters
    to the right (0.3 is a strong italic). The pen wanders a little so the lines do
    not look printed.
    """
    rng = rng or random.Random()
    glyphs = font(face)
    line_h = line_gap * scale
    space = 9 * scale

    def word_width(w):
        return sum((glyphs[ord(c) - 32][1] - glyphs[ord(c) - 32][0]) * scale for c in w)

    strokes = []
    y = y0 + 22 * scale  # baseline-ish origin: glyph y runs about -12 .. +9
    for para in to_ascii(text).split("\n"):
        x = x0 + rng.uniform(0, 14)
        drift = 0.0
        for word in para.split():
            ww = word_width(word)
            if x > x0 + 20 and x + ww > x0 + width:
                x = x0 + rng.uniform(0, 14)
                y += line_h
                drift = 0.0
            drift = max(-5.0 * wobble, min(5.0 * wobble, drift + rng.uniform(-1.6, 1.6) * wobble))
            tilt = rng.uniform(-0.035, 0.035)
            size = scale * rng.uniform(0.96, 1.04)
            for c in word:
                left, right, gs = glyphs[ord(c) - 32]
                for g in gs:
                    pts = []
                    for gx, gy in g:
                        px = (gx - left) * size
                        py = gy * size
                        pts.append((x + px - py * (tilt + slant), y + drift + py + px * tilt * 0.4))
                    pts = chaikin(pts)
                    strokes.append([(px + rng.uniform(-0.6, 0.6), py + rng.uniform(-0.6, 0.6)) for px, py in pts])
                x += (right - left) * size
            x += space * rng.uniform(0.85, 1.2)
        y += line_h
    return strokes, y - 22 * scale


# ---------------------------------------------------------------- SVG line art

NUM = r"[-+]?(?:\d*\.\d+|\d+\.?)(?:[eE][-+]?\d+)?"
TOKEN = re.compile(r"([MmLlHhVvCcSsQqTtAaZz])|(" + NUM + ")")
ARGS = {"M": 2, "L": 2, "H": 1, "V": 1, "C": 6, "S": 4, "Q": 4, "T": 2, "A": 7, "Z": 0}


def _bezier(p0, p1, p2, p3, n=14):
    out = []
    for i in range(1, n + 1):
        t = i / n
        u = 1 - t
        out.append((
            u * u * u * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t * t * t * p3[0],
            u * u * u * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t * t * t * p3[1],
        ))
    return out


def _arc(p0, rx, ry, phi, large, sweep, p1, n=20):
    """SVG endpoint arc -> points (excluding p0)."""
    if rx == 0 or ry == 0 or p0 == p1:
        return [p1]
    rx, ry = abs(rx), abs(ry)
    phi = math.radians(phi)
    cp, sp = math.cos(phi), math.sin(phi)
    dx, dy = (p0[0] - p1[0]) / 2, (p0[1] - p1[1]) / 2
    x1, y1 = cp * dx + sp * dy, -sp * dx + cp * dy
    lam = x1 * x1 / (rx * rx) + y1 * y1 / (ry * ry)
    if lam > 1:
        rx *= math.sqrt(lam)
        ry *= math.sqrt(lam)
    num = rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1
    den = rx * rx * y1 * y1 + ry * ry * x1 * x1
    co = math.sqrt(max(0.0, num / den)) if den else 0.0
    if large == sweep:
        co = -co
    cx1, cy1 = co * rx * y1 / ry, -co * ry * x1 / rx
    cx = cp * cx1 - sp * cy1 + (p0[0] + p1[0]) / 2
    cy = sp * cx1 + cp * cy1 + (p0[1] + p1[1]) / 2
    a0 = math.atan2((y1 - cy1) / ry, (x1 - cx1) / rx)
    a1 = math.atan2((-y1 - cy1) / ry, (-x1 - cx1) / rx)
    da = a1 - a0
    if sweep and da < 0:
        da += 2 * math.pi
    if not sweep and da > 0:
        da -= 2 * math.pi
    out = []
    for i in range(1, n + 1):
        a = a0 + da * i / n
        ex, ey = rx * math.cos(a), ry * math.sin(a)
        out.append((cp * ex - sp * ey + cx, sp * ex + cp * ey + cy))
    return out


def path_to_polylines(d):
    out, cur = [], []
    pos = start = (0.0, 0.0)
    last_ctrl = None
    last_cmd = ""
    tokens = [(m.group(1), m.group(2)) for m in TOKEN.finditer(d)]
    i = 0
    cmd = None
    while i < len(tokens):
        if tokens[i][0]:
            cmd = tokens[i][0]
            i += 1
        elif cmd is None:
            i += 1
            continue
        up = cmd.upper()
        n = ARGS[up]
        if i + n > len(tokens) or any(tokens[i + k][0] for k in range(n)):
            if up != "Z":
                break
        a = [float(tokens[i + k][1]) for k in range(n)]
        i += n
        rel = cmd.islower()
        ox, oy = pos if rel else (0.0, 0.0)
        if up == "M":
            if len(cur) > 1:
                out.append(cur)
            pos = start = (a[0] + ox, a[1] + oy)
            cur = [pos]
            cmd = "l" if rel else "L"  # further pairs are line-tos
        elif up == "L":
            pos = (a[0] + ox, a[1] + oy)
            cur.append(pos)
        elif up == "H":
            pos = (a[0] + ox, pos[1])
            cur.append(pos)
        elif up == "V":
            pos = (pos[0], a[0] + oy)
            cur.append(pos)
        elif up in "CS":
            if up == "C":
                c1 = (a[0] + ox, a[1] + oy)
                c2 = (a[2] + ox, a[3] + oy)
                end = (a[4] + ox, a[5] + oy)
            else:
                c1 = (2 * pos[0] - last_ctrl[0], 2 * pos[1] - last_ctrl[1]) if last_ctrl and last_cmd in "CS" else pos
                c2 = (a[0] + ox, a[1] + oy)
                end = (a[2] + ox, a[3] + oy)
            cur.extend(_bezier(pos, c1, c2, end))
            last_ctrl, pos = c2, end
        elif up in "QT":
            if up == "Q":
                c = (a[0] + ox, a[1] + oy)
                end = (a[2] + ox, a[3] + oy)
            else:
                c = (2 * pos[0] - last_ctrl[0], 2 * pos[1] - last_ctrl[1]) if last_ctrl and last_cmd in "QT" else pos
                end = (a[0] + ox, a[1] + oy)
            c1 = (pos[0] + 2 / 3 * (c[0] - pos[0]), pos[1] + 2 / 3 * (c[1] - pos[1]))
            c2 = (end[0] + 2 / 3 * (c[0] - end[0]), end[1] + 2 / 3 * (c[1] - end[1]))
            cur.extend(_bezier(pos, c1, c2, end))
            last_ctrl, pos = c, end
        elif up == "A":
            end = (a[5] + ox, a[6] + oy)
            cur.extend(_arc(pos, a[0], a[1], a[2], a[3] != 0, a[4] != 0, end))
            pos = end
        elif up == "Z":
            if cur:
                cur.append(start)
            pos = start
            if len(cur) > 1:
                out.append(cur)
            cur = [pos]
            cmd = None
        last_cmd = up
    if len(cur) > 1:
        out.append(cur)
    return out


COLORS = {
    "black": (0, 0, 0), "red": (200, 30, 30), "blue": (30, 60, 180), "green": (30, 130, 60),
    "yellow": (230, 190, 20), "orange": (230, 120, 20), "purple": (120, 50, 150),
    "pink": (220, 90, 140), "brown": (120, 75, 40), "gray": (110, 110, 110), "grey": (110, 110, 110),
    "cyan": (30, 150, 180), "magenta": (190, 40, 150),
}


def parse_color(s):
    s = (s or "").strip().lower()
    if s in COLORS:
        return COLORS[s]
    m = re.fullmatch(r"#([0-9a-f]{3}|[0-9a-f]{6})", s)
    if m:
        h = m.group(1)
        if len(h) == 3:
            h = "".join(c * 2 for c in h)
        rgb = tuple(int(h[k:k + 2], 16) for k in (0, 2, 4))
        # Near-white ink would be invisible on the page.
        return rgb if sum(rgb) < 600 else (110, 110, 110)
    return None


def _f(el, name, default=0.0):
    try:
        return float(re.match(NUM, el.get(name, "")).group(0))
    except (AttributeError, ValueError):
        return default


def _ellipse(cx, cy, rx, ry, n=40):
    return [(cx + rx * math.cos(2 * math.pi * k / n), cy + ry * math.sin(2 * math.pi * k / n)) for k in range(n + 1)]


def svg_to_strokes(svg):
    """SVG text -> (list of {"c": rgb, "w": width, "p": points}, (w, h))."""
    svg = re.sub(r'\sxmlns(:\w+)?="[^"]*"', "", svg)
    root = ET.fromstring(svg)
    vb = [float(v) for v in re.findall(NUM, root.get("viewBox", ""))]
    if len(vb) != 4:
        vb = [0, 0, _f(root, "width", 400), _f(root, "height", 300)]
    out = []

    def walk(el, color, width):
        tag = el.tag.split("}")[-1]
        style = dict(p.split(":", 1) for p in el.get("style", "").split(";") if ":" in p)
        stroke = el.get("stroke") or style.get("stroke", "").strip()
        if stroke and stroke != "none":
            color = parse_color(stroke) or color
        elif not stroke:
            # Shapes drawn only with a fill still get an outline in that colour.
            fill = el.get("fill") or style.get("fill", "").strip()
            if fill and fill != "none" and tag != "svg" and tag != "g":
                color = parse_color(fill) or color
        sw = el.get("stroke-width") or style.get("stroke-width")
        if sw:
            try:
                width = float(re.match(NUM, sw.strip()).group(0))
            except AttributeError:
                pass
        lines = []
        if tag == "path":
            lines = path_to_polylines(el.get("d", ""))
        elif tag == "line":
            lines = [[(_f(el, "x1"), _f(el, "y1")), (_f(el, "x2"), _f(el, "y2"))]]
        elif tag in ("polyline", "polygon"):
            n = [float(v) for v in re.findall(NUM, el.get("points", ""))]
            pts = list(zip(n[0::2], n[1::2]))
            if tag == "polygon" and pts:
                pts.append(pts[0])
            lines = [pts]
        elif tag == "circle":
            lines = [_ellipse(_f(el, "cx"), _f(el, "cy"), _f(el, "r"), _f(el, "r"))]
        elif tag == "ellipse":
            lines = [_ellipse(_f(el, "cx"), _f(el, "cy"), _f(el, "rx"), _f(el, "ry"))]
        elif tag == "rect":
            x, y, w, h = _f(el, "x"), _f(el, "y"), _f(el, "width"), _f(el, "height")
            lines = [[(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y)]]
        for pts in lines:
            if len(pts) > 1:
                out.append({"c": color, "w": width, "p": pts})
        for child in el:
            walk(child, color, width)

    walk(root, (0, 0, 0), 2.0)
    for s in out:
        s["p"] = [(x - vb[0], y - vb[1]) for x, y in s["p"]]
    return out, (vb[2] or 400, vb[3] or 300)


# ---------------------------------------------------------------- PNG of the pen strokes


def strokes_png(strokes, scale=0.5, pad=24):
    """Pen strokes (page pixels) -> PNG bytes, cropped to the ink."""
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
                row = img[yy]
                for xx in (xi, xi + 1):
                    if 0 <= xx < w:
                        row[xx] = 0

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
