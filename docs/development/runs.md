# Runs and the coordinator

A **run** is one user turn: in slice 2 a typed message, later a voice hold or an ask-about. The
brain's **coordinator** (`crates/brain/src/coordinator/`) is its core loop, written sans I/O: it
takes inputs and returns effects, and the systems around it (the store, the engine, the pipes,
timers) do the work and report back. So the order of Esc, late completions and interruption is
tested as plain unit tests.

## The shell's input target

`crates/shell/src/runs.rs` owns `Target::{Kumo, Task(ThreadId), New}` separately from
`Threads.open`, the history being viewed. Viewing doesn't change the target or selection
revision. **Switch to this task**, **Back to Kumo** and **New task** advance the revision;
switching to the id in `SettingsView.main_thread` normalizes to `Target::Kumo`.

The default is Kumo at shell start, when `met()` meets a new brain incarnation and drops a
non-durable target, and when the user confirms deletion of the targeted task. These fallbacks
advance the revision too. A task known from a stored row or an answered run is durable and
survives reconnect; an unconfirmed new task falls back to Kumo when the brain incarnation
changes. This replaces slice 2's new-conversation default (OSC-157).

Each submit captures the target in `TextSubmit` and queues the current revision in a FIFO.
`Target::Kumo` resolves to the connection's `SettingsView.main_thread`; `Target::New` sends
`None`. Submits are refused as offline until that connection has supplied its `SettingsView`,
even for New task. Disconnect clears the main id and pending submissions, so a previous
connection's id cannot be reused before the new view arrives.

The shell also retains the submitted text until its named run ends. A named Failed state
with SessionEnded, AccountRefused, SignInNeeded, ClaudeCodeSignedOut, AnthropicKeyRejected or
RenewalUnconfirmed returns that text once before Answering, only while its captured selection revision and
thread still match the current input target. The typing view restores it only into an empty
input; even whitespace is a newer draft. No frame resends a turn automatically. General
account errors and TurnRefused cannot restore text because neither identifies a failed run.
Esc, a new submit attempt, a target change, disconnect, a new brain incarnation, confirmed
deletion, success or interruption discard the recovery content. Duplicate terminal states
cannot offer it again. Recovery carriers redact their text in Debug output.

Otherwise, a named failure declaring Retry can transfer its submitted snapshot to one
run-owned recovery slot. Automatic restore and this transfer are exclusive: a draft already
returned to the input has no second retained copy. `Runs::retry` takes the matching slot
before checking its captured revision, thread and the current draft, or attempting delivery.
Refusal consumes it too. New's named run establishes the stored thread before capture can
be retried; Retry never submits another unbound New turn. Dismissal also drops the slot.

`chat_notice.rs` retains the typed error and its run, account or settings-field owner.
Presence pushes that notice into the composer, which observes Settings and Input for
enabled-state changes. The recovery dispatcher rechecks the owner and declared action,
then uses the existing run, account or settings admission. An account Retry cannot resend
run text. Public usage links use the existing URL, and settings routes select their exact
tab without toggling a visible settings window closed.

Renewal warnings coalesce within the connection lifetime. A named run promotes a general
warning to run ownership; later duplicates cannot demote or resurrect a dismissed incident.
The Accounts diagnostic remains independent. Authoritative account recovery, an accepted
new turn or a connection reset starts the next incident. No timer correlates the frames.

Answering is ordered before a later Failed on the control pipe. It suppresses automatic
restoration even if text on the data pipe is delayed: authentication can fail on a later
model round after earlier output. Thinking alone does not prove no model or tool ran.

While `Target::New` has a submission pending at the current revision, another submit is
refused as `Busy` without sending a second `TextSubmit { thread: None }`. The typing draft
stays, so after the first `RunState` adopts the task, retrying continues that task. Pending
turns from older selections don't block New task: explicitly choosing **New task** again
advances the revision and permits a separate task.

A new `RunState` answers the oldest pending turn. Only a matching queued revision while the
target is still `New` can adopt that run's thread, without advancing the revision. An empty
FIFO never adopts, and a task holding a delete slot cannot become the target. A delayed state
after an explicit switch still answers its own turn but doesn't move input. Esc cancels work,
not the target or revision. Settings updates replace names and the main id without changing
the input draft, keyboard focus, viewed history, target or revision.

