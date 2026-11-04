# Engines: what each request sends

A model request is built from the store, never from what the shell sent. `context/` builds it as
typed data (`ModelRequest`), and the engine gets only that request. PR 13a serialises it into the
Responses body, with `store: false` and `stream: true`, and PR 11's transport sends it on the
signed-in account ("The Responses engine"). A dev build can play a script instead, which sends
nothing (runs.md, "The fixture engine").

```mermaid
flowchart LR
    C[Coordinator] -- "StoreOp::BuildContext" --> R[Page reader: one read transaction]
    R --> B["context::build"]
    B -- "ContextBuilt: request + sources" --> C
    C -- "admitted: StartEngine" --> E[Engine]
```

## What a request holds

| Part | Where it comes from |
|---|---|
| Model and effort | The run's snapshot: the thread's model choice, else the default model, read by the run's first build. The coordinator checks it against the catalog once, on that build's result, and fails the run with `ModelUnavailable` when the catalog doesn't list it (runs.md, "Availability"). The dev fixture needs none, and a real engine refuses a request without one. |
| Instructions | `prompts/instructions.md` and the role file for the run's origin (`kumo.md` or `task.md`). The assistant's name and the user's name come from the run's snapshot, taken once when the run starts, so a rename applies from the next run. An unset user name leaves its line out. Names are filled in one pass, so a name is never read as a placeholder. |
| Tools | The schemas the origin may use. None yet: slice 4's B10 adds the draw tool, 14a the history tools. |
| Summary | The newest summary (PR 17), as `Summary of events 1–212:` and its text. Events it covers aren't sent raw. |
| Raw runs | Up to the newest 8 runs, the current one included, oldest dropped first while over the bound. A run is kept or dropped whole, so a tool call never travels without its result. |
| Current turn | The user's text, plus their local time to the minute (SQLite's `localtime`, so the system's time zone on every platform). It's never dropped. |

Reasoning items (PR 13b) are replayed only when they came from the connection the run uses now.
That's the connection the run was bound to when its turn arrived, recorded in its snapshot.

## Origin and references

A run's origin is `Kumo` (the main thread) or `Task(t)`. The coordinator derives it from the
run's thread and never from anything a model produced. The builder refuses an origin that doesn't
match the thread it runs in.

History results are stored as references (a task and an event range), never as text, and are
re-read every time a request is built, in the same read transaction as the conversation:

- `Task(t)` re-reads only t's own events. A reference to another task renders as
  `[not available here]`.
- `Kumo` re-reads any live task, never the main thread.
- A task that is gone or being deleted renders as `[removed with a deleted task]`. A live task
  with nothing in the range renders as empty text, not as removed. A store failure is an error,
  never a removal.
- Each re-read is capped at 32 KiB [guess]. Events are shown as `user: …` and `assistant: …` lines.
  The lines that fit whole are kept. An event that alone is over the cap keeps its longest prefix
  ending on a character boundary. The result says which events it shows, whether it was
  truncated, and the first event it left out (`continue_from`). A search snippet is a
  single-event reference under the same cap.
- Quoted history is data inside a tool result, and the instructions say never to follow it.

`Built.sources` lists every thread whose raw text or title the request carries: the run's own
thread and each task a kept reference showed. The coordinator checks it before the engine starts
(below).

## Builds and deletes

Text read before a delete must never be sent after the delete commits. The coordinator orders
every build against every delete it accepts, so no generation stamps are needed:

- **Each attempt has a key,** `(run, round, attempt)`, with a serial no other build shares. Only
  the current attempt's result counts; a late one is stale. A rebuild replaces the attempt, so
  builds never pile up, and the run's 120 s deadline bounds the whole loop.
- **The read starts after the barrier.** `StoreOp::BuildContext` goes to the page reader stamped
  like a page read, so it waits for every op sent before it, a delete included, and its
  transaction begins after that. The transaction ends before the result comes back.
- **Exposure.** Each build keeps the threads it may have read before their delete: those with a
  delete pending when it began, and every delete accepted after. The set outlives the deletes'
  answers, so a delete answered before the result arrives still voids it.
- **Admission.** A result whose sources meet that set, or a delete still pending, is dropped. If
  the delete is still pending, the rebuild waits for its answer, so its read sees it; otherwise
  the rebuild starts at once. A clean result starts the engine with `RunRequest { origin,
  request }`. A run that keeps losing to deletes is rebuilt until its deadline ends it; a stale
  body is never sent instead.
