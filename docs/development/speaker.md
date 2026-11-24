# The speaker

The queued CPAL worker is the only playback engine. It can open a paused float stream at
the default endpoint rate and feed bounded 24 kHz
mono packets. It converts on the worker through `kumochi-audio`, which also serves brain
speech and microphone input, then fills a preallocated pool of device-format blocks. The
callback takes only atomically published blocks and fills silence without waiting for the
worker. The fixed single-producer/single-consumer rings return slot IDs for reuse.
Closing either the backend handle or the transport cancels the callback. Backend
destruction starts only after cancellation; a later callback writes silence. Worker-side retirement retains the pool and rings
until the backend actually destroys its callback closures, then wipes and frees them off
the audio thread. At most one native pool may be live or awaiting retirement; if a backend never
releases its closures, that pool stays retained and new opens are refused. The worker clears
converter history at close and scrubs blocks as they become exclusively owned while waiting.
The callback checks an atomic fence before each bounded copy. A denied copy ends that
callback's copying without retrying. Stop discards ready blocks and ramps the last sample
to silence in at most 5 ms. Source observations cover copied
speech only; their positions follow the converter's device-frame phase, not how much
source it accepted. The callback publishes those observations into a bounded ring. It
latches the last non-silent frame independently, including the ramp; missing timing stays
unknown. Neither filler nor the ramp advances the source cursor.

The `speaker::queued` state keeps one bounded source queue per run. Admission or stale
discard returns ingress byte credit once; consumption or discard frees source capacity.
An old logical output generation reaching the worker cannot take a sequence slot; the link
rejects queued frames from a mismatched output owner. Timed commands settle the prior playback interval before changing
eligibility: Pause keeps fractional virtual time, and Stop settles only due evidence before
discarding and never admits more input. Stale open results do not settle the clock.

The queued worker has a separate retirement report for the brain's synthesis window.
Each segment reports cumulative retired samples and its validated final in order,
including virtual or discarded samples. Only the contiguous heard prefix contributes
to heard history or the Stop cursor.
With healthy receipts, the conservative Stop heard cursor can trail actual playback by zero to
about one callback (for example, 10–20 ms), bounded for a gap-free run by the
480,000-sample source window; nothing currently reads the persisted cursor to resume playback,
mark the transcript, or build model context.
Reported retirement neither releases a drawing
binding nor confirms a quiet tail. A retiring worker retains its report sink through
a confirmed future quiet deadline; an unknown tail never produces a quiet receipt.
The sink accepts or refuses each report in order. Refusal keeps the accepted prefix,
closes ingress, and stops the owner, which signals report failure once. It attempts
no more reports and makes no quiet claim.

Only contiguous, generation-matched observations on a live route can extend the heard
prefix. Loss keeps the last settled prefix; delivery time cannot prove pending estimates.
The handed-off uncertain interval is consumed as unheard, leaving app-only PCM queued.
A gap or observation overflow immediately fences further staging and presentation on that
route, including across timed commands. The owner can then retire it with the real callback
handoff boundary, or Stop can discard the queue without the handoff boundary. Stale
observations don't request recovery. Virtual pacing never crosses the first unheard gap
in heard history. Finality after Stop still requires every applicable generation-specific
tail proof and the latest known deadline. Two unresolved device tails refuse another
physical open, but virtual pacing continues.

Stop is sticky and bypasses the bounded command mailbox. The owner handles commands
before admitting ingress, and a terminated owner reports disconnection, not backpressure.
Open, play, pause and stream destruction run on a separate backend thread. One process-wide
slot is held from before device discovery until the native stream is destroyed, so a stuck
backend can't start more native opens. If Stop closes the gate during discovery, a late
successful open is dropped without starting playback. If playback admission wins the race,
Stop still fences the callback's source. Callback work and route notifications wake the owner.
A native resume must return within the existing 200 ms stall interval, even if
callbacks continue. Callback activity refreshes the deadline only for staged speech
outside a pending resume. A stall fences the old route and allows virtual pacing
without waiting for the device call to return or starting a second stream.
Loss, an invalid observation, or overflow enters the same fenced recovery path. A retired
route whose tail never arrived counts as quiet once the backend confirms its stream destroyed,
since that stream can't play anything more. A
reroute replaces the stream instead of assuming that old endpoint timing still applies.
An xrun rechecks refill readiness on every tick, even without new ingress.
Only an authoritative typed RunState starts this worker on an internally selected queued pair.

The offline tests that require a positive heard prefix open the fake route before the
worker admits PCM: the first run for Plain, and each run in the two-reply Presence
onset test. Each fake playback has its own retirement queue so
independent fixture pools can't fill the single native queue. A callback can render
identifiable samples after virtual playback has left an unheard gap, so rendered samples alone
don't establish a heard prefix. The Plain tests separately check the first source
handoff and the original run's retirements accepted by the Brain. The quiet cursor
matches those receipts even when Done made the later quiet report stale to the Brain.

