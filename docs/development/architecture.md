# Architecture

How Kumochi works inside. This page is the map. Each module has its own page under `docs/development/` with the detail. The full design record lives outside the repo.

The [glossary](glossary.md) maps user-facing words to code and protocol terms.

Kumochi is built in slices. Slice 0 is the skeleton: the two processes, their pipes and handshake, and the Job Objects. Sections below describe the whole first release, so some of what they show arrives with later slices.

## The two processes

Kumochi is a GPUI **shell** and a headless **brain**. The shell owns everything you touch, see and hear. The brain owns history, models and authority. The brain keeps its helpers in a Windows Job Object, so a brain crash never leaves them running.

```mermaid
flowchart LR
  subgraph Shell["kumochi.exe (GPUI)"]
    In["Shortcut and Esc<br/>Raw Input"]
    Au["Mic and speakers<br/>played-sample clock"]
    Co["Conductor"]
    Pr["Overlay, panels<br/>2D and 3D presenter"]
  end
  subgraph Brain["kumochid.exe (headless, tokio)"]
    Core["Core loop"]
    Disp["Dispatcher and ledger"]
    Store[("SQLite + FTS5")]
    Voice["Parakeet, Phonon"]
    Conn["Connections"]
  end
  subgraph Job["Job Object"]
    Scr["kumochid --screen"]
    CC["claude (one per question,<br/>in its own run job)"]
    Br["kumochid --mcp-bridge"]
  end
  Shell <-->|"control pipe + data pipe"| Brain
  Core --> Scr
  Core --> CC
  CC --> Br
  Br -->|"run-bound capability"| Disp
  Conn -->|HTTPS| Cloud["ChatGPT, Anthropic, GitHub"]
```

| Process | Owns |
|---|---|
| Shell | The shortcut, Esc, microphone, speakers and their clock, the overlay and panels, scene layout and painting |
| Brain | Runs, the tool dispatcher, connections and credentials, the store, scene validation, voice models, the Job Object |
| Screen worker | Screen capture, UI Automation, OCR and image encoding. It's killable, because a stuck UIA provider can't be stopped any other way |
| Claude Code | The user's own Claude Code, started fresh for each question in its own nested run job, restricted to Kumochi's tools. Ending a question terminates that run job only |

The shell keeps no state on disk, only its log and an empty single-instance lock file, so restarting it is always safe. Both processes log through `tracing` to the `Kumochi\logs` folder in the local application data root (`Kumochi Dev` for debug builds), one file per process per day, kept for seven days. Content is never logged, and panics record only their source location. A shell started by hand finds a running brain through a rotating instance capability in Windows Credential Manager.

Each crate resolves its data folder behind its own `platform` interface, and on Windows the
shell and the brain ask one resolver in `kumochi-storage`. An ordinary release build keeps each
row's route: the store, logs, voice models, session lock, settings and the shell read
`LOCALAPPDATA` or `APPDATA` and require an absolute path, so missing, empty or relative values
never put data under the working directory, and the Claude Code base and sherpa staging read the
Known Folder. It never reads the override variables.

Gate-identity and debug builds take one root pair from one source, so a run can't land in the
real profile by accident:

```mermaid
flowchart TD
    A{KUMOCHI_GATE_LOCALAPPDATA and KUMOCHI_GATE_APPDATA} -->|both set| B[Each half exists on a drive, strictly below the canonical Local AppData, halves apart]
    A -->|one set| X[Refused: InvalidOverride]
    A -->|neither| C{LOCALAPPDATA or APPDATA set and unlike its Known Folder?}
    C -->|yes| Y[Refused: OldDriver]
    C -->|no| K[Both Known Folders]
    B -->|no| X
    B -->|yes| D[Hold both roots' chains for the process's life]
    D --> E[Checked create of the product folders and logs, each existing one held to the product-folder check]
    E --> F[Refuse a linked, hard-linked or non-file store, authority, SQLite side file, session.lock, config.toml or log]
    F --> G[Writers open the admitted paths]
```

