# Screen worker

`kumochid --screen` installs its separate screen log and location-only panic hook before serving
stdin/stdout. It answers Ping, Capture and Elements (below). Request contents and panic payloads never enter logs;
transport failures and foreign errors are numeric codes, not foreign error strings.

```mermaid
flowchart LR
  Client[Brain systems] -->|request, absolute deadline| Supervisor[One tokio task]
  Supervisor -->|bounded channel| Writer[std writer thread]
  Writer -->|anonymous stdin pipe| Worker[kumochid --screen]
  Worker -->|anonymous stdout pipe| Reader[std reader thread]
  Reader -->|bounded channel| Supervisor
  Supervisor -->|reply or WorkerLost| Client
  Supervisor -->|broadcast loss| Watches[Watch owners]
```

The supervisor is the shared one in `supervise`, which also runs the speech worker (see
[voice.md](voice.md), "The speech worker"); this page gives the screen kind's numbers. The task
owns the process, pending replies, request ids and crash history. The last request handle
closing stops it. Pipe threads own their file ends; they end when their channels close and the
worker terminates. The worker inherits exactly two handles through an explicit handle list and is
created inside the brain job, never a run job. Closing the brain job kills it.

Frames use the protocol's 256 KiB data cap before allocation. A request and a reply each carry
serialized body bytes in ordered chunks starting at zero, with one request id throughout, under
64 KiB per screen request and 4 MiB per reply [guess]. A worker that reads a broken request exits.
Unknown ids, gaps, duplicates, empty chunks, chunks after completion, malformed or truncated frames,
trailing bytes and EOF all lose the worker. Sixteen pending requests and sixteen queued frames per
channel bound pressure [guess]; overload fails fast rather than buffering more work. A hostile
worker can therefore make the brain hold up to 16 × 4 MiB of partial replies, plus 16 queued
256 KiB frames, for at most one deadline.

Each request has one 1.5 s absolute deadline [guess], including queue time; fixtures can inject it.
A loss terminates the worker, fails all pending operations and broadcasts `WorkerLost`. The first
two losses respawn immediately; a third within 60 s waits 0.2 s, then 1 s and 5 s for later losses
[guess]. Requests during backoff fail fast. During an in-flight respawn, requests wait for the new
worker under their original absolute deadline; a failed spawn immediately fails its queued callers.
Respawns are serialized and run off the async thread, so process creation cannot delay delivery
of `WorkerLost`; only the first spawn, at startup, runs on it, before anything is served. A loss
after the last handle has closed doesn't respawn, so shutdown can't start a worker in a closing job. Loss history expires at exactly 60 s.

Debug builds accept one-request fault fixtures and `KUMOCHI_SCREEN_FAULT=hang|crash|garbage|panic`.
The environment switch applies on the next request of every respawn; neither path exists in a
release worker. The portable element operation is implemented; Windows UI Automation and OCR are not yet connected.

The macOS platform equivalent is `posix_spawn` file actions for stdio and a process-group kill;
it remains unimplemented and unverified.

## Capture under a ticket

`Capture { ticket, display, exclude }` takes one frame of one physical display into the worker's
`RawFrame` and answers `Captured(FrameMeta)` or `Refused`; **pixels never cross the pipe**. A4
encodes them and A6 decides what leaves the worker; nothing here sends them anywhere, and there's
no operation that reads or exports them.

- **Authority.** `TicketWire` can only be built inside the brain, and B2a's ticket book is the only
  minter: from a recognized hold or an Include send, under a live exclusion lease. Until B2a
  lands, nothing in production sends `Capture`. `Exclusion::KumochiWindows` is a declaration the
  caller makes because it holds the shell's lease, not proof of one.
- **Local validity.** The worker starts the ticket's `ttl_ms` at receipt on its own monotonic
  clock. It refuses at exactly `ttl_ms` and accepts a millisecond before, checks again right
  before starting the capture session, stops waiting at expiry, and discards a frame that
  completes at or after it. This is a local bound, never a renewed grant: the brain keeps its
  ticket's original deadline and drops a late reply.
- **Spent generations.** Every generation at or below the highest one this worker has seen is
  refused (`Reused` when equal, `Stale` when lower), and a failed or refused capture still spends
  its generation. A respawned worker starts empty; the brain's single-use tickets guard across
  workers, and a lost worker's requests are failed, never replayed.
