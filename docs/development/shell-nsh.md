# The NativeShell shell

`crates/shell-nsh` (package `kumochi-shell-nsh`) is the shell Kumochi is moving to: the same
job as the GPUI shell in `crates/shell`, built on NativeShell instead of GPUI. For now it is
an empty crate. Its `main` starts no framework, opens no window, file or log, and exits.

It is not the app that ships. Kumochi's shell binary stays `kumochi`, built from the GPUI
shell. The Mac Dev bundle (`cargo xtask mac-bundle`) and the third-party notices both select
`-p kumochi-shell -p kumochi-brain`, so neither includes this crate. This crate builds a
separate binary, `kumochi-nsh`, so both shells can share one target folder. It is a workspace
member but not a default member: a plain `cargo build` or `cargo test` leaves it out, and
`-p kumochi-shell-nsh` or `--workspace` includes it.

Until it replaces the GPUI shell, it refuses the release identity: a build of `kumochi-nsh`
with the release identity exits with code 69 (unsupported) before it does anything, so it can
never touch an installed Kumochi's folders, pipe, lock or credentials. Builds with the Dev or
Gate identity exit successfully, including a release-profile build with
`kumochi-protocol/gate-identity`. The refusal, and its test, go away when this shell takes over.

## The NativeShell pin

The framework is consumed from upstream, `github.com/longbridge/nsh`, at one pinned revision:
NativeShell v0.6.1, `2f51d8611677c8a6bbad937b39756e8d0cc45350`. No local copy or patch of it is
kept. The pin is written in two `[workspace.dependencies]` entries in the root `Cargo.toml`,
`nsh-core` and the Windows backend `nsh-platform-windows`, with the same revision; the
backend's default features are off.
`deny.toml` allows that one git source.

Moving the pin is its own change, made for a measured failure that a newer revision fixes or
for a fix of ours that lands upstream, never to follow upstream by habit.
