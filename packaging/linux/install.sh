#!/bin/sh
# Install Multibrowser for the current user: the binary in ~/.local/bin, plus a menu
# entry and icons. Run it from the unpacked release folder.
#
#   ./install.sh               install (or upgrade)
#   ./install.sh --uninstall   remove it again
#
# Set PREFIX to install somewhere other than ~/.local (e.g. PREFIX=/usr/local, as root).
# The engines the app downloads live in ~/.local/share/multibrowser; uninstalling leaves
# them, so delete that folder too to free about 1.3 GB.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
prefix=${PREFIX:-$HOME/.local}
bin=$prefix/bin/multibrowser
desktop=$prefix/share/applications/multibrowser.desktop
icons=$prefix/share/icons/hicolor

refresh() {
  command -v update-desktop-database >/dev/null && update-desktop-database -q "$prefix/share/applications" 2>/dev/null || true
  command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$icons" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$bin" "$desktop" "$icons/scalable/apps/multibrowser.svg"
  for s in 128 256 512; do rm -f "$icons/${s}x${s}/apps/multibrowser.png"; done
  refresh
  echo "Removed Multibrowser from $prefix."
  exit 0
fi

install -Dm755 "$here/multibrowser" "$bin"
for s in 128 256 512; do
  install -Dm644 "$here/icons/icon-$s.png" "$icons/${s}x${s}/apps/multibrowser.png"
done
install -Dm644 "$here/icons/icon.svg" "$icons/scalable/apps/multibrowser.svg"
mkdir -p "$(dirname "$desktop")"
sed "s|^Exec=.*|Exec=$bin %u|" "$here/multibrowser.desktop" >"$desktop"
refresh

echo "Installed Multibrowser $("$bin" --version | cut -d' ' -f2) to $bin."
echo "Start it from your app menu, or run: multibrowser [url]"
case ":$PATH:" in
  *":$prefix/bin:"*) ;;
  *) echo "Note: $prefix/bin isn't on your PATH, so add it to run multibrowser from a terminal." ;;
esac
