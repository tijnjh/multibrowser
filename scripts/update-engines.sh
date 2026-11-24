#!/usr/bin/env bash
# Update every engine multibrowser runs.
#
#   1. Bumps the pinned Node.js (latest LTS) and playwright-core (latest release) in
#      src/runtime.rs. A new playwright-core brings new Chromium, Firefox and WebKit builds.
#   2. Rebuilds the app.
#   3. Runs `multibrowser --update`, which updates Ladybird to its latest release,
#      downloads the pinned Node.js, playwright-core and browsers (checksum-verified, the
#      same way the app does on first launch) and deletes versions that are no longer used.
#
# Usage: scripts/update-engines.sh [--check]
#   --check   only report what would change; edits and downloads nothing.
#
# Needs curl and python3 (for reading JSON). Honours MB_HOME like the app does.
set -euo pipefail

check=0
for arg in "$@"; do
  case "$arg" in
    --check) check=1 ;;
    -h | --help)
      sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown option: $arg (try --help)" >&2
      exit 2
      ;;
  esac
done

cd "$(dirname "$0")/.."
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"
for tool in cargo curl python3; do
  command -v "$tool" >/dev/null || { echo "error: $tool not found" >&2; exit 1; }
done

if [[ $check == 0 ]] && pgrep -x multibrowser >/dev/null; then
  echo "error: multibrowser is running; quit it first so its engines aren't replaced under it" >&2
  exit 1
fi

runtime=src/runtime.rs
pin() { sed -n "s/^pub const $1: &str = \"\(.*\)\";/\1/p" "$runtime"; }
cur_node=$(pin NODE_VERSION)
cur_pw=$(pin PLAYWRIGHT_VERSION)
[[ -n $cur_node && -n $cur_pw ]] || { echo "error: version pins not found in $runtime" >&2; exit 1; }

echo "Checking latest versions…"
latest_pw=$(curl -fsSL https://registry.npmjs.org/-/package/playwright-core/dist-tags |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["latest"])')
# Newest LTS release with builds for every platform the app supports.
latest_node=$(curl -fsSL https://nodejs.org/dist/index.json | python3 -c '
import json, sys
need = {"osx-arm64-tar", "linux-x64", "linux-arm64"}
print(next(r["version"] for r in json.load(sys.stdin) if r["lts"] and need <= set(r["files"])))')

changed=0
report() { # name current latest
  if [[ $2 == "$3" ]]; then
    echo "$1 $2: up to date"
  else
    echo "$1: $2 → $3"
    changed=1
  fi
}
report Node.js "$cur_node" "$latest_node"
report playwright-core "$cur_pw" "$latest_pw"

if [[ $changed == 1 && $check == 0 ]]; then
  python3 - "$runtime" "$latest_node" "$latest_pw" <<'PY'
import re, sys
path, node, pw = sys.argv[1:]
src = open(path).read()
src = re.sub(r'(pub const NODE_VERSION: &str = ")[^"]*(";)', rf'\g<1>{node}\g<2>', src)
src = re.sub(r'(pub const PLAYWRIGHT_VERSION: &str = ")[^"]*(";)', rf'\g<1>{pw}\g<2>', src)
open(path, "w").write(src)
PY
  echo "Updated the pins in $runtime."
fi

echo "Building…"
cargo build --release --quiet

echo
if [[ $check == 1 ]]; then
  echo "Data folder, for the versions pinned now:"
  target/release/multibrowser --update --check
else
  target/release/multibrowser --update
  echo
  if [[ $changed == 1 ]]; then
    echo "Done. Check the app, then commit the new pins in $runtime."
  else
    echo "Done."
  fi
fi
