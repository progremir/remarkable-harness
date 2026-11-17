"""Unhash qmldiff QMD files with a hashtab, and hash names.

Formats follow asivery/qmldiff (src/hash.rs, src/hashtab.rs):
hashtab entries are big-endian u64 hash, u32 length, then the string;
hash is djb2-style with seed 5481 over the string's bytes.
"""
import re
import struct
import sys


def load(path):
    tab = {}
    data = open(path, 'rb').read()
    i = 0
    while i + 12 <= len(data):
        h, n = struct.unpack('>QI', data[i:i + 12])
        s = data[i + 12:i + 12 + n].decode('utf-8', 'replace')
        i += 12 + n
        if h:
            tab[h] = s
    return tab


def qhash(s, seed=5481):
    h = seed
    for ch in s:
        h = ((h << 5) + h + (ord(ch) & 0xFF)) & 0xFFFFFFFFFFFFFFFF
    return h


def unhash(text, tab):
    look = lambda n: tab.get(int(n), f'#{n}')
    text = re.sub(r'~&(\d+)&~', lambda m: look(m.group(1)), text)
    return re.sub(r'\[\[([\d.]+)\]\]',
                  lambda m: '.'.join(look(p) for p in m.group(1).split('.')), text)


if __name__ == '__main__':
    if sys.argv[1] == 'hash':
        for name in sys.argv[2:]:
            print(name, qhash(name))
        sys.exit()
    tab = load(sys.argv[1])
    for f in sys.argv[2:]:
        sys.stdout.write(f'===== {f}\n{unhash(open(f).read(), tab)}\n')
