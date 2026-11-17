#!/bin/sh
# Build PaperTerm from upstream plus this repo's changes (physical keyboard
# via evdev, rotation, faster typing) and install it as an AppLoad app.
# Needs: git, rustup target aarch64-unknown-linux-gnu, cargo-zigbuild, zig.
set -e
cd "$(dirname "$0")"
HERE=$(pwd)
SRC=build/paperterm
if [ ! -d "$SRC/.git" ]; then
  git clone -q https://github.com/MaximeRivest/paperterm "$SRC"
fi
git -C "$SRC" checkout -q -f "$(cat UPSTREAM_COMMIT)"
git -C "$SRC" clean -qfd -e target
git -C "$SRC" apply "$HERE/paperterm.patch"
cp files/*.rs "$SRC/src/"
(cd "$SRC" && cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.39)
STAGE=$(mktemp -d)/paperterm
mkdir -p "$STAGE"
install -m 755 "$SRC/target/aarch64-unknown-linux-gnu/release/paperterm" "$STAGE/"
install -m 644 "$SRC/external.manifest.json" "$SRC/icon.png" "$SRC/LICENSE" "$SRC/fonts/LICENSE.spleen" "$STAGE/"
../tools/push.sh "$STAGE" /home/root/xovi/exthome/appload
rm -rf "$(dirname "$STAGE")"
