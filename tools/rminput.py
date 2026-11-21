"""Send fake touch and pen input to the tablet over SSH.

Coordinates are screenshot pixels (1620 x 2160, portrait, as tools/shot.sh
saves them). Events go straight into the kernel input devices, which deliver
them like a real finger or pen.

Each frame (a group of events ending in SYN_REPORT) is uploaded as its own
small file and written with one `cat`, so the kernel always receives whole
24-byte events: a write that splits an event is refused.
"""
import base64
import os
import struct
import subprocess

SCREEN_W, SCREEN_H = 1620, 2160

TOUCH_DEV, TOUCH_W, TOUCH_H = "/dev/input/event3", 2064, 2832   # "Elan touch input"
PEN_DEV, PEN_W, PEN_H = "/dev/input/event2", 11180, 15340       # "Elan marker input"

EV_SYN, EV_KEY, EV_ABS = 0, 1, 3
BTN_TOOL_PEN, BTN_TOUCH = 320, 330
ABS_X, ABS_Y, ABS_PRESSURE, ABS_DISTANCE = 0, 1, 24, 25
ABS_MT_SLOT, ABS_MT_TOUCH_MAJOR = 47, 48
ABS_MT_POSITION_X, ABS_MT_POSITION_Y = 53, 54
ABS_MT_TRACKING_ID, ABS_MT_PRESSURE = 57, 58


def ev(etype, code, value):
    # struct input_event on aarch64: 16-byte timeval, u16 type, u16 code, s32 value
    return struct.pack("<qqHHi", 0, 0, etype, code, value)


SYN = ev(EV_SYN, 0, 0)


def send(device, frames, pause_s):
    """Write each frame to `device` in one write, pausing between frames."""
    lines = ["set -e", "D=$(mktemp -d)", "exec 3>%s" % device]
    for i, f in enumerate(frames):
        lines.append("echo %s | base64 -d > $D/%d" % (base64.b64encode(f).decode(), i))
    for i in range(len(frames)):
        lines.append("cat $D/%d >&3" % i)
        if i < len(frames) - 1:
            lines.append("usleep %d" % int(pause_s * 1e6))
    lines.append("rm -rf $D")
    subprocess.run(["ssh", os.environ.get("RM_HOST", "rm"), "sh -s"], input="\n".join(lines).encode(), check=True)


# --- finger (multitouch protocol B) ---

def touch_frames(points):
    out = []
    for i, (x, y) in enumerate(points):
        tx, ty = round(x * TOUCH_W / SCREEN_W), round(y * TOUCH_H / SCREEN_H)
        f = b""
        if i == 0:
            f += ev(EV_ABS, ABS_MT_SLOT, 0) + ev(EV_ABS, ABS_MT_TRACKING_ID, 4242)
            f += ev(EV_ABS, ABS_MT_TOUCH_MAJOR, 20) + ev(EV_ABS, ABS_MT_PRESSURE, 80)
            f += ev(EV_KEY, BTN_TOUCH, 1)
        out.append(f + ev(EV_ABS, ABS_MT_POSITION_X, tx) + ev(EV_ABS, ABS_MT_POSITION_Y, ty) + SYN)
    out.append(ev(EV_ABS, ABS_MT_TRACKING_ID, -1) + ev(EV_KEY, BTN_TOUCH, 0) + SYN)
    return out


# --- pen ---

def pen_xy(x, y):
    return round(x * PEN_W / SCREEN_W), round(y * PEN_H / SCREEN_H)


def pen_stroke_frames(points, pressure=2600):
    """One pen stroke: hover in, touch down, move through points, lift, leave."""
    x0, y0 = pen_xy(*points[0])
    out = [ev(EV_KEY, BTN_TOOL_PEN, 1) + ev(EV_ABS, ABS_X, x0) + ev(EV_ABS, ABS_Y, y0)
           + ev(EV_ABS, ABS_DISTANCE, 40) + SYN]
    for i, (x, y) in enumerate(points):
        px, py = pen_xy(x, y)
        f = ev(EV_ABS, ABS_X, px) + ev(EV_ABS, ABS_Y, py) + ev(EV_ABS, ABS_PRESSURE, pressure)
        if i == 0:
            f = ev(EV_ABS, ABS_DISTANCE, 0) + ev(EV_KEY, BTN_TOUCH, 1) + f
        out.append(f + SYN)
    out.append(ev(EV_ABS, ABS_PRESSURE, 0) + ev(EV_KEY, BTN_TOUCH, 0) + ev(EV_ABS, ABS_DISTANCE, 40) + SYN)
    out.append(ev(EV_KEY, BTN_TOOL_PEN, 0) + SYN)
    return out