## Typed drawing admission

`drawing/` is a deterministic reducer beside the coordinator. Direct complete calls and
closed records share validation, repair and per-run step budgets, including calls opened
before another call exhausted the run. It owns one global scene
budget and serializes store lookups, step transactions and effective cuts. A step is admitted
without mutation, written, then committed to the canonical state only after its store reply;
only then do `SceneBegin` and `SceneStepCommit` leave on the scene-owned data FIFO. A failed
step write leaves no handles, revision or reserved scene name behind. Call outcomes contain
model-visible ids and fixed error codes, never diagnostic drawing content.

Provider completion drains the accepted drawing prefix before ending its run. Stop closes
queued records but still drains an accepted step transaction before acknowledging its cut.
An empty Stop received before the first scene id arrives cuts that run's unseen, unactivated
scene to zero after the step commits. Effective cuts are durable before acknowledgement;
failed cuts remain owed and block subsequent step writes. A no-op cut is acknowledged without
adding a log entry. A fifth scene clears the oldest before attempting the new admission, and
the new first step waits for that clear and any earlier typed clears to commit. A failed
clear holds the queued first step, not an extra uncleared scene in the store. Replacement
alone does not cut old scenes. Failed cut retries share the clear backoff, from 500 ms up to
30 seconds [guess], without resetting while a cut is in flight.

`StepShown` and `StepFailed` validate lifetime indices and the run that committed the step.
Bad observations or held-scene cuts break the connection; removed scenes are stale. Pending
thread deletion holds late drawing frames until the outcome: a refusal releases them in
order, while a hide or removal discards them and retires the canonical scenes. Reconnect
waits for pending writes and clear debt before freezing checked descriptors and whole-event
chunks. A scene whose producer-built descriptor or chunks fail validation or frame encoding
is omitted whole and cleared durably before the remaining scenes are described. Any transition
that drains or discards drawing work rechecks run completion, including clear and deletion.
Quit drains in-flight typed writes, supersedes failed cut debt with its clear and waits for
the clear acknowledgement within the existing deadline; an unconfirmed clear is incomplete.
During staging, a v1 recovery is sent only when typed scenes exist; the production
fixture and an empty placeholder-only recovery remain legacy until B3 switches both ends.

## Ids and staleness

