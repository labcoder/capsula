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

A request can also be streamed (`request_stream`): the worker sends several whole replies under
one id, and the kind's `is_terminal` reply ends it. A plain request requires one terminal reply;
an intermediate reply to it is a protocol break. Every reply of today's screen and speech kinds
is terminal, so they get one reply as before.
A request takes at most 16 messages. A stream also takes at most 12 MiB of serialized replies in
total, counted on every chunk before it is stored, partial messages included; a plain request
keeps its kind's cap. Going over either bound loses the worker, and so does any reply after the
terminal one, whose id is no longer pending. A stream queues 4 whole replies for its reader
[guess]. A reader that falls behind gets `WorkerLost` after the replies already queued: the
supervisor keeps the id and discards its replies until the terminal one or the
deadline. Closing a stream does not itself lose the worker, but its deadline still can.
No reply extends the stream's absolute deadline. A loss ends each stream after the replies it had
already queued. Dropping a stream doesn't tell the
worker yet; the screen kind sends its Drop control with the capture streams that come next.
A hostile worker can make the brain hold up to 16 × 12 MiB of streamed replies for one deadline.

Each request has one 1.5 s absolute deadline [guess], including queue time; fixtures can inject it.
A loss fails all pending operations and requests worker termination. The supervisor then waits
off the async thread, up to 2 s [guess], for the process to exit, and broadcasts
`Exit { generation, confirmed }` and then the loss's `Generation`: the first worker is 0 and each
respawn the next, and every reply names the generation that sent it. `WorkerLost` alone is not
confirmation that the native process exited. The loss broadcast is the one recovery boundary.
Requests that arrive while the exit is confirmed wait in the queue under their absolute deadlines,
as during a respawn, so a caller that retries at once after a failed request isn't refused; the
replacement serves them after the loss. Outside crash backoff, a request made after the loss also
reaches the next worker without a retry. An exit that isn't seen in 2 s is fatal. The supervisor
marks every handle refused before it broadcasts that loss, starts no worker beside one that may
still run, and ends; held requests and every request from then on get `ExitUnconfirmed`
(`ScreenError::ExitUnconfirmed`, `SpeechError::ExitUnconfirmed`).
The first
two losses respawn immediately; a third within 60 s waits 0.2 s, then 1 s and 5 s for later losses
[guess]. Requests during backoff fail fast. During an in-flight respawn, requests wait for the new
worker under their original absolute deadline; a failed spawn immediately fails its queued callers.
Respawns are serialized and run off the async thread, so process creation cannot delay delivery
of `WorkerLost`; only the first spawn, at startup, runs on it, before anything is served. A loss
after the last handle has closed doesn't respawn, so shutdown can't start a worker in a closing job. Loss history expires at exactly 60 s.

Debug builds accept one-request fault fixtures and `KUMOCHI_SCREEN_FAULT=hang|crash|garbage|panic`.
The environment switch applies on the next request of every respawn; neither path exists in a
release worker. Windows UI Automation supplies the element operation; OCR is not connected.

The macOS platform equivalent is `posix_spawn` file actions for stdio and a process-group kill;
it remains unimplemented and unverified.

## Local shell exclusion

Presence owns a never-published local transaction in `presence/exclusion.rs`. No production
caller acquires it, and it cannot send HoldRecognized, Include, a ticket or a capture request.
The one private acquisition path snapshots exact native affinity before changing the dock,
hit proxy, status panel, typing panel, threads panel, settings panel and drawing overlay.
Absent lazy panels have no pixels; their create/re-show hooks exclude them after the existing
owner stores the hidden window and before placement can show it. A failed hook leaves the
new window hidden. The zero-area taskbar host and Explorer-owned UI are outside this claim.

Drawing keeps the overlay's `Visibility` recovery lifecycle, paint scratch and paint notices.
Its GPUI surface adapter attaches the drawing receipt before show, including a replacement
window. New drawing windows start capture-allowed because production holds are dormant;
a future activation must acquire the shared exclusion owner before announcing a hold and
retain its receipts until authoritative native retirement, not restore them on key release.

