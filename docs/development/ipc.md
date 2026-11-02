# The pipes and the handshake

The shell and the brain talk over two named pipes per Windows session: **control** for small,
urgent messages and **data** for everything that can grow. Frames are a `u32` little-endian
length followed by a `postcard` payload from the closed enums in `crates/protocol`. The length
is checked against the pipe's cap (64 KiB control, 256 KiB data) before anything is allocated.

## Who may connect

- The brain creates both pipes with a protected DACL that grants only the user who made them,
  reads the DACL back before listening, and refuses remote clients.
- Each pipe allows two instances: the one in use, and its replacement, created before the used
  one closes so another process can never take the name in between.
- The brain only talks to the `capsula.exe` beside its own executable, and the shell only to
  the `capsulad.exe` beside its own. Each side checks the other's image path through the pipe.
- On every start the brain writes a fresh random 32-byte **instance capability** to Windows
  Credential Manager. A shell proves it runs as the same user by reading it back. It's never on
  a command line, in a file, or in the environment.

## The handshake

```mermaid
sequenceDiagram
  participant S as Shell
  participant CM as Credential Manager
  participant B as Brain
  S->>B: open control pipe (image path checked both ways)
  S->>CM: read the instance capability
  S->>B: Hello(build id, capability) on control
  B->>B: capability compared in constant time, then build id
  alt capability wrong
    B-->>S: close
  else build id differs
    B-->>S: BuildMismatch, then the brain exits
  else match
    B-->>S: Welcome(incarnation)
    S->>B: open data pipe, Hello again
  end
```

The capability is checked before anything else, so a client without it learns nothing, not even
whether its build matches. A shell that meets a brain from another build waits for that brain to
exit and starts the one beside itself.

## After the handshake

Each pipe has its own reader and writer, all polled together in one task with the core loop
(`crates/brain/src/runtime/`). A reader never waits behind a write, so `Stop` reaches the
coordinator even while the data writer is blocked on a shell that stopped reading.

```mermaid
flowchart LR
  CR[control reader] --> L[core loop]
  DR[data reader] --> L
  SYS[store thread, engine, timers] --> L
  L --> CQ[control queue, 256 frames] --> CW[control writer]
  L --> DQ[data queue] --> DW[data writer]
  DR -. Credit .-> DQ
```

| Pipe | Carries | Bound | When it's full |
|---|---|---|---|
| Control | `RunState`, `Ping`, `Scenes` | 256 queued frames [guess] | The connection ends, a deliberate overload policy; store, engine and timer effects still apply |
| Data | `AssistantText`, `StepCommit`, `RunEnd`, `ThreadList`, `Page`, `PageFailed`, `SceneSnapshot` | 512 KiB queued pauses the engine and new turns [guess]; credit limits bytes in flight to 512 KiB [guess] | Frames wait; 2 s with bytes waiting and no write finishing, on credit or on a shell that stopped reading, ends the run as `view_stalled` |

| Page message | Direction | Fields |
|---|---|---|
| `PageRequest` | Shell to brain, data | `request`, `PageOf::Threads { search, after }` or `PageOf::Events { thread, before }` |
| `ThreadList` | Brain to shell, data | `request`, thread rows (`id`, `title`, `updated_ms`), `next` |
| `Page` | Brain to shell, data | `request`, `thread`, events (`id`, optional `run`, `kind`, `text`, `cut`), `before` |
| `PageFailed` | Brain to shell, data | `request`; the read failed and the brain logged its kind |

- **Credit.** The brain may send 512 KiB of data frames, headers included, before the shell
  grants more with `Credit`. That holds the largest frame the pipe allows, so no frame is too
  big to send, and several 64 KiB text frames. Grants never raise the credit past the window.
- **Stop drops the stopped run's unsent data**, and so does a new turn that replaces a run, so
  the replacement's first frame isn't stuck behind text nobody will show. Only a `Stop` or turn
  the coordinator accepts drops anything, and the run's `RunEnd` is queued after the drop, so it
  still arrives, as long as the shell keeps reading data.
- **Watchdog.** Only while a run is active, the brain sends `Ping` every 500 ms and ends the
  connection if one goes unanswered for 2 s (checked on the next tick, so 2 to 2.5 s). Outside
  runs there are no timers, and a run's deadline goes when the run does.
