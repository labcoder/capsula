# Third-party notices

Kumochi is `AGPL-3.0-only`. These are the third-party sources the repository itself carries;
crates Cargo fetches keep their own license files and are checked by `cargo deny`.

## gpui-pre-windows 0.3.10 (`vendor/gpui-pre-windows/`)

- Upstream: Zed's `gpui_windows` crate (github.com/zed-industries/zed, `crates/gpui_windows`),
  published to crates.io as `gpui-pre-windows` 0.3.10, a snapshot of zed@08cbb88.
- License: Apache-2.0. The full text is in `vendor/gpui-pre-windows/LICENSE-APACHE`.
- Changes: Kumochi modifies five files in `src/` so that idle windows stop waking every vblank.
  Each modified file says so in its first line; the change is described in
  [docs/development/gpui-patch.md](docs/development/gpui-patch.md).