- **Frame validity.** Bounds are checked before anything is allocated: no empty side, no side over
  8,192 px, no edge outside `i32`. The held image is invalidated before every attempt and becomes
  valid only after a complete copy whose size equals the requested display, before expiry. A
  smaller frame shrinks the logical length, so an old tail is never part of the image, while the
  CPU buffer keeps its allocation for equal or smaller frames after warm-up. That says nothing
  about COM or GPU allocations, which happen per capture.
- **Windows.** One D3D11 device for the worker's life, created on the first capture on the serve
  thread. Per ticket: the monitor must have exactly the requested physical bounds
  (per-monitor-v2 DPI awareness), then a WGC item, a one-buffer free-threaded pool and a session
  with the cursor off. Border suppression is best-effort: Windows can accept it and still draw its
  yellow border on screen. Frames and the map poll at 1 ms, and a result that is ready at or
  after expiry is released and refused, and nothing late is published. Frame, session and pool
  close once the GPU copy into a staging texture is issued, so the CPU copy runs after capture has
  stopped. All three close on every path; if a close fails the worker
  answers `Fault` and exits, so no capture can keep running. A lost device
  (`DXGI_ERROR_DEVICE_REMOVED`, `_RESET` or `_HUNG`) retires the worker the same way, so the next
  ticket gets a fresh device. Foreign failures are HRESULT codes.
- **macOS.** `SCScreenshotManager` with Kumochi excluded from the content filter, not built yet;
  other platforms answer `Fault` with `E_NOTIMPL`'s code.
- The supervisor's `capture` accepts only a `Captured` for its own capture id and display, maps
  `Refused` through, and treats any other reply as `WorkerLost`.

```mermaid
sequenceDiagram
  participant B as Brain (B2a)
  participant W as Worker
  participant P as Platform capturer
  B->>W: Capture { ticket, display, exclude }
  W->>W: spend generation, start local expiry, check bounds
  W->>P: monitor check, session (until expiry)
  P-->>W: frame, copied into RawFrame
  W->>W: expiry and size checks, then publish
  W-->>B: Captured(FrameMeta) or Refused
```

## Masking and encoding

The portable pixel path borrows a committed `RawFrame` through `sanitize`. It clips every
`MaskSet` rectangle to the physical display and writes opaque black in place, then lends a
`SanitizedFrame` stamped with the mask generation. An uncommitted frame is refused as stale.
Pixel consumers accept only this sanitized type. Adding a password rectangle bumps the mask
generation; sanitizing again applies all masks idempotently before another encode.

Masks belong to one capture: another capture's mask set is refused as stale. Every sanitized
frame and encoded image carries the capture's mask completeness (`Pending`, `Complete`,
`Partial`, or `Unknown` with an availability reason); an empty set never implies a completed
walk. Both publishing a mask and finishing the walk bump the generation. The first finish wins:
a late `Complete` can't overwrite `Unknown` or `Partial`. Encoded images also
carry their capture id and generation: A6 must never stage a superseded generation.

The platform's write-only `RawFrame::begin` seam remains crate-visible because the adapter
fills it, but invalidates the image before lending its buffer. Read access to a committed image
is restricted to `screen/` through `image_mut` and `sanitize`; sanitized fields are private.
Rust's visibility rules enforce this boundary; no test checks it.

The overview uses exact area weights (a box filter), rounding channel averages to the nearest
byte with halves upward. It fits within 1280×720, preserves the aspect ratio to integer-pixel
rounding, and never enlarges. The focus crop clips to the display, then centres a native-pixel
window within that intersection, capped at 1024×576; it never scales. Empty intersections are
refused. Both encode RGB JPEG at quality 85 through `image`, with no platform encoder.

`CaptureTransform` carries the display origin, DPI, overview size and optional crop rectangle.
Overview coordinates scale per axis to physical desktop pixels; logical coordinates subtract
the display origin and multiply by 96/DPI. Inverse overview mapping rounds to the nearest pixel
(halves away from zero). Crop coordinates add the crop origin. Coordinate spaces have distinct
protocol types; physical-to-overview mapping refuses points outside the display. External
floating-point coordinates must be validated as finite by their first parser. Image Debug
output contains only dimensions, byte counts and mask generations,
and encoding errors expose only a fixed numeric code, never pixels or foreign error text.

These entry points have no production caller yet: A6 owns scheduling, staged replies and
generation checks. Nothing here exports images or captures another frame.

