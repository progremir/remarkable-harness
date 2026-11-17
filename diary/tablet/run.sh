#!/bin/sh
# Diary: write with the pen, pause, and the page answers (needs diaryd.py on the Mac).
cd "$(dirname "$0")"
killall diary 2>/dev/null
exec ./diary 2>/tmp/diary.log
