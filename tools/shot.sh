#!/bin/sh
# Take a screenshot of the whole tablet screen (main interface included)
# and copy it to the Mac. Uses the rm-shot xovi extension.
#
#   tools/shot.sh [output.png] [delay_ms]
set -e
OUT=${1:-shot.png}
DELAY=${2:-0}
ssh rm "rm -f /tmp/rm-shot.png
echo '>etakeScreenshot:/tmp/rm-shot.png,$DELAY' > /run/xovi-mb
timeout 10 cat /run/xovi-mb-out >/dev/null
i=0; while [ ! -s /tmp/rm-shot.png ] && [ \$i -lt 20 ]; do sleep 0.5; i=\$((i+1)); done
test -s /tmp/rm-shot.png"
scp -q rm:/tmp/rm-shot.png "$OUT"
echo "$OUT"