With the pair set, `LOCALAPPDATA` and `APPDATA` are ignored. The canonical containment check is
only an early refusal; admission holds the chain of each half as spelled, so a junction or link
on it refuses at its own component, and nothing is created at its target. The first row a
process resolves admits the pair, which in every process is the log folder, before its first
write. The shell checks its instance lock the same way before it opens it. The Claude Code
folder, the voice models and the staging pins were already made by the checked create under a
held parent; the Claude Code base and the staging root now hold their parent's chain while they
create the product folder, in every build. An existing local or roaming product folder, and the
logs folder, must also be a folder and pass the product-folder check below: no one untrusted may add to them or
inherit write on what's made in them. Both halves pass before the logs folder is made under the
local one. A refusal ends startup with the fixed data-root error.

The chain check itself (`hold_chain`, the checked create and the checks they're built from)
lives in `kumochi-storage`'s Windows module, shared by the shell and the brain; the brain's
markers, private trees, deletion and model storage build on it. The brain's session lock uses the
same local data root. It is the profile's one
writer lock: the brain takes it before it opens the store and holds it for its whole run.

The ordinary shell asks its instance-lock owner to acquire the lock. Windows holds one lock
per session until exit, named `<SHELL_LOCK>-<session id>.lock` in the existing data folder.

macOS has no Windows-style session ID. Its lock is per user and product identity:
`~/Library/Application Support/<DATA_DIR>/<SHELL_LOCK>.lock` (`kumochi-dev-shell.lock` for Dev).
The file is mode `0600` and is locked exclusively without waiting. The file stays in place:
closing the lock handle or exiting the process releases the lock. Platforms without an
admitted lock target still refuse before opening a file, with no fallback location.

Every first write to a product folder goes through one check: the Shell's instance lock and
log, every Brain process's log and lock, the Claude Code and speech staging folders, and the
settings folder, which settings also check before they're read. It creates any missing parent as
an ordinary folder, then the product folder owner-only. An existing folder is checked, not
repaired. On Windows, it must be a plain folder with a trusted owner and no write access for
untrusted principals, including inheritable write access. On Unix, creation requests mode
`0700` and the final component is opened without following links. The held directory must
belong to the effective user or root and lack group or other write bits. macOS also applies
its existing ACL inspection; Linux requires the default POSIX ACL to be absent. The Unix
check doesn't hold the parent chain or set the permissions of files made inside the folder.

Detached brain startup acquires both pipes before touching the store. It takes the profile's
writer lock, then opens and recovers the store, loads settings, starts accounts, and builds the Brain. It retries any interrupted
voice model removals before creating the helpers' Job Object, then starts the optional workers.
The mode check before the removal pass and the Claude Code setting read the running mode, so a
mode without containment removes nothing. Startup crosses four checkpoints: after the opened
state, after accounts, after the Brain and after the Job. Detached passes a no-op at each. A
parent-bound start makes one commit claim and then passes a fence at each
checkpoint. The watch thread latches the parent's loss in an atomic gate before it publishes,
so a claim that comes after a loss fails, and a fence refuses once a loss or a closed watch is
visible. Each passed checkpoint admits the work up to the next one; the fences owed inside the
helpers' Job and worker starts come with containment. A loss during admitted work lets it
finish, and the next fence refuses with the same unwind as an error there.
Both modes run the stages through one owner of what has started. Started accounts stay there
until the Brain takes them, and a failed `Brain::new` hands them back. Detached drops whatever
started, as before. The parent-bound root settles it instead. With no Brain, it
closes the accounts, which waits for the credential thread to let the vault go, then drops the
raw store, recorded Unconfirmed because no receipt confirms that drop; so is a store an open
acquired before it failed. A refused or panicked startup load returns only after the credential
thread has let the vault go, so no worker outlives a failed start unseen. Then it settles the
instance attempt, drops custody, and only then ends the process services that its injected
prerequisites start after the claim. This build's prerequisites are never ready.
Only then does it publish the instance capability and serve the shell. The lifecycle owner
keeps the pipes alive through teardown. Before capability publication, a failure drops the
resources that started without entering graceful shutdown. The store and credential workers
each keep a reference to the writer lock until they finish, and so does each settings write,
including one the removal pass starts, until its blocking write returns or panics. So neither a
failed start nor a close that stopped waiting releases it while a writer still runs. After
serving, it quits or closes the Brain, shuts down the Job off the async thread, and deletes the
capability in that order.

Each of those steps reports what it was seen to do: Confirmed when an explicit success was
observed before its deadline, Late when it was observed at or after it, Failed on an error, a
refusal, a panic or a closed sender, and Outstanding when the deadline passed first. Success is
timed by when it's observed, because a wait that was already ready returns it even past the
deadline. The Brain reports the store's close receipt (SQLite closed, not its helpers joined or
the lock released); the settings writes owed from the start of shutdown, graded by their last
save, where a later save cannot clear a refusal; Claude Code's lane; and the accounts,
whose credential job is still waited for past the deadline and then reads Late, and which are
Failed when an issued set was held after a refused save or a GitHub save or rotation was left
unfinished or ended in an error while closing.
The Job's shutdown and the capability's deletion have no deadline, so they're only Confirmed or
Failed. Nothing reads these yet: Detached keeps the result of serving as its exit status, so an
incomplete Quit or any stage short of Confirmed doesn't change it. A parent-bound exit requires
every stage Confirmed after a complete Quit (see below).

