# The speaker

`kumochi_shell_core::speaker` owns one connection's output device: it keeps the run's queued
speech, writes it to the device, and decides from the device's own clock which samples played.
The brain still owns what is spoken and when a run ends ([voice.md](voice.md)). The speaker
only reports what played and when its output is quiet.

**Dormant.** Nothing opens a speaker.

## Pieces

| Piece | File | Owns |
| --- | --- | --- |
| `Clock` | `speaker/clock.rs` | the device epoch, the ledger, the deadline and what counts as played |
| `Engine` | `speaker/engine.rs` | the 20 s arena, up to 256 segments, runs, stops, pauses and the reports |

## The device seam

An adapter drives a pinned stream through the shared `Engine`, which keeps the `Clock`.
Everything else is shared and tested offline against a fake device clock
(`speaker/clock_tests.rs`, `speaker/engine_tests.rs`).

- `begin(frequency, origin)` once, with the position read before the first write. A second
  epoch, or a zero frequency, freezes it.
- `observe(Observation { position, frequency, host_time, host_frequency, precise, service })`
  comes first in each pass, so a reading is checked against what was written before the refill.
- `stage(out, unread) -> Staged` writes mono samples for the next device frames into the
  adapter's scratch and consumes nothing; `unread` is the padding read before it. While the
  `Staged` lives the engine takes nothing else. `Staged::commit(unread, released)` applies it
  once the native release succeeded at `released`, with the padding read after it, or `None`
  when that read failed; dropping it takes nothing back.
- `Staged::release(put, unread)` gives back the native buffer the staged frames were copied
  into. Its last look at the session's revocation, right before `put` makes the native release,
  picks the kind: silent once the link let go, taking nothing staged, or content, committed with
  the release time `put` returns and the padding read after it. That look is the in-flight
  boundary: a revoke after it can't recall the buffer, whose content is then owed like any
  other, and nothing promises silence for a release already under way.

## What played means

- **Frames.** A device frame is `(position − origin) × rate / frequency`. The ledger is a fixed
  ring of 64 intervals of written frames, each tagged with a segment and its count before it,
  or as filler. An interval enters the ledger only at commit.
- **Played** is what the observed position has passed. It is never what was written, and never
  what the padding implies. The cursor is a count, not an index: A[0..3), filler[3..103),
  B[103..105) gives A:3 at frames 3 and 103, then B:1 at 104.
- **Content and filler.** Speech samples equal to zero are content. Silence written while
  waiting for audio or a resume is filler, and filler never counts.
- **Continuity.** Before a refill, a device holding none of the frames written so far may already
  have run dry, so nothing more is released. After it, `unread` is how many written frames the
  device held, these included. If none of the earlier ones were left, the device may have run
  dry before the write landed and played silence nothing mapped, so the clock freezes instead of
  counting anything after it.
- **Released but unplaced.** Once a release succeeds its content is owed, whether or not the
  clock can place it. When the continuity check fails or the padding read after the release
  fails, the ledger never takes the frames, and the clock is never quiet again, so no pause goes
  quiet and no stop is answered on its account. A fault after the last content frame played
  stays quiet, and a stage that was never released takes nothing.
- **The deadline.** While written frames are unplayed, a trusted advance must come within 200 ms
  [guess] of the last advance, or of the release that left frames outstanding when none were; a
  refill while frames wait doesn't renew it. Both times are on the adapter's own monotonic clock
  (`service` and the release time), never on the device's times; the epoch's first reading pins
  the host time units. A reading whose host time repeats the last one is no advance even if its
  position moved: it neither counts nor renews the deadline, though a position past the last
  frame written still freezes. Readings that stay imprecise can't hold it off either,
  and an advance that comes after the deadline is refused.
- **Freezes.** These freeze the clock for good and withdraw the output with
  `AudioOutputUnavailable`, once: a position or host time that goes backward (half a frame
  included), a position before the origin or past the last frame written, a missed deadline, a
  device that may have run dry, a full ledger, a changed, repeated or missing epoch, host time
  units other than the first reading's, a failed device call, speech from the brain that can't
  be true, or reports the brain left unread past their bound. A single imprecise reading never
  counts and never freezes.

