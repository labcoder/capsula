# Screen worker

`kumochid --screen` installs its separate screen log and location-only panic hook before serving
stdin/stdout. It answers Ping and Capture (below). Request contents and panic payloads never enter logs;
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
release worker. No capture, UI Automation or OCR is implemented here yet.

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
  with the cursor off and, where Windows allows it, no border. Frames and readback poll at 1 ms
  until expiry. Session, pool and frame are closed on every path; if a close fails the worker
  answers `Fault` and exits, so no capture can keep running. Foreign failures are HRESULT codes.
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