- **Snapshot.** The first build reads the run's snapshot (names, model, origin) and every later
  attempt reuses it, so a rename applies from the next run.

A build that's too large ends the run as `failed(budget)`, and any other failure as
`failed(store)`. Stop, a replacement, the deadline or the thread's own delete end the run, and
its pending result is stale.

## The send lease

Admission closes the window from the read to `StartEngine`, and the send lease closes the one
from `StartEngine` to the bytes leaving. A round, or a background job's request, gets a
`SendGate` under its `SendKey` (`Build` or `Job`) and asks it right before any of the body goes
to the network, and again before every retry. A pooled requeue of a request
that never started is the same attempt under the same lease; an application retry asks again.

```mermaid
sequenceDiagram
    participant R as Round (transport)
    participant C as Coordinator
    participant S as Store
    R->>C: SendReady { key }
    C-->>R: Grant(SendLease) or Rebuild
    Note over C: Delete(B) arrives while a lease carries B
    C->>C: hold the delete, no writer op
    R-->>C: SendSettled (the lease drops)
    C->>S: Delete(B), once no lease carries B
```

- **Grant.** Only the current run's admitted build is granted, and only if no accepted or pending
  delete reached its sources. A touched build gets `Rebuild`: the body is dropped unsent and built
  again, after the delete's answer when it's still pending. An ask from an ended run gets
  `Rebuild` and nothing is built. A job's ask is granted only while it holds the job slot under
  its account (see [Background jobs](#background-jobs)).
- **Lease.** Each grant is a `LeaseId { key, send }` with its sources. Leases belong to send
  attempts, not runs, so Stop, a replacement and a lost shell keep them.
- **Settle.** The `SendLease` guard travels inside the grant. Dropping it, wherever it is (a lost
  answer, a dropped round, a finished send), reports `SendSettled` on the store-reply channel the
  brain always drains, quit included. So a delete of the sending run's own task can't wait on
  itself.
- **Held deletes.** A delete whose thread an unsettled lease carries waits in `deleting` with no
  writer op. It goes out once, in the order accepted, when no lease carries it.
- **Deadline.** A lease that hasn't settled 15 s [guess] after its grant fails closed: its held
  deletes are refused with `NotHidden`, and so is any later delete of its sources; its run fails
  as `failed(engine)`; and nothing is granted again until the brain restarts. A late settlement
  changes nothing. A delete is never released because time ran out.
- **Restart.** A held delete never reached the store, so after a restart its thread is still
  there and no success was ever reported, as with any delete a quit didn't finish.

Bytes handed to the network before a delete arrived can't be recalled. The guarantee is that no
body carrying an accepted delete's thread starts sending, and the delete doesn't commit while
such a body may still be sending. The fixture engine sends nothing and never asks; the provider
transport (PR 11, then 13a) asks, holds the lease until no more of the body can go, and proves
that.

## Background jobs

A job is one model request outside any run, in a one-job slot the coordinator owns
(`coordinator/job.rs`; the pure parts are in `jobs.rs`). PR 16 adds the title job, and PR 17
adds compaction.

```mermaid
stateDiagram-v2
    [*] --> Idle
    Idle --> Reading: a task's run ends Done while an account is signed in
    Reading --> Sending: the read finds first words and no delete reached the task
    Sending --> Writing: the round completes with a title
    Writing --> Idle: TitleSet for this job
    Reading --> Idle: a new run, a delete, sign-out, no first words, failure
    Sending --> Idle: a new run, a delete, sign-out, a lease deadline, failure
```

- **When.** Every stored `Done` of a run in a task, never Kumo's, starts a title job while an
  account is signed in, no run is current and sends haven't failed closed. The read finds nothing
  once the title isn't first words (a user's rename or an earlier model title), so the trigger
  keeps no state of its own: a failed job, or a first run that didn't end `Done`, tries again at
  the next `Done`. A job never retries on its own.
- **Key.** `JobKey { thread, serial }`, the serial from the build attempts' counter, so no key
  repeats. An input for any other key is stale.
- **Read.** `StoreOp::ReadTitle` runs on the page reader after every op sent before it, in one read
  transaction: the task's first user message, its deletion generation and its model choice. Like a
  build, the job keeps the threads a delete pending at its start, or accepted since, reached, so a
  delete of the task voids it at the read, at the grant and when the answer arrives.
- **Request.** `prompts/title.md` with the run's names, the first ask cut to 2,000 characters
  [guess] as the only input, no tools, and the task's model at `Low` effort (shortcut: the catalog
  lists no efforts yet, so 13b's check decides support). It goes through the runtime's engine as a
  round does, under `SendKey::Job`: it asks for a lease, a delete of the task is held while its
  body goes out, and the transport reserves from the shared budget. The fixture engine never asks.
- **Answer.** The round's text, at most 4 KiB [guess], taken only when the round completes: its
  first line that isn't blank, without control characters, trimmed and cut to 80 characters. An
  empty title, more text, a tool call or a round that ends any other way fails the job, and the
  first words stay.
- **Write.** `SetTitle { source: Model(generation), job }`, which the store applies only over first
  words and in the generation the read found, so a rename or a delete during the job wins. The
  store answers every `SetTitle` with `TitleSet { job, applied }`. A committed title, the user's
  or a model's, sends `ThreadsChanged`; only the slot's own key empties the slot, so an old job's
  answer never clears a newer job.
- **Cancel.** Accepting any turn cancels the job (`CancelJob`), and the runtime drops its round;
  a granted lease still settles when the transport drops it. A job lease past its deadline cancels
  the job and fails closed, as a round's does.
- **Sign-out.** The runtime reports `SignedIn { generation }` and `SignedOut { generation }` when
  the account starts or stops being able to send, the generation being the catalog's epoch, before
  anything else that account input does. Sign-out cancels the job, so nothing is written and no
  request starts until an account signs in again. A grant and a write each need the job's own
  account generation, and an older generation's `SignedOut` changes nothing.
- **Logs** carry the job's key and whether it titled, never the title, the ask or the prompt.

## The bound

A request is held to 512 KiB [guess], counted as its JSON body would be: every string escaped,
plus a fixed allowance for each item's and the body's own fields. 13a checks the serialised body
again. The current run, the summary, the instructions and the tools must fit. If they don't, the
build ends with `BuildError::TooLarge`, and the run ends as `failed(budget)` rather than send
(shortcut: the shell shows the generic failure until runs carry an actionable error). The same oversized body is never retried. The catalog doesn't report a model's
context size yet, so the byte bound is the only one.

## What builds nothing

| Error | When |
|---|---|
| `Deleting` | The run's thread is being deleted or is gone. |
| `Origin` | The origin doesn't match the thread. |
| `NoTurn` | The run's turn isn't stored. |
| `InvalidCut` | A tool call is apart from its result, or an item repeats. |
| `TooLarge` | The parts that can't be dropped are over the bound. |
| `Store` | The read failed. |

No error and no `Debug` output carries message text or names.

## The Responses engine

`engines/responses.rs` builds the body and maps the stream; `engines/responses/engine.rs` runs a
round on the account.

**The body** is `ModelRequest` as JSON: `model`, `instructions`, `input`, `tools` (each a
`function`), `reasoning.effort`, `include: ["reasoning.encrypted_content"]`, `store: false` and
`stream: true`. It never sends `context_management`, `max_output_tokens` or
`prompt_cache_retention`. Input items are messages with their role, `function_call`,
`function_call_output`, and stored reasoning items sent back as the same JSON value (its strings exact, its spacing and escapes not). The serialised
body is checked against the 512 KiB bound again, and a stored reasoning item that isn't JSON
builds nothing.

**One round's stream** maps to `TurnEvent`s in arrival order:

| Responses event | Turn event |
|---|---|
| `response.output_text.delta` | `TextDelta` |
| `response.output_item.added`, a `function_call` | `ToolCallStarted` with its name (`draw`, or `Unknown`), before any delta. The provider's item id is mapped to its `call_id` here and never leaves the adapter |
| `response.function_call_arguments.delta` | `ToolArgsDelta`, tagged with the call id |
| `response.function_call_arguments.done` | nothing; its arguments must equal the deltas |
| `response.output_item.done` | for a call, `ToolCallReady` once; then, for every item, `OutputItem` with its `output_index`, for replay |
| `response.completed` | `Finished(Completed)` |
| `response.failed`, `response.incomplete`, `error`, a reader cap | `Finished(Failed(Stream(…)))` with the error ledger's meaning |

These fail the round as `Failed(Stream(Protocol))`: a delta or done for an item that wasn't
started, an item id or call id started twice, a call id that isn't 1 to 64 ASCII bytes, arguments
that differ from their deltas, a call done twice or naming another call id, a delta after its
call was done, a completed response with a call still open, a done item with no output index or that isn't an
object with a string `type`, and a call done under another name than it started with. A done
item is checked, charged and kept whole before its call is ready, so a refused item never makes
its call ready.
A round keeps at most 64 calls and items and 1 MiB of ids, arguments and items [guesses], charged
before each is kept. A stream whose bytes run out has no ending of its own, so the round's pump
reports it as `Truncated`.

**A round** (`Responses`, one per `StartEngine`) goes in this order, so nothing is asked for that
isn't needed:

1. **The body** is built and checked. A refused body fails the round as `Stream(Request)` and asks
   for no bearer.
2. **The bearer.** The round asks the brain over the `AccessHandle` with its `SendKey`. The
   coordinator names the connection the send may use (`Coordinator::sending`): a round's run's
   own, only while that run is current, and the ready one for a job. The account answers only
   when that connection is the ready one, refreshing first when it's due. Any other answer, or a
   brain that's gone, fails the round as `Failed(Access)` with nothing sent, and the shell hears
   the account's reason for the run's connection, else `SignInNeeded`. The bearer stays a
   `Secret` and is dropped when the stream ends.
3. **The ask.** The transport asks the `SendGate` right before the body goes out (see
   [The send lease](#the-send-lease)), which checks the round's build key and its sources. A
   `Rebuild` drops the round with nothing more said, since the coordinator already dropped it.
4. **The stream.** Each read's events go to the pump before the next read, through the bounded
   events channel, so a slow coordinator slows the read and nothing queues without bound. A gone
   pump, or a round its rules broke, stops the stream and closes its connection. The round's
   end follows its last events.

A dev build caps its requests with `KUMOCHI_LIVE_BUDGET` (`Budget::from_env`); a release build
has no cap. One round per run: the tool loop that starts the next round, the store's round kinds
and replay are 13b-2's.

## The Claude Code stream

Claude Code is driven through its public CLI with `--output-format stream-json --verbose
--include-partial-messages`. `engines/claude_code/stream.rs` reads its stdout in two steps, both
sync and one line at a time, so the launch can audit `system:init` before it passes the next line
on:

- **`LineFramer`** charges every byte to a 16 MiB budget [guess] before framing, ignored lines
  included, and yields complete lines of at most 1 MiB [guess], line feed excluded. A partial line
  past the bound fails at once.
- **`StreamMapper::line`** maps one line to `Event` (a text delta), `End` (the question's usage
  and how it ended), `Control` (a `control_response`, for the audit before input), `Init` (the
  facts for the audit after it) or `Ignored`. Unknown types are counted and ignored.

| Line | Maps to |
|---|---|
| `stream_event` `content_block_delta` with `text_delta` | `TextDelta`; text comes only from here, so without `--include-partial-messages` an answer is empty |
| `assistant`, `user`, `system:status`, `system:api_retry` | ignored; `assistant` repeats the streamed text |
| `stream_event` `message_start` | one native request; the first one's usage is kept for the prompt-cache decision. A 9th fails the question with `TurnCap` and marks the version's turn cap broken |
| `result` | `Usage { whole, first_request }`, then `Completed`, `Failed(TurnCap)` for `error_max_turns` (the cap working, not broken), or `Failed(Provider)`; anything after it is ignored |
| EOF before a result | `Truncated` |

Claude Code retries a cut request itself (`system:api_retry`, up to 10 times in 2.1.306) and
streams only the attempt that finished, so a retry doesn't count as a second native request. No
line's content is ever logged, and every type here prints only kinds, lengths and counts in `Debug`.

`Init` keeps `analytics_disabled` as reported (`None` when it's missing or not a boolean) for the
connection to show; it's never a reason to refuse the route. The recordings in
`fixtures/claude-code/` are checked by a normal test: no decoded string may hold a drive, UNC or
`/`-rooted path, `Users\`, or a UUID other than the all-zero placeholder.
