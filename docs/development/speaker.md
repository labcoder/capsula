# The speaker

`kumochi_shell_core::speaker` is growing into the owner of one connection's output device. So
far it holds the played-content clock: the shared rules that decide, from the device's own
clock, which written samples played. The brain owns what is spoken and when a run ends
([voice.md](voice.md)).

**Dormant.** Nothing opens an output device.

## The clock

`speaker/clock.rs` is shared by every platform and tested offline against a fake device clock
(`speaker/clock_tests.rs`). A device adapter only reports to it:

- `begin(frequency, origin)` once, with the position read before the first write. A second
  epoch, or a zero frequency, freezes it.
- `lease(unread)`, with the padding read before the refill, then `push(lease, frames, tag)` for
  each run of frames it stages, then `commit(lease, unread)` once the native release
  succeeded, with the padding read after it, or `None` when that read failed. A lease of no
  frames commits nothing. Dropping a lease takes nothing back.
- `observe(reading, played)`, with `Observation { position, frequency, host_time,
  host_frequency, precise, service }`. Each content interval the reading reaches goes to
  `played` as its segment and the count it got to.

## What played means

- **Frames.** A device frame is `(position − origin) × rate / frequency`. The ledger is a fixed
  ring of 64 intervals of written frames, each tagged with a segment and its count before it,
  or as filler. An interval enters the ledger only at `commit`.
- **Played** is what the observed position has passed. It is never what was written, and never
  what the padding implies. The count is a count, not an index: A[0..3), filler[3..103),
  B[103..105) gives A:3 at frames 3 and 103, then B:1 at 104.
- **Content and filler.** Silence written while waiting for audio is filler, and filler never
  counts. `quiet()` holds once the last content frame written has played, not one frame
  before. `unplayed()` counts content written and not yet played.
- **Continuity.** Before a refill, a device holding none of the frames written so far may already
  have run dry, so no lease is given. After it, `unread` is how many written frames the device
  held, the lease's included. If none of the earlier ones were left, the device may have run dry
  before the lease landed and played silence nothing mapped, so the clock freezes instead of
  counting anything after it.
- **Released but unplaced.** Once a release succeeds its content is owed, whether or not the
  clock can place it. When the continuity check fails, or the padding read after the release
  failed, the ledger never takes the frames and `quiet()` never holds again. A fault after the last
  content frame played stays quiet, and a lease that was never committed takes nothing.
- **The deadline.** While written frames are unplayed, a trusted advance must come within 200 ms
  [guess] of the last advance, or of the release that left frames outstanding when none were; a
  refill while frames wait doesn't renew it. Both times are on the adapter's own monotonic clock
  (`service` and the release time), never on the device's times; the epoch's first reading pins
  the host time units. A reading whose host time repeats the last one is no advance even if its
  position moved: it neither counts nor renews the deadline, though a position past the last
  frame written still freezes. Readings that stay imprecise can't hold it off either,
  and an advance that comes after the deadline is refused.
- **Freezes.** These freeze the clock for good: a position or a host time that goes backward
  (half a frame included), a position before the origin or past the last frame written, a
  missed deadline, a device that may have run dry, a full ledger, or a changed, repeated or
  missing epoch. A single imprecise reading never counts and never freezes.