The transaction reads all present prior values before mutation. A failed getter grants nothing;
a failed setter rolls back every potentially changed slot, including that setter's slot.
Rollback and final restoration continue after an error, retaining failed receipts and refusing
another acquisition until cleanup succeeds or the exact original instance is confirmed destroyed.
Receipts keep the original GPUI handle or native owner and a destruction witness, never a delayed
HWND as identity. A recycled native number cannot inherit an old window's snapshot. Failed GPUI
updates retain debt; they are not proof of destruction. Destruction is witnessed by WM_NCDESTROY
or a successful native-owner DestroyWindow, not by pointer-capture loss or owner Drop.

One weak-owner timer carries the full session/input/checked lease-generation key. Its three-second
cleanup is safe only because this local transaction never authorized a native acquisition.
CaptureDone/Failed travel with the actual reader's connection generation; Presence checks its live
session and input before treating them as local cleanup hints. They do not establish native drain.
Connection loss and direct session replacement revoke before accepting another session, retaining
failed cleanup debt; quit attempts bounded cleanup without waiting on a worker or pipe.
No local cleanup sends ExclusionLapsed, so queue acceptance cannot be mistaken for retirement.

Future published grants require both authoritative fencing of every pending/late stage and native
source drain or confirmed worker exit before restoration. Expiry, cancellation, a queued lapse,
link loss and the name of a terminal message prove neither. An already accepted frame keeps its
normal capture lifetime. At most one published lifetime may use a session/CaptureFor pair; an
exposed input cannot be re-leased, and no replacement is allowed over restoration debt. Input
allocation and actual coordinator/worker retirement are later integration work, not implemented
by this dormant transaction. Source regressions do not establish compositor timing or pixel privacy.

## Capture under a ticket

`Capture { ticket, display, exclude }` takes one frame of one physical display into the worker's
`RawFrame` and answers `Captured(FrameMeta)` or `Refused`; **pixels never cross the pipe**.
The worker can sanitize and encode them locally, but no operation exports them.

- **Authority.** `TicketWire` can only be built inside the brain. Nothing in production sends
  `Capture` yet; ticket minting and the capture flow are not connected.
  `Exclusion::KumochiWindows` is the caller's declaration of a live shell exclusion lease,
  not proof of one.
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
  about COM or GPU allocations, which happen per capture. Releasing the frame zeroes the buffer's
  whole capacity with `zeroize` and frees it; the next capture allocates afresh.
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
  participant B as Brain
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
carry their capture id and generation: consumers must never stage a superseded generation.

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

These entry points have no production caller yet; scheduling, staged replies and generation
checks are not connected. Nothing here exports images or captures another frame.

## Regional OCR

`screen/ocr.rs` accepts only `SanitizedFrame`. Sanitizing records the exact clipped physical
rectangles it painted at that generation; these, not a later independent mask list, filter OCR
words. `OcrRegion` comes from the focus crop's `CaptureTransform` or a readable completed
`ElementTree` window belonging to that capture. Regions clip to the captured display; empty
intersections are refused. There is no display source or fallback, but a maximized window is
valid even when its bounds equal the display.

The portable trigger uses fewer than 20 nonempty named elements [guess], or the exact native
class `Chrome_RenderWidgetHostHWND` of the **focused element's HWND**, not the top-level window.
A6 supplies that metadata and integrates the operation; A5 adds no UIA reads or production
request/reply route. OCR remains disconnected from the worker and exports no content.

One bounded channel (capacity one) feeds a dedicated OS thread. Only that thread creates its
MTA apartment, English WinRT OCR engine, BGRA8 bitmap and recognition operation; no native object
crosses threads. Regional rows retain the capture's BGRA order without a channel swap.
Missing English or a native failure returns fixed `Unavailable`, never foreign
error text. macOS has no adapter yet. The only pixel copy is the selected sanitized region.
Before allocating a native bitmap, oversized regions scale aspect-preservingly to
`OcrEngine::MaxImageDimension`, without enlargement. Small text can be lost at that scale;
tiling is deferred until integration measurements or a user's misses justify it.

