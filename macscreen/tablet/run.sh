#!/bin/sh
# Mac Screen: show a Mac window (see ~/remarkable-dev/macview on the Mac).
cd "$(dirname "$0")"
killall macview 2>/dev/null
exec ./macview 2>/tmp/macscreen.log
