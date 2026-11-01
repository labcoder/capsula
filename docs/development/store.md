# The store

The brain keeps threads, their events and runs in one SQLite file,
`%LOCALAPPDATA%\<product>\capsula.db`, through `rusqlite` with SQLite and FTS5 built in. Only the
brain opens it. The code is `crates/brain/src/store/`.

## Settings

| Pragma | Why |
|---|---|
| `journal_mode=WAL` | Readers don't block the writer |
| `synchronous=FULL` | Every commit is durable, so "saved" means the transaction committed |
| `foreign_keys=ON` | Rows can't point at a missing thread or run |
| `secure_delete=ON` | Deleted content is overwritten with zeros, not left in free pages |

## Tables

| Table | Holds |
|---|---|
| `threads` | Title, timestamps, `state` (`live` or `deleting`) and `deletion_generation` |
| `runs` | One per user turn: thread, `state` (`active`, `done`, `interrupted`, `failed`), reason, timestamps. `AUTOINCREMENT`, so a run id is never issued twice, even after the thread holding the highest one is deleted |
| `events` | Append-only: thread, run, kind (`user`, `assistant_text`, `scene_step`), searchable `text`, JSON `body` |
| `events_fts` | FTS5 over `events.text`, kept by triggers, with FTS5's `secure-delete` on so a deleted row leaves the index at once |

Later slices add assets, sends, the tool ledger, actions, summaries and cleanup jobs, each as a
new migration. Migrations are numbered by `PRAGMA user_version`, run forward only, one
transaction each, and a brain refuses a file whose version is newer than it knows.

Search takes the user's words as one quoted phrase, so FTS5 query syntax in them is plain text,
and never returns a thread being deleted.

## Deleting a thread

```mermaid
sequenceDiagram
  participant B as Brain
  participant DB as SQLite
  B->>DB: tx 1: state=deleting, deletion_generation+1
  Note over B: hidden from lists and search, takes no new runs or events
  B->>B: stop the thread's producers (later slices)
  B->>DB: tx 2: delete events (and FTS rows), runs, thread
  B->>DB: wal_checkpoint(TRUNCATE)
```

The foreign keys have no cascade, so the second transaction names every table and fails if it
misses one, rather than leaving rows behind. The checkpoint copies the WAL into the main file and
truncates it to zero bytes, so the content survives in neither. A test plants a marker in a
thread's title, user text, assistant text and step body, deletes the thread, and scans the raw
`.db`, `-wal` and `-shm` bytes for it; it fails without either `secure_delete` setting.

SSD wear levelling, backups and the pagefile are beyond what deleting rows can reach.

## Recovery at start

The brain opens the store and runs the recovery scan before it serves any shell:

- runs still `active` become `failed` with reason `crash`, keeping what they committed;
- threads left `deleting` by a crash between the two transactions finish deleting;
- the WAL is checkpointed again, in case a reader blocked the last one.
