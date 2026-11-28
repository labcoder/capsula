# The NativeShell shell

`crates/shell-nsh` (package `kumochi-shell-nsh`) is the shell Kumochi is moving to: the same
job as the GPUI shell in `crates/shell`, built on NativeShell instead of GPUI. For now it starts
the way the GPUI shell does, holds the same lock and log, links to its brain and follows the link's
state in an empty NativeShell app with a hidden window. It draws nothing and plays nothing.

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
Gate identity pass this check, including a release-profile build with
`kumochi-protocol/gate-identity`; on Windows a build with no flags runs, since the flag is
optional, and off Windows every build exits 69. The refusal, and its test, go away when this
shell takes over.

## Staging it beside a brain

The brain admits only the shell named `SHELL_EXE` (`kumochi.exe`) beside its own executable, and
the shell checks the brain's full path too, so `kumochi-nsh` can't pair with a brain from its
build folder. `stage-nsh`, on Windows only, prepares a folder with those names, where the shell's
link (below) can reach its brain.

As the gate identity only, it builds `kumochi-shell-nsh` and `kumochi-brain` as one debug Gate
build, creates the new folder, copies the shell in as `kumochi.exe` and the brain as
`kumochid.exe`, and checks each copy as the gate tools check their images
([gate identity](gate-identity.md#starting-a-gate-tool)). Before building, it refuses a folder
that already exists or whose parent doesn't. It checks both built images before it makes the
folder, so a pair that doesn't fit leaves nothing behind. It launches neither image. It never
writes over a file, and it holds the new folder open while it copies, so the folder can't be
renamed or removed until they are done. It expects a parent folder that nothing else is changing
while it runs. It removes nothing either: if a copy fails, the folder stays and the error says it
is incomplete. On success it prints the folder. One build selects both packages, so the brain's
dependencies resolve with NativeShell's feature requests.

```bash
cargo run --release --package xtask --features kumochi-protocol/gate-identity -- stage-nsh <new-folder>
```

## Start-up

On Windows, `main` goes in this order:

1. The release identity exits 69, as above.
2. It reads the flags and keeps the result.
3. `platform::start` calls `kumochi_shell_core::startup::start`, the GPUI shell's own
   start-up. It admits the lock's folder and name, which can create the product folders; a
   refusal exits 1 without opening the daily log or taking the session lock. Then it starts the
   log (a failure exits 1), logs bad flags once as `invalid launch flags` with the parser's fixed
   text (exit 1), and takes the lock. A lock another shell holds logs `another shell is running
   in this session` and exits 0.
4. It arms the backstop (below), before anything else can block.
5. It logs `shell started` with the build, the identity and the edition.
6. `platform::run` runs the app, whose root starts the link.

Off Windows, `platform::start` exits 69 (unsupported) at step 3, before anything is admitted,
opened or logged: there's no NativeShell backend here for other systems yet.

**One lock and one log.** It uses the GPUI shell's `SHELL_LOCK` and `SHELL_LOG` on purpose: the
two shells will own the same session. For the same edition, data folder and session, once
start-up succeeds, they exclude each other (whichever starts second exits 0), and they write one
daily log file.

**The flag.** `--quit-after-ms=<n>`, from 1 to 120000 milliseconds (shell-core's
`quit_after_ms`), at most once, is optional: the brain relaunches a lost shell with no flags, and
the link is now a way to quit. A repeated or out-of-range value, or any other argument, is refused
with fixed text and exit 1; no argument value reaches the log or an error.

## The app

The app is a Rust root with an empty view. Its main window is created hidden, with no taskbar entry,
under the app id `SHELL_APP_ID` (`ai.kumochi.dev.shell`, `ai.kumochi.gate.shell`; release's
`ai.kumochi.shell` stays unused while it's refused) and the title `DISPLAY_NAME`. NativeShell's
image cache is off. The root runs on NativeShell's engine thread and does everything below in its
own tasks there, and hands anything that waits to the blocking pool. A failed engine or window
start never mounts the root, so none of it runs then: the backstop ends the process, unless a
person dismisses NativeShell's error dialog first, which ends it with 1. Mounting alone doesn't
disarm the backstop: only an open connection does.

This crate writes no log on the engine thread, since a log write can block and the engine thread
still has work that frees the lock. Its two logs there, a link that couldn't start and a refused
scene refusal, are written on the blocking pool.

## The link and the pump

When it mounts, the root starts the scene dropper (`kumochi_scene::deferred`), then shell-core's
link with `link::prepare` and `LinkStart::spawn`, the same link the GPUI shell runs, on its own
threads. It uses `AudioSession::Legacy` and never announces an audio output, so the brain makes no
speech for it, and it gives the empty text profile (`DrawingText::default()`). The event channel is
unbounded and always drained, since a Legacy admission can poll a full receiver forever. The link
holds the only event sender, so the channel ends when the link does. If the dropper, `prepare` or
`spawn` fails, the shell asks the quit and logs `can't start the link` from the blocking pool.

The pump (`shell.rs`) is one task that takes the link's events in order, in batches
(`link::batch`), and hands each to a plain reducer, `Shell`, which holds the open `Session` and a
`presenter::Refuser` ([scene.md](scene.md)). It knows no NativeShell type, so it's tested without
the framework. What it does with each event:

- **`Connected`** keeps the session and tells the refuser. An open one disarms the start limit.
- **Any other state** drops the session and tells the refuser, and nothing is credited until the
  next `Connected`. `Reconnecting` waits for the link, which reconnects on its own after a first
  loss, and with no flag re-arms the backstop's attempt limit. `Failed` and
  `OutputUnavailable` mean it won't come back, since nobody can press Retry on a shell with no
  window: the shell quits. That's final; nothing after it is taken.
- **`Data` and `SceneData`**: every delivered frame goes to `Refuser::data` once, with
  `link::scene_naming`'s naming, or `Naming::Other` when it names none. Its credit is granted on
  that frame's own connection, so the brain's writer refills its 512 KiB window, and a scene a call
  draws on is refused once with `SceneFailed` through `send_control`. A stale generation earns
  nothing. If the mailbox refuses a refusal, the shell seals the session at once, forgets it and
  ends it with `LinkHandle::end` on NativeShell's blocking pool, as the GPUI shell does; the link
  then reconnects.
- **Everything else** is dropped: control lists (`ScenesV1`, legacy `Scenes`) owe no credit,
  settings and account state arrive unasked on connect and owe no reply, and the rest answer flows
  this shell never starts.

When the event channel ends, the pump asks for quit. Neither the pump nor the reducer logs, and
no log carries a frame's content.

A reconnect repeats the credential read and the brain launch that the start limit exists for, and
the link's own attempt deadline can't interrupt them, so with no flag `Reconnecting` arms the
attempt limit again until the next open `Connected`.

## Quit

There is one quit, owned by the root. Every way out asks it: the main window's close press (the
window keeps `close_on_press` off and listens with `on_close_press`), the app's quit request
(`on_quit_request`), the deadline, the link's `Failed` or `OutputUnavailable`, the event
channel's end, and a link that couldn't start. The first ask goes on and later asks do nothing:

1. It switches the backstop to the quit grace.
2. It calls `LinkHandle::begin_quit`.
3. It waits for `finish_quit` on NativeShell's blocking pool, since that can take 13 s and quit
   hooks get 200 ms. A link that never started has nothing to wait for. However that wait
   ends, a failed blocking task included, the quit goes on, and nothing is logged for it.
4. It calls NativeShell's `quit()`, once.

With `--quit-after-ms`, `main` takes the deadline from taking the lock. The app can't carry an
instant in its props, which take numbers, so the engine samples the milliseconds left when it
mounts the root, which can come late (up to 10 s after a replaced process): none at all when it
mounted late. The root's task waits those milliseconds on NativeShell's `timer`, starting one
commit after the mount, so the quit can trail the deadline by that commit, and then asks the quit.

Once `quit()` runs, the engine stops and `run_app` usually ends the process itself with
`std::process::exit`: 0 when its message loop's result, the quit message's `wParam`, is 0, and 1
when it's nonzero. A quit ends it with 0, but so does the last window closing, so 0 doesn't say
which quit it was. Then nothing in `main` is dropped: the OS closes the lock's file handle at exit,
and the log loses nothing because it writes synchronously. When the main window can't be created,
`run_app` exits 1 once its message box is dismissed; when the UI thread can't even start,
`run_app` returns 1 instead, and `main` drops the lock normally. A UI thread that starts and then
ends without reporting a result, as a panic does in a build that unwinds, also gives 1, since
`run_app` exits 0 only on a reported success. Release builds abort on a panic. The shell's own
code uses no Tokio API; NativeShell runs its own runtime inside.

## The backstop

NativeShell can keep running without the root that runs the link and the quit: if the engine
thread can't start, it only logs that and keeps a hidden window with no root, and a window or
UI-thread failure waits on a message box for a person. Either way a hidden process would keep the
session's lock, and the GPUI shell of that edition couldn't start. So right after taking the lock,
before even its first log line (a log write can block), `main` starts a thread of its own that
waits on a channel with a timeout and then exits 124. It's always armed, in one of three ways:

| Case | Fires at |
|---|---|
| With `--quit-after-ms` | The deadline plus the grace. A connection never disarms it and a quit never moves it later |
| With no flag | `START_LIMIT` after the lock, and again `START_LIMIT` after each `Reconnecting`, unless an open `Connected` is applied before that limit passes, which disarms it |
| Once a quit begins | The grace from the ask. With the flag, the deadline plus the grace if that's sooner; with no flag, the grace replaces the limit, so a quit just before it still gets its whole grace |

Each notice carries the instant it was decided, so one decided at or after the time the backstop
would fire changes nothing, however late its thread reads it. Nothing disarms a quit's grace.

Losing the backstop's channel disarms nothing: what's armed still fires. A terminal state or a
start error asks the quit, which switches to the grace before shutdown begins, so a quit that hangs in `finish_quit` still ends the process.
The thread writes nothing, since a log write can block; the code is how a run shows the backstop
fired, because nothing else in this shell or in `run_app` exits with it. If the thread can't
start, the shell lets go of the lock, logs `can't start the deadline backstop` and exits 1
without running the app.

The grace, 15 s, is a budget, not a bound: 13 s for `finish_quit` (10 s to connect, 3 s to quit),
200 ms for quit hooks, and the rest spare, which also covers the pin's 10 s wait for a process it
replaces (only when `NSH_RESTART_AFTER` is set) at a start with the flag. `START_LIMIT`, 30 s,
covers a brain launch and one connect attempt, at start or after a loss. One native run measured
a warm start connecting in 1.1 s, a reconnect after a killed brain in 0.2 s and a healthy linked
quit ending 68 ms past its deadline, so both limits have wide margins there; a cold start isn't
measured. Window creation, mounting and the engine's join have no time limit of their own, so a
start or quit that is slow but healthy can end with 124. The waits are minimums, and a suspended
machine stretches them.

## What isn't covered

- A brain that is alive but wedged stays `Connected`: nothing here detects it, and with no flag the
  shell runs on.
- An engine that hangs while a connection is open and no quit is in progress isn't caught with
  no flag; with a flag it is caught only at the deadline plus the grace.
- A Windows session end (`WM_ENDSESSION` stops the surface directly), a killed process and exit
  124 skip the ordered quit. A forced exit neither guarantees `Quitting` reaches the brain nor
  undoes one already sent, and a brain that survives may relaunch the shell.
- A close press or a quit request that arrives before the root has registered its listeners isn't
  guaranteed to be intercepted: it can close the app without the ordered quit, and with
  `close_on_press` off an early close press can simply be ignored.
- The tests run the reducer on `Session::detached` and `LinkHandle::detached()` with
  `record_controls()`: they show its credit and refusals and that quit is asked once. `Quitting`
  on the wire, a real reconnect and a real kill are left to the native session.

NativeShell's `log` lines go nowhere: no logger is installed, and no NativeShell crate uses
`tracing`, so none reach Kumochi's log. Their content isn't vetted for it.

## What `run_app` does that matters to a user

Read at the pin, in `nsh-platform-windows/src/app.rs` and what it calls:

| What | Effect for this shell |
|---|---|
| Sets the process's AppUserModelID and registers a toast class factory with COM | For the process's life, and reachable by COM activation while it runs, with no persistent key. The registry key `HKCU\Software\Classes\AppUserModelId\<id>` is written only when an app asks for notification permission or shows a toast, and this one does neither |
| URL schemes, file types and one-instance handling | None are set, so it returns before any registry write or mutex |
| Login item | Off by default: no `Run` key |
| Data folder `%APPDATA%\<app id>` | Its storage file is made only on a first write, and this app writes nothing, so no file is created |
| Image cache | Off |
| Waiting for a replaced process | Only when `NSH_RESTART_AFTER` is set |
| DPI awareness and OLE | Process-local |
| A window or UI thread that can't start | A message box with the error text, which waits for a person, then exit 1; the backstop ends it first, with 124, if nobody answers |
| A message-only dispatcher window and a `WH_GETMESSAGE` hook on the UI thread | Thread-local, for keys a capture takes |
| Watches of this process's window cloaking, the session and the display's power | Notifications to this process's own windows only |
| F11 | Toggles full screen in the app's own window, which is hidden here |
| An inherited `NSH_FPS` set to anything but empty, `0` or `false` | Turns on the performance monitor, which keeps a refresh ticker running |

## The updater and network

NativeShell's update, network, web view, media and JS features are off, and `deny.toml` bans
them: its `[[bans.features]]` entries fail `cargo deny check` if `nsh-platform-windows`'s
`net`, `updater`, `websocket`, `webview`, `dev`, `js` or `media`, `nsh-core`'s `updater`,
`loader` or `websocket`, or `nsh-engine`'s `net`, `updater` or `js-core` is turned on. With
them off, this root asks for no connection: the engine gets no network, the updater serves only
JS apps, `nsh_engine::updates::launch` is called only by NativeShell's packaged runners, and only
the `updater` feature's code can leave work to run at exit.

That isn't the same as no network code in the binary. `nsh-core` still compiles its native
networking module (`runtime/net.rs`, on Tokio's `net`), and `nsh-updater` is in the graph as a
dependency `nsh-engine` always has. Neither is called on this path.

## The NativeShell pin

The framework is consumed from upstream, `github.com/longbridge/nsh`, at one pinned revision:
NativeShell v0.6.1, `2f51d8611677c8a6bbad937b39756e8d0cc45350`. No local copy or patch of it is
kept. The pin is written in two `[workspace.dependencies]` entries in the root `Cargo.toml`,
`nsh-core` and the Windows backend `nsh-platform-windows`, with the same revision; the
backend's default features are off, and the update and network features are banned (above).
`deny.toml` allows that one git source.

Moving the pin is its own change, made for a measured failure that a newer revision fixes or
for a fix of ours that lands upstream, never to follow upstream by habit.
