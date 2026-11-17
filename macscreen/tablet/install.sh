#!/bin/sh
# Build the Mac Screen viewer (macview) and install it as an AppLoad app.
# Needs: rustup target aarch64-unknown-linux-gnu, cargo-zigbuild, zig.
set -e
cd "$(dirname "$0")"
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.39
STAGE=$(mktemp -d)/macscreen
mkdir -p "$STAGE"
install -m 755 target/aarch64-unknown-linux-gnu/release/macview run.sh "$STAGE/"
install -m 644 external.manifest.json icon.png "$STAGE/"
ssh rm 'killall macview 2>/dev/null; true'
../../tools/push.sh "$STAGE" /home/root/xovi/exthome/appload
rm -rf "$(dirname "$STAGE")"
