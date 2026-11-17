"""Checks the app list: draws it, scrolls it with message 251, saves PNGs. No taps."""
import importlib.util, socket, struct, sys, time, zlib
spec = importlib.util.spec_from_file_location('tc', 'test_client.py')
W, H = 1620, 2160


def recv(s, n):
    b = b''
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise EOFError
        b += c
    return b


s = socket.create_connection(('127.0.0.1', 5900))
recv(s, 12); s.sendall(b'RFB 003.008\n')
recv(s, recv(s, 1)[0]); s.sendall(b'\x01'); recv(s, 4); s.sendall(b'\x01')
recv(s, 20); recv(s, struct.unpack('>I', recv(s, 4))[0])
s.sendall(struct.pack('>BxxxBBBBHHHBBBxxx', 0, 16, 16, 0, 1, 31, 63, 31, 11, 5, 0))
s.sendall(struct.pack('>BxHi', 2, 1, 6))
fb = bytearray(W * H * 2)
z = zlib.decompressobj()


def update(incremental):
    s.sendall(struct.pack('>BBHHHH', 3, incremental, 0, 0, W, H))
    assert recv(s, 1)[0] == 0
    recv(s, 1)
    for _ in range(struct.unpack('>H', recv(s, 2))[0]):
        x, y, w, h, enc = struct.unpack('>HHHHi', recv(s, 12))
        data = z.decompress(recv(s, struct.unpack('>I', recv(s, 4))[0])) if enc == 6 else recv(s, w * h * 2)
        for r in range(h):
            o = ((y + r) * W + x) * 2
            fb[o:o + w * 2] = data[r * w * 2:(r + 1) * w * 2]


def save(path):
    rows = bytearray()
    for y in range(0, H, 3):
        rows.append(0)
        for x in range(0, W, 3):
            v = fb[(y * W + x) * 2] | fb[(y * W + x) * 2 + 1] << 8
            rows += bytes(((v >> 11) * 255 // 31, ((v >> 5) & 63) * 255 // 63, (v & 31) * 255 // 31))
    ch = lambda t, d: struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d))
    open(path, 'wb').write(b'\x89PNG\r\n\x1a\n' + ch(b'IHDR', struct.pack('>IIBBBBB', len(range(0, W, 3)), len(range(0, H, 3)), 8, 2, 0, 0, 0)) + ch(b'IDAT', zlib.compress(bytes(rows))) + ch(b'IEND', b''))


update(0)
time.sleep(0.5)
save(sys.argv[1] + '/list.png')
s.sendall(struct.pack('>BBHHhh', 251, 0, 800, 1000, 0, -600))  # swipe up two fingers
update(1)
save(sys.argv[1] + '/list-scrolled.png')
s.close()
print('ok')
