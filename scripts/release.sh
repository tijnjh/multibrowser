#!/usr/bin/env bash
# Cut a release:
#
#   scripts/release.sh 0.2.0          # or 0.2.0-beta.1 for a prerelease
#
# Sets the version in Cargo.toml, commits it, tags v<version> and, after you confirm,
# pushes the commit and the tag. GitHub Actions (.github/workflows/release.yml) then
# builds the macOS and Linux packages and publishes the GitHub release, in about 15 minutes.
#
# Pass --yes to skip the confirmation.
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

yes=0
version=
for arg in "$@"; do
  case "$arg" in
    -y | --yes) yes=1 ;;
    -h | --help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) die "unknown option $arg" ;;
    *) version=${arg#v} ;;
  esac
done
[[ -n $version ]] || die "usage: scripts/release.sh <version>, e.g. 0.2.0"
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || die "'$version' isn't a version like 1.2.3 or 1.2.3-beta.1"
tag=v$version

cd "$(dirname "$0")/.."
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"

branch=$(git symbolic-ref --short HEAD 2>/dev/null) || die "not on a branch"
[[ $branch == main ]] || die "releases are made from main (you're on $branch)"
[[ -z $(git status --porcelain) ]] || die "commit or stash your changes first"
git remote get-url origin >/dev/null 2>&1 || die "no 'origin' remote; push the repo to GitHub first"

git fetch --quiet --tags origin
if git rev-parse --verify --quiet "origin/main" >/dev/null; then
  [[ $(git rev-list --count HEAD..origin/main) == 0 ]] || die "main is behind origin/main; pull first"
fi
git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null && die "tag $tag already exists"

current=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [[ $current != "$version" ]]; then
  # Only the first `version =` line, which is [package]'s.
  python3 - "$version" <<'PY'
import re, sys
src = open("Cargo.toml").read()
open("Cargo.toml", "w").write(re.sub(r'(?m)^version = "[^"]*"', f'version = "{sys.argv[1]}"', src, count=1))
PY
  cargo check --quiet # updates Cargo.lock
  git add Cargo.toml Cargo.lock
  git commit --quiet -m "Release $tag"
  echo "Set the version to $version ($current before) and committed it."
fi
git tag -a "$tag" -m "Multibrowser $version"
echo "Tagged $tag."

if [[ $yes == 0 ]]; then
  read -r -p "Push main and $tag to origin to publish the release? [y/N] " answer
  if [[ ! $answer =~ ^[Yy] ]]; then
    echo "Not pushed. To publish later: git push --atomic origin main $tag"
    echo "To undo instead: git tag -d $tag$([[ $current != "$version" ]] && echo " && git reset --hard HEAD~1")"
    exit 0
  fi
fi
git push --atomic origin main "$tag"

url=$(gh repo view --json url -q .url 2>/dev/null || true)
echo
echo "Pushed. GitHub Actions is building the packages now."
[[ -n $url ]] && echo "Progress: $url/actions  ·  Release: $url/releases/tag/$tag"
