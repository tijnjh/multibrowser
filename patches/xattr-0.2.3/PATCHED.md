# xattr 0.2.3, patched

An unmodified copy of [xattr 0.2.3](https://crates.io/crates/xattr/0.2.3) (MIT or Apache-2.0, see the license
files), except for `src/sys/mod.rs` and a `[lints]` table in `Cargo.toml` that hides deprecation warnings. On Linux and Android it now uses `libc::ENODATA` where it used
`libc::ENOATTR`, which libc removed for those targets in 0.2.190. Without this, the crate doesn't build on Linux.

`gpui` depends on xattr 0.2 through `gpui_http_client` → `zed-async-tar`, and 0.2.3 is the last 0.2 release, so
the root `Cargo.toml` swaps in this copy with `[patch.crates-io]`. Delete this folder and that patch entry once
`gpui` no longer depends on xattr 0.2.
