# The store

The brain keeps threads, their events and runs in one SQLite file,
`%LOCALAPPDATA%\<product>\capsula.db`, through `rusqlite` with SQLite and FTS5 built in. Only the
brain opens it. The code is `crates/brain/src/store/`.

## Settings

| Pragma | Why |
|---|---|
| `journal_mode=WAL` | Readers don't block the writer, so a purge can copy the file while it serves. A store that refuses WAL doesn't open |
| `synchronous=FULL` | Every commit is durable, so "saved" means the transaction committed |
| `foreign_keys=ON` | Rows can't point at a missing thread or run |
| `secure_delete=ON` | Deleted rows are overwritten with zeros, not left in freeblocks or free pages. Stale cell copies in the gaps a page rebuild leaves are out of its reach, so the purge replaces the file (see below) |
| `temp_store=MEMORY` | Statement journals, which hold the pages a delete removes, never spill to `%TEMP%` |

## Tables

| Table | Holds |
|---|---|
| `threads` | Title, timestamps, `state` (`live` or `deleting`) and `deletion_generation`. `AUTOINCREMENT`, so a deleted thread's id never comes back for a new thread that late work could be mistaken for |
| `runs` | One per user turn: thread, `state` (`active`, `done`, `interrupted`, `failed`), reason, timestamps. `AUTOINCREMENT`, so a run id is never issued twice, even after the thread holding the highest one is deleted |
| `events` | Thread, run, kind (`user`, `assistant_text`, `scene_step`, `revealed`), searchable `text`, JSON `body`. A trigger refuses updates. An event's run must be an active run of the same thread, so a late completion after a run ends is refused |
| `events_fts` | FTS5 over `events.text`, kept by triggers, with FTS5's `secure-delete` on so a deleted row's entries leave the index at once. A deleted term can still survive whole as an `events_fts_idx` separator key until the purge rebuilds the index (see below) |
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
  participant S as Store thread
  participant H as Purge helper
  participant DB as capsula.db
  S->>DB: tx 1: state=deleting, deletion_generation+1
  Note over S: hidden from lists and search, takes no new runs or events
  S->>S: stop the thread's producers (later slices)
  S->>DB: tx 2: delete events (and FTS rows), runs, thread; purge_pending=1
  Note over S: no run active, no op for 200 ms
  S->>H: copy
  H->>DB: read-only: VACUUM INTO capsula.db.purge-snapshot
  H->>H: on the snapshot: purge_pending=0, FTS rebuild, optimize
  H->>H: VACUUM INTO capsula.db.purge-clean, sync, remove the snapshot
  H-->>S: copied
  S->>DB: no change since the snapshot? TRUNCATE checkpoint, close
  S->>DB: rename purge-clean over capsula.db, reopen
```

The foreign keys have no cascade, so the second transaction names every table and fails if it
misses one, rather than leaving rows behind. Deleting the rows isn't enough on its own, as the
CAPS-22 review found, because two kinds of copy survive secure-delete:

- **Page gaps.** When SQLite rebuilds a b-tree page it repacks the cells but never zeroes the gap
  between the cell pointers and the content, so an old copy of a moved cell can stay there after
  the row is deleted.
- **FTS5 separator keys.** `events_fts_idx` keeps a prefix of each leaf page's first term, often
  the whole term, and secure-delete removes it only when the page loses its last term.

So the second transaction also records a **purge** as owed. The purge replaces the file with a
clean copy, built off the store's thread so turns and runs don't wait for it:

1. **Snapshot.** A helper thread opens the file read-only and runs `VACUUM INTO` into
   `capsula.db.purge-snapshot`. A WAL reader never holds the writer, so the store keeps serving.
2. **Clean.** On the snapshot it clears the `purge_pending` row, runs FTS `rebuild`, which
   rewrites the index from the remaining events and so drops the deleted terms' separator keys,
   then `optimize`, which merges the rebuilt index into one segment. Without `optimize`, FTS5's
   first merges after a rebuild stalled single store ops for up to about 180 ms at 232 MiB on Rez.
3. **Compact.** `VACUUM INTO` writes the snapshot into fresh pages in
   `capsula.db.purge-clean`, so no gap or free page the rebuild left keeps an old key. The
   result is synced and the snapshot removed.
4. **Swap**, on the store thread. Unless its connection changed something since the snapshot
   started, it TRUNCATE-checkpoints the WAL so no committed data lives only there, closes the
   file, checks that no WAL is left that SQLite could replay onto the new file, renames the copy
   over `capsula.db` and reopens. This is the only step store work waits for, tens of
   milliseconds at the sizes the purge's pull request measured. Should the reopen fail after the
   rename, every later operation fails rather than write anywhere else.

The purge is cleanup that can be retried:

- **It runs only while nothing else does.** It starts once no run is `active` and no op has
  come for 200 ms [guess]. An op that arrives during the copy stops it, and the swap throws away
  a copy made before a write, so no write is lost; the purge waits for the next quiet moment.
- **It's retried in session.** A swap a reader blocked, a rename that failed (a scanner holding
  the file, say) and any SQLite or file error leave the old file serving and the purge owed. The
  next try waits 1 minute [guess], doubling up to 30 minutes [guess], so a full disk isn't
  rewritten constantly. Every try runs all four steps. Once nothing is owed the thread sleeps
  until an op arrives.
- **It's retried at start.** The row survives a crash. A crash before the rename leaves the old
  file and a copy, which opening the store deletes; after it, the new file has no owed row.
  Recovery only reports an owed purge.
- **It costs disk, not memory.** While it runs it needs room for two more copies of the store,
  and `VACUUM INTO` writes straight to its file, with no in-memory temporary database.

A test ports CAPS-22's remnants scan: it writes 100 threads one event per transaction and
deletes 30 of them, first checking that their FTS keys survive until a rebuild and that, on a
copy of the file, a deleted cell survives a rebuild in a page gap. Then it purges and scans the
raw `.db`, `-wal` and `-shm` bytes, with the store open and after closing it, checks every
`events_fts_idx` key, and checks that no purge copy is left. It fails if the purge skips the
rebuild or replaces nothing. It doesn't fail if the compact step is skipped: at its scale no gap
the rebuild leaves keeps a deleted key, so that step rests on the mechanism CAPS-22 found.

Beyond what deleting rows can reach: SSD wear levelling, the disk clusters a removed or
truncated file frees (the old store, the snapshot, the WAL), Volume Shadow Copy restore points,
backups and the pagefile.

## Recovery at start

The brain opens the store and runs the recovery scan before it serves any shell:

- threads left `deleting` by a crash between the two transactions finish deleting. One that fails
  again is logged by id, stays hidden, and is retried at the next start, so it can't stop the brain;
- runs still `active` in live threads become `failed` with reason `crash`, keeping what they
  committed;
- an owed purge is reported, and the store thread runs it once no run is active.

The "store recovered" log line carries only counts and flags: crashed runs, finished and failed
deletions, and whether the purge is still owed.

shortcut: two sessions of the same user run two brains on one store, and the second's recovery
fails the first's live runs. That needs Windows Server or multi-session Windows, so a
per-session store or a lock waits until Capsula supports them.
