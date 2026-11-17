#!/bin/sh
# Install the page-tint patch and restart the reMarkable interface.
# Needs xovi with qt-resource-rebuilder and qt-command-executor.
set -e
cd "$(dirname "$0")"
DIR=/home/root/xovi/exthome/qt-resource-rebuilder
scp -q pageTint.qmd rm:/tmp/pageTint.qmd
ssh rm "set -e
test -f /home/root/xovi/extensions.d/qt-command-executor.so || { echo 'qt-command-executor is missing' >&2; exit 1; }
mv /tmp/pageTint.qmd $DIR/pageTint.qmd
chown root:root $DIR/pageTint.qmd && chmod 644 $DIR/pageTint.qmd
systemctl restart xochitl
ls -ln $DIR/pageTint.qmd"