### Parent-bound startup

`run_parent_bound` takes one 10-second admission clock before its first bootstrap action, then
hardens the process, adopts the inherited pair and arms the parent watch. Admission, the stages
and serving run on one current-thread runtime through one `Root`, which owns the stages, the
instance attempt and the process services. The parent watch drops only after that runtime has.

Custody is the profile's one writer lock for a parent-bound start. On macOS, acquiring it admits
the Support folder, holds or privately creates the product folder, locks `session.lock` on the
descriptor it opened, and returns that lock with the absolute product path it walked, so nothing
resolves Application Support a second time. Only custody lends the lock: a lent `SessionLock`
shares custody's lock and carries that path as its data folder and its authority. `open` runs on
the loan and takes no lock of its own, which would be Busy against custody's. The store, the
settings and the authority all come from that path, with Detached's existing-or-create store
handling, a missing authority created, and the same conservative `acquired` marking. The stages
and their workers hold loans, and the instance attempt holds custody itself, so the lock releases
only after the stages have dropped and the item has settled. This binds the pathname, not the
folder's identity if it's replaced. A fixture's token custody lends nothing, so its fixture opens
a scratch profile instead.

```mermaid
flowchart TD
  admit["admit: fence, custody, fence, item, READY,<br/>handshake, greeting, fence, claim"]
  stage["stage: fence, ready, fence,<br/>init, open, accounts, Brain, containment"]
  serve["serve: ShellTuning, then both legs"]
  settle["settle: quit or close, the helpers, the legs,<br/>the item, custody, then the process services"]
  admit --> stage --> serve --> settle
  admit -- refused --> settle
  stage -- refused --> settle
```

The stage driver fences and checks the clock on entry, asks the prerequisites whether they're
ready, and fences again before anything starts; each checkpoint fences too. It never starts
voice: this mode builds no models, so held speech answers NotLoaded, and only what the
prerequisites' containment allows would start. Serving writes ShellTuning only while the parent
and the same clock both pass, checked right before every poll of the writer, preferred over a
writer that is ready at the same time, and checked again after it. It then lends both legs to the runtime with
a terminal that watches the pair's cell and the parent. The root keeps the pair through
settlement and drops it after the Brain and its helpers, before the item, so the shell sees EOF
only once the Brain has settled. A refused admission, or a fatal end of serving, drops it at
once, before any cleanup is awaited. A failed tuning ends with the fatal cause the pair's cell or
the parent already holds, such as a cancelled write's refusal, and as lost only without one.
Settlement has no
admission deadline. The 5-second handshake caps sit inside the admission clock.

