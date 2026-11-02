# The store

The brain keeps threads, their events and runs in one SQLite file,
`%LOCALAPPDATA%\<product>\capsula.db`, through `rusqlite` with SQLite and FTS5 built in. Only the
brain opens it. The code is `crates/brain/src/store/`.

## Settings

| Pragma | Why |
|---|---|
| `journal_mode=WAL` | Readers don't block the writer, and the checkpoint after a delete's purge truncates the WAL. A store that refuses WAL doesn't open |
| `synchronous=FULL` | Every commit is durable, so "saved" means the transaction committed |
| `foreign_keys=ON` | Rows can't point at a missing thread or run |
| `secure_delete=ON` | Deleted rows are overwritten with zeros, not left in freeblocks or free pages. Stale cell copies in the gaps a page rebuild leaves are out of its reach, so the purge's `VACUUM` removes them (see below) |
| `temp_store=MEMORY` | Statement journals, which hold the pages a delete removes, and `VACUUM`'s temporary copy never spill to `%TEMP%` |

## Tables

| Table | Holds |
|---|---|
| `threads` | Title, timestamps, `state` (`live` or `deleting`) and `deletion_generation`. `AUTOINCREMENT`, so a deleted thread's id never comes back for a new thread that late work could be mistaken for |
| `runs` | One per user turn: thread, `state` (`active`, `done`, `interrupted`, `failed`), reason, timestamps. `AUTOINCREMENT`, so a run id is never issued twice, even after the thread holding the highest one is deleted |
| `events` | Thread, run, kind (`user`, `assistant_text`, `scene_step`, `revealed`), searchable `text`, JSON `body`. A trigger refuses updates. An event's run must be an active run of the same thread, so a late completion after a run ends is refused |
| `events_fts` | FTS5 over `events.text`, kept by triggers, with FTS5's `secure-delete` on so a deleted row's entries leave the index at once. A deleted term can still survive whole as an `events_fts_idx` separator key until the purge's `rebuild` (see below) |
| `purge_pending` | One row while a deletion's purge is owed, written in its second transaction and cleared only after the purge finishes |

The coordinator issues thread and run ids itself, above `high_water()` (the highest ids in
`sqlite_sequence`, deleted ones included), and the store refuses any id that isn't new. A whole
user turn (a new thread when there is one, the run, and the user's text) is one transaction.

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
  B->>DB: tx 2: delete events (and FTS rows), runs, thread; purge_pending=1
  B->>DB: purge: FTS rebuild, VACUUM, wal_checkpoint(TRUNCATE)
  B->>DB: purge_pending=0
```

The foreign keys have no cascade, so the second transaction names every table and fails if it
misses one, rather than leaving rows behind. Deleting the rows isn't enough on its own, as the
CAPS-22 review found, because two kinds of copy survive secure-delete:

- **Page gaps.** When SQLite rebuilds a b-tree page it repacks the cells but never zeroes the gap
  between the cell pointers and the content, so an old copy of a moved cell can stay there after
  the row is deleted.
- **FTS5 separator keys.** `events_fts_idx` keeps a prefix of each leaf page's first term, often
  the whole term, and secure-delete removes it only when the page loses its last term.

So the second transaction also records a **purge** as owed, and the purge runs right after it:
FTS `rebuild` rewrites the index from the remaining events, `VACUUM` rewrites every page, and a
TRUNCATE checkpoint copies the result into the main file and empties the WAL. Only then is the
`purge_pending` row cleared, followed by one more checkpoint, which has only that row to copy.

The purge is cleanup that can be retried, and it never holds up a run:

- **It gives way to store work.** Every op queued for the store thread is counted under a lock.
  A purge step doesn't start while the count is above zero, and an op queued during a step
  aborts it through SQLite's interrupt. The step rolls back, the purge stays owed, and the op
  runs at once. Stop's answer never waits on the store at all (`runs.md`).
- **It's retried in-session.** The store thread retries an owed purge once its queue has been
  empty for 200 ms [guess], so a purge doesn't start mid-run. After a reader blocked the
  checkpoint, or SQLite failed, it waits 1 s [guess] first. Within a session a retry only redoes
  the steps that didn't succeed; once the rebuild and `VACUUM` are done, only the checkpoint is
  owed, because later writes never bring deleted text back.
- **It's retried at start.** The row survives a crash, so the next start's recovery runs the
  whole purge again.
- **The checkpoint never waits on a reader.** It runs with no busy timeout, so a reader holding an
  older snapshot makes it return at once, and `finish_delete` reports `Deletion::Pending`.

The cost grows with the file: `VACUUM` rewrites every page, media included once slice 6 stores
captures, and needs room for a second copy, in memory for the temporary database and in the
WAL. The pull request that added the purge measured it at several sizes on Rez.

A test ports CAPS-22's remnants scan: it writes about 100 threads one event per transaction,
deletes 30 of them, then scans the raw `.db`, `-wal` and `-shm` bytes for their markers and
tokens and checks every `events_fts_idx` key. On the bundled SQLite it fails if the purge skips
the rebuild, or if it skips `VACUUM`.

Beyond what deleting rows can reach: SSD wear levelling, the disk clusters a truncated WAL
frees, Volume Shadow Copy restore points, backups and the pagefile.

## Recovery at start

The brain opens the store and runs the recovery scan before it serves any shell:

- threads left `deleting` by a crash between the two transactions finish deleting. One that fails
  again is logged by id, stays hidden, and is retried at the next start, so it can't stop the brain;
- runs still `active` in live threads become `failed` with reason `crash`, keeping what they
  committed;
- an owed purge runs. If a reader blocks its checkpoint again, `purge_pending` says so and the
  store thread keeps retrying it.

The "store recovered" log line carries only counts and flags: crashed runs, finished and failed
deletions, and whether the purge is still owed.

shortcut: two sessions of the same user run two brains on one store, and the second's recovery
fails the first's live runs. That needs Windows Server or multi-session Windows, so a
per-session store or a lock waits until Capsula supports them.
