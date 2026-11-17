#!/bin/sh
# Build the Mac server and put it in build/Mac for reMarkable.app.
# The app is signed with the keychain certificate "rmscreen local signing",
# so macOS keeps its Screen Recording and Accessibility permissions across
# rebuilds. Without that certificate, set SIGN=- for an ad-hoc signature
# (macOS then asks for the permissions again after each build).
set -e
cd "$(dirname "$0")"
SIGN=${SIGN:-rmscreen local signing}
APP="build/Mac for reMarkable.app"
mkdir -p "$APP/Contents/MacOS"
clang -fobjc-arc -O2 -Wall -Wno-deprecated-declarations rmscreen.m -o "$APP/Contents/MacOS/rmscreen" \
  -framework Foundation -framework AppKit -framework CoreText -framework CoreGraphics \
  -framework ScreenCaptureKit -framework CoreMedia -framework CoreVideo \
  -framework ApplicationServices -framework ImageIO -lz
cp app/Info.plist "$APP/Contents/Info.plist"
install -m 755 app/launch "$APP/Contents/MacOS/launch"
codesign --force --deep --sign "$SIGN" "$APP"
echo "built and signed $APP"