Pending masks are refused before recognition. Results retain capture, generation and mask
completeness, including `Partial` and `Unknown`; neither claims full protection. Native floating
word bounds scale back to the region, round outward, clip, and translate to desktop physical
coordinates before any mask intersection. Any intersecting word is removed **before** caps of
2,000 words and 32,768 UTF-8 bytes [guesses]. Byte truncation preserves scalar boundaries and is
reported explicitly. After completion the caller re-locks the shared `MaskSink`; changed capture
or generation (or poison) discards all words and returns content-free `Superseded`. A6 must still
admit only current generations when publishing.

Admission, response and final mask-lock waits observe cancellation, channel EOF and the same
absolute deadline. Pixel copying checks between rows; native operation polling and word extraction
observe the shared stop flag and deadline. An aborted caller retires that recognizer, so a late
reply cannot revive it. A native failure also stays failed for that recognizer's lifetime.
Drop does not join or forcibly kill the native thread; native cancellation
is cooperative and a hung call remains contained by the killable screen worker. A6 owns worker
retirement. All carriers have content-redacted Debug, and errors contain fixed kinds only.

Offline fixtures use synthetic pixels and fake engines, including a blocked engine with a newly
published mask. They cover trigger boundaries, region provenance/clipping, maximized windows,
one pixel over the engine maximum, outward inverse geometry, mask filtering before both caps,
Unicode bounds, completeness, supersession, every caller wait's cancellation/EOF/deadline, closed
engine channels, fixed unavailability, and sentinel-safe diagnostics. They never read real windows,
capture the desktop or run native OCR. An ignored, manually named Windows smoke test checks
that the real recognizer accepts a blank synthetic BGRA bitmap and returns zero words.
It never runs in CI. Real-region recognition and timing need a separately scheduled check.

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

## Dormant native ownership loop

`screen/actor.rs` is an alternate worker-local protocol and loop, not selected by the
production brain or worker entry point. Those still use the original synchronous request path.
The alternate loop creates a dedicated capture producer; that thread constructs, uses and drops
its non-Send capturer, resolving the window under the hold's point and capturing that window
alone. Only the sole-owned
`RawFrame` and content-free result identity move back to the actor. Capture and UIA each reserve
one completion slot until taken; their nonblocking wakes may coalesce in the bounded inbox.
The actor checks the slots after every event and nearest-expiry/deadline wake.

The alternate Context stream emits only acquisition metadata and completion. Worker-local Drop,
Retire and expiry revoke publication before clearing the frame and mask state. A returning image
is scrubbed/freed rather than published, and a late element tree is taken and discarded. Disposal
acknowledgement waits for both native jobs to finish; stuck or retiring native work cannot
acknowledge. At most 16 controls may wait for disposal, matching the inbox cap; the seventeenth
gets a fixed Busy refusal. Repeated release is idempotent and retains no release history.
Each request is owned by a hold: its shell connection's epoch, then its gesture, ordered in that
order. The worker keeps one high-water retirement fence and refuses any request at or below it, so
a hold with no thread yet still has an identity, and one Retire covers every earlier hold.

The current allocation is zeroized before growing, releasing or dropping a RawFrame. The tests
observe synthetic bytes immediately after scrubbing and before freeing; a plain-fill replacement
remains observationally equivalent in that seam, so optimizer resistance relies on `zeroize`.
This does not claim the legacy production path implements Drop or expiry disposal. Brain-side
streams, fences across respawn, full-queue termination and matching-generation release waits
remain A6b2b, which alone may activate the alternate route. Images, OCR and Supersede remain A6c.

## Window under the pointer (dormant)

