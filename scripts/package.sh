#!/usr/bin/env bash
# Build the release package for this machine into dist/:
#
#   macOS (Apple Silicon)   dist/Multibrowser-<version>-macos-arm64.dmg   Multibrowser.app, drag to Applications
#   Linux (x86_64, arm64)   dist/multibrowser-<version>-linux-<arch>.tar.gz   binary, icons, menu entry, install.sh
#
# The release workflow runs this on each platform; run it yourself to try a package locally.
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[[ -n $version ]] || { echo "error: no version in Cargo.toml" >&2; exit 1; }
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"

cargo build --release --locked
bin=target/release/multibrowser
stage=dist/stage
rm -rf "$stage"
mkdir -p "$stage"

case "$(uname -s)" in
  Darwin)
    [[ $(uname -m) == arm64 ]] || { echo "error: macOS packages are Apple Silicon only" >&2; exit 1; }
    app=$stage/Multibrowser.app
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$bin" "$app/Contents/MacOS/multibrowser"
    cp assets/Multibrowser.icns "$app/Contents/Resources/"
    sed "s/__VERSION__/$version/g" packaging/macos/Info.plist >"$app/Contents/Info.plist"
    # Ad-hoc signature: Apple Silicon won't run an unsigned app. It isn't notarized, so
    # macOS asks for confirmation on first launch (see the README).
    codesign --force --sign - "$app"
    codesign --verify --strict "$app"
    ln -s /Applications "$stage/Applications"

    out=dist/Multibrowser-$version-macos-arm64.dmg
    rm -f "$out"
    # hdiutil sometimes fails with "Resource busy" on CI machines; retry.
    for attempt in 1 2 3 4 5; do
      if hdiutil create -volname "Multibrowser $version" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$out"; then
        break
      fi
      [[ $attempt == 5 ]] && exit 1
      sleep $((attempt * 3))
    done
    ;;
  Linux)
    name=multibrowser-$version-linux-$(uname -m)
    dir=$stage/$name
    mkdir -p "$dir/icons"
    cp "$bin" "$dir/"
    cp packaging/linux/multibrowser.desktop packaging/linux/install.sh "$dir/"
    cp assets/icon.svg assets/icon-128.png assets/icon-256.png assets/icon-512.png "$dir/icons/"
    out=dist/$name.tar.gz
    tar -C "$stage" -czf "$out" "$name"
    ;;
  *)
    echo "error: unsupported OS $(uname -s)" >&2
    exit 1
    ;;
esac

rm -rf "$stage"
echo "Built $out"
