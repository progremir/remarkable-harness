"""Test client for rmscreen: behaves like the tablet viewer and checks updates.

Connects to localhost, asks for RGB565 + Zlib like VNSee does, taps the first
window in the picker, then decodes a few updates and saves the result as a PNG.
"""
import socket
import struct
import sys
import time
import zlib

W, H = 1620, 2160


def recv(s, n):
    buf = b''
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise EOFError('server closed')
        buf += chunk
    return buf


def main(out_png):
    s = socket.create_connection(('127.0.0.1', 5900))
    assert recv(s, 12) == b'RFB 003.008\n'
    s.sendall(b'RFB 003.008\n')
    n = recv(s, 1)[0]
    assert 1 in recv(s, n)
    s.sendall(b'\x01')
    assert struct.unpack('>I', recv(s, 4))[0] == 0
    s.sendall(b'\x01')
    w, h = struct.unpack('>HH', recv(s, 4))
    recv(s, 16)
    recv(s, struct.unpack('>I', recv(s, 4))[0])
    assert (w, h) == (W, H)
    # RGB565 little-endian, like the tablet framebuffer.
    s.sendall(struct.pack('>BxxxBBBBHHHBBBxxx', 0, 16, 16, 0, 1, 31, 63, 31, 11, 5, 0))
    s.sendall(struct.pack('>BxHi', 2, 1, 6))
    fb = bytearray(W * H * 2)
    z = zlib.decompressobj()

    def request(incremental):
        s.sendall(struct.pack('>BBHHHH', 3, incremental, 0, 0, W, H))

    def read_update():
        t, = struct.unpack('>B', recv(s, 1))
        assert t == 0, t
        recv(s, 1)
        nrect, = struct.unpack('>H', recv(s, 2))
        total = 0
        for _ in range(nrect):
            x, y, rw, rh, enc = struct.unpack('>HHHHi', recv(s, 12))
            if enc == 6:
                clen, = struct.unpack('>I', recv(s, 4))
                data = z.decompress(recv(s, clen))
                total += clen
            else:
                assert enc == 0, enc
                data = recv(s, rw * rh * 2)
                total += len(data)
            assert len(data) == rw * rh * 2, (len(data), rw, rh)
            for row in range(rh):
                o = ((y + row) * W + x) * 2
                fb[o:o + rw * 2] = data[row * rw * 2:(row + 1) * rw * 2]
        return nrect, total

    # Like libvncclient: one request, then wait for its answer before the next.
    request(0)
    print('picker update', read_update())
    request(1)
    time.sleep(4)  # sit through a picker refresh with the request outstanding
    # Tap the first row of the picker (down, up).
    s.sendall(struct.pack('>BBHH', 5, 1, 300, 275) + struct.pack('>BBHH', 5, 0, 300, 275))
    for i in range(4):
        s.settimeout(8)
        t0 = time.time()
        print('update', i, read_update(), f'{time.time() - t0:.2f}s')
        request(1)
    s.close()

    # Save as PNG (RGB565 -> RGB888).
    rgb = bytearray()
    for yy in range(0, H, 1):
        rgb.append(0)
        for xx in range(W):
            v = fb[(yy * W + xx) * 2] | fb[(yy * W + xx) * 2 + 1] << 8
            rgb += bytes(((v >> 11) * 255 // 31, ((v >> 5) & 63) * 255 // 63, (v & 31) * 255 // 31))

    def chunk(tag, data):
        return struct.pack('>I', len(data)) + tag + data + struct.pack('>I', zlib.crc32(tag + data))

    png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', W, H, 8, 2, 0, 0, 0))
    png += chunk(b'IDAT', zlib.compress(bytes(rgb), 6)) + chunk(b'IEND', b'')
    open(out_png, 'wb').write(png)
    print('saved', out_png)


if __name__ == '__main__':
    main(sys.argv[1])