`AdmissionPair` is the seam between that shared driver and the platform's pair: it keys,
admits, exposes the pair's cell and serves. macOS implements it on the inherited pair; other
platforms have no pair to make one, and the tests drive the same root with a fake.

| Served, then settled | Result |
|---|---|
| Any stage short of Confirmed, or an incomplete Quit | `Incomplete`, over every other row |
| An authenticated Quit that completed | `Ok` |
| Lost, Fatal, Broken or Unavailable, even after a clean close | `Refused` |
| A refusal before serving | that refusal: `Unsupported`, `Busy` or `Refused` |

This build's prerequisites refuse at `ready`, after a real admission, so a parent-bound start
still never runs a stage; the support table refuses before that anyway.

## Crates

```mermaid
flowchart TD
  protocol["protocol<br/>messages, ids, constants"]
  scene["scene<br/>drawing format, validator,<br/>layout, 3D geometry"]
  brain["brain<br/>bin kumochid"]
  core["shell-core<br/>what the shell knows<br/>and decides"]
  shell["shell<br/>bin kumochi"]
  xtask["xtask<br/>bench, eval, fixtures"]
  brain --> protocol
  brain --> scene
  shell --> protocol
  shell --> scene
  shell --> core
  core --> protocol
  xtask --> protocol
  xtask --> scene
  protocol --> scene
```

`scene` has no GPUI and no tokio. The shell gives it text measurement through a small trait.

`shell-core` holds what the shell knows and decides, apart from how it is drawn. No UI
framework is in its dependency tree, dev-dependencies included, so nothing in it can name
GPUI; the shell is the crate that does. `cargo xtask check-core` holds that in CI: the core's
direct dependencies are a list in `xtask/src/check_core.rs`, so a new one is a reviewed line,
and the whole tree is searched for a framework's name.

`kumochi-storage` groups its non-Windows code by OS family. `platform/unix/` holds POSIX
helpers the macOS adapter shares: descriptor-relative opens and looks that never follow links,
and the owner, mode and link-count checks. `platform/macos/` keeps what only Darwin has (ACLs,
the APFS volume policy, dataless files, `sysdir` and the Keychain) and the policy built on both.
The crate builds only for Windows and macOS.

On macOS, shell and brain use the same Application Support lookup in `kumochi-storage`.
It asks Darwin for the user-domain path, resolving any home-relative result through the
OS account, not `HOME`. It opens and creates nothing. Callers append or admit the product
folder separately; a candidate does not authorize a write.

`kumochi_storage::admit_support` holds and inspects that folder the way `open_shell_storage`
does, creating nothing. On the held anchor, `Support::product_folder` looks up this edition's
product folder: only `ENOENT` from opening that exact name in the held folder is `Missing`, and a
folder there must pass the same private-folder checks. `open_shell_storage` starts with
`admit_support`; the product-folder lookup has no caller yet.

Linux is a build and test host for `shell-core`, not a supported runtime. Its native
IPC, process launch, credential storage, file logging, and audio entry points return
`Unsupported`. The shared tests use scripted audio and need ALSA development headers
to compile CPAL, not an audio device. Real Windows pipe and device tests and tests that
need the brain stay on Windows while the brain cannot build on Linux.

The shell's tests build the core as a dependency, without `cfg(test)`. So a test double in
the core is gated on `any(test, feature = "test-support")`, and only a dev-dependency may
turn that feature on. `check-core` fails on any other spelling of a test gate that could
switch behaviour, and when the feature reaches a normal dependency anywhere in the
workspace. One build is outside its reach: `cargo build --all-targets` resolves features
once for the whole command, so the `kumochi.exe` it writes carries the core's test support.
That has to fail safe, so test support only ever takes something away: a device becomes a
stub that reports itself unavailable, never the other way round.