- **The tap** records control frames only; data frames carry what the user typed and the
  answer, so they're never written to it.
- **Admission.** The core loop never awaits a pipe, the store or an engine, and it always takes
  control frames and store replies, so `Stop` is never queued behind data. The data reader never
  waits on the core either, so credit is always read. While the data queue holds 512 KiB,
  frames and their bookkeeping counted, engine events wait in a 16-event channel and the engine
  waits on it, and new turns wait too. While 16 ended runs wait on the store, new turns wait
  until it catches up [guess]. Memory stays bounded however slow the store or the shell is: the
  cap is checked before each event and each turn, and only a turn adds a run and so a `RunEnd`,
  so the queue can reach the cap plus one event, the text held behind a step, and one small
  `RunEnd` per run still ending. That bound excludes typed turns not yet taken: up to 64
  `TextSubmit`s of up to 256 KiB each wait ahead of the core, and one more ends the connection
  as broken. Separately, at most four page requests may remain unanswered [guess], counting
  replies waiting on credit or a blocked write; a fifth breaks the connection. Pages are at most
  50 rows and 32 KiB per encoded frame [guess]. The shell keeps only one request per list, two
  total. Their frames use at most 64 KiB plus bounded queue metadata, so pages alone cannot
  stop turn admission. See [panels](panels.md) for continuations, repair, and stale replies.
- shortcut: the control queue reserves no slots for urgent frames and doesn't coalesce
  `RunState`, so a burst can fill it and end the connection even with a slow but healthy shell;
  both come with the first urgent brain-to-shell message.

## Scenes and recovery

A drawing is a **scene**, named by a `SceneId`: the id of the store event that began it. At most
four scenes are visible; [processes](processes.md) has the rule that keeps that set durable. Each
`StepCommit` names its scene and its index in it.

| Message | Pipe | Fields |
|---|---|---|
| `Scenes` | Control, brain to shell, once per connection | up to four `SceneDescriptor`s: `scene`, `revision` (every step committed, never goes back), `committed` (the steps kept after any Stop) |
| `SceneSnapshot` | Data, brain to shell | `scene`, `revision`, `chunk` of whole steps, `last` |
| `ClearDrawing` | Control, shell to brain | `scene_ids`, at most four; five or more end the connection as broken |

- **The connect barrier.** Each connection starts awaiting `Scenes`. The brain serves `Stop`,
  `ClearDrawing` and `Pong` but takes no turn, and sends `Scenes` only once the visible set has
  settled: no earlier connection's run still ending and no clear still being written. If it
  hasn't settled within 2 s [guess], the brain ends the connection as unavailable and the shell
  reconnects; it never sends a partial list.
- **Snapshots.** After `Scenes`, each scene's kept steps go out in chunks of whole steps packed up
  to 32 KiB [guess], one at a time whenever the data queue has room, ahead of engine events. A
  step bigger than the target goes alone. Turns are taken only after `Scenes`, so no live step
  belongs to a scene in recovery. A `ClearDrawing` stops the cleared scenes' chunks that haven't
  started writing, queued ones included. Snapshot frames are data, so the tap never records them.
- **Clears.** A cleared scene leaves the visible set at once, so no later `Scenes` lists it, and
  counts as cleared once its write commits. A failed write, a clear's or a late Stop's kept
  selection, is retried with the next store operation, at the next connect barrier, and on a
  timer that waits 0.5 s and doubles up to 30 s while the writes keep failing [guess]. So a
  store that recovers lets typing go on without the user doing anything, and one that keeps
  failing is tried rarely, not constantly. An id that isn't visible changes nothing.
- **Owed writes hold back turns.** While eight scenes [guess] have a clear or a kept selection
  owed, new turns wait; Stop, `ClearDrawing` and `Pong` are still served, and nothing owed is
  ever dropped. A scene counts once even when both are owed, as when it's cleared while its
  selection is being written. Eight is a watermark, not a cap: after turns stop, debt can come
  only from scenes that already exist, the four visible ones and one for each admitted run, the
  current one and up to 16 ending. So at most 7 + 4 + 17 scenes are owed, each at most twice.

The shell's [`Drawing`](presence.md#the-drawing) follows each connection's recovery:

