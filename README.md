# reMarkable harness

Tools to see and drive my reMarkable Paper Pro (OS 3.29.1, developer mode) from the Mac: screenshots, taps, swipes and pen drawing. Also everything installed on the tablet, with the source and install script for each custom part, and links and versions for the third-party parts.

`skill/` is a Claude Code skill (`remarkable`) that teaches Claude to use these tools. Install it with:

```sh
ln -sfn "$PWD/skill" ~/.claude/skills/remarkable
```

Scripts run on the Mac and reach the tablet with `ssh rm`.

## What runs on the tablet

### Custom (source in this repo)

| Part | What it does | Folder |
|---|---|---|
| Page tint | Yellow tint over the interface. A 5-step slider sits next to brightness in quick settings. It hides the Type Folio backlight slider. | [`page-tint/`](page-tint) |
| Mac Screen | Shows one Mac window on the tablet over Tailscale. One finger moves the pointer, two fingers scroll. Rotation resizes the Mac window. It also forwards the keyboard. | [`macscreen/`](macscreen) |
| Diary | A page that writes back. Write with the pen and pause; your ink dissolves, Claude reads it, and the answer is written out with a quill and then fades. It also has a notebook mode that keeps the ink. | [`diary/`](diary) |
| PaperTerm | Terminal, based on [MaximeRivest/paperterm](https://github.com/MaximeRivest/paperterm). Adds Type Folio keyboard input through evdev, rotation and faster typing. | [`paperterm/`](paperterm) |

### Third-party

Most of these are [Vellum](https://github.com/vellum-dev/vellum) packages.

| Part | Version | Source | Notes |
|---|---|---|---|
| xovi | | [asivery/xovi](https://github.com/asivery/xovi) | Extension loader. `xovi/start` runs again after each reboot. |
| AppLoad | | [asivery/rm-appload](https://github.com/asivery/rm-appload) | App launcher and qtfb, plus the qtfb shims in `appload/shims`. |
| qt-resource-rebuilder | | [asivery/rm-xovi-extensions](https://github.com/asivery/rm-xovi-extensions) | Applies the `.qmd` interface patches. |
| qt-command-executor | 19.0.1 | [asivery/rm-xovi-extensions](https://github.com/asivery/rm-xovi-extensions) | Lets QML run commands. Page tint and Bluetooth settings need it. |
| bluetooth-settings | 0.2.1 | [rmitchellscott/xovi-bluetoothsettings](https://github.com/rmitchellscott/xovi-bluetoothsettings) | Bluetooth page in Settings. Files: `bluetoothSettings.qmd`, `bluetooth.rcc`. |
| quicksettings-bluetooth | 1.0.1 | Vellum | Bluetooth toggle in quick settings. |
| tap-to-change-view-or-page | 1.0.1 | [ingatellent/xovi-qmd-extensions](https://github.com/ingatellent/xovi-qmd-extensions) | Tap the page edges to turn pages. |
| scroll-screen-up-or-down | 1.0.1 | [ingatellent/xovi-qmd-extensions](https://github.com/ingatellent/xovi-qmd-extensions) | Scrolls the screen up or down. |
| rm-shot | 1.2.0 | [rmitchellscott/rm-shot](https://github.com/rmitchellscott/rm-shot) | Screenshots of the whole screen (`tools/shot.sh`). Needs framebuffer-spy and xovi-message-broker, which ship with xovi and are copied from `inactive-extensions` into `extensions.d`. |
| KOReader | 2026.07.1 | [koreader/koreader](https://github.com/koreader/koreader) | AppLoad app. |
| Tailscale | 1.104.5 | Vellum | Userspace networking. |
| mount-utils | 1.2.0 | Vellum | |
| curl | 8.11.0 | Vellum | `/home/root/.vellum/bin/curl` |
| Amp CLI | | ampcode.com | `/home/root/.amp/bin/amp`, used from PaperTerm. |

There is no TUN device on this OS: the tun module is only built for 3.28. Tailscale therefore runs with `--tun userspace-networking`, and outgoing connections to the tailnet go through `tailscale nc`. Its unit file lives on the root filesystem, so an OS update removes it. The hook `.vellum/hooks/post-os-upgrade/tailscale` puts it back.

[`tablet-snapshot/`](tablet-snapshot) holds a copy of the tablet's patch files, app manifests, the list of xovi extensions and the Tailscale flags. It's there to compare against after an OS update. Secrets (`xochitl.conf`, Tailscale state) are left out on purpose.

## Install

You can rebuild the custom parts from this repo. The results of these checks on 2026-11-17:

- `macscreen/tablet` and `paperterm` (upstream plus `paperterm.patch`) build binaries that are byte-for-byte the same as the ones on the tablet.

| Part | Command | Effect |
|---|---|---|
| Page tint | `page-tint/install.sh` | Copies the patch and restarts the interface. |
| Mac Screen, tablet | `macscreen/tablet/install.sh` | Builds the viewer and installs it in AppLoad. |
| Mac Screen, Mac | `macscreen/mac/build.sh` | Builds `build/Mac for reMarkable.app`. Put the tablet's Tailscale address in `~/.config/rmscreen/tablet`, then run that app on the Mac. |
| Diary, tablet | `diary/tablet/install.sh` | Builds the app and installs it in AppLoad. |
| Diary, Mac | `diary/mac/start.sh` | Starts the server that asks Claude. It must be running while you use the app. |
| PaperTerm | `paperterm/install.sh` | Clones upstream, applies the changes, builds and installs. |

Rust parts need `rustup target add aarch64-unknown-linux-gnu`, `cargo-zigbuild` and `zig`.

The Mac app is signed with the keychain certificate "rmscreen local signing", so its Screen Recording and Accessibility permissions survive rebuilds. On another Mac, create a code-signing certificate with that name, or build with `SIGN=-`.

**Copying to the tablet.** Use `tools/push.sh <folder> <tablet-parent>`. It archives only the named folder and makes every entry owned by root. Don't tar `.` into `/home/root`. That once made `/home/root` owned by the Mac user and broke SSH.

## Mac Screen details

- **Mac side** (`macscreen/mac/rmscreen.m`):
  - A small RFB (VNC) server. It lists windows and apps (with search and pins), mirrors one window with ScreenCaptureKit, and sends zlib-compressed updates.
  - Touch and keyboard input reach the Mac as CGEvents. Taps, drags, two-finger scroll (message 251) and long-press right-click are supported.
  - It resizes the window to the tablet's shape and handles orientation (message 250).
  - It only accepts the tablet's addresses (Tailscale and USB) and localhost.
  - Logs go to `~/.config/rmscreen/`.
- **Tablet side** (`macscreen/tablet/`):
  - Rust qtfb app with a chooser for which Mac to connect to (from `tailscale status --json`).
  - It connects through `tailscale nc`.
  - It grabs the Type Folio keyboard while open.
  - Set `MACVIEW_TEST` to run its self-test.
- **Screenshots:** `tools/shot.sh out.png` captures the whole tablet screen, main interface included, through rm-shot.
- **Taps and swipes:** `tools/tap.py X Y` taps and `tools/tap.py X1 Y1 X2 Y2 [ms]` swipes, in screenshot pixels. It writes touch events into the touchscreen device (`/dev/input/event3`).
- **Drawing:** `tools/draw.py X1 Y1 X2 Y2 ...` draws one pen stroke, and `tools/draw.py strokes.json` draws many. Open a notebook first. It writes pen events into the pen device (`/dev/input/event2`). Both tools send each event group in one write, because the kernel refuses a write that splits an event (`tools/rminput.py`).
- **Test clients:**
  - `macscreen/mac/test_client.py` and `test_list.py` connect to the server locally.
  - `tools/fb2png.py` turns an app's framebuffer (`/dev/shm/qtfb_*`, RGB565) into a PNG, so you can see what an app draws without taking a screenshot.

## Diary details

Open **Diary** in AppLoad. It is a bare page with nothing else on screen. Write with the pen and stop for about 3 seconds:

1. Your message dissolves into the paper.
2. Three dots show while the answer is on its way.
3. The answer is written at the top of the page with a quill: a slanted hand with thick and thin strokes.
4. It stays a few seconds, longer for longer answers, then dissolves. Writing again makes it go at once.

**Notebook mode** keeps the ink instead. Hold a finger in the bottom left corner for 2 seconds to open it. It has ruled paper, pages, finger scrolling on long pages and a bar at the bottom: `<` and `>` turn pages, `+` adds one, **Wipe** (tap twice) clears the page, **Diary** goes back. Notebook pages are saved on the tablet in `/home/root/.local/share/diary/pages.json`.

- **How it works:**
  - The tablet app (`diary/tablet`) reads the pen straight from the digitizer, records strokes and replays the strokes it is sent.
  - It sends the new strokes to `diary/mac/diaryd.py` on the Mac, port 5910, through `tailscale nc`.
  - The server turns the strokes into a picture and gives it to `claude -p`, using the Claude Code login on the Mac, so it needs no API key.
  - It turns the answer into pen strokes: text through a single-line Hershey font drawn with a broad nib, drawings from the SVG line art Claude writes.
- **Memory:** one Claude conversation lasts for as long as the app stays open. Closing the app starts a fresh one.
- **Settings on the Mac** (environment variables for `diary/mac/start.sh`):
  - `DIARY_MODEL`: `opus` (default, draws better) or `sonnet` (faster). Text takes about 2 seconds, a drawing about 25.
  - `DIARY_SCALE` (2.4), `DIARY_FACE` (`cursive` or `scripts`), `DIARY_NIB` (5; 0 is a round pen), `DIARY_SLANT` (0.3): the handwriting.
  - The persona, reply length and drawing rules are the `PERSONA` text in `diaryd.py`. Drawings are made only when asked for.
- **Settings on the tablet** (in the app's `run.sh`):
  - `DIARY_PAUSE_MS` (2800): how long the pen must rest before a message is sent.
  - `DIARY_REFRESH`: `ufast` (default), `fast` or `default` screen mode.
  - The Mac's Tailscale address is read from `/home/root/.config/diary/host`. Create that file once.
  - On the Mac, the tablet's Tailscale address goes in `~/.config/rmdiary/tablet` (or pass it to `start.sh`).
- **Try it without the tablet:** `diary/mac/test_client.py "Draw me an owl" page.png` saves a picture of the page with the reply.
- **Limits:** replies use ASCII letters only, and the server accepts connections only from the tablet's address and from the Mac itself.
- **Why the bar is at the bottom:** AppLoad shows its own title bar with a close button over the top of the screen when the top is touched.
- **Fonts:** `diary/mac/fonts/*.jhf` are the Hershey fonts (free to use; credit: A. V. Hershey, U.S. National Bureau of Standards).

## Interface patches

- **Patch files.** `.qmd` files in `/home/root/xovi/exthome/qt-resource-rebuilder/` change the reMarkable interface's QML. Patches can be written with plain names.
- **Hashed patches.** Published patches often use hashed names. `tools/unhash.py` decodes them with the tablet's `hashtab` (djb2, seed 5481).
- **Reading the stock QML.** The original files are zstd frames inside `/usr/bin/xochitl`, each with a 4-byte big-endian length prefix.
- **No shaders.** The renderer (`libqsgepaper`) has no shaders or blend modes. That's why page tint is a see-through layer and not a colour filter.