Tests that need files use the shared scratch guard in `tests/support/scratch.rs`. It creates
a new child of Temp (or of an existing trusted root for fixtures that need one), skips
occupied names, and removes its recorded child path on return or unwind. It does not
sweep old folders or follow symbolic links in the scratch tree, but removal by path
cannot distinguish a directory swapped into that path during a test. Speech fixtures
leave scratch in place if they cannot confirm child teardown.

Dev and test builds compile ten dependencies at opt-level 2: image, zune-jpeg, libsqlite3-sys,
rusqlite, serde_json, sha2, ring, memchr, postcard and base64, every version of each
(`[profile.dev.package]` in the root `Cargo.toml`). The brain's tests spend most of their CPU in
them. The workspace crates and every other dependency stay at 0, and release builds are
unchanged. CI alone runs the tests four at a time (`RUST_TEST_THREADS: 4`), because they mostly
wait on processes, pipes and deadlines; elsewhere libtest uses one thread per core. A dev or test
measurement pair whose two commits sit on either side of the commit that added that table
compares two different builds of those crates, so build both sides with the same table.

CI runs two jobs side by side. `lint` runs fmt, cargo-deny for dependency or CI configuration
changes and on every scheduled or manual run, `check-vendor`, `check-core`, `check-platform-cfg`,
clippy on the core alone without test support, and clippy on the workspace. `test` runs the workspace tests and the
vsync test. Each job restores its own cache, and only runs on main save one. The test job points
`TEMP` and `TMP` at the runner's work disk: the tests' scratch stores make many durable writes,
and the profile's temp folder on C: takes them seven to ten times slower.

`cargo xtask check-platform-cfg` keeps plain OS predicates out of product code outside
`platform.rs` and `platform/`. It scans Rust source under `crates/*/src`, skipping conventionally
named test and fixture files and items gated on tests or `test-support`. A plain predicate uses
only OS selectors (`windows`, `unix`, `target_os`, or `target_family`), not feature, debug,
or test gates. This includes plain predicates applied by a product-reachable `cfg_attr`.
The baseline counts occurrences of each predicate per file; line changes and relocating an
identical predicate within the same file do not change that count. Delete an entry when removing
its site (use `--list` to see the current inventory). CI compares against the PR base or current
`origin/main` for batch pushes so the baseline cannot grow to admit a new site.

## The brain is a game loop

The brain has a small **core loop**: a coordinator that takes events and returns effects, with no I/O of its own. Everything else is a **system** that does I/O and reports back as events.

```mermaid
flowchart LR
  subgraph Systems
    E["engines<br/>responses, anthropic, claude_code"]
    D["dispatch"]
    S["store"]
    V["voice"]
    W["screen"]
    C["connections"]
    X["context"]
    I["ipc"]
  end
  Core(("core<br/>runs, ids, budgets"))
  Systems -- "events (run id, deletion generation)" --> Core
  Core -- "effects" --> Systems
```

Every event carries its run id and the thread's deletion generation. The core drops stale ones, so a late result from a stopped run or a deleted thread changes nothing. Because the core does no I/O, the order of Esc, interruptions and deletion is tested as plain unit tests.

## A voice question, end to end (direct route: ChatGPT or the Anthropic key)

```mermaid
sequenceDiagram
  participant U as You
  participant Sh as Shell
  participant Br as Brain
  participant Sw as Screen worker
  participant M as Model
  U->>Sh: hold the shortcut
  Sh->>Sh: Listening in the same frame, mic opens
  Sh->>Br: ScreenHoldRecognized (after 300 ms)
  U->>Sh: release
  Sh->>Br: ScreenRelease, last audio, KeyUp
  Br->>Sw: capture the display under the pointer
  Sw-->>Br: one picture, the pointer's path painted on it
  Br->>Br: Parakeet transcribes on the CPU
  Br->>M: request (context, overview and optional focus crop from one capture)
  M-->>Br: text and draw-tool stream
  Br->>Br: each closed step enters the dispatcher, validates, commits
  Br->>Sh: StepCommit + speech PCM
  Sh->>Sh: prepare the step (speech plays only once it's prepared)
  Sh->>Sh: step activates on its first played sample, visible next frame
```

