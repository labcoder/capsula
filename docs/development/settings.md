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
checked. An empty user name is `None`, never an empty name. Their `Debug` prints a length, and
nothing logs a name, a value from the file, or the TOML parser's messages.

## Changes

```mermaid
sequenceDiagram
  participant S as Shell
  participant C as Brain core
  participant W as Write (blocking pool)
  C-->>S: Settings(view), before Scenes
  S->>C: Settings(SetAssistantName)
  C->>W: stage on a copy of the table, write config.toml.tmp, sync, replace
  S->>C: Stop (served while the write runs)
  W-->>C: written
  C->>C: the staged table and view become the settings
  C-->>S: Settings(view)
  Note over C,S: a refused or failed change sends Error(key) just before its view
```

- At most one write is in flight, off the core loop, so a slow disk never delays Stop, Pong or
  Quit. Changes that arrive meanwhile wait and are applied in order.
- Each change gets exactly one `Settings` view back, in order. A refused or failed change gets
  an `Error` first (`ConfigUnreadable` for a bad file, otherwise `SettingsRefused`), then the
  unchanged view, so the shell's field goes back to what's saved.
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
