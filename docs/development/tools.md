# Tools

A run's model, and later a Claude Code launch over MCP, can call Kumochi's tools. Every call goes
through the dispatcher (`crates/brain/src/dispatch/`), which runs it at most once, from what its
run may reach, and records it in the run's ledger. The coordinator owns the ledger and answers each
call only after the store has acknowledged the call's write.

Slice 3's 13b-2 tool loop and slice 8's C4b-2 MCP listener hand calls to
`Coordinator::tool_call`. The production tools are `create_task` and `focus`. No tool is offered
to a model yet, because `context::tools` is empty until the staged producers are enabled
(OSC-116); slice 4's B10 adds `draw`'s ledger entry. The test tools in `dispatch/tests.rs`
exercise the rest of the path.

## Who made a call

| Type | What it holds |
|---|---|
| `Caller::Model { run, call }` | the model's function call in a run's round, with the provider's call id |
| `Caller::Mcp { run, launch, key }` | an MCP `tools/call`, frozen on arrival with its launch and `CallKey` |

A call's *reach* is not the caller. It comes from the run's thread: Kumo's runs reach every live
task, and a task's run reaches only itself. It never comes from the arguments.

## One call, in order

```mermaid
flowchart TD
    A[call arrives] --> B{its run is the current, admitted run,<br/>and its thread isn't being deleted?}
    B -- no --> E1[Ended]
    B -- yes --> V{a delete reached a thread<br/>its build rendered?}
    V -- yes --> E7[Ended; the engine is cancelled<br/>and the round built again]
    V -- no --> C{MCP: its launch is the one<br/>authorized for the run now?}
    C -- no --> E2[NotHere, key not used]
    C -- yes --> D{the key has an entry?}
    D -- refused --> E3[the same refusal]
    D -- running/done, same hash --> J[joins / stored output]
    D -- running/done, other or bad args --> E4[Reused]
    D -- none --> F{known, offered on this route,<br/>allowed for this reach?}
    F -- no --> E5[Unknown / NotHere, recorded]
    F -- yes --> G{arguments within 16 KiB,<br/>an object that parses with no unknown fields,<br/>output within 64 KiB?}
    G -- no --> E6[TooLarge / Invalid, recorded]
    G -- yes --> H[CommitCall: the write and the ledger row,<br/>one transaction]
    H --> I[CallCommitted: answer every waiting caller]
    H --> K[Refused by the write's own check: answer every<br/>waiting caller with the error, run goes on]
```

- **The run.** The coordinator resolves the caller's run first. A call queued by a run that was
  stopped or replaced, even in the same thread, gets `Ended` and touches nothing.
- **Its sources.** A call comes from a response to a build, and that build rendered threads. If a
  delete has reached any of them since, pending or answered, the call gets `Ended`, the engine is
  cancelled and the round is built again once the delete is answered, as for a send the lease
  voids. So a deleted task's text can't reach a write through a call's arguments, even when they
  name no id of it.
- **MCP authority.** `Run::mcp` is the launch the coordinator authorizes for the run right now,
  set at bind and cleared at revoke (C4b-2). The check and the `CommitCall` it allows happen in one
  core-loop step, so a revocation either comes first, and the call is refused, or after the write
  was accepted.
- **The key** is the run, the route (`Model`, or `Mcp` with its launch) and the caller's id. The
  same id on two routes, or under two launches, is two calls.
- **Final per key.** Once the run and the launch check out, everything is recorded against the key,
  refusals included. A retry gets the same answer, a corrected call needs a new id, and a malformed
  retry never changes a running or finished call.
- **The hash** is SHA-256 over the tool's name and the serialized *parsed* struct, so whitespace,
  key order and an omitted default don't change it.
- **One completion rule.** Every first successful call, read-only included, sends one
  `StoreOp::CommitCall` and is answered only on `Input::CallCommitted`. A duplicate while it runs
  joins it. A failed write fails the run as `failed(store)`. An acknowledgement for a run that ended
  confirms storage only. One for a launch no longer authorized answers nobody.
- **Outcomes.** The store settles a write as `Committed::Done`, `Refused(error)` or `Failed`.
  `Refused` means the write's own check failed and nothing was written: the key turns refused,
  every caller waiting on it gets the error, and the run goes on. A later acknowledgement for that
  key changes nothing.
- **Follow-ups.** A write can carry a `Then`, which comes back with its acknowledgement. The
  coordinator runs it once, only on `Done` for a key that was still running, and only while the run
  is current, so a stopped run's or a repeated acknowledgement's follow-up never runs.

## `create_task`

`create_task { title, focus }` is Kumo-only and model-only. It creates one empty live task with
title source `model`, and starts no run. Its description and Kumo's instructions say Kumo creates a
task only when the user explicitly asks for a separate task or conversation (OSC-117). The title
follows the rename rules, and anything else is `Invalid`.

