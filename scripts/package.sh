#!/usr/bin/env bash
# Build the release package for this machine into dist/:
#
#   macOS (Apple Silicon)   dist/Multibrowser-<version>-macos-arm64.dmg   Multibrowser.app, drag to Applications
#   Linux (x86_64, arm64)   dist/Multibrowser-<version>-linux-<arch>.AppImage
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
    arch=$(uname -m)
    # Pinned AppImage tooling (appimagetool 1.9.1, type2-runtime 20251108), verified by SHA-256.
    case $arch in
      x86_64)
        tool_sha=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
        runtime_sha=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
        ;;
      aarch64)
        tool_sha=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
        runtime_sha=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
        ;;
      *) echo "error: no AppImage tooling for $arch" >&2; exit 1 ;;
    esac
    tools=target/appimage-tools
    mkdir -p "$tools"
    fetch() { # url dest sha256
      if [[ ! -f $2 ]] || ! echo "$3  $2" | sha256sum -c --quiet 2>/dev/null; then
        curl -fsSL -o "$2.part" "$1"
        echo "$3  $2.part" | sha256sum -c --quiet || { echo "error: checksum mismatch for $1" >&2; exit 1; }
        mv "$2.part" "$2"
      fi
    }
    tool=$tools/appimagetool-$arch
    runtime=$tools/runtime-$arch
    fetch "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$arch.AppImage" "$tool" "$tool_sha"
    fetch "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-$arch" "$runtime" "$runtime_sha"
    chmod +x "$tool"

    appdir=$stage/Multibrowser.AppDir
    install -Dm755 "$bin" "$appdir/usr/bin/multibrowser"
    install -Dm644 packaging/linux/multibrowser.desktop "$appdir/usr/share/applications/multibrowser.desktop"
    for s in 128 256 512; do
      install -Dm644 "assets/icon-$s.png" "$appdir/usr/share/icons/hicolor/${s}x${s}/apps/multibrowser.png"
    done
    install -Dm644 assets/icon.svg "$appdir/usr/share/icons/hicolor/scalable/apps/multibrowser.svg"
    # AppImage layout: desktop entry, icon and AppRun at the root.
    cp packaging/linux/multibrowser.desktop "$appdir/"
    cp assets/icon-256.png "$appdir/multibrowser.png"
    ln -s multibrowser.png "$appdir/.DirIcon"
    cat >"$appdir/AppRun" <<'APPRUN'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/multibrowser" "$@"
APPRUN
    chmod +x "$appdir/AppRun"

    out=dist/Multibrowser-$version-linux-$arch.AppImage
    # Extract-and-run: appimagetool is itself an AppImage, and CI machines have no FUSE.
    ARCH=$arch APPIMAGE_EXTRACT_AND_RUN=1 "$tool" --no-appstream --runtime-file "$runtime" "$appdir" "$out"
    ;;
  *)
    echo "error: unsupported OS $(uname -s)" >&2
    exit 1
    ;;
esac

rm -rf "$stage"
echo "Built $out"
