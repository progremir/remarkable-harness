#!/bin/sh
# Copy a local folder to the tablet as root-owned files.
#
#   tools/push.sh <local-folder> <tablet-parent-folder>
#
# The folder lands at <tablet-parent-folder>/<name of local-folder>.
# Only that named folder is archived (never "."), and every entry is owned
# by uid/gid 0, so the copy cannot change the owner of /home/root or of
# any folder above the target. The script then prints the owners to check.
set -e
[ $# -eq 2 ] || { echo "usage: $0 <local-folder> <tablet-parent-folder>" >&2; exit 1; }
SRC=$(cd "$1" && pwd)
NAME=$(basename "$SRC")
DEST=$2
COPYFILE_DISABLE=1 tar -C "$(dirname "$SRC")" --uid 0 --gid 0 --no-xattrs -cf - "$NAME" \
  | ssh rm "mkdir -p '$DEST' && tar -C '$DEST' -xf - && ls -ldn '$DEST/$NAME' /home/root"
