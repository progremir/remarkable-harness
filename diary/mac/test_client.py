#!/usr/bin/env python3
"""Try the diary without the tablet: writes a message in the handwriting font,
sends it to the server and saves a picture of the page with the reply.

  test_client.py "Hello, who are you?" [out.png] [host]
"""
import json
import os
import socket
import struct
import sys
import time
import zlib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ink

W, H, S = 1620, 2160, 0.5


def save(strokes, path):
    w, h = int(W * S), int(H * S)
    img = bytearray(b"\xff" * (w * h * 3))
    for s in strokes:
        c, r, nib = bytes(s["c"]), max(1, round(s["w"] * S / 2 + 0.3)), s.get("n", 0) * S
        pts = [(x * S, y * S) for x, y in zip(s["p"][0::2], s["p"][1::2])]
        for (ax, ay), (bx, by) in zip(pts, pts[1:] or pts):
            n = int(max(abs(bx - ax), abs(by - ay))) + 1
            for k in range(n + 1):
                x, y = ax + (bx - ax) * k / n, ay + (by - ay) * k / n
                if nib:  # a broad nib held at 40 degrees, as the tablet draws it
                    spots = [(int(x + t * 0.77), int(y - t * 0.64), 1) for t in range(-round(nib), round(nib) + 1)]
                else:
                    spots = [(int(x), int(y), r)]
                for sx, sy, sr in spots:
                    for yy in range(sy, sy + sr):
                        for xx in range(sx, sx + sr):
                            if 0 <= xx < w and 0 <= yy < h:
                                img[(yy * w + xx) * 3:(yy * w + xx) * 3 + 3] = c

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    raw = b"".join(b"\x00" + bytes(img[y * w * 3:(y + 1) * w * 3]) for y in range(h))
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def main():
    text = sys.argv[1] if len(sys.argv) > 1 else "Hello, who are you?"
    out = sys.argv[2] if len(sys.argv) > 2 else "page.png"
    host = sys.argv[3] if len(sys.argv) > 3 else "127.0.0.1"
    mine, _ = ink.write_text(text, 150, 150, 1300, scale=3.0, face="cursive")
    msg = {"strokes": [[int(v) for p in s for v in p] for s in mine], "endless": True}
    with socket.create_connection((host, 5910)) as sock:
        f = sock.makefile("rw")
        f.readline()  # greeting
        t = time.time()
        f.write(json.dumps(msg) + "\n")
        f.flush()
        reply = json.loads(f.readline())
    print(f"{time.time() - t:.1f}s, clear={reply['clear']}, {len(reply['strokes'])} strokes, bottom {reply['bottom']}")
    page = [] if reply["clear"] else [{"c": [60, 60, 60], "w": 4, "p": s} for s in msg["strokes"]]
    save(page + reply["strokes"], out)


if __name__ == "__main__":
    main()
