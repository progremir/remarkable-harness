#!/bin/sh
# Start (or restart) the diary server in the background.
#   start.sh [tablet-ip]        log: ~/.config/rmdiary/diary.log
# DIARY_MODEL=sonnet answers faster; the default, opus, draws better.
cd "$(dirname "$0")"
mkdir -p "$HOME/.config/rmdiary"
pkill -f "diaryd.py" 2>/dev/null && sleep 0.5
nohup python3 diaryd.py "$@" >> "$HOME/.config/rmdiary/diary.log" 2>&1 &
echo "diary server started (log: ~/.config/rmdiary/diary.log)"
