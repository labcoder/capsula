# Microphone ownership

The Shell microphone has shared Rust authority/framing and one narrow Windows
input adapter. There is no screen, playback, transcript, resampler, device watcher
or automatic reopen here. Brain admission and the connection-local hold identity
remain the caller's responsibility.

## Handoff

`CollectionGate::arm(GestureId) -> Result<Generation, MicError>` accepts a checked,
strictly increasing nonzero gesture. The gate is cheaply cloneable for the native
gesture reducer. `release(gesture)` closes collection for normal finalization;
`cancel(gesture)` and `cancel_current()` discard. Cancellation upgrades release,
never the reverse. The native reducer must close this gate before deferring GPUI
effects. The link worker separately closes the shared `Arc<AtomicBool>` session
permit on either pipe ending; restoring that bool cannot resurrect a cancelled
generation.

If admission is refused or the gesture ends before any source start attempt,
`Generation::discard_unstarted() -> Result<(), MicError>` consumes its single-use
start claim and retires it without a device receipt. It cannot retire a generation
already claimed by `start`; that path always needs physical completion. This keeps
a refused hold from permanently occupying the shared gate.

`MicOwner::start(Generation, Arc<AtomicBool>) -> Result<MicOwner, MicError>` consumes
the generation's one start attempt and spawns the device worker. Only call it after
Brain admission. No native audio work runs on the calling/UI thread. The owner
exposes bounded `running: Receiver<Running>` and `pcm: Receiver<MicPcm>` channels.
Forward audio on a separate nonblocking/cancellable link worker, not the UI thread.

`Running { gesture, rate }` means a stream exists but collection is **disarmed**.
After accepting this exact receipt and publishing the corresponding UI state,
call `accept_running(Running) -> Result<(), MicError>`. The source discards native
startup/backlog buffers until it observes an empty queue after collection was
enabled, then drops the first complete packet: it may straddle UI acceptance even
though it was not yet available at the empty observation. Following packets begin
after acceptance, so no startup/pre-roll samples enter the packetizer. Preparing and stopping
must remain distinct from Listening. A late open/running result cannot reopen a
released/cancelled gate.

`stop(StopReason::Release | StopReason::Cancel)` is an idempotent request, never a
physical receipt. `try_receipt(&mut self) -> Option<Result<StopReceipt, MicError>>`
polls completion without waiting on the UI; a reaper joins the source off-UI before
publishing completion. Each completion/error is consumed once. A successful receipt contains the
gesture and either `Released { packets }` or `Discarded(MicError)`. Only after native
stop, interface destruction and thread join may the matching generation retire.
A failed stop/buffer release or worker panic returns `Unconfirmed`, leaves the gate
unavailable, and forbids another arm. Dropping an owner requests cancel but cannot
replace a join receipt. The caller must retain it through cleanup.

Normal release freezes collection immediately, stops the device, finalizes the
already-packetized samples and reports only the successfully queued packet count.
The parent sends that count and source receipt over control, with PCM on data;
neither cross-pipe order is assumed. Cancel/reset, session loss, permission/device
loss, deadline, format failure and overflow use discard, not KeyUp submission.
Other-key/reset, suspend, input-session loss, presence destruction, target/model
changes, deletion and newer explicit input must all reach the same gate/stop path.
Losing foreground alone does not cancel a global gesture.

## Bounds and privacy

The worker and its independent reaper check a monotonic 60-second cap even when no
audio buffers arrive, without GPUI or Brain progress. Preparation has the approved
`HOLD_START_TIMEOUT_MS` (10-second) bound starting at admitted `start`, continuing
through matching running/UI acceptance. A stalled native call cannot keep that
authority alive: the reaper closes the gate independently, but still requires
actual source stop/join before releasing ownership. Polling waits last at most one existing
20 ms packet interval, and explicit stop unparks the worker. Native COM/driver
calls themselves cannot be forcibly interrupted safely. An outstanding native
call keeps ownership unconfirmed, never authorizes replacement, and its eventual
return must recheck the generation/permit before collecting. There is no new
drain deadline in this source and no timeout advertised as successful teardown.

PCM is mono i16 at the pinned native rate, sequenced from zero. The packetizer
retains one full 20 ms packet until a later sample proves it nonfinal. Release
marks the retained full/partial packet last. Zero samples means zero packets.
The 3,000-packet limit is terminal, as is a full/disconnected output queue; later
valid input cannot repair failure. The output queue holds four packets, the
packetizer holds one, and raw storage holds at most 3,840 frames in the supported
format (at most 30,720 bytes). A native packet exceeding that bound fails instead
of growing storage. No 60-second native-rate accumulator exists in Shell.

Raw storage, retained samples and reachable queued packets are zeroized on
discard. A packet already transferred to the link/Brain belongs to that owner and
must be fenced/cleared there; cancellation cannot erase someone else's copies.
Debug implementations expose identifiers/counts only. Errors are fixed kinds,
never driver strings, paths or audio. No audio is persisted or logged.

## Windows boundary

CPAL 0.18.2 was source-audited and rejected: its WASAPI xrun path calls a
mutex-protected error callback while the input buffer is borrowed; its CoreAudio
error path has the same problem. The Windows fallback uses the existing
`windows` 0.62 bindings, not copied CPAL code. Required feature additions are
`Win32_Media_Audio`, `Win32_System_Com`,
`Win32_System_Com_StructuredStorage`, and `Win32_System_Variant`.

The dedicated COM worker selects `eCapture`/`eCommunications` once, pins that device
and its mix rate, checks the mix format between borrowed buffers, and starts shared input with flags zero: never loopback or
automatic application-side conversion/migration/reopen. It supports mono/stereo
unsigned PCM8, signed little-endian PCM16/24/32 and IEEE float32. Extensible formats
must have the same valid/container bit width; padded 24-in-32 and other formats
refuse. Rates must meet the existing 8–192 kHz wire bounds and be divisible by 50.
Channels are averaged equally; finite float samples clamp to [-1, 1], conversion
truncates toward zero and saturates to i16. Nonfinite floats fail. Silence is
format-correct, including unsigned PCM8's midpoint.

While a native buffer is held, application work is bounded byte copying/filling,
integer checks and atomic gate/permit reads only. Raw HRESULT vtable calls avoid
constructing allocating COM errors on acquisition/release paths. No channel,
conversion, packet allocation, mutex or foreign error formatting runs there.
After returning the native buffer, recheck authority/deadline before packetization.
Cancellation discards scratch; normal release may finish samples copied while
collection was still admitted. Discontinuity (apart from WASAPI's undefined
first-buffer flag), timestamp failure and endpoint loss fail closed. Failed
native release or teardown never becomes a successful stop receipt.

## Evidence

The adjacent shared tests use only synthetic PCM, explicit monotonic timestamps
and injected worker-completion results, including the actual owner/reaper with
a fake delayed source. They never open/enumerate audio devices.
They cover delayed open/running revocation, release-to-cancel upgrade, session
loss, callback-free cutoff, one-start/one-receipt ownership, exact framing and
sample order, queue/hold limits, conversions and outer-carrier redaction.

Compilation, native permissions/device/rate/stop behavior and hot-path allocation,
latency and memory measurements still require their separately approved checks.
Windows 11 x64 is the initial target, not a measured support claim. Other Windows
versions/ARM64 and built-in/USB/Bluetooth differences remain unverified. There is
no Mac implementation or parity claim.
