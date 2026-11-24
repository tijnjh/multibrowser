# !! this is vibecoded !!


# multibrowser

A GPUI app that loads every page in four browser engines at once and shows them in a 2×2 grid:

| Pane | Engine | Build | Driven by |
|------|--------|-------|-----------|
| top-left | Blink | Playwright's Chromium (headless shell) | Playwright sidecar |
| top-right | Gecko | Playwright's Firefox | Playwright sidecar |
| bottom-left | WebKit | Playwright's WebKit | Playwright sidecar |
| bottom-right | LibWeb | Official Ladybird release | Ladybird's own WebDriver |

**No browsers need to be installed.** On first launch the app downloads everything it needs into its own data
folder, verifies each download, and reuses it afterwards:

| Component | Source | Verified with |
|-----------|--------|---------------|
| Node.js `v26.13.1` | nodejs.org | `SHASUMS256.txt` |
| `playwright-core` `1.65.1` | npm registry | npm `sha512` integrity |
| Chromium, Firefox, WebKit | Playwright's CDN, via `playwright install` | Playwright |
| Ladybird (macOS) | `download.ladybird.org/releases.json` → `.dmg` | SHA-256 from the manifest |
| Ladybird (Linux) | `.deb` from `apt.ladybird.org`, unpacked locally | SHA-256 from the apt index |

The first launch downloads about 400 MB and takes a few minutes; each pane shows its own progress. The total
on disk is about 1.3 GB. Later launches start in a few seconds.

Data folder: `~/Library/Application Support/multibrowser` (macOS) or `$XDG_DATA_HOME/multibrowser`
(Linux, default `~/.local/share/multibrowser`). Delete it to reinstall; set `MB_HOME` to use a different folder.

Supported platforms: macOS (Apple Silicon) and Linux (x86_64 and arm64). Windows isn't supported. Ladybird only
publishes Apple Silicon builds for macOS, so its pane shows an error on Intel Macs.

## Install

Download the latest build from the GitHub **Releases** page:

- **macOS (Apple Silicon):** open `Multibrowser-<version>-macos-arm64.dmg` and drag Multibrowser to Applications.
  The app is signed ad hoc but not notarized, so the first time you open it macOS says it can't verify it. Click
  **Open Anyway** in System Settings → Privacy & Security, or run
  `xattr -dr com.apple.quarantine /Applications/Multibrowser.app` once.
- **Linux (x86_64, arm64):** unpack `multibrowser-<version>-linux-<arch>.tar.gz` and run `./install.sh` in it.
  This puts `multibrowser` in `~/.local/bin` and adds a menu entry with the icon. `./install.sh --uninstall`
  removes it.

## Run from source

```sh
cargo run --release -- https://example.org     # URL is optional
```

## Use

- **URL bar**: Enter loads the page in all four engines. Text that isn't a URL is sent to a DuckDuckGo search.
- **Click** a pane to focus it and click into that page, **type** while it's focused, and **scroll** over it.
- **Sync input** (on by default) repeats clicks, hovering, typing and scrolling in every engine. A synced click or
  hover finds the element under the pointer in each engine's own layout and uses the same spot on it, so it hits
  the right thing even where the engines lay the page out differently; if an engine can't find the element, it uses
  the same relative point instead. Turn it off to drive one engine on its own, for example to dismiss a banner only
  one engine shows.
- Each engine lays the page out as if its browser window filled the **whole app window**, and the pane shows
  that frame scaled down, at about 50% for a 2×2 grid. On a Retina display that's one screenshot pixel per screen
  pixel, so it stays sharp. **Zoom −/+** makes that virtual window larger or smaller; the label shows the actual
  scale.
- Shortcuts: ⌘L focus URL · ⌘R reload all · ⌘[ / ⌘] back/forward · ⌘+ / ⌘− / ⌘0 zoom · ⌘Q quit.

## How it works

- `src/runtime.rs`: downloads, verifies and unpacks the pinned engine runtimes.
- `src/engine.rs`: runs one worker thread per engine behind a shared `Backend` trait. Each worker applies UI
  commands and streams viewport screenshots to the grid.
- `src/sidecar.js`: the Playwright helper script, embedded in the binary. One copy runs per engine; it speaks
  line-delimited JSON over stdin and stdout.
- `src/webdriver.rs`: a minimal W3C WebDriver client used for Ladybird.
- `src/main.rs`: the GPUI window, toolbar and grid.

## Updating the engines

```sh
scripts/update-engines.sh            # update everything
scripts/update-engines.sh --check    # only report what would change
```

The script bumps the pinned Node.js (latest LTS) and `playwright-core` (latest release) in `src/runtime.rs`, which
brings new Chromium, Firefox and WebKit builds, then rebuilds the app and runs `multibrowser --update`. That
updates Ladybird to its latest release, downloads and verifies everything the pins need, and deletes versions
that are no longer used. Quit the app first. Ladybird is otherwise never updated after its first download, so
`multibrowser --update` on its own is enough to get a newer Ladybird without changing any pins.

## Releasing

```sh
scripts/release.sh 0.2.0          # or 0.2.0-beta.1 for a prerelease
```

This sets the version in `Cargo.toml`, commits it, tags `v0.2.0` and, once you confirm, pushes `main` and the tag.
The tag starts `.github/workflows/release.yml`, which builds on GitHub's machines:

| Package | Built on |
|---------|----------|
| `Multibrowser-0.2.0-macos-arm64.dmg` | macOS 15, Apple Silicon |
| `multibrowser-0.2.0-linux-x86_64.tar.gz` | Ubuntu 22.04, x86_64 |
| `multibrowser-0.2.0-linux-aarch64.tar.gz` | Ubuntu 22.04, arm64 |

It then publishes a GitHub release with those files, a `SHA256SUMS.txt`, install instructions
(`packaging/release-notes.md`) and notes generated from the commits since the previous release. That takes about
15 minutes. To check that everything builds before releasing, run the workflow by hand (Actions → Release →
Run workflow): it builds the packages and attaches them to the run without publishing anything.

Before releasing, you'll usually want the newest engines: run `scripts/update-engines.sh`, try the app and commit
the new pins. Released apps keep their Playwright engines at the pinned versions; Ladybird stays at whatever
version it first downloaded until you run `multibrowser --update`
(`/Applications/Multibrowser.app/Contents/MacOS/multibrowser --update` on macOS).

Other packaging files:

- `scripts/package.sh` builds the package for the machine it runs on into `dist/`, which is how you try a
  package locally.
- `assets/icon.svg` is the icon source. After editing it, run `scripts/make-icons.sh` (macOS, needs
  `brew install librsvg`) and commit the regenerated `.icns` and PNGs.
- `packaging/macos/Info.plist` is the app bundle's metadata, and `packaging/linux/` holds the menu entry and
  `install.sh`.

## Linux notes

Playwright's browsers and Ladybird link against common desktop libraries: GTK, GStreamer, Vulkan, X11 and
Wayland. A normal desktop install usually has them. On a minimal system, install the missing ones with
`npx playwright install-deps` and the `Depends:` list of Ladybird's `.deb`.

## Debugging

- `MB_TRACE=1` logs commands and browser stderr.
- `MB_TRACE_HTTP=1` logs Ladybird WebDriver requests.
- `MB_DUMP=<dir>` writes each engine's latest frame as a PNG.
- `MB_LADYBIRD_WEBDRIVER=<path>` uses an existing Ladybird install instead of downloading one.
