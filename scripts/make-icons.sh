#!/usr/bin/env bash
# Regenerate the app icons from assets/icon.svg (macOS; needs rsvg-convert: `brew install librsvg`).
#   assets/Multibrowser.icns          macOS app bundle icon
#   assets/icon-{128,256,512}.png     Linux hicolor icons (the SVG is installed as the scalable one)
# The outputs are committed, so building and releasing don't need these tools.
set -euo pipefail
cd "$(dirname "$0")/../assets"
svg=icon.svg
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

iconset="$tmp/Multibrowser.iconset"
mkdir "$iconset"
for s in 16 32 128 256 512; do
  rsvg-convert -w $s -h $s "$svg" -o "$iconset/icon_${s}x${s}.png"
  rsvg-convert -w $((s * 2)) -h $((s * 2)) "$svg" -o "$iconset/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$iconset" -o Multibrowser.icns

for s in 128 256 512; do
  rsvg-convert -w $s -h $s "$svg" -o "icon-$s.png"
done
echo "Wrote $(pwd)/Multibrowser.icns and icon-{128,256,512}.png"
