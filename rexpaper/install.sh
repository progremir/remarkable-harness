#!/bin/sh
# Build the Rex client (rexpaper) and install it as an AppLoad app.
# Needs: rustup target aarch64-unknown-linux-gnu, cargo-zigbuild, zig.
#
#   rexpaper/install.sh [server]
#
# `server` is the Rex server's Tailscale name, such as
# macbook16-rex.example.ts.net (see `rex server status` on the Mac). It is
# written to /home/root/.config/rexpaper/server on the tablet. Leave it out
# to keep the one already there.
set -e
cd "$(dirname "$0")"
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.39
STAGE=$(mktemp -d)/rexpaper
mkdir -p "$STAGE"
install -m 755 target/aarch64-unknown-linux-gnu/release/rexpaper run.sh "$STAGE/"
install -m 644 external.manifest.json icon.png LICENSE.paperterm fonts/LICENSE.spleen "$STAGE/"
ssh rm 'killall rexpaper 2>/dev/null; true'
../tools/push.sh "$STAGE" /home/root/xovi/exthome/appload
rm -rf "$(dirname "$STAGE")"
# Where the handwriting reader is on this Mac: the client starts it there.
ssh rm "mkdir -p /home/root/.config/rexpaper && printf '%s\n' '$PWD/mac/scribe.py' > /home/root/.config/rexpaper/scribe"
if [ -n "$1" ]; then
  ssh rm "mkdir -p /home/root/.config/rexpaper && printf '%s\n' '$1' > /home/root/.config/rexpaper/server && ls -ldn /home/root"
fi