- **Capture happens only on a recognized hold or Include screen.** A tap or double tap never captures. The screen worker refuses any request without a ticket, and no tool can mint one.
- **Screenshots go only to the model you chose**, and every one sent is saved in the thread.

## Drawings

A drawing is one tool call holding `begin`, a list of `step` records, and `end`. Each step is a sentence plus a few shapes.

Direct route shown. On Claude Code, the whole call arrives at once and its valid steps commit in order.

```mermaid
flowchart LR
  A["step closes in the stream"] --> B{"valid?"}
  B -- yes --> C["commit to the thread"]
  C --> P{"prepared within 250 ms?"}
  P -- yes --> E["speak it, show it on its first played sample"]
  P -- no --> G["mark failed, drop that drawing's queued speech"]
  B -- no --> F["stop the call, keep committed steps,<br/>mark the drawing incomplete,<br/>tell the model what was drawn"]
```

- **ChatGPT and the Anthropic key:** steps commit one by one as they stream.
- **Claude Code:** the MCP call is the authority. When it arrives, its valid steps commit in order up to the first invalid one, then play in order.
- **Nothing from an unclosed step is shown**, and speech only describes steps that already validated.
- **Speech waits for its picture.** A step's sentence plays only once the shell has prepared the step, after a wait of up to 250 ms. A step that isn't prepared by then shows as failed and is never spoken, and the rest of that drawing's queued speech is dropped.
- **Caps** are checked on expanded work before anything is allocated, and nothing is silently truncated.

A committed replacement removes its victims durably. The shell can keep only their frozen
presentation owners for up to 250 ms while preparing the successor, then select its first
nonempty sample on the same shown host. Capacity pressure releases this visual hold instead
of refusing an otherwise-fitting successor. Expiry and late failure do not restore old scenes;
fixed original-thread feedback reports when a replacement failed after removing them.

**3D** uses a CPU depth-buffered raster; the planned shell host paints its BGRA image because
GPUI on Windows has no depth buffer for element content. Opaque solids write per-sample depth;
translucent polygons follow in centroid order without writing depth, which can blend intersecting
translucent geometry in the wrong order (OSC-242). Rest frames supersample edges. The scene frame
stage transforms indexed source geometry at the sampled, attachment-composed pose, culls back
faces, clips near and guard planes in f64, projects, snaps and shades into reserved raster scratch.
Every source face shares one reciprocal-depth plane across its triangles. Camera defaults target
the 3D bounds' centre; authored cameras can enter geometry, and clipping rather than a distance
clamp handles that case. The near plane includes a world-coordinate precision floor. These scene
primitives exist and scene admission charges 3D steps, but the brain refuses them at its ingress
until the shell host lands. Scene picking reads an
owned `DisplayedFrame`: ids, camera, sampled poses, geometry revision, solid-to-part handles and
the frozen logical destination/DPI/exact integer raster dimensions. Each pointer axis maps
independently; labelled geometry returns a part, unlabelled geometry blocks and background misses.
An edge neighbourhood of one internal texel can differ from an exact ray; translucent ids follow
the approximate painted order. The later host must publish the image and this entire snapshot
together at accepted paint, never on worker completion alone. A worker writes only the other
owned id buffer; failed, skipped or superseded paints leave the displayed pair unchanged.

## Esc

The shell stops first, with no round trip to the brain.

