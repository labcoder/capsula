# Presence and native surfaces

Overlay hit testing belongs to the presenter's `OverlayOwner`, not to a view. The native
`OverlayWindows` adapter binds the dock paint window and body proxy in the Dock layer,
panels in Panels, and every newly created drawing host in Overlay before it can be shown.
Regional and selection surfaces attach through the same seam. The dock paint and drawing
windows pass mouse input through their entire bounds, including painted pixels; only the
dock's separate body proxy takes its input. Regions enables only the
currently registered input windows. The [mode diagram and exit rules](input.md#overlay-pointer-ownership)
cover the temporary Selecting mode; no production hold enters it yet.

The adapter restacks top-first as panels, dock/body proxy, regional input, drawing overlay,
then dormant selection. Showing a drawing reapplies this order after its native placement,
even when the input mode has not changed. A replacement host, including after clearing or
reconnecting, binds and configures again while hidden. Selecting puts its surface above those
groups, then drops it back on exit. Every restack uses
`SWP_NOACTIVATE`, and every non-panel surface keeps `WS_EX_NOACTIVATE`; only the existing
explicit panel activation path can take foreground. GPUI cannot express cross-process regional
click-through or these Win32 restacks. The macOS equivalent is nonactivating `NSPanel`,
`ignoresMouseEvents` and window levels: it must ignore mouse events over the entire drawing,
never activate it, and restore this order after every show or placement. It is not implemented
or verified here.

The shell owns the dock, status panel, typing panel and tray on GPUI's UI thread. The dock opens
and the keyboard is armed before the brain link starts connecting; [input.md](input.md) covers
the shortcut, the typing panel and Esc. A link failure shows in the status panel; Retry never sends
the previous input again.

`presence::stage` unions the accepted dock layout, the status peek's minimum reservation
and the accepted bounds of the visible typing, Threads or Settings panel. It chooses the
largest-area usable band below, above, left or right, in that tie order, with a
32-logical-pixel inset and at least 240×160 logical pixels left after the inset. If no band
is usable, it retries with only the dock and peek reserved, then uses the inset work area
as a last resort. On a display too small for both, an open panel can cover a drawing and
its entrance can play unseen. Opening or closing a panel updates placement before the next paint;
when the dock lands, panels re-anchor before the stage is recomputed. The typing panel
retains its accepted size when re-anchored, like Threads and Settings.
A retained GPUI display observer refreshes the dock's scale and anchor,
the drawing stage and a shown overlay's bounds after display changes. Existing views snap
to the new stage; their scene clocks and prepared geometry stay intact. See
[scene.md](scene.md#canvas-and-overlay-placement) for the canvas and scale-envelope rules.
The work area is read fresh at placement transitions, not polled while sampling frames.
Work-area-only changes wait for the next placement; add a work-area notification through
the presence adapter.

Before the link starts, the shell starts the scene dropper or fails startup. Typed scene frames
are protected at the pipe reader, including stale frames and queued or undelivered event batches.
Preparation and recovery inputs and answers keep that protection through cancellation, rejected
results and lost recipients. Clear, Stop and disconnect swap out their owned containers; the UI
does not destroy their scene payloads. A dead dropper stops scene intake and new preparation.

```mermaid
flowchart LR
    Link["Brain link thread"] -->|status| Owner["Presence owner, UI thread"]
    Native["Native tray and hit proxy"] -->|commands / button-down / pointer boundary| Owner
    Owner --> Dock["Transparent GPUI popup"]
    Owner --> Panel["Nonactivating status panel"]
    Owner -->|Quit off the UI thread| Link
```

The dock is a transparent `PopUp` overlay, sized to its slot. Its native styles include
layered, transparent, no-activate and tool-window; app-window is removed. A separate native hit
proxy owns exactly the line's trigger band and reports pointer entry and exit (see
[The line and hit testing](#the-line-and-hit-testing)). The panel is interactive, nonactivating and excluded from the taskbar in the same way.
The panel is created hidden at startup, so the first click doesn't create a window.
The same panel is hidden and reused, never recreated for each click. The typing panel is the
one exception to never activating: it's a tool window without no-activate, created on the first
double tap, activated only explicitly, and hidden rather than closed. While it's open, pointer
events and dock clicks leave it alone and the status panel stays hidden. `presence/text.rs`
owns its lifetime and retains a GPUI `observe_window_activation` subscription. A previously
active, visible typing surface hides on deactivation, preserving its `InputState` and dropping
the saved foreground without restoring it. Reopening an already visible surface preserves its
activation latch; only a hidden-to-visible transition resets it. Initial inactive notifications
leave an unactivated box clickable, and hiding clears the latch before Esc restores focus.
GPUI's activation state is queued, not a current native foreground query: rapid away/back
within one queued activation can still hide a now-active box. Revisit that limit if reproduced.
Esc and explicit dismiss keep their existing conditional foreground restoration.
Neither surface uses GPUI's normal title bar. Native creation, styles, placement and lifetime
live under `platform/windows`; Rust view code handles no raw window pointers.

The nine positions are calculated from the primary display's physical work area, not its
entire screen. Logical dimensions are converted using the dock's scale factor. The panel
touches the dock and clamps inward at an edge. Crossing their shared edge keeps it open.
Pointer entry on the band opens it without activation; a button-down does nothing. A Stop
fallback stays open regardless of pointer departure while needed.
Leaving both starts one 80 ms grace timer; there is no idle polling. The timer rechecks typing
and Stop ownership before hiding. Synchronous native
placement calls `bounds_changed` afterwards so GPUI receives its otherwise-dropped resize.

The native tray menu offers position, Retry and Quit. Position is deliberately session-local
until the brain's settings flow and Settings → General arrive. No settings file is written by
the shell. Closing a transient panel is not Quit. Quit uses the existing idempotent link
shutdown and waits off the UI thread before exiting.

`kumochi --smoke` shows the panel programmatically after one second and requests the same
graceful Quit after ten seconds, without moving the pointer or sending input. Initial surfaces are
small even before placement, so a transparency failure cannot cover the desktop. This is a
bounded launch check, not a performance benchmark or permission to inject input.

For idle measurements, build the workspace in release mode, then run `cargo xtask bench idle`.
Before launching the shell, the benchmark spins one thread in three two-second trials,
measuring `QueryThreadCycleTime` divided by wall time and printing each rate and the highest.
Each role and the process-tree total report calibrated cycle CPU as
`cycles / (highest_rate * wall) * 100`, beside `GetProcessTimes`. The slice 1 CPU gate reads
the calibrated column because tick-charged process times can alias the 144 Hz workload.
This is a measured saturated-thread normalization, not a fixed clock-frequency assumption;
preemption and frequency changes can affect calibration, so retain the rates and both columns.
It refuses an existing product process, launches the release shell at normal priority with a
bounded graceful-quit deadline, settles for thirty seconds, and samples user plus kernel time
over sixty seconds. CPU is percent of one core; memory is endpoint private bytes, not working
set or peak memory. `--duration 1..60` shortens a harness smoke check, not an acceptance run.
The task checks that the same shell, brain and worker survive the sample and waits for all
three to exit. Run only in an agreed quiet window; the benchmark sends no input.
`--baseline <release-directory>` samples older binaries with the same counters. After the sample,
it sends `WM_CLOSE` to the spawned shell's presence proxy, which requests the normal graceful
Quit path. Pre-dock binaries fall back to closing their single visible status window. Both
paths verify the spawned shell's PID; neither targets another app.
`--panel` opens the current shell's panel programmatically before settling, so the sample
includes the panel's steady-state resources rather than only the dock before any panel opens.
The current shell also counts its dock frames: the benchmark passes `--input-diagnostics` and
`--settle-after-ms=30000`, the shell writes `dock settled` when the sample starts and
`dock paints settled=N` at Quit, and the benchmark fails unless `N` is 0. A baseline may predate
the count, so it isn't checked.

Geometry tests cover all anchors, negative work-area origins, edge containment, the
hidden-panel boundary and the overlay. Transparency, cross-process click-through, native
focus and Alt-Tab behavior need desktop evidence; a style bit or unit test alone cannot certify
them.

The ignored five-second `presence::overlay_fixture::smoke_primary_overlay` test uses the
production dock/drawing setup and drawing-show functions. It checks the real GPUI windows'
pass-through styles, dock-above-drawing order and UI-thread activation before painting;
run it only in an approved native window turn. Hidden adapter tests serialize their windows,
take one owned-handle z-order snapshot per assertion batch and inspect only their own thread's
activation, so ordinary desktop app switching does not invalidate them.

## The line and hit testing

At rest the dock is a short line, 56 × 3 logical pixels, near the work area's edge
(`presence/line.rs`). GPUI draws it as a rounded rect with a fill and a 1 px border, so its ends
are antialiased. The hit proxy owns the line plus a trigger band 8 logical pixels above and
below it, so the pointer finds the line without it growing. Paint and hit are kept apart: a
`LineFrame` holds the line's snapped device pixels (`paint`, each edge rounded half toward zero
as GPUI snaps what it draws) and the band (`hit`, `paint` grown by the trigger and rounded
outward), and only `hit` is owned.

```mermaid
flowchart LR
    Pos["Position choice"] --> Owner["Presence owner"]
    Owner --> Frame["LineFrame<br/>paint + hit band"]
    Frame --> Mask["Hit mask<br/>the band, one UpdateLayeredWindow"]
    Mask -->|accepted| Overlay["Overlay window<br/>moves to the slot"]
    Overlay -->|placed| Dock["Dock<br/>publishes the layout, draws paint"]
    Mask -->|refused| Retry["Next frame<br/>at most three tries"]
    Overlay -->|refused: mask put back| Retry
```

- **The hit proxy** is a per-pixel-alpha layered window (`platform/windows/hit_mask.rs`).
  Owned pixels have alpha 1, which is invisible but hit-tested. The rest have alpha 0, which
  Windows passes through to whatever lies beneath, in any process. It never gets
  `WS_EX_TRANSPARENT` or a region. Its bitmap is allocated once, sized to the dock's slot, and
  the proxy sits exactly on the band. Raw Input, the tray, the Esc hotkey and capture exclusion
  stay on this same window.
- **Acceptance.** `Line` keeps the accepted placement, the requested one, and what Windows
  holds: the proxy's mask, the overlay's position and whether the proxy is shown. A request
  becomes current only when all three hold it, in that order: the mask upload, the overlay
  move, then showing the proxy and checking its tray entry. Only then are the layout, the
  overlay's origin and the line published. A refused upload moves nothing. A refused overlay
  move puts the accepted mask back first; if that's refused too, the surfaces are apart, and
  the next attempt finishes the move under the mask Windows already holds. A refused show is
  retried the same way. Each attempt carries on from what Windows holds, at the dock window's
  next frame and with that frame's own window, at most three times in a row. A new position
  choice replaces the request and gets three more. If the last attempt fails with the surfaces
  apart, or with the proxy never shown, the dock fails closed: its line is cleared and the
  overlay and the proxy are hidden, so no line shows without its band and no band takes clicks
  without its line. An overlay that won't hide is left empty. A proxy that won't hide has its
  mask emptied instead, and if that's refused too the shell quits with a logged error rather
  than leave a band taking clicks. The dock comes back on the next position choice; a display
  change doesn't bring it back yet, under the display limitation (OSC-188). Drawing frames come from the dock's
  window, so drawings stop moving while it's hidden (frame-rate.md). The surfaces sit behind a small `Surfaces` trait, so
  the tests run every failure against a fake desktop. Windows may not report the mask moving
  under a still pointer, so the owner rechecks hover after the dock moves.
- **Painting.** The dock window is the click-through overlay, placed once per accepted
  placement. It repaints when the accepted placement changes and whenever the dock is refreshed:
  Listening, a link change or a settings notice. A settled dock has none of these, so it costs
  nothing.
- **Listening** turns the line's fill to the theme's primary colour.
- **Peek.** Pointer entry on the band opens the status panel as the peek, with Kumo beside the
  link status, the offline notice and the settings-not-saved notice; a button-down does
  nothing. An open peek stays while the pointer is over the band or the panel, and the 80 ms
  grace timer covers the gap between them. The panel's background is blurred where Windows
  allows it, with 85% opacity as the fallback. The stretch is gone with the old pill: motion
  trials return with the pull (C2a), and `cargo xtask bench stretch` says so.
- **Kumo in the peek.** `presence/kumo_view.rs` owns a 64-pixel Kumo. Its body is baked once,
  on the background executor at the launch scale, and kept across hiding as a prewarm; a scale
  change needs a restart (OSC-188 wires display changes). The peek is 380 wide and only as tall
  as its content (124): Kumo, the name and the status form one row, with Chat…, Tasks… and
  Settings… below; Quit lives in the tray. Size and weight carry the hierarchy, since muted
  text fails AA contrast over the glass. Chat… opens the typing panel in its own 380×140
  rect, anchored to the accepted dock like the threads and settings panels, so a refused
  move never detaches it. The typing and threads panels get their notices pushed by
  presence instead of reading presence when they draw, because a new window draws its first
  frame while presence is still opening it. Opening the peek blinks once, or shows one still
  frame under reduced motion, and Listening changes the mood. Each opening and each Listening
  edge re-reads the system's reduced-motion preference through Kit's
  `apply_system_reduce_motion`, since Windows posts no change Kit can follow. Frames come from the peek window's next-frame
  callback, paced by the frame-rate cap, and stop once nothing animates, so a settled peek
  shades nothing. Each animated frame is a new image and the one it replaced is dropped a
  frame later (OSC-175's exception until the images are pooled). Quit logs how many frames
  were shaded and how many repeated the one before.

Unit tests put the line at 125% and 150% from a fractional origin and check the last owned
pixel and the first pass-through pixel on each side, check that the band fits its slot at
every scale, and check that only an accepted upload moves the line. Real click receipt on the
band and pass-through beyond it are desk checks.

## Gate observer

`kumochi --gate-observer=<file>` records what the dock really did, for the gate harness. Like
the fixtures it needs `--quit-after-ms` or `--smoke`, or the shell refuses to start, and so
does `--receipt-fixture`, which also needs the observer and only starts the dock middle-left
for the gate's click-receipt phases. The tray's nine positions are unchanged. A normal run has no observer, no file and no writer thread;
the only per-frame cost is checking that the observer is absent.

```mermaid
sequenceDiagram
    participant UI as UI thread
    participant Proxy as Hit proxy (same thread)
    participant W as Writer thread
    UI->>UI: place: UpdateLayeredWindow
    UI->>W: commit + geometry (exact rows)
    UI->>UI: draw: the dock's stamp paints
    UI->>W: paint frame=F
    UI->>Proxy: PostMessage(frame F)
    UI->>UI: Present returns, WM_PAINT ends
    Proxy->>W: drawn frame=F (dispatched after)
```

`main` creates the file, a bounded queue of 256 fixed-size records, 256 spare row tables and the
writer thread before GPUI starts. The UI thread only stamps the performance counter and moves a
record into the queue without waiting; the writer formats it and flushes whenever it catches up.
A record is a few words. A geometry's 512-row table stays on the heap: the UI thread fills its
own table, swaps it for a spare and moves only the pointer through the queue, and the writer
hands the table back once it's written. So a commit allocates nothing and never copies a row
table on the stack; with no spare left, the geometry is dropped and counted like a full queue.
Each record is one line:

- `startup version=2 pid= dock_hwnd= panel_hwnd= proxy_hwnd= scale= position= work_area= dock= overlay=
  panel= qpc_frequency= row_cap=512 queue=256 repaint_message=0x8010 marker=0x43505331`. It
  follows the first position's commit and the paints GPUI makes while it opens each window.
  Rectangles are `x,y,width,height` in physical pixels.
- `geometry id=G origin_y=Y rows=N trimmed=true|false spans=…`, sent the first time a mask
  proposal's rows differ from the last one sent. Each of the `N` rows from `Y` is
  two `drawn` spans and then four `owned` spans, comma-separated, rows separated by `;`, each
  span `left:right` (half-open) or `-`. The drawn spans are one per shape: the line with its
  drip, then the body with its stub. Four is `ROW_SPANS`, what a hit-mask row holds: two shapes,
  each with a current and a previous snapshot. For the line, the first `drawn` is its `paint`
  and the first `owned` the band's `row_spans` the mask is given, not a copy; the rest are `-`,
  and `trimmed` is always `true`. Version 2 is this row; version 1 had one drawn and two owned.
  The gate harness requires
  each row to own what it draws, and a committed geometry to own exactly its `proxy` rectangle.
  Taller than 512 rows, it's `… overflow` with no rows, never truncated.
- `commit seq= begin_qpc= qpc= result=shape|moved|unchanged|failed proposal=G epoch=E
  geometry=A proxy=…`, after every `UpdateLayeredWindow` call. `begin_qpc` is read right before
  the call and `qpc` when it returned, so `[begin_qpc, qpc]` holds the whole upload; when Windows
  starts routing by the new mask inside it isn't known. A gate excludes a click whose input
  interval overlaps a changed-epoch upload's. Without the observer, no counter is read. `proposal` is the
  uploaded geometry; `epoch` and `geometry` are the accepted state after it. Only an accepted
  change of rows (initial, or a new position) advances the epoch; a failure logs its proposal
  and leaves both.
- `hover id=H qpc= visible=true|false click_qpc=`, on every status panel show or hide. The
  legacy name and paint IDs are unchanged. `qpc` is state entry, before native placement;
  `click_qpc` keeps its legacy name but carries the native pointer-entry counter when a pointer
  entry opened the panel, and zero otherwise; older builds always log zero.
- `paint frame=F qpc= kind=dock|panel hover= drawn_epoch= drawn_geometry= mask_epoch=`. A dock
  frame is stamped by the dock's zero-size `Stamp` element with `hover=0`; `drawn_geometry`'s `drawn` spans
  are the rows it paints, and `0` means no commit yet. A panel frame is stamped by a zero-size
  element in the panel, present only with the observer, with the latest `hover` id and
  `drawn_epoch=0 drawn_geometry=0`. Frame ids count across both.
- `target frame=F qpc= kind=stop hover=H rect=x,y,w,h inside=true|false`, from the status
  panel's Stop button each time it paints: its painted bounds in physical screen pixels (the
  panel's client origin plus the bounds GPUI laid out, times the scale), not a guessed spot. A
  wrapper hands every phase to the button, so its layout and handling are unchanged. The
  panel's stamp is its first child, so `frame` is the panel frame this paint belongs to and
  the paint lies in its `[paint qpc, drawn qpc]`; `inside` says the bounds lie within the
  panel's client area. Only the status panel's Stop is recorded, not the typing panel's.
- `drawn frame=F qpc=`: the paint posts its frame id to the proxy. GPUI on Windows draws and
  calls `Present(0, 0)` synchronously inside the window's paint message, so the proxy, on the same
  thread, dispatches the post only after that returns, for the panel as for the dock. A present
  in `[paint qpc, drawn qpc]` on that surface's swap chain is that frame's; this bounds when it
  was submitted, not when it was shown. `open_window` paints each window once before the proxy
  exists, and presents it on a later frame. Up to 8 such frames are kept and posted from that
  window's next frame after `startup`, ahead of its present, so the dock's cold first frame
  keeps a wider but still bounding interval. A ninth counts as `post_failed`.
- `click qpc= point=x,y mask_epoch= marker=0x43505331`: a left press on the proxy whose extra
  info is `INPUT_SESSION_MARKER`. `point` is the press's physical screen point from
  `GetMessagePos`, as Windows routed it, not the client point converted with the proxy's
  origin at dispatch, which a move in between would shift. Other clicks are never read or
  recorded.
- `repaint qpc=`: the harness posted `OBSERVER_REPAINT_MESSAGE` to the proxy, and the dock is
  asked to repaint with no input. Without the observer, the proxy ignores it.
- `close commits= frames= dropped= overflow= post_failed= unposted=`, then the writer's `end
  lines= write_errors=`. A missing close or end line, or any nonzero `dropped`, `overflow`,
  `post_failed` or `write_errors`, makes the run invalid for the gate. `unposted` counts early
  frames whose window never drew again, such as the hidden panel's; they have no `drawn` line,
  so a gate that joins one of them is invalid. At Quit the observer stops before it takes these
  counts: later frames, commits and proxy notes record nothing, so the counts match the lines
  above them, and frames still in flight keep no `drawn` line. The writer then writes every
  record it already accepted, then `close` and `end`.

Records hold geometry, handles and times only: no text, prompts, screenshots or other apps'
content. Unit tests cover the exact rows, the overflow, the epoch rule and every line's format;
display timing and click receipt need an approved session.

## The drawing

`crates/shell/src/drawing.rs` holds what's drawn: at most four scenes by `SceneId`, each a list of
steps, and the recovery that restores them on each connection. It's plain state, no GPUI, so its
rules are unit-tested. `runs.rs` keeps the runs, turns, the fence and the answer text, and hands
it the steps it lets through.

- A live step for a new scene admits it the way the brain does: the newest four, evicting the
  oldest, so a `ClearDrawing` never names more than four.
- `drawing()` is true while a scene is shown or a recovery Esc could cancel is awaiting or
  receiving, so Esc with nothing moving clears, recovery included. A moving run still stops
  first.
- On a disconnect the shown scenes stay until the next `Scenes` reconciles them; recovery state
  and held chunks go.
- Scene ids, revisions and counts are the only things it reports; its errors never carry labels.

[ipc](ipc.md#scenes-and-recovery) has the recovery states and the install rules.

## Perch hosts

The perch (C2b) drops Kumo on another app's title bar. C2b-1 adds the reads and the geometry.
C2b-2's carry calls them; kumo.md, "The carry", says how.

- `platform::host` holds the plain data: `PhysicalRect`, `HostId`, `WatchGen`, `HostChange`,
  the raw caption `Facts` and `Label`. A `Label` is another app's window title, so it's screen
  text. It sits in an inline buffer of 64 UTF-16 units, cut at a unit boundary without half of a
  surrogate pair, and its `Debug` prints only its length. Nothing logs it or sends it to the brain.
- `platform::windows::host::host_at` walks the z-order under a point, skipping Kumochi's own
  windows and any hidden, minimized, cloaked or click-through ones. The desktop and the
  taskbars end the walk with no host. It reads the window rectangle, the DWM frame, the
  monitor's work area and effective DPI, the host's own DPI, the title bar, DWM's caption-button
  bounds, and the standard top inset and button width. A host without `WS_CAPTION` gets a
  standard resizable window's metrics at its DPI instead of its own zero inset. The title comes
  from `GetWindowTextW`, which for another process's window reads the copy the system keeps
  and sends no message into that app's thread.
- `watch_host` installs five out-of-context `SetWinEventHook`s (location change, minimize,
  hide, cloak and destroy), filtered to the host's process and thread. They deliver
  `PresenceEvent::HostChanged { w, change }` on the UI thread's message loop. If any hook fails,
  the ones before it are removed in reverse order. Dropping the watch unhooks every hook before
  its route is cleared. A hook that `UnhookWinEvent` refuses stays in its route, marked no
  longer live so its callback sends nothing, and keeps that slot taken; the next `watch_host`
  retries it, and the slot frees only once every hook is gone.
- `presence::perch::caption` applies §10.4's caption rules. When the reported title bar and
  buttons agree, the caption is the bar left of the leftmost exclusion. Otherwise it's the
  standard band, scaled from host units by `monitor_dpi / layout_dpi` and rounded up, cut to the
  frame and the work area. For a host without a caption, the band runs from the frame's top
  (revision 12's guess). A usable reported button always stays an exclusion, and while the two
  DPIs differ it's read both as physical pixels and as host units.
- `presence::perch::place` sits the body above the title bar if the work area has room, and
  otherwise just below the caption inside the frame, with the card beside the body. Sizes
  convert at the host monitor's scale. A placement counts only if the body and the card stay
  in the work area and 4 px clear of every exclusion along the whole straight settle from the
  drop point.
