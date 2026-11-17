---
name: remarkable
description: See and drive the user's reMarkable Paper Pro from the Mac. Take screenshots of the whole screen, tap, swipe, type on the on-screen keyboard, and draw with the pen, then check the result. Also covers installing and updating the user's tablet mods (page tint, Mac Screen, PaperTerm, xovi extensions) safely. Use whenever a task involves the reMarkable tablet, such as "open X on the tablet", "draw something on the remarkable", "check that the patch works", "take a screenshot of the tablet", "install this on the remarkable", or verifying any UI change on it yourself rather than asking the user.
---

# reMarkable harness

The tools live in `~/remarkable-dev/remarkable-harness` (GitHub: progremir/remarkable-harness). The tablet is reachable as `ssh rm` (root). All coordinates are **screenshot pixels: 1620 × 2160, portrait**, the same as the images `shot.sh` saves.

| Do this | Command (run from the repo) |
|---|---|
| Screenshot the whole screen | `tools/shot.sh out.png [delay_ms]` |
| Tap | `python3 -I tools/tap.py X Y` |
| Swipe | `python3 -I tools/tap.py X1 Y1 X2 Y2 [ms]` |
| Draw one pen stroke | `python3 -I tools/draw.py X1 Y1 X2 Y2 ...` |
| Draw many strokes | `python3 -I tools/draw.py strokes.json` (`[[[x,y],...], ...]`) |
| See what an AppLoad app draws | read `/dev/shm/qtfb_*` and convert it with `tools/fb2png.py` |
| Copy a folder to the tablet | `tools/push.sh <folder> <tablet-parent-folder>` |

## Working loop

1. **Look first.** Take a screenshot before acting, and shrink it to read it: `sips -Z 900 out.png` (a scale of 2.4 from the small image back to screen pixels).
2. **Act.** Do one tap, swipe or drawing at a time.
3. **Check.** Take a screenshot after every action, with a delay of 500–1500 ms for animations and e-ink refresh, and confirm the action did what you meant before the next step.

Put screenshots and stroke files in the session scratchpad, not in the repo.

## Places on screen

These are in full-size pixels and were measured on OS 3.29.1, portrait.

- **My files:**
  - Menu ☰: (58, 60).
  - Status icons, which open quick settings: (1450, 60).
  - Bottom bar: search (706, 1999), **+** (809, 1999), calendar (912, 1999).
  - **+** opens a "Create new" menu. Notebook is at (751, 1603).
- **New notebook dialog:**
  - Cancel: (77, 55). Create: (1543, 55).
  - Templates: Blank (1111, 888), Lined medium (1416, 888), Grid medium (202, 1229).
- **Inside a notebook:**
  - Close ✕: (1562, 55).
  - Undo: (55, 838). Redo: (55, 950).
  - The writing area starts to the right of x ≈ 120.
- **On-screen keyboard (row centres):**
  - QWERTY row: y 1697.
  - ASDF row: y 1822.
  - ZXCV row: y 1946.
  - Spacebar: y 2083.
  - To find a key's position, take a screenshot.
- **Close quick settings:** tap anywhere outside the panel, e.g. (700, 1500).

When a screenshot disagrees with these numbers, trust the screenshot.

## Rules

- **Draw only in a new notebook.** Create a fresh one first, never draw in the user's existing notebooks. Don't delete or rename the user's files unless they ask.
- **Undo test strokes.** Remove them with Undo once they've served their purpose.
- **Only `tools/push.sh` copies to the tablet.** Never tar `.` into `/home/root`. Every copied file must be owned by root, and `/home/root` must stay owned by uid 0. Afterwards check with `ls -ldn /home/root`.
- **Keep secrets off the tablet's outputs.** Never print or copy `/home/root/.config/remarkable/xochitl.conf` or Tailscale state.
- **Restarting the interface.** `systemctl restart xochitl`, or `/home/root/xovi/start` when the xovi extensions changed. The main screen needs about 20 seconds to come back, so wait before taking a screenshot.
- **Shell loops.** The shell is zsh, so loops over "x y" pairs don't split words. Run separate commands, or loop in Python.

## How it works

- **Screenshots:** they come from the rm-shot xovi extension, through the message broker: `echo '>etakeScreenshot:/tmp/x.png,DELAY' > /run/xovi-mb`. It needs `framebuffer-spy.so` and `xovi-message-broker.so` in `xovi/extensions.d`.
- **Touch:**
  - Multitouch events go to `/dev/input/event3` ("Elan touch input", range 2064 × 2832).
  - Pen events go to `/dev/input/event2` ("Elan marker input", range 11180 × 15340, pressure up to 4096).
  - `tools/rminput.py` scales screen pixels to these ranges. It sends each group of events in a single write, because the kernel refuses a write that splits a 24-byte event.

## Diary app

The Diary app (`diary/`) writes back in handwriting. It needs its server on the Mac: `diary/mac/start.sh` (log in `~/.config/rmdiary/diary.log`). It opens as a bare page where ink fades; hold the bottom left corner 2 seconds (`tap.py 100 2060 102 2062 2800`) for the notebook mode, whose bar is at the bottom (y 2110). Test on a new notebook page, not the user's. To test it, open Diary in AppLoad, write a message with `tools/draw.py` using strokes from `ink.write_text(...)` in `diary/mac/ink.py`, wait, and take a screenshot. `diary/mac/test_client.py` tests the server alone. Don't pass `shot.sh` a delay above about 2000 ms; sleep first instead.

For the full list of mods, how to build each one and the interface-patch notes, see the repo's `README.md`.