## Reports

All go out on the connection's control queue, in order. The announcement comes first.

- `AudioOutput { rate, generation }` is the engine's first report, so a withdrawal can never
  overtake it.
- `SegmentPlayed` goes out once per segment, in order, when its total is known and has played.
  An empty segment counts at 0, and a final that arrives late is reported from what already
  played.
- `SpeechProgress` reports the playing segment at most every 250 ms, timed at the reading. An
  unread progress report is replaced by a newer one.
- `SpeechStopped { run, cursor }` answers a revoke (or a local Stop) only once every content
  frame written for the run has played. One frame short is not quiet. The cursor names the
  latest segment reported played or crossing, at its verified count. After A:3 and an empty
  B:0 it is B:0, which the brain accepts and whose heard point stays A:3. A device loss with
  written frames unverified withholds the answer. Each run's stop is answered once within the
  last 8 [guess] answers. A repeat past them is answered again, maybe with no cursor, and the
  brain ignores it for a run already final. Stops come out of order, so an older run's first
  stop is still answered, and a floor of run ids can't stand in for that memory.
- `AudioOutputUnavailable { generation }` goes out on the first freeze.
- **Bound.** At most 512 reports wait unread: every segment's receipt and as many again. Past
  that the brain isn't reading: `backlogged()` holds, whatever faulted first, so the speaker can end
  rather than grow the queue or drop a receipt in silence. The first fault stays the diagnostic.

## Runs, fences and pauses

- **Admission.** Only the run control named last may start playing; until it's named its frames
  are held, uncredited. The run already playing may finish its own audio after a newer name. An
  older run that was never named last is dropped and credited, and a held run isn't released
  because a larger one was named.
- **One run owns the device.** A newer run's frames are held, uncredited, until the old run
  has been stopped and gone quiet, or has played and reported everything. A revoke that
  arrives after the old run gave way is answered with its last cursor. A revoke for a run
  never seen is answered at once with no cursor.
- **Fences.** A stop fences its run and every older one: their later frames, and PCM for
  another output generation, are dropped and credited. Frames released before a stop still play
  and count.
- **Pause and resume.** `pause(serial)` closes admission. `quiet()` names the serial once every
  content frame written before it has played. `resume(serial)` withdraws that pause, quiet or
  not, and speech goes on at the first unwritten sample, with no skip or repeat.
- **Steps.** Every segment plays in order. Holding a step's narration until its drawing is
  shown, and dropping a failed scene's audio, come with the speech timeline (E).

## Bounds

- **Validation.** Segments are announced in order, at most 256. Chunk sequences start at 0
  with no gap or repeat, and nothing follows a final. A final's `last` equals the segment's
  total, at most 30 s. The arena plays in segment order, so a segment's audio is accepted only
  once every earlier segment has its final; the brain sends one segment's audio at a time.
  Audio goes no more than two segments ahead of the first unreported one, while empty finals
  and announcements may come any time. Anything else freezes the speaker.
- **Memory.** The arena holds 20 s at the device rate (7,680,000 bytes at 192 kHz). Twenty
  seconds plus one sample unplayed is refused, counting content already in the device and not
  yet played, but a long segment streamed at playback pace fits.

## Privacy and teardown

- **No content in diagnostics.** No sample or text is logged; `Data`'s Debug shows lengths
  only.
- **What is zeroed.** Decoded samples: in the arena as they're written to the device and when
  they're discarded, and the whole arena on drop.
- **What isn't.** The transport's framed bytes and the data reader's buffer, which predate the
  speaker, and the device's own copy of what it was given.
- **Revocation.** Once the engine's revocation flag is set it stages only silence; content
  already released is owed, as above.

## Not here yet

- Check 4, the isolated endpoint session that compares this clock with an independent endpoint
  reference and measures startup, stop and revoke latency, is pending. Nothing here measures
  native timing or what was audible.
- The `Played` snapshot and per-segment crossing times come with the conductor (E).
- What the user sees when the device is lost mid-reply is D's decision. Today a withheld
  `SpeechStopped` lets the brain's 2 s quiet timeout end the connection.