The coordinator hands out thread and run ids, starting above the store's high-water marks, so an
id is never reused, even across restarts and deletions. One run is current at a time. An engine
event, a control input or a timer that names any other run is **stale**: it returns `Stale` and
changes nothing. A step an engine finishes after `Stop` never gets that far, since cancelling a
round closes its pump (see [engines and rounds](#engines-and-rounds)); the coordinator would
refuse it as stale, and the store again, since it only takes events for active runs. Store replies are the exception:
those for a run that is still ending are accepted, so its outcome is still written and a failure
still reported (see [the store contract](#the-store-contract)).

## One run, start to end

```mermaid
sequenceDiagram
  participant S as Shell
  participant C as Coordinator
  participant DB as Store
  participant E as Engine
  S->>C: TextSubmit
  C->>DB: BeginTurn (thread, run, user text)
  C->>S: RunState Thinking
  DB-->>C: TurnStored
  C->>E: StartEngine (RunRequest)
  E-->>C: TextDelta
  C->>S: RunState Answering, AssistantText
  E-->>C: ToolCallStarted (draw), ToolArgsDelta (one closed line)
  C->>DB: CommitStep (text so far and the step)
  DB-->>C: StepStored
  C->>S: StepCommit
  E-->>C: ToolCallReady, Finished (Completed)
  C->>DB: EndRun (remaining text, done)
  DB-->>C: RunStored
  C->>S: RunState Done
```

- **The turn is stored before the engine starts** (OSC-105 boundary 8). A blank turn (empty or
  whitespace only) starts nothing.
- **A thread's events stay in run order.** A new turn's write waits until every older run's
  outcome is queued, so a replaced run's last text lands before the new turn and the new engine
  starts only after its predecessor is stored. The shell still hears the old run's `Interrupted`
  and the new run's `Thinking` at once. A held turn that is stopped or replaced in turn is
  still stored, then its outcome, in order.
- **A step is sent only after it's committed.** When the engine's round completes while a step is
  still in the store, the run ends right after that step is stored and sent.
- **Data keeps the engine's order.** Text the engine produces after a pending step waits behind
  it and goes out right after its `StepCommit`. Control inputs such as `Stop` are handled at
  once while it waits.
- **Text goes out in pieces of at most 64 KiB**, split on character boundaries, so every frame
  fits the data pipe.
- **A voice run** will show `Listening` while the key is held and `Transcribing` while speech
  becomes text; both phases are reserved until hold admission lands. Its `Transcript`, `SegmentQueued` and `SpeechPcm` frames belong to the run, so
  the shell fences them like `AssistantText`.
- **Streamed text is stored with each step and with the outcome**, in the same transaction.
  shortcut: text after the last step is lost if the brain dies mid-run; store it on a timer once
  real engines stream long answers (slice 3).

## The store contract

The store runs operations in order, each in one transaction, and answers every one:
`TurnStored`, `StepStored` or `RunStored` on success, `StoreFailed` on failure. So a
`StoreFailed` always belongs to the run's oldest unanswered operation, and step
acknowledgements arrive by index in order. An acknowledgement for anything else (a repeat, an
index out of range, a step before its turn is stored) is `Stale` and changes nothing.

## Ending a run

Every ending runs in the same order: cancel the engine so it produces nothing more (unless it
finished, or never started because the turn wasn't stored yet), drop the data it still held for
the shell, tell the shell (every ending but `done`), then store the outcome
with the unstored text and, when the user stopped it, the `revealed` set. The outcome is
written once the run's earlier operations are answered, so a run stopped before its turn was
stored gets its outcome after the turn, and its engine never starts.

An ended run stays with the coordinator until the store answers for its outcome, so a failure is
never lost:

- if the turn failed, there's no run row, and nothing more is written;
- if a step or the outcome failed, the outcome is written as `failed(store)` without its text,
  but with the Stop's `revealed` set and its scene's clear, so the drawing keeps what the shell
  kept; if that fails too, the run stays `active` and the next start's recovery marks it
  `failed(crash)`. Its scene's clear, or its kept selection, is then retried on its own until
  it commits, and a Quit is incomplete until it has, unless the Quit's own clear durably
  removes that scene: a kept selection that keeps failing still ends in a complete Quit then.

| Cause | Outcome | Shell told |
|---|---|---|
| Engine finished | `done` | once the store has it |
| `Stop` (Esc) | `interrupted(stop)`, with `revealed` | at once |
| A new turn | `interrupted(replaced)` | at once |
| The shell went away | `interrupted(shell_lost)` | no |
| The shell took no data for 2 s | `interrupted(view_stalled)` | at once |
| Its thread is being deleted | `interrupted(deleted)` | at once |
| A budget ran out | `failed(budget)` | at once |
| The engine failed, its round ended truncated or interrupted, or it sent an oversized step or a malformed call | `failed(engine)` | at once |
| A store write failed | `failed(store)` | at once |

`revealed` keeps only indices of steps already sent to the shell, each once, in order.

### Draining a spoken run

A run with spoken replies (see [voice.md](voice.md#a-spoken-run)) ends `done` in one place,
`Coordinator::complete`, and only when four things hold: the engine finished, nothing it wrote is
pending, its drawing settled, and its speech is final. Until then the run is draining: it stays
current and stoppable, the 120 s deadline no longer applies, and the settle pass that follows each
input checks the run again.

- **The stall timer.** A draining run that makes no progress for 5 s (no audio made, no progress
  played, no segment started) ends `interrupted(stop)`.
- **Every other ending revokes the speech.** TTS stops, and the shell gets `SpeechRevoked`,
  unless the ending is its own Stop. Then the terminal `RunState` and the `RunEnd` wait until
  the outcome is stored *and* the shell's `SpeechStopped` says its output is quiet, in either
  order. That's the one exception to "told at once" above. If the shell went away, nobody waits
  for quiet.
- **A shell that never goes quiet.** If `SpeechStopped` hasn't come 2 s after a revoke, the
  connection ends as broken. A second `SpeechStopped`, from a Stop that crossed a revoke,
  changes nothing.
- **A report that can't be true**, such as a played total that isn't the samples sent, also ends
  the connection as broken.
- A store failure while waiting for quiet is told as `Failed` once it's settled. If the store
  gave up, it's told without waiting for quiet.

### What the shell can rely on

- **A run's data is complete at its `RunEnd`, on the data pipe.** Every run that reaches a
  terminal `RunState` also gets one `RunEnd`, after all of its data frames. The two pipes aren't
  ordered with each other, so a terminal `RunState` on control says nothing about what has
  arrived on data; the shell keeps reading the run's data until its `RunEnd`.
- `RunState Done` means the run's text, steps and outcome are committed.
- Every other terminal `RunState` comes at once, whatever the store is doing, so `Stop`'s
  answer never waits on an fsync or a long write. It's the shell's to show, not a promise that
  the outcome is stored.
- A terminal `RunState` can be followed by `RunState Failed` for the same run, when its
  writes later fail. The shell shows the failure; nothing else follows it.
- A `Stop` for a run that already ended doesn't change its outcome: it still ends `done`,
  `failed` or `interrupted(replaced)`, and its data still drains. But the shell could still
  stop it while it drew the run's last steps, so the scene keeps only what the shell kept. While
  the run's writes are pending, the selection goes with its outcome once they settle, or with
  the late first step that begins its scene; once they're done and the scene is visible, the
  brain writes the selection on its own, or the scene's clear when nothing stayed. A scene is
  trimmed once: a later `Stop` for its run is stale. So is one for a run an earlier connection
  started, ended or not, and after a restart, since a connection can stop only runs it started
  itself. One that arrives while the engine has finished but its last step is still being stored interrupts
  the run, since the shell never showed that step.

## Budgets

Budgets are absolute for the run and never reset (OSC-105 boundary 5): 120 s of wall clock, 1 MiB
of visible text and 64 steps. Exceeding one ends the run as `failed(budget)`, and the text or
step that went over is never sent. The wall clock starts when the turn is accepted, so time a held
turn spends waiting counts against it, and it stops applying once the engine says it's done, even
while the run's last writes are still pending. The deadline and the text counter belong to the run,
so they carry across rounds, and so do the tool loop's budgets: 8 model rounds and 16 tool calls
[guesses] ([The tool loop](#the-tool-loop)). Speech budgets arrive with the engines that use them.

## Availability

Whether a run can be answered comes from the account, as `Input::Engines(Availability)`. The
runtime sends it at start and after every account input. It's `Ready`, with the connection and
the catalog, or `Unavailable` with what the user can do (`coordinator/availability.rs`).

- **The connection** names the sign-in that issued the account's token set. It's saved with the
  set, kept through refreshes and restarts, and new on every sign-in (random per process, with
  the top bit set, so a set saved before connections existed reads as 0).
- **The account's reason is kept.** Signed out, signing in or with a sign-out accepted gives
  `SignInNeeded`; a declined plan `PlanNotEnabled`; an ended session `SessionEnded`; a plan
  the account can't use `PlanUnavailable`; a held credential lock `CredentialsUnavailable`.
- **A turn while unavailable is stored**, so the text is kept, and its run fails
  `failed(engine)` at once, telling the shell the reason. A sign-in before the turn is written
  doesn't revive it.
- **A run is bound to the connection** that was ready when its turn arrived, and its
  snapshot records it. The binding holds while the turn is still being written, so a sign-out
  then fails the run at once and a later account never answers it. A catalog-only change keeps the run. Any other change, a sign-out the
  account accepted included, cancels the engine and fails the run with the reason. A held ask
  for that run is answered `Rebuild`, and `send_ready` checks the binding again before every
  `Grant`.
- **S4: the model is checked once,** when the first build's result arrives: a model missing from
  the catalog, an empty catalog or no choice at all fails the run with `ModelUnavailable` and
  Open settings, and nothing goes to another model. A run in flight keeps its check when the
  catalog changes. shortcut: the catalog lists no efforts yet, so effort isn't checked.
- **Draws come only from an offered tool** (D13-H2). A `draw` call opens the draw buffer only
  when the admitted request offered `draw`, or when the dev fixture answers (`trusted_draw`,
  which exists only in debug builds). Any other draw is an unknown call and draws nothing.
- **The shell hears why** in `RunState.error`, sent only with `Failed`. shortcut: it isn't
  stored, so a reloaded page shows the generic failure.

## Engines and rounds

An engine (`crates/brain/src/engines/`) implements one trait: `Engine::start` takes a `RunRequest`
(the run, its thread and the turn) and plays **one model round**, sending `TurnEvent`s: `TextDelta`,
`ToolCallStarted { call, name }` (always before the call's first argument delta),
`ToolArgsDelta`, `ToolCallReady` with the whole arguments, `OutputItem` with its output index for
native items kept for replay, and `Finished` with `Completed`, `Truncated`, `Failed` or `Interrupted`. Call ids are the
provider's, ASCII and at most 64 bytes, refused rather than cut. `Debug` prints only the sizes of
text, arguments, turns and items.

One round per `start` is the direct-provider rule: an engine that calls a model API ends its
round when the model's response does, and the coordinator starts the next round itself. An engine
that runs its own tool loop (Claude Code, slice 8) may play several model rounds inside one start;
the coordinator never dispatches those calls again.

```mermaid
flowchart LR
  E[engine round] -->|TurnEvent| P[the round's pump]
  P -->|Input::Engine run| Q[engine queue, 16] --> C[coordinator]
  C -->|CancelEngine| X[drop Cancel] -.aborts.-> P
```

- **Each start gets its own pump and channel.** The pump forwards the round's events tagged with
  its run, and closes after the first `Finished`, so nothing of a finished round reaches the
  coordinator, even under a later round of the same run.
- **A round that ends without `Finished` is `Truncated`**, even after text: the pump reports it,
  in one place for every engine, and the run fails as `failed(engine)`, never `done`.
- **The brain owns cancellation.** Dropping the round's `Cancel` aborts the pump and the engine's
  future with it, even while the pump waits on a full engine queue, so `Stop` under backpressure
  ends the run at once, and a local `Stop` is never reported as truncation.
- **A round's end isn't the run's.** The coordinator decides, through the tool loop below.
  `Truncated`, `Failed` and an engine's `Interrupted` fail an active run, since only the
  coordinator interrupts runs. Only events for a run that already ended are stale.
- **The draw call's arguments are split into steps** in `coordinator/run.rs`, one per closed
  line. The open line is charged against the 256-byte label cap before it grows, so arguments with
  no newline fail the run at the cap. `ToolCallReady` must match what streamed; a last line
  with no newline is one more step.

## The tool loop

`coordinator/tools.rs` answers a direct-provider engine's calls, one model round at a time:

```mermaid
flowchart LR
  R[round completes] --> S{steps stored?}
  S -->|not yet| W[wait for the last step's ack]
  W --> S
  S -->|yes| C{calls?}
  C -->|yes, round 8| B[failed budget]
  C -->|yes| I[RoundItems: text, items, results] -->|RoundStored| N[build the next round]
  C -->|no, items| F[RoundItems] -->|RoundStored| D[done]
  C -->|no items| D
```

- **Every call gets one result.** A call is charged when it starts, before anything of it is kept
  (D13-M2), and the 17th in a run fails it `failed(budget)`. Only the offered `draw` call keeps
  state (its steps); every other call keeps only its id, drops its arguments, and gets
  `{"error":"unknown tool"}`, which names nothing the provider sent. A repeated call id, arguments
  for a ready or unknown call, or a round that completes with a call not ready fails the run
  `failed(engine)`. shortcut: the offered `draw` call gets no result and ends the loop, which only
  the fixture's draw-only rounds rely on until B10 offers `draw` to a model.
- **A round is stored before the next one is built.** `StoreOp::RoundItems` writes, in one
  transaction, the text streamed since the last write, the round's native items in output order,
  then each call's result in call order. `Input::RoundStored { run, round }` answers it, and only
  then does the next round's build go out through admission, so it reads what was just stored.
  The round counter is the build key's: a round that would be the 9th is refused before its build,
  so no ninth ask or send happens. A delete's rebuild keeps `(run, round)` and the counters.
- **A round with no calls** is stored too when it has native items, so the final message replays
  natively; `done` waits for its acknowledgement. A round with neither ends the run at once.
- **A late acknowledgement only confirms storage.** After a Stop, a sign-out, a replacement or the
  deadline, `RoundStored` lets the ended run's outcome be written and never starts a build. A
  failed round write ends the run `failed(store)` and its late acknowledgement is stale.
- **The deadline still counts** while a round with calls is being stored. It stops counting only
  for a round with no calls whose last writes are still pending.

The builder replays stored rounds per run (engines.md, "What a request holds").

**One run at a time.** The coordinator keeps one current run, and a new turn replaces it,
whatever its thread. That's a limit of slice 3, not of the seam: engines, and everything new that
works for a run, take the run id from the request and never ask for "the current run", so runs
keyed by id (OSC-56) need no rework.

## The fixture engine

The runtime picks the engine once at start (`runtime::Selected`). A dev build with
`KUMOCHI_FIXTURE` set plays that script for every run, ready with no account; a release build
can't pick it. Otherwise the Responses engine answers once the account is ready (engines.md, "The
Responses engine"). A script plays through `engines/fixture.rs`; `crates/brain/fixtures/default.txt`
is a sample. One line per event: `<ms> text <words>`, `<ms> fill <bytes>`,
`<ms> step <label>`, `<ms> fail`, and `late <label>`, a step the engine still sends after it's
cancelled. Its round's pump has closed by then, so the late step reaches nobody; the coordinator
and the store would refuse it too.

shortcut: steps stream as one `draw` call: the first step starts it, each step is one
newline-terminated argument delta, and the script's end sends `ToolCallReady` with the joined lines,
then `Finished(Completed)`. Slice 4's B3 replaces this adapter with recorded draw arguments.

## Around the coordinator

The store runs on its own thread, which owns the connection and keeps the store contract above:
operations in order, one transaction each, every one answered. The engine's pump, the store
thread and the deadline timer report back as inputs, and the pipes' readers and writers are
described in `ipc.md`. A shell that disconnects ends the current run as
`interrupted(shell_lost)`. Every Stop and Clear the control reader read before the pipe ended is
applied first, even one read in the same poll as the end. Store replies keep reaching the coordinator while no shell is
connected, and on exit the brain waits up to 2 s for ended runs to be stored, then up to 2 s for
the store to close. A deletion's purge (`store.md`) copies the store on a helper thread only
while no run is active. A write arriving, or closing, stops the copy and waits only until it has
let go of the live store, never for the rest of the copy: the worst of 10 samples at 984 MiB on
Rez was 29 ms, though none forced the cancel deep into `VACUUM INTO`.
Removing its files runs on a helper too, so turns and runs stop only for the swap, about
10 ms on Rez up to 984 MiB, and otherwise share the disk with the helper.

shortcut: a store that never answers leaves a finished run unconfirmed. At the next start, runs
still active in the store become `failed(crash)`; outcomes already committed stay, and work not
yet committed may be lost. Fine while the store is local SQLite.

On Quit the brain shuts down by one deadline instead, described in [processes](processes.md).

### Shell crash containment

The supervisor keeps the existing 200 ms, 1 s and 5 s relaunch delays, then gives up on
the fourth death within 60 s. Each connection starts with no recovery membership; only a
nonempty `ScenesV1` records native scene ids. A repeat death while recovery chunks are
still queued, or within 10 s of queueing the final chunk, clears that connection's described
drawings before relaunch. The 10 s value is a guess for preparation, not paint: B5/B6 must
re-anchor it to actual painting. Pre-connect deaths cannot reuse the preceding membership.

On give-up, the brain instead clears the union of every membership described in the rolling
60 s window, regardless of the 10 s condition. Expired memberships and confirmed cleared ids
are forgotten. Both paths use existing clear debt and asynchronously drain store replies under
one 2 s deadline [guess]. Only a successful acknowledgement for every requested id and settled
drawing work permits relaunch; failure, unavailable storage or timeout suppresses it. An
empty new clear plan does not bypass this barrier: unavailable drawing recovery or
unconfirmed earlier clear debt still suppresses automatic relaunch. An
in-process hand-started connection still waits behind settlement before describe. Failed debt
is memory-only and does not survive brain exit; typed drawing history is never deleted.

The step is a labelled placeholder until slice 4's scene format. A run's steps draw in one scene:
its first step's write begins the scene and returns its id, and the steps after it wait in the
run until that ack. A Stop, a replacement, a failed first write or a lost shell drops the waiting
steps, and the run still settles.
