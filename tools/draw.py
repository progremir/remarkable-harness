#!/usr/bin/env python3
"""Draw with the pen on the tablet from the Mac (open a notebook first).

    tools/draw.py X1 Y1 X2 Y2 [...]   one stroke through these points
    tools/draw.py strokes.json        [[[x, y], [x, y], ...], ...], one list per stroke

Coordinates are screenshot pixels (1620 x 2160, portrait). Long segments
are split into short steps so lines come out smooth.
"""
import json
import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rminput import PEN_DEV, pen_stroke_frames, send  # noqa: E402

STEP_PX = 6


def densify(points):
    out = [points[0]]
    for (x1, y1), (x2, y2) in zip(points, points[1:]):
        n = max(1, int(math.hypot(x2 - x1, y2 - y1) / STEP_PX))
        out += [(x1 + (x2 - x1) * i / n, y1 + (y2 - y1) * i / n) for i in range(1, n + 1)]
    return out


def main(argv):
    if len(argv) == 1 and argv[0].endswith(".json"):
        strokes = json.load(open(argv[0]))
    elif len(argv) >= 4 and len(argv) % 2 == 0:
        nums = [float(a) for a in argv]
        strokes = [list(zip(nums[0::2], nums[1::2]))]
    else:
        sys.exit(__doc__)
    frames = []
    for s in strokes:
        frames += pen_stroke_frames(densify([tuple(p) for p in s]))
    send(PEN_DEV, frames, 0.004)


if __name__ == "__main__":
    main(sys.argv[1:])
