# The slice 2 gate

`cargo xtask slice2-gate` checks slice 2's promises against the real dev brain, with no shell
window and no mouse or keyboard input. It runs on Rez, at normal priority, on a quiet machine.

## How it runs

```mermaid
flowchart LR
  X[cargo xtask slice2-gate] -->|cargo build| B[kumochid.exe, dev build]
  X -->|copies| F["target/slice2-gate/&lt;time&gt;/bin"]
  F --> C["kumochi.exe (a copy of xtask)"]
  F --> D[kumochid.exe]
  C -->|starts, kills, restarts| D
  C <-->|control and data pipes| D
```

- The brain serves only the `kumochi.exe` beside its own executable (see [ipc](ipc.md)), so the gate
  copies the brain and itself, renamed `kumochi.exe`, into a fresh folder and runs the scripted
  client from that copy. The client reads the instance capability from Credential Manager and
  sends `Hello` on both pipes, as the shell does.
- Each item gets its own `%LOCALAPPDATA%` folder and fixture script (`KUMOCHI_FIXTURE`, dev builds
  only), so items never share a store and the developer's dev store is never touched. The gate
  refuses to start while any `kumochi.exe` or `kumochid.exe` runs, because a dev brain would hold
  the session's pipes.
- The client answers `Ping`, grants credit for each data frame it takes, and records every frame
  with the instant its reader thread took it. Its data reader can pause, take frames one at a
  time, or keep reading while it withholds credit.
- It prints a table of verdicts and numbers and writes it to `report.md` in the gate's folder.
  `cargo xtask slice2-gate <words>` runs only the items whose names contain them.

## The items

| Item | What the client does | Pass |
|---|---|---|
| Esc against a non-reading shell | Stops reading data during a stream, sends `Stop`; 20 times | `Stop` written to `RunState Interrupted` read at p95 under 50 ms [guess]; every run stored `interrupted(stop)`, never `view_stalled` |
| Esc against a saturated data writer | The same, reading but granting no credit while the fixture streams 17 deltas of 61,000 B inside the 1 MiB text budget | As above. Before each `Stop`, the frames taken leave no room in the credit window, and the fill frames still unsent (fixture count minus taken, times the observed frame size) are over the brain's 512 KiB data-queue cap |
| Replacement run after Esc | `Stop` then `TextSubmit` within 1 ms, 10 times in a row | The new run's id is higher; the old run's last state is `Interrupted`; nothing of the old run arrives after its `RunEnd`, which precedes the new run's data. Old-run frames read after `Interrupted` are counted, not failed |
| Late closed step after Stop refused | Stops a run whose fixture finishes a `late` step on cancel | One `StepCommit` (the one before `Stop`); the late label is in no table, FTS or file |
| Kill mid-run, then recover | `TerminateProcess` after two committed steps and later text, then restarts | The run is `failed(crash)`; both steps and the text stored with them are kept, the text after them isn't |
| Reconnect with a multi-chunk drawing while Stop arrives | Draws 4 full scenes, reconnects, takes one chunk, sends `Stop` and `ClearDrawing`, then reconnects again | `Scenes` names 4 distinct whole drawings (committed = revision = 64). Applying chunks by the shell's rules (a scene installs only at its last chunk, with exactly its committed steps; a chunk at another revision spoils it for the connection; more steps than committed is an overrun; a scene cleared locally or not named never installs), exactly the 3 uncleared scenes install, each with the fixture's 64 steps; the cleared one is absent after the clear and after every later chunk; nothing is left short of its last chunk. The next connection names the same 3, unchanged, and installs them whole |
| §6.4 deletion on fixture data | Plants a marker in a thread's title, user text, assistant text and step label, deletes it, kills and restarts the brain | The purge's whole result (no row, no `purge_pending`, no purge file); the marker is in no raw file (`.db`, `-wal`, `-shm`, logs; UTF-8, any case), no table and no FTS match, while a kept thread's marker is found |
| Esc on the real key in the real shell | Not run | Needs an input session on the desk (OSC-132) |

- The two pipes aren't ordered with each other, so frames already in the data pipe can arrive after
  the control pipe's `Interrupted`. The gate's replacement criterion is the wire order the protocol
  promises, nothing of the old run after its `RunEnd`; the shell's fence that drops those late
  frames when it applies them is covered by its unit tests.
- The client applies recovery chunks by the shell's rules and checks them against the fixture's
  known drawings. The real shell's install logic is covered by its unit tests, as its fence is.
- `Stale` is the brain's own verdict and isn't on the pipe, so the late-step item checks its effects.
- Every pipe write, and the wait for `Welcome`, is bounded; a stuck transfer is cancelled and
  waited for before its buffer goes. The brain build is killed, with everything it started, after
  20 minutes, and the whole gate, with its brains, after another 20; a `taskkill` that fails is
  reported with the process it left.
- A brain still running after its item is ended, and that item fails, saying whether `taskkill`
  ended it, so the next item doesn't meet it on the pipes.
  It refuses to run from a release xtask, whose brain would use the installed product's names.
- shortcut: one scene can't reach three snapshot chunks today (32 KiB target, at most 64 steps of
  256 B), so the reconnect item's chunks span four scenes; chunking inside a scene is covered by
  the brain's recovery tests. Use one scene once slice 4's steps are bigger.
- The deleted and kept markers start with letters no fixture word starts with (`qgone…` and
  `zkept…`). FTS5 stores a term after the first on an index page only as what it adds to the term
  before it, so a marker sharing a prefix with its neighbour would never appear whole in the raw
  bytes, and a stale one would pass the scan.
- Retrieval, summaries and Claude Code also hold thread content; slices 3 and 8 add them to the
  deletion item.
- The brain is a dev build, because only dev builds read `KUMOCHI_FIXTURE`, so its timings are
  slower than a release brain's would be.