```mermaid
stateDiagram-v2
  [*] --> Awaiting: Connected
  Awaiting --> Awaiting: SceneSnapshot held, credit withheld
  Awaiting --> Receiving: Scenes (held chunks applied in order)
  Receiving --> Receiving: SceneSnapshot, credited as it's classified
  Awaiting --> Offline: connection lost
  Receiving --> Offline: connection lost
  Offline --> Awaiting: Connected
```

- **`Scenes` is authoritative.** A shown scene it doesn't name goes at once. A named one stays
  until its snapshot installs and then is replaced, even at the same revision.
- **Install.** A scene's `last` chunk installs it only on the connection that described it, with
  no clear since, the descriptor's revision, and exactly `committed` steps. Installed steps show
  as already activated, and installing sends nothing.
- **Esc during recovery.** While awaiting, Esc drops the held chunks, granting their credit, and
  keeps a clear intent through disconnects and Retry: the next complete `Scenes` clears every id
  it names in one `ClearDrawing`. Once `Scenes` has arrived, Esc clears the shown and recovering
  scenes at once and leaves no intent, so a later scene is never cleared by it.
- **Cleared ids.** The shell remembers the newest 64 cleared ids [guess]. One that `Scenes` lists
  again, after a clear the brain never got, is sent again and never shown.
- **Validation.** More than four descriptors, a repeated id, counts outside `committed ≤
  revision ≤ 64`, a label over 256 bytes, or more steps than `committed` end the connection as
  broken input. Counts are checked before a step is kept, and errors name sizes, never labels.

## The shell's side

`crates/shell/src/link.rs` connects and reads control on the link thread, because pipe reads
block. Once connected, each pipe gets a writer thread and data gets a reader thread. The UI
thread sends on bounded channels and never waits: with no connection or a full channel the send
fails at once, so a typed turn shows it wasn't sent, and `Stop` or `ClearDrawing` is logged as
unsent after its local effect already happened.

Each connection reaches the UI as `Connected` with a session that carries the shell's own
connection generation, not the brain's incarnation, since the same brain may be reconnected to.
Sends and grants go through that session, and data events carry its generation, so a turn,
page request, `Stop` or grant meant for a connection that has ended does nothing on the next one.

| Channel | Bound | When it's full |
|---|---|---|
| Control out | 64 messages [guess] | The send fails; the shell sends only `Stop`, `ClearDrawing`, `Pong` and `Quitting` |
| Data out | 64 turns plus 8 for pages and credit | Not reached: the shell refuses a turn first and keeps at most two page requests unanswered |
| Brain to UI | unbounded, one channel in arrival order | The link thread never waits on the UI |

- **Credit** for a data frame is granted once the UI thread has taken it, applied or dropped by
  the fence, so a UI that stops taking frames stops the brain within its window. Grants add up
  in the connection's own counter, without a lock, and the data writer sends them after each
  wake-up, so a grant waits at most for one data write.
- **Data before its run is named.** A frame for a run no `RunState` has named yet is held, and its
  credit withheld, until the state arrives: dropped if Esc cancelled that run's turn, applied
  otherwise. Since withheld credit stops the brain, its credit window bounds what's held.
- **Pages.** Each list has one request in flight. A superseded query or closed thread drops its
  reply when applied, but still earns credit. Page frames are never held for a RunState.
- **Turns.** The shell keeps at most 63 turns unanswered; a 64th is refused until a new run's
  first `RunState` answers one. The brain's queue holds 64, so the shell never sends the turn
  that would end the connection. It refuses blank turns, which the brain ignores, and turns too
  long for a data frame.
- **Broken input.** A malformed frame on either pipe, a repeated `Welcome`, or scenes or
  chunks that break the rules above end the connection, and the link reconnects as after any loss. A lost brain fences every run it knew
  and forgets the unanswered turns.

## Starting and reconnecting

A per-session mutex keeps one shell. If no brain pipe answers, the shell starts `capsulad.exe`
detached, outside its own job where the job allows it, and retries for up to ten seconds. A shell
started by hand finds a running brain the same way, so after a shell crash nothing needs
restarting.

## Debugging

A debug brain records the control frames it sends and receives, and the handshake with
capabilities zeroed, when `CAPSULA_PIPE_TAP` names a file. `cargo xtask pipe-dump <file>`
decodes the recording with the same message types.
