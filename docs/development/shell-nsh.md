# The NativeShell shell

`crates/shell-nsh` (package `kumochi-shell-nsh`) is the shell Kumochi is moving to: the same
job as the GPUI shell in `crates/shell`, built on NativeShell instead of GPUI. For now it is
an empty crate. Its `main` starts no framework, opens no window, file or log, and exits.

It is not the app that ships. Kumochi's shell binary stays `kumochi`, built from the GPUI
shell, and packaging and the third-party notices name only that shell and the brain. This
crate builds a separate binary, `kumochi-nsh`, so both shells can share one target folder.

Until it replaces the GPUI shell, it refuses the release identity: a release build of
`kumochi-nsh` exits with code 69 (unsupported) before it does anything, so it can never touch
an installed Kumochi's folders, pipe, lock or credentials. Dev and Gate builds exit
successfully. The refusal, and its test, go away when this shell takes over.

## The NativeShell pin

The framework is consumed from upstream, `github.com/longbridge/nsh`, at one pinned revision:
NativeShell v0.6.1, `2f51d8611677c8a6bbad937b39756e8d0cc45350`. No local copy or patch of it is
kept. The pin is declared once, in `[workspace.dependencies]` in the root `Cargo.toml`, for
`nsh-core` and the Windows backend `nsh-platform-windows`, whose default features are off.
`deny.toml` allows that one git source.

Moving the pin is its own change, made for a measured failure that a newer revision fixes or
for a fix of ours that lands upstream, never to follow upstream by habit.