The queued link keeps its authenticated pair in the existing output owner. It does not grant
capture or accept turns before the matching registration receipt. Ending or withdrawing
that pair drops its run input before aborting both pipes and closing both writer mailboxes.
On reconnect, an otherwise admissible queued pair waits for the exact retired-microphone proof
while an old bound inhibit still owns capture. Nothing registers or starts during this bounded
wait. A missing proof never drops the inhibit, and audio tail debt still refuses admission.
A worker's exact known-tail stop report can still pay its local quiet debt after disconnection.
It cannot send that old receipt to another pair. A live pair needs the report queued before
admitting another turn; a refused live report ends the pair. After an earlier retirement report
is refused, the original worker can observe its fenced callback for at most 500 ms without
feeding audio or sending another report. That local proof doesn't release the device permit;
the backend still owns disposal. Unknown tails still block replacement. An
absolute startup deadline ends the same pair even while control traffic continues. Ordinary launch still selects Legacy. That session starts quiet and never opens a playback
device. Unexpected speech PCM is wiped, credited and dropped; typed replies and voice input
continue on the Legacy pair. There is no user-facing queued selector or fallback.

## Dormant segment gate

A hold blocks native staging and virtual pacing at one segment, not its predecessor.
Releasing it does not catch up elapsed hold time. Discard keeps PCM ingress in sequence
and returns its credit, but skips the discarded source in head order without marking it
heard. Later PCM for that segment cannot restart it.

The worker applies a hold before it stages the segment. A step fence discards an
unstaged line without touching the device; accepting the first source packet for
staging commits the line, even if the callback hasn't taken it. The eight transport
blocks hold about 160 ms of source, but staging is not a time bound: a committed
line can wait seconds during a pause and play on resume unless its run is stopped
or replaced. No production caller enables this gate yet.

## Dormant drawing conductor

The internal narrated fixture uses an exact source reservation at queued ingress.
A matching presenter level can be replaced or disappear, but cannot erase ownership
of its unallowed bytes. The worker applies the gate after its command drain and Stop
check, and a separate keyed Allow permits PCM. A fence retires and credits never-staged
PCM without playback; already-staged PCM keeps the line's commitment and original tail.
Fence and terminal reports are ordered;
the onset level is accepted by the presenter only for the same binding and live
physical route, even if the segment finishes before the presenter reads the onset.
An empty final validates the segment, returns credit and retires metadata without
opening an idle route. It does not shorten an already committed physical tail, and
neither it nor virtual source proves Played. Quiet remains a separate result from
the original route's tail. Plain and Legacy do not select this mode.

The dormant drawing conductor decides only an assignment's audio gate and picture timing.
The presenter supplies readiness and fences, the worker supplies actual handoff and the
current physical route's source span, the link reports worker absence, and the brain supplies
synthesis finals. It doesn't infer submission from staged or virtually consumed samples.
Preparation allows sound without waiting for visual eligibility; after a 250 ms hold the
whole line is allowed and the picture takes silent timing. On rebuild, an unshown speech
step has 250 ms to receive a handoff from the new route before taking silent timing.
A route rebuild mid-line ends speech-following for that shown step; it takes authored
timing immediately. A shown or fenced step can't show again. Step-level Drop uses the
staging commitment above. These defaults aren't measured. The dormant worker now uses
one reducer and the session clock supplied by its caller. It reads the keyed view and
applies the matching hold before feeding source, services hold and rebuild deadlines on
its own wake, and publishes one keyed picture. An accepted first handoff on the current
route predicts onset; shown motion is authored and later progress doesn't publish a level.

A sticky end publishes independently of timing. An end before visual eligibility can
carry Wait: it releases the audio binding while the presenter keeps the step. Retiring
unheard source never produces a played terminal; an earlier gap doesn't prevent a
later fully heard segment from ending. A fully retired unheard line takes silent
fallback even when a device opens as virtual pacing exhausts it. Retirement still
keeps the existing rebuild grace. Ready PCM arms the hold during a pending device
open, so expiry can release it to virtual pacing without another packet.
Missing owner, backend startup failure and worker exit
are reported to the pair's bridge, which supplies keyed Unavailable when there's no
onset to preserve. An ordered Applied for the exact binding lets the bridge send
Allow; only then can that assignment's PCM reach the worker. An unresolved assignment is
not unassigned. The narrated adapter is fixture-only.

## The output owner

Each link has one output owner. A Legacy connection gets a fresh no-output epoch before
Connected, so an old capture claim cannot match a replacement session. A queued pair can
lend the capture inhibit only after its exact logical output is registered, with Show
Screen confirmed off and no pending setting change. With Talk-back on, the pair must be
Plain, with no typed turn reserved, no draining predecessor, and the previous run's
`SpeechStopped` accepted by the control queue. While the inhibit is held, no typed turn
can reserve the pair. Registration cannot prove that an earlier route or microphone retired.

The inhibit token binds one gesture and microphone generation. The output owner rechecks
the session, output key and token before the microphone starts or collects. Ending a
session cancels its bound microphone but keeps the inhibit until the owner sees proof of
retirement. See [the capture inhibit](voice.md#the-capture-inhibit) for the input lifecycle.

The worker also publishes a `Speech` lease for Esc: positive presentation evidence that
expires on its own. [Esc](input.md#esc) describes it. It never stands in for a quiet receipt.
The callback normally sends one first/last nonzero pair to a separate, optional ring.
If extending a pair would reach 500 ms, it keeps that pair and starts another from
the current copy's actual timestamps. It sends at most eight pairs after copying;
capacity loss drops some Esc evidence without delaying PCM, receipts or Stop.

Once the brain's Stop has been queued, a queued stop closes admission immediately. If the
owner is busy, Presence can offer the same stop again; an undeliverable stop ends the pair.
Talk-back off also ends a queued pair. The link closes run input before aborting both pipes.