- **The id.** The coordinator hands the call the next id, one above every id it has issued, and
  advances its counter at dispatch, so the next ordinary new task takes the one after it.
- **Reserve, then create.** The store first raises the threads high-water in
  `sqlite_sequence` to that id, in its own commit. Only then is the id a reservation: issued and
  never reused, even if the task never lands. Then the task row and the ledger row commit in one
  transaction. If the run's thread is gone (`Failed`) or the store fails, nothing of the task is
  written and the run fails, but the reservation stays burned. A candidate that never reached
  the reserve commit was never named to anyone, so reusing it after a restart is fine.
- **After the commit.** `ThreadsChanged` goes to the shell even when the run has ended, because
  the task exists. While the run is current, the model reads `Created task <id>.`, and with
  `focus` the shell gets `Focus` for the new task under the same fences as `focus`.
- **Content.** `LocalWrite::CreateTask` and everything that carries it print the title as its
  length only.

## `focus`

`focus { task }` moves the user's typed input to a task, or back to Kumo. Kumo's runs may name any
task; a task's run may name only Kumo, and anything else is `NotHere` (a named task's own run
waits for OSC-116). Its write checks, in the call's transaction, that the target is a live thread,
and is refused with `NoTask` otherwise. Once it commits, the coordinator sends
`ControlToShell::Focus { run, thread }`.

The shell applies it, as an explicit switch, only when all of these hold:

- it came over the current connection;
- `run` is the newest run the shell named, past the Esc and delete fence;
- `run` answered a turn the user typed, under the turn number that is still the last one this
  connection sent, at the selection that is still current.

So a manual switch, a later turn (even one the brain refused), Esc, a reconnect or a new brain
each make a pending `Focus` stale. The turn number is taken at send and kept with the submission,
so with several turns pending only the run answering the last can move the input. The numbering
restarts with each connection, together with the fence.

## A round's calls

The model's calls stream in with the round. When the engine finishes the round, the loop hands
each call to the dispatcher in call order, and the round is stored once every call has its
answer: at once for a refusal or a finished call, on `CallCommitted` for a new one. The round's
write holds one result per call, tagged as in [Results](#results), and the next round is built
from the store.

- **Ids.** A round refuses a second call with an id it already has, which fails the run. The same
  id in a later round is the same ledger key: identical arguments get the stored answer with no
  second write, and others get `Reused`.
- **A voided build.** When the first call finds that a delete reached a thread the build rendered,
  the round's calls, items and unstored text are dropped, and the round is built again. Steps the
  response drew and text already on screen stay (a `shortcut:` until B10).
- **A delete during a call's write.** The round is checked again when its last call is answered.
  If a delete reached a rendered thread meanwhile, the calls stay settled in the ledger but the
  round is dropped and built again as above; a call the new round repeats gets the ledger's
  answer, so it never runs or writes twice.
- **Stop.** A stop while a call's write is pending ends the run; the write's late acknowledgement
  is stale, and no round is stored.
- **Failure.** A failed call write fails the run as `failed(store)`. The store reports it as
  `StoreError::Call`, which carries no SQLite text.

## The ledger

The run holds its ledger in memory, and it's dropped with the run. The run's 16-call budget bounds
it, because the loop and the listener charge the budget before dispatch. The store keeps a
`tool_calls` row for each committed call, written in the same transaction as the call's own write
(see [the store](store.md)). Recovery fails a run that was active, so nothing resumes a call from
its row.

## Results

`ToolOutput` is `Text`, `History` or `Error`, stored as tagged JSON: `{"text": …}`,
`{"history": [{"thread", "first", "last"}]}` or `{"error": "unknown tool"}`. A history result is
references only. Today a stored `History` result replays as its reference JSON, not as the re-read
content, and no production tool returns one. Before the first history tool is enabled, its results
must go through the context builder's live re-read of each range at every build, fenced against
deletes by 12b's admission and send lease, so a deleted task's text is never copied into the
ledger or another task.

History tools are **model-only**. MCP gets no schema for them, and the dispatcher refuses them from
`Mcp`, because an MCP reply doesn't pass through a model build's delete fence. Before history is
offered over MCP, C4b-2 must build a bounded, live, reach-filtered read whose source set includes
titles. That read must be materialized fresh on every reply, ledger retries included, and fenced
against deletion and revocation at the reply-write boundary.

## Schemas

Each tool's JSON Schema comes from `schemars` on the same `deny_unknown_fields` struct that parses
its arguments, so the two can't drift, and the schema is closed with `additionalProperties: false`.
`dispatch::schemas(reach, route)` lists what a run is offered on a route.

## Diagnostics

`Caller`, `LedgerKey`, `CommitCall`, `ToolOutput`, `LocalWrite` and `Ledger` print kinds, runs,
lengths and counts only: never arguments, outputs, call ids, keys or launches. A failed write logs a
fixed line.