```mermaid
sequenceDiagram
  participant K as Esc
  participant Sh as Shell
  participant Br as Brain
  participant E as Engine
  K->>Sh: first Esc
  Sh->>Sh: fence the run, silence audio,<br/>keep objects that finished entering,<br/>remove objects still entering and anything not shown
  Sh->>Br: Stop (control pipe)
  Br->>Br: revoke run and its capability
  Br->>E: drop stream / kill Claude Code
  K->>Sh: second Esc
  Sh->>Sh: clear the drawing (the thread keeps it)
```

Kumochi holds Esc from other apps only while it's speaking or a drawing is on screen.

## Tools and authority

Every tool call from every engine goes through one dispatcher. A Claude Code call arrives through the stdio bridge with a capability bound to one run, and that run is fixed the moment the request arrives. A retried request with the same id is answered from the ledger and never runs twice. Filing an issue needs a complete call and your confirmation of the exact content. If GitHub's reply is lost, the action stays "unknown" unless a lookup finds exactly one match.

| Tool | Effect |
|---|---|
| `draw` | Screen |
| `look`, `get_screenshot`, `search_history`, `read_history` | Read |
| `new_thread` | Local write |
| `draft_issue` | Outside write, after confirmation |

## The pipes

Two owner-only named pipes per connection, with `postcard` frames and caps checked before allocation.

| Pipe | Carries |
|---|---|
| Control | Esc and Stop, shortcut edges, run state, consent, errors, handshake |
| Data | Steps, speech audio, chat text, images and pages, under receiver credits |

The run id is the epoch. The shell drops any message from a revoked run when it applies the message, not when it arrives.

## Deleting a thread

```mermaid
flowchart LR
  A["mark deleting,<br/>generation + 1"] --> B["revoke runs,<br/>cancel jobs,<br/>kill Claude Code"]
  B --> C["delete rows, FTS,<br/>media in one transaction"]
  C --> D["checkpoint WAL"]
  D --> E["delete Claude Code's<br/>owned temp folder"]
  E --> F["done<br/>(retried until it is)"]
```

Deletion covers every copy Kumochi controls. It can't reach issues already filed or what providers keep under their own terms, and the delete dialog says so.

## Claude Code

Each question starts a fresh process:

```text
claude -p --restricted --strict-mcp-config --mcp-config <owned>
       --disable-slash-commands --tools "" --no-session-persistence ...
```

It runs with an allowlisted environment, telemetry opt-outs, and a working folder and temp folder that Kumochi owns. Before any real input, an audit of its startup facts, checked against a schema for that exact Claude Code version, must show only Kumochi's tool and the reviewed built-in plugins, no skills or commands, and the user's own plan. Anything else fails closed. Claude Code doesn't report hooks or memory, so those rest on restricted mode and launch settings rather than the audit. Kumochi never reads Claude Code's credentials.

## Rules that protect you

- Kumochi itself sends no telemetry. Network calls are limited to the accounts you set up and
  model downloads you authorize. During those HTTPS handshakes, including setup, macOS trust
  services may fetch certificate status or missing certificates. An OS trust evaluation admitted
  before refusal or cancellation may still make those lookups without receiving credentials
  or application data from Kumochi. See [Checked HTTP and roots](accounts.md#checked-http-and-roots).
- Claude Code is your own program. Kumochi asks it to switch telemetry off, shows what it reports, and runs it either way.
- Secrets live only in Windows Credential Manager, never in logs, arguments or files. The one exception: Claude Code's own credential file may exist only inside Kumochi's Claude Code folder, readable by the user and SYSTEM only. Kumochi never reads, copies or relays its contents, and Disconnect removes it (logout, then delete).
- Prompts, transcripts and screenshots are never logged.

## Where to read next

`processes.md`, `ipc.md`, `settings.md`, [frame-rate.md](frame-rate.md), `scene.md`, [engines.md](engines.md), `store.md`, [accounts.md](accounts.md), [voice.md](voice.md), `screen.md`, [claude-code.md](claude-code.md), [onboarding.md](onboarding.md), each added with its module.
