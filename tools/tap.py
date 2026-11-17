#!/usr/bin/env python3
"""Tap or swipe on the tablet's touchscreen from the Mac.

    tools/tap.py X Y                 tap at screen pixel (X, Y)
    tools/tap.py X1 Y1 X2 Y2 [ms]    swipe from (X1, Y1) to (X2, Y2)

Coordinates are screenshot pixels (1620 x 2160, portrait).
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rminput import TOUCH_DEV, send, touch_frames  # noqa: E402


def main(argv):
    nums = [int(a) for a in argv]
    if len(nums) == 2:
        points, ms = [(nums[0], nums[1])] * 2, 60
    elif len(nums) in (4, 5):
        x1, y1, x2, y2 = nums[:4]
        ms = nums[4] if len(nums) == 5 else 300
        n = 12
        points = [(x1 + (x2 - x1) * i / n, y1 + (y2 - y1) * i / n) for i in range(n + 1)]
    else:
        sys.exit(__doc__)
    send(TOUCH_DEV, touch_frames(points), ms / 1000 / len(points))


if __name__ == "__main__":
    main(sys.argv[1:])
