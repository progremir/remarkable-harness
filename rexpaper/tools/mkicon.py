"""Draw the Rex app icon: a letter R and a cursor on a black rounded tile. 256x256 PNG."""
import struct, sys, zlib
N, SS = 256, 4            # final size, supersampling
M = N * SS
def seg_dist(px, py, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return ((px - ax - t * dx) ** 2 + (py - ay - t * dy) ** 2) ** 0.5
def inside_tile(x, y):
    r, m = 44, 14        # corner radius, margin
    cx = min(max(x, m + r), N - m - r); cy = min(max(y, m + r), N - m - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r
def ink(x, y):
    if not inside_tile(x, y): return 255
    # the R: stem, bowl and leg as thick strokes; then the cursor bar
    if min(seg_dist(x, y, *a, *b) for a, b in zip(R, R[1:])) <= 11: return 255
    if seg_dist(x, y, 118, 128, 150, 184) <= 11: return 255
    if 168 <= x <= 208 and 162 <= y <= 186: return 255
    return 0
import math
R = [(76, 184), (76, 72)] + [(112 + 28 * math.sin(t * math.pi / 12), 100 - 28 * math.cos(t * math.pi / 12)) for t in range(13)] + [(76, 128)]
rows = bytearray()
for y in range(N):
    rows.append(0)
    for x in range(N):
        tot = 0
        for sy in range(SS):
            for sx in range(SS):
                tot += ink(x + (sx + .5) / SS, y + (sy + .5) / SS)
        v = tot // (SS * SS)
        rows += bytes((v, v, v))
def chunk(t, d): return struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d))
open(sys.argv[1], 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', N, N, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(bytes(rows), 9)) + chunk(b'IEND', b''))