A screen hold captures one window, not the display. `Capturer::pointer_window` resolves the
window a click at the hold's point would land on, and `Capturer::capture_window` takes one frame
of that window alone, so whatever is stacked above it stays out of the image. Nothing in
production calls either yet.

On Windows the walk reads the top-level z-order from the point. It skips windows whose
rectangle or window region leaves out the point, before it opens any process, then Kumochi's own
windows (the worker, the connected shell and Kumochi images: the same rule the element walk
uses), hidden and cloaked windows, and click-through windows: layered ones with
`WS_EX_TRANSPARENT` or zero constant alpha. `WS_EX_TRANSPARENT` alone doesn't let clicks
through, so such a window is taken like any other. A disabled window is still the target, which
`WindowFromPoint` wouldn't give. Reaching the desktop (`Progman`, `WorkerW`) means `NoWindow`.
The target is that window's `GA_ROOT`.

A layered window drawn by `UpdateLayeredWindow`, or colour-keyed, lets clicks through its
transparent pixels, and no other process can read which those are. For one at the point, the
walk asks `WindowFromPoint`. If the hit is that window, it's the target; if the hit is a window
below it in the z-order, the walk skips it. A hit anywhere else (a window above it that the walk
skipped, such as Kumochi's, or none) says nothing about the drawn window, and `WindowFromPoint`
skips disabled windows, so in those cases the walk refuses with `Ambiguous` rather than capture
a blank overlay or the window under it. The probe saw no message sent to the drawn window's
process, and no wait while that process was hung.

Before any pixels, the target is refused when it's minimised, cloaked, Kumochi's, elevated (a
process the worker can't open counts as elevated; for a UWP frame the hosted core window's
process is checked too) or capture-protected by display affinity. The resolved `Target` keeps the
window, its pid, a second identity word (the creating thread on Windows), its DPI and its extended
frame bounds, which are the frame's origin and size. Capture creates a `GraphicsCaptureItem` for
the window, checks the same identity again, and requires the item's size to equal the frame
bounds. The window closing, being minimised, cloaked or capture-protected, or any change to its
identity, frame or DPI before the pixels are read, refuses with `WindowChanged`; so does an item
that can't be created for a window that has since changed. Every failed capture zeroes and frees
the frame before it returns (`RawFrame::fill`).

`platform::windows::window::tests::native_window_probe` is an ignored native check over sentinel
windows owned by a second test process (the worker's own windows count as Kumochi's). It covers
the occluder, click-through, disabled, region and drawn-overlay cases, the desktop stop, the
frame origin against a password edit, and the refusals, including a window that changes after
the walk. It opens topmost windows on the main display for a few seconds, so run it on an idle Windows
desktop:

```
cargo test -p kumochi-brain --lib native_window_probe -- --ignored --exact --nocapture
```

## Sealing a hold's image (dormant)

In the alternate loop, a hold's `Context` carries the hold's point instead of a display. The
capture thread resolves the window under it and captures that window alone. A refusal or a fault
answers that hold only: there is no screen for it, and the same capturer serves the next hold.
Only `Retire` ends the worker.

After the element walk, `Seal` hands out the hold's one image as a `ScreenContext`: the frame
masked and encoded as an overview JPEG, with its `CaptureTransform`. It seals only when the
hold's one walk read the captured window itself (its window and process) and found every
password mask, and refuses with `Unmasked` otherwise. The frame is scrubbed and
freed whatever the answer, so a second `Seal` is `Stale`.

The JPEG is capped at 2 MiB (`JPEG_CAP`), a guess until real windows are measured. The encoder
writes into one buffer allocated at the cap, so growth never copies it, and an image over the cap
refuses with `TooLarge`; it is never downscaled further. `TooLarge`, `Unmasked` and
`Ambiguous` are ordinary refusals.

The JPEG's bytes are user content. `ScreenContext` zeroes them on drop and its Debug prints only
the byte count and size. The encoder's buffers (the resampled RGB and the JPEG) are zeroed on
drop, errors included. Nothing writes the bytes to a log, a file or a store.

**Copies of a reply.** A screen reply can carry window text or the image, so the screen worker
and the brain keep no copy of one they don't zero:

- The worker writes with `write_sized`: the message is sized first and serialized into one
  zeroing buffer of that size, and its frames, each up to 262,080 body bytes, are encoded into
  one zeroing buffer that fits the largest, so nothing grows. It writes to an unbuffered stdout
  (`platform::raw_stdout`, a duplicate of the handle), not the standard library's line buffer.
- The brain reads the screen kind's replies with `Zeroed`: each frame into a zeroing buffer of
  its own length, each chunk's body zeroed once copied. A reply of several chunks is put together
  in one zeroing buffer allocated at the 4 MiB reply cap when its first chunk arrives, so it
  never grows; each such reply pays a 4 MiB clear when it's dropped. A reply of one chunk is
  decoded from that chunk. Frames left queued when a worker is lost are zeroed too.
- `ScreenContext`'s JPEG is sent as one byte string, the same bytes as before, and received
  with one exact allocation. A received context is registered for dump exclusion like the
  worker's.

Speech replies and the shell pipe keep their plain framing (`Assembly`, `write_message`).

What's beyond reach: the kernel's pipe buffers between the worker and the brain, the TLS
library's buffers of plaintext not yet sent, the socket's buffers (ciphertext), and paging,
since no buffer is page-locked.

On Windows, a frame's pixel buffer and the sealed JPEG are each registered with
`WerRegisterExcludedMemoryBlock` while they hold content. That keeps the block out of Windows
Error Reporting's reports only: not out of dumps a debugger or another tool takes, nor out of a
system memory dump, and a process can register only a limited number of blocks. A frame's buffer
stays registered while it's reused and is unregistered once zeroed, before it's freed. The JPEG is
registered once encoding finishes, and the resampled RGB never is; both live only for the encode.
The registration is in-process only. When Windows refuses it, capture carries on, and that
buffer's contents can appear in an error report written while it holds them.

Chromium's control view can miss a rendered page's fields. Every `Chrome_RenderWidgetHostHWND`
element the walk passes through starts a page, and each page needs 20 named elements of its own
among those the walk stored; otherwise the masks are `Partial` and the hold doesn't seal. A page
first reached after the walk's element or byte cut has none stored, so it's `Partial` too, and a
large page the walk cuts short refuses for now. The threshold is a guess until a real page is
measured.

## Portable element data and walk

The control-view data and walk run under capture authority on a bounded reader thread.
On Windows, the source creates `CUIAutomation8` in that thread's MTA, with 100 ms connection
and transaction timeouts. A control-view walker builds a per-node cache of name, control type,
physical bounds and password/offscreen flags. Password and offscreen names are not fetched
from that cache. Root handles are checked for top-level visibility and ownership before the UIA
lookup, and the native window owner is checked again after it. The worker and the
connected shell are excluded by pid, with Kumochi process-image names also excluded by the
portable policy. Other platforms return E_NOTIMPL. Nothing in production requests Elements
yet because the brain's capture flow is not connected.

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
at most 2,048 additional reads [guess] and the same deadline. Finishing that discovery can leave masks
Complete despite the text cut. Depth skips, provider failures, deadline/read-cap exhaustion and
password rectangles with a zero side leave masks Partial, retaining all published rectangles.
Unavailability is Unknown. Elevated, Busy, Excluded and NoWindow mean masks are unknown, never that nothing
needs masking: image consumers must withhold or flag those frames, not call them fully masked.

Offline fake-source fixtures cover exact limits, Unicode names, provider/budget cuts, relative
geometry, excluded roots and password/descendant sentinels. Reader fixtures use real threads and
clocks, but never read real windows or measure platform calls. There is no cancel operation yet;
worker authority and late/hung-thread handling are described below.

The ignored owner-run UIA smoke/timing checks require explicit `UIA_TEST_HWND` and
`UIA_TEST_PID` values; they never select the foreground or launch an application. An
outside-repo wrapper owns the fixture or newly launched Settings process and its cleanup.
The `test-support` feature exposes `WindowSession` and `WindowProbe` for a release worker
that measures the actual bounded reader, serializes the complete tree in memory and discards it.
Cold samples use separate worker processes and UIA objects; warm samples retain one process
and discard its first walk. The first check gap includes lazy UIA setup; a walk with no checks
reports its entire duration as both the longest unchecked gap and the tail. The harness reports only
structural counts and timings: text and mask reads, navigation-call count, deadline-check count,
the longest interval between deadline checks, and the tail interval from the last check to
completion. It never reports window names, element text, rectangles or captured content.
The first walk is timed separately through the bounded reader, including lazy UIA initialization;
the second walk uses the same initialized source. Initialization remains lazy until measurement
justifies changing it. A COM guard balances initialization even if later setup fails or unwinds.
The source's root pid is the native window owner's pid, matching foreground pinning, not the UIA
provider process, which can differ for hosted UWP content. The second ignored check verifies
that contract for the explicitly supplied Settings root. Live checks belong only to an
owner-approved session; short unlocked dry runs do not qualify as timing measurements.

## Capture-scoped element operation

`Elements { capture, target }` spends the held frame's walk once, including refused or unavailable
attempts. A different capture id is Stale and spends nothing; a second attempt for the held
capture is Reused. Each new Capture drops authority and installs a fresh mask sink before checking
its ticket, so refused captures leave no walkable frame and late writes reach only the old sink.

Capture pins the foreground top-level window, pid and physical rectangle just before acquiring
the frame. Foreground walks use that pin; explicit Window targets carry a pid too. Portable walks
reject a root whose pid differs from that hint as NoWindow; the source must also recheck native
root visibility and ownership before reading. A walk must start before both ticket expiry and
frame arrival + 250 ms [guess]; its deadline is the earliest of those two and start + 120 ms [guess].

One disposable thread constructs and owns the non-Send platform source. Both channels have one
slot, sends never block, and jobs/replies carry capture ids. `begin` hands the thread a job
without waiting and `poll` takes its completion. A `begin` while a job is pending, even one
whose completion landed and wasn't taken, retires the reader rather than queue a second job or
drop that completion, so the reply slot never fills. The reader opener takes a wake that the
thread calls after each completion lands; it never blocks, and a dropped wake leaves the
completion in its slot for `poll`. Today's serve path passes a wake nobody listens to. The foreground window is sampled on the capture side, never on this
thread, so a stuck walk can't delay the next capture's foreground. Today's `walk` is the
compatibility path: it discards a timed-out walk's late completion, calls `begin`, then waits
on the reply channel until the
cooperative deadline plus 200 ms (100 ms connection + 100 ms transaction timeouts). The first
timeout returns Unavailable(Busy), retaining reached masks. Its Unknown(Busy) completeness is
sticky: a late successful walk can never upgrade it to Complete or Partial. A later walk first
drains and discards that reply, then reads only its own capture. If the source is still busy,
the next walk replies Busy and the worker exits after writing the reply. A dead thread or poisoned
sink also replies Busy and retires. Drop never joins the provider thread.

A reader-opener failure does not discard a successfully acquired frame. Its first Elements
attempt publishes Unknown(Busy) and reports the numeric fault, but the worker keeps serving Ping
and Capture. The portable capture state retries a failed reader opener, including a failed
reader-thread spawn, on the next capture attempt. The reader thread likewise discards a failed
source initialization and retries it for the next capture's Elements; a successfully initialized
source stays on that thread. Neither retry replays a spent walk or renews capture authority.

The supervisor accepts only an Elements tree naming the requested capture. Refusals pass through;
other reply kinds or capture ids become WorkerLost. There is no cancel operation yet;
new captures replace authority and the mask sink, and worker termination tears down stuck threads.
No live window is read by the fake-source tests; they do not verify native UIA behavior or timing.