## Deadline evidence on Rez

CI uses the injected-clock deadline fixture, not a scheduler-speed assertion. Build the integration
binary first with `CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only
nice -n 10 cargo test -p kumochi-brain --test screen --no-run`. Then acquire `runs/measuring.lock`,
wait for no cargo or rustc processes for 30 s (restart on any), and run at normal priority:

```bash
binary=$(ls -t target/debug/deps/screen-*.exe | head -n 1)
"$binary" --exact hang_uses_one_absolute_deadline_and_kills_all_pending_work --ignored --nocapture
```

Remove the lock afterward. The strict fixture asserts 1,500–1,600 ms and prints the measured time;
record Rez's scale with that result.

## Portable element data and walk

The control-view data and walk run under capture authority on a bounded reader thread. A3b
supplies Windows UIA; the production element opener returns E_NOTIMPL on every platform until
then. Nothing in the brain requests Elements until A6/B2a connects the capture flow.

One pre-order walk retains at most 512 elements, depth 16, 128 characters per name and 60,000
bytes of element JSON (commas and the reply envelope are additional). The byte cap usually binds
first. A cut records Elements, Depth, Bytes,
Budget or Provider; a hard stop replaces an earlier Depth cut. Capture-scoped ids (`c12.e7`)
carry physical desktop and window-relative rectangles from the same cached properties. Names
never appear in Debug or errors. Names outside the captured display are empty. Password names are discarded, their rectangles are published
immediately, and password subtrees are never descended into.

A source validates the root window and pid and returns its owning pid; the portable layer rejects
roots off the captured display and excludes Kumochi's process-image names case-insensitively for
every target, including Foreground. Each capture owns a fresh mask sink with its capture id,
generation and completeness. Publishing a rectangle and the first finish both advance generation.
After a Bytes or Elements text cut, mask-only discovery continues on the remaining stack, using
at most 2,048 additional reads and the same deadline. Finishing that discovery can leave masks
Complete despite the text cut. Depth skips, provider failures, deadline/read-cap exhaustion and
password rectangles with a zero side leave masks Partial, retaining all published rectangles.
Unavailability is Unknown. Elevated, Busy, Excluded and NoWindow mean masks are unknown, never that nothing
needs masking: image consumers must withhold or flag those frames, not call them fully masked.

Offline fake-source fixtures cover exact limits, Unicode names, provider/budget cuts, relative
geometry, excluded roots and password/descendant sentinels. No real window or platform timing
is exercised by these tests. Cancellation belongs to A6; worker authority and late/hung-thread handling are described below.

## Capture-scoped element operation

`Elements { capture, target }` spends the held frame's walk once, including refused or unavailable
attempts. A different capture id is Stale and spends nothing; a second attempt for the held
capture is Reused. Each new Capture drops authority and installs a fresh mask sink before checking
its ticket, so refused captures leave no walkable frame and late writes reach only the old sink.

Capture pins the foreground top-level window, pid and physical rectangle just before acquiring
the frame. Foreground walks use that pin; explicit Window targets carry a pid too. The source must
recheck root visibility and pid before reading. A walk must start before both ticket expiry and
frame arrival + 250 ms [guess]; its deadline is the earliest of those two and start + 120 ms [guess].

One disposable thread constructs and owns the non-Send platform source. Both channels have one
slot, sends never block, and jobs/replies carry capture ids. The serve thread waits until the
cooperative deadline plus 200 ms (100 ms connection + 100 ms transaction timeouts). The first
timeout returns Unavailable(Busy), retaining reached masks. Its Unknown(Busy) completeness is
sticky: a late successful walk can never upgrade it to Complete or Partial. A later walk first
drains and discards that reply, then reads only its own capture. If the source is still busy,
the next walk replies Busy and the worker exits after writing the reply. A dead thread or poisoned
sink also replies Busy and retires. Drop never joins the provider thread.

A reader-opener failure does not discard a successfully acquired frame. Its first Elements
attempt publishes Unknown(Busy), reports the numeric fault and retires the worker.

The supervisor accepts only an Elements tree naming the requested capture. Refusals pass through;
other reply kinds or capture ids become WorkerLost. There is no cancel operation yet: A6's Drop
will free capture authority and its mask sink, and the process supervisor tears down stuck threads.
No live window is read by the fake-source tests, and no Windows or timing claim is made here.
