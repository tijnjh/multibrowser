## Install

**macOS (Apple Silicon):** download `Multibrowser-__VERSION__-macos-arm64.dmg`, open it and drag Multibrowser to
Applications. The app isn't notarized, so the first time you open it macOS says it can't verify it. Open
**System Settings → Privacy & Security**, scroll down and click **Open Anyway**. Or run this once in Terminal:

```sh
xattr -dr com.apple.quarantine /Applications/Multibrowser.app
```

**Linux (x86_64 or arm64):** download the `.AppImage` for your machine, then:

```sh
chmod +x Multibrowser-__VERSION__-linux-*.AppImage
./Multibrowser-__VERSION__-linux-*.AppImage
```

To add it to your app menu with its icon, open it with an AppImage manager such as Gear Lever or AppImageLauncher.

On first launch the app downloads the four browser engines (about 400 MB). `SHA256SUMS.txt` lists the
checksums of the files below.
