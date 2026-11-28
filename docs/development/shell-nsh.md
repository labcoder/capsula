# The NativeShell shell

`crates/shell-nsh` (package `kumochi-shell-nsh`) is the shell Kumochi is moving to: the same
job as the GPUI shell in `crates/shell`, built on NativeShell instead of GPUI. For now it starts
the way the GPUI shell does, holds the same lock and log, and runs an empty NativeShell app in a
hidden window until a deadline. It has no link to the brain and shows nothing.

It is not the app that ships. Kumochi's shell binary stays `kumochi`, built from the GPUI
shell. The Mac Dev bundle (`cargo xtask mac-bundle`) and the third-party notices both select
`-p kumochi-shell -p kumochi-brain`, so neither includes this crate. This crate builds a
separate binary, `kumochi-nsh`, so both shells can share one target folder. It is a workspace
member but not a default member: at the workspace root, a plain `cargo build` or `cargo test`
leaves it out, and `-p kumochi-shell-nsh` or `--workspace` includes it.

CI checks this crate in its own steps and leaves it out of the shared gates with
`--exclude kumochi-shell-nsh`. Cargo unifies features across every package one command
selects, so the shared gates then resolve without NativeShell's feature requests. A local
`--workspace` command still selects both shells and unifies their features.

Until it replaces the GPUI shell, it refuses the release identity: a build of `kumochi-nsh`
with the release identity exits with code 69 (unsupported) before it does anything, so it can
never touch an installed Kumochi's folders, pipe, lock or credentials. Builds with the Dev or
Gate identity exit successfully, including a release-profile build with
`kumochi-protocol/gate-identity`. The refusal, and its test, go away when this shell takes over.

## Start-up

On Windows, `main` goes in this order:

1. The release identity exits 69, as above.
2. It reads the flags and keeps the result.
3. `platform::start` calls `kumochi_shell_core::startup::start`, the GPUI shell's own start-up. It admits the
   lock's folder and name (a refusal exits 1 and writes nothing), starts the log (a failure exits
   1), logs bad flags once as `invalid launch flags` with the parser's fixed text (exit 1), then
   takes the lock. A lock another shell holds logs `another shell is running in this session`
   and exits 0.
4. It logs `shell started` with the build, the identity and the edition.
5. `platform::run` runs the app, which ends the process.

Off Windows, `platform::start` exits 69 (unsupported) at step 3, before anything is admitted,
opened or logged: there's no NativeShell backend here for other systems yet.

**One lock and one log.** It uses the GPUI shell's `SHELL_LOCK` and `SHELL_LOG` on purpose: the
two shells own the same session, so they exclude each other (whichever starts second exits 0),
and they write one daily log file.

**The flag.** `--quit-after-ms=<n>`, from 1 to 120000 milliseconds (shell-core's
`quit_after_ms`), exactly once, is required for now. The window is hidden and there's no link
yet, so nothing else could quit it. A missing, repeated or out-of-range value, or any other
argument, is refused with fixed text and exit 1; no argument value reaches the log or an error.

## The app and quit

The app is a Rust root with no view. Its main window is created hidden, with no taskbar entry,
under the app id `SHELL_APP_ID` (`ai.kumochi.dev.shell`, `ai.kumochi.gate.shell`; release's
`ai.kumochi.shell` stays unused while it's refused) and the title `DISPLAY_NAME`. NativeShell's
image cache is off.

Its one task waits the delay on NativeShell's `timer` and calls NativeShell's `quit()`. The
engine stops and `run_app` ends the process itself with `std::process::exit`: 0 after a quit, 1
if its UI thread failed. So nothing in `main` is dropped. The lock's file handle closes with the
process, and the log loses nothing because it writes synchronously. The shell's own code uses no
Tokio API; NativeShell runs its own runtime inside.

NativeShell's `log` lines go nowhere: no logger is installed, and no NativeShell crate uses
`tracing`, so none reach Kumochi's log. Their content isn't vetted for it.

## What `run_app` does that matters to a user

Read at the pin, in `nsh-platform-windows/src/app.rs` and what it calls:

| What | Effect for this shell |
|---|---|
| Sets the process's AppUserModelID and registers a toast class with COM | In-process only, for the process's life. The registry key `HKCU\Software\Classes\AppUserModelId\<id>` is written only when an app asks for notification permission or shows a toast, and this one does neither |
| URL schemes, file types and one-instance handling | None are set, so it returns before any registry write or mutex |
| Login item | Off by default: no `Run` key |
| Data folder `%APPDATA%\<app id>` | Its storage file is made only on a first write, and this app writes nothing, so no file is created |
| Image cache | Off |
| Waiting for a replaced process | Only when `NSH_RESTART_AFTER` is set |
| DPI awareness and OLE | Process-local |
| A window or UI thread that can't start | A message box with the error text, then exit 1 |

## The updater and network

NativeShell's update and network code stays off, and `deny.toml` enforces it: its
`[[bans.features]]` entries fail `cargo deny check` if `nsh-platform-windows`'s `net`,
`updater`, `websocket`, `webview`, `dev` or `js`, `nsh-core`'s `updater`, `loader` or
`websocket`, or `nsh-engine`'s `net`, `updater` or `js-core` is turned on. With them off,
nothing on the `run_app` path updates or connects: the engine gets no network, the updater serves
only JS apps, `nsh_engine::updates::launch` is called only by NativeShell's packaged runners, and
only the `updater` feature's code can leave work to run at exit.

## The NativeShell pin

The framework is consumed from upstream, `github.com/longbridge/nsh`, at one pinned revision:
NativeShell v0.6.1, `2f51d8611677c8a6bbad937b39756e8d0cc45350`. No local copy or patch of it is
kept. The pin is written in two `[workspace.dependencies]` entries in the root `Cargo.toml`,
`nsh-core` and the Windows backend `nsh-platform-windows`, with the same revision; the
backend's default features are off, and the update and network features are banned (above).
`deny.toml` allows that one git source.

Moving the pin is its own change, made for a measured failure that a newer revision fixes or
for a fix of ours that lands upstream, never to follow upstream by habit.
