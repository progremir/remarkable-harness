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
| Rex | Client for a [Rex](https://www.superlogical.com/rex/docs) server. Lists the server's sessions and tabs, shows a tab with its split panes, types into them, and makes, renames and closes them, over Tailscale. | [`rexpaper/`](rexpaper) |

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
| Rex | `rexpaper/install.sh [server]` | Builds the client and installs it in AppLoad. `server` is the Rex server's Tailscale name. |

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

## Rex details

Rex is a server that runs terminals and keeps them running. The Mac app and the `rex` command are clients of it, and `rexpaper/` is one more client, for the tablet.

Open **Rex** in AppLoad. It lists every session on the server and the tabs in each one, with a mark such as `[working]` or `[blocked]` for programs that report their status. Port forwards and detached blocks are listed under their session.

- **The list:**
  - Tap a tab to open it.
  - Tap a session's name for its menu: **New tab**, **Rename session**, **End session**.
  - **+ New session** makes a session with one shell and opens it.
  - Swipe up or down when the list is longer than the screen.
- **A tab** shows its panes split the way they are on the Mac:
  - Tap a pane to type into it. Its title strip turns black.
  - Drag a finger to scroll back.
  - The key row has Esc, Tab, Ctrl, ^C, the arrows and Enter, for use without a keyboard. **Ctrl** applies to the next key.
  - **< Sessions** in the bar goes back to the list.
- **Menu** (the last key of the row):
  - **New tab**, **Split right**, **Split down**, **Close this pane**, **Rename tab**.
  - **Zoom this pane** fills the screen with one pane, and **Show all panes** undoes it.
  - **Select text**, then drag over the text, copies it. **Paste** types it into the pane. Text copied in a Rex terminal on the Mac can be pasted too.
  - **Smaller text** and **Larger text** switch between three sizes. The choice is kept.
  - **Refresh screen** reads the terminals again and redraws everything.
- **Keyboards:**
  - The Type Folio works as it is.
  - For AppLoad's keyboard, swipe down from the top edge and tap the keyboard button at the top left. On the first key, the terminals and the bar move up to leave room for it. When you close the keyboard, choose **Use the whole screen** in the menu.
- **Handwriting:** write on a tab with the pen, anywhere over the terminals. About a second after you lift the pen, the ink is read and typed at the cursor of the pane you are in.
  - The text is not submitted. To submit it, end with a return arrow (the bent arrow on an Enter key), or tap **Enter**.
  - A finger still scrolls and taps. The pen taps the key row, the bar and the menus.
  - Claude reads the ink on the Mac, through its Claude Code login, so `claude` must be installed there. It takes 3 to 5 seconds. `SCRIBE_MODEL` in the Mac's shell profile picks the model (default `sonnet`).
- **Notifications:** the bar shows a line when a program in any terminal reports that it is blocked, done or failed, or sends a desktop notification. Tap the line to dismiss it.
- **Set-up:**
  - On the Mac, turn on Tailscale for the Rex server and read its name from `rex server status` (the `tailscale:` line).
  - Run `rexpaper/install.sh that-name.ts.net`. The name is kept on the tablet in `/home/root/.config/rexpaper/server`.
- **How it works:**
  - It connects through `tailscale nc` to port 443 of the server and speaks TLS and WebSocket itself (`src/wire.rs`).
  - Commands and events are JSON on the control connection, `/api/control` (`src/rex.rs`).
  - Each terminal on screen has a data connection, `/api/data`, that streams its output (`src/stream.rs`). The stream starts with a snapshot in Ghostty's format, which this client can't read. It skips the snapshot and asks for the screen as escape sequences with `com.superlogical.terminal.format`.
  - That read races with the stream, so the client reads the screen once more when the terminal goes quiet.
  - Handwriting goes through Rex too. The client starts `mac/scribe.py` in a terminal block of its own (`session.new_block`, labelled `rexpaper scribe`), types the strokes into it as a line of JSON, and reads the text off its screen when the block changes its title. No window shows that block, and it closes when the app does.
  - The output feeds the same emulator and e-ink renderer as PaperTerm. Keys go back with `com.superlogical.terminal.write`.
  - The server documents this protocol itself: `curl --unix-socket ~/Library/Application\ Support/rex/server.sock http://rex/llms.txt`.
- **Size:** opening a tab claims its terminals (`session.own_block`) and sizes each one to its pane on the tablet. The Mac app then shows them at that size. When another device takes one back, the bar says so, and the tablet shows that terminal's left columns and last rows. Tap the bar to fit them to the tablet again. Leaving the tab hands them back.
- **Connection:** the client pings the server every 20 seconds. When the connection drops, or the tablet wakes from sleep, it connects again by itself and reopens the tab you were in.
- **Settings:** each is an environment variable in the app's `run.sh`, or a file of the same name, in lower case and without the prefix, in `/home/root/.config/rexpaper/`.
  - `REXPAPER_SERVER`: the server. `host[:port]` goes through `tailscale nc`, `tls://host[:port]` connects directly, `unix:/path` uses a local socket.
  - `REXPAPER_FONT`: `12x24`, `16x32` (default) or `32x64`. The menu writes this file.
  - `REXPAPER_SCRIBE`: where `mac/scribe.py` is on the server's machine. `install.sh` writes this file.
  - `REXPAPER_KIND`: set it to `app` to receive the questions that actions in Rex's `init.lua` ask with `rex.ui`. The tablet then shows them and sends your answer. It is off by default, because the `rex` command picks "the app" by itself only while one app is connected. With the tablet as a second app, commands that act on a client need `-C`.
- **Try it without the tablet:** `REXPAPER_HEADLESS=frame.raw` draws into a file that `tools/fb2png.py` turns into a PNG, and writes the focused terminal's text to `frame.raw.txt`. For example, on the Mac:

  ```sh
  cd rexpaper && cargo build
  REXPAPER_SERVER="unix:$HOME/Library/Application Support/rex/server.sock" \
    REXPAPER_HEADLESS=/tmp/frame.raw REXPAPER_EXIT_AFTER=2 target/debug/rexpaper
  python3 -I ../tools/fb2png.py /tmp/frame.raw /tmp/frame.png 2
  ```

  These variables drive a headless run:

  | Variable | Effect |
  |---|---|
  | `REXPAPER_OPEN=<end of a block ID>` | Opens that terminal's tab. |
  | `REXPAPER_TYPE=<text>`, `REXPAPER_TYPE_AT=<seconds>` | Types the text, 2 seconds in unless told otherwise. |
  | `REXPAPER_TAPS="3:1540,2050 4:10,80-300,80"` | Taps and drags at times, in canvas pixels. |
  | `REXPAPER_ROT=<0-3>`, `REXPAPER_KBD=1` | Starts turned, or with room left for the keyboard. |
  | `REXPAPER_INK=strokes.json`, `REXPAPER_INK_AT=<seconds>` | Writes those strokes with the pen: `[[[x, y], ...], ...]`. |
  | `REXPAPER_DROP_AT=<seconds>` | Drops the connection, to see it come back. |
  | `REXPAPER_INJECT=<json>` | Handles a control message as if the server sent it. |
  | `REXPAPER_EXIT_AFTER=<seconds>` | Ends the run. |
- **Limits:**
  - Floating layers are not shown. The bar says how many a tab has.
  - A block that is not a terminal shows a few lines about itself instead of its content.
  - Handwriting can't be cancelled once the pen has rested, and the picture of it goes to Claude.
  - Copied text stays on the tablet: Rex has no call that sets the Mac's clipboard.
  - Rotation and waking from sleep were tested only in headless runs, and the Type Folio only with a virtual keyboard.
- **Credits:** the renderer, keyboard and rotation code come from PaperTerm (MIT, `LICENSE.paperterm`). The fonts are Spleen (`fonts/LICENSE.spleen`). `tools/mkicon.py` draws the icon.

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
