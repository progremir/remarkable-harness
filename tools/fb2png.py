"""Convert a reMarkable qtfb framebuffer dump (1620x2160 RGB565 LE) to PNG."""
import struct
import sys
import zlib

W, H = 1620, 2160
raw = open(sys.argv[1], 'rb').read()
step = int(sys.argv[3]) if len(sys.argv) > 3 else 1  # downscale factor
rows = bytearray()
for y in range(0, H, step):
    rows.append(0)
    for x in range(0, W, step):
        v = raw[(y * W + x) * 2] | raw[(y * W + x) * 2 + 1] << 8
        rows += bytes(((v >> 11) * 255 // 31, ((v >> 5) & 63) * 255 // 63, (v & 31) * 255 // 31))


def chunk(tag, data):
    return struct.pack('>I', len(data)) + tag + data + struct.pack('>I', zlib.crc32(tag + data))


w, h = len(range(0, W, step)), len(range(0, H, step))
png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0))
png += chunk(b'IDAT', zlib.compress(bytes(rows), 6)) + chunk(b'IEND', b'')
open(sys.argv[2], 'wb').write(png)
