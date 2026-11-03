# Settings

The brain owns the user's settings and keeps them in `config.toml`, in the `Kumochi` folder
under `%APPDATA%` (`Kumochi Dev` for debug builds, so a dev build never touches an install).
The shell shows them and asks for changes; it never reads the file.

| Key | Type | Default |
|---|---|---|
| `assistant_name` | 1–24 characters | `Kumo` |
| `user_name` | 1–24 characters, or absent | absent (unset) |
| `onboarded` | boolean | `false` |

**Names** follow one rule, checked by `AssistantName` and `UserName` in `crates/protocol` on
both sides: trimmed, 1–24 Unicode scalar values, one line, with no control characters and no
U+2028 or U+2029. A name that breaks the rule fails to decode, so a shell sends only names it has
checked. An empty user name is `None`, never an empty name. A name's `Debug` prints a length, and
nothing logs a name, a value from the file, or the TOML parser's messages. Every `SettingsView`
also names `main_thread`, the store's main thread, whose row the shell labels with
`assistant_name`.

## Changes

```mermaid
sequenceDiagram
  participant S as Shell
  participant C as Brain core
  participant W as Write (blocking pool)
  C-->>S: Settings(view, answer: false), before Scenes
  S->>C: Settings(SetAssistantName)
  C->>W: stage on a copy of the table, write config.toml.tmp, sync, replace
  S->>C: Stop (served while the write runs)
  W-->>C: written
  C->>C: the staged table and view become the settings
  C-->>S: Settings(view, answer: true)
  Note over C,S: a refused or failed change sends Error(key) just before its answer
```

- At most one write is in flight, off the core loop, so a slow disk never delays Stop or Pong.
  Changes that arrive meanwhile wait and are applied in order.
- Each change gets exactly one `Settings` with `answer: true` back, in order, while the
  connection that sent it lasts. A refused or failed change gets an `Error` first
  (`ConfigUnreadable` for a bad file, otherwise `SettingsRefused`), then the unchanged view, so
  the shell's field goes back to what's saved. An `Error` only ever comes just before an
  `answer: true` view. A `Settings` with `answer: false` answers nothing: it's
  the connection's first frame, or the settled settings after a change another connection sent.

## Across connections and Quit

A change the brain has read is written even if its shell is gone: the queue keeps moving while
no shell is connected, and Quit and close write what's left before they end.

- **A later connection** gets the settings as saved so far, then, as each earlier connection's
  change is saved, the settled settings with `answer: false`. An earlier change's refusal is only
  logged: it sends no `Error` and no view. So a new shell's own changes are answered only by `answer: true`, and nothing from a
  gone shell can pass for one.
- **Quit** writes the queue within its one absolute deadline (2 s from the accepted Quit). A
  change still unconfirmed then is logged by count, with no value. The runtime then shuts down
  without waiting for a write stuck on a slow disk, so Quit still ends on time. That save is
  unconfirmed, not lost: the write may still finish before the process exits. Either way
  `config.toml` is the whole old file or the whole new one, because it's only ever replaced.
- **Close** (the shell gone for good, or from another build) gives the settings the same 2 s
  budget. Its waits for ended runs and for the store each keep their own 2 s, so closing as a
  whole can take longer.
- A change becomes the settings only once the file is replaced, so a failed write changes
  neither memory nor disk. A leftover `config.toml.tmp` is never read; the next write overwrites
  it.
- Keys this build doesn't know survive every write, so a newer build's settings outlive a
  downgrade.

## When the file is bad

- **Missing:** the defaults, and the first change creates it.
- **A known key with a bad value:** that key reads as its default and stays in the file as it is
  until the user sets it.
- **Unreadable or not TOML:** the brain runs on the defaults and refuses every change, so the file
  is never overwritten. Repair or delete it, then restart Kumochi.
- **No absolute `%APPDATA%`:** the defaults, and changes are refused.
