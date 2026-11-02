//! The store's thread: one thread owns the connection and runs the coordinator's store
//! operations in order, answering with acknowledgements. Once no run is active it also runs an
//! owed purge: a helper thread copies the store while this one keeps serving, this one swaps the
//! copy in, and a helper removes the purge's files.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::coordinator::{Input, StoreOp};
use crate::store::{Copied, EventKind, NewEvent, Store, StoreError, Swap};

/// How often the thread looks at a running helper.
const POLL: Duration = Duration::from_millis(50);
/// Free space a copy leaves for foreground writes [guess]. shortcut: the snapshot's FTS rebuild
/// may grow it by about the index's size before old segments are freed, which this doesn't
/// count; add `events_fts_idx`'s size once indexes near 256 MiB.
const RESERVE: u64 = 256 * 1024 * 1024;

/// When an owed purge is tried.
#[derive(Debug)]
pub(crate) struct Schedule {
    /// How long after the last op [guess].
    pub(crate) quiet: Duration,
    /// The wait after an attempt that didn't finish, doubling up to `last_retry`, so a store
    /// that keeps being written, a full disk or a stuck file isn't rewritten constantly [guess].
    pub(crate) first_retry: Duration,
    pub(crate) last_retry: Duration,
    /// The volume's free space for a folder.
    pub(crate) free_space: fn(&Path) -> io::Result<u64>,
    /// Hears every attempt's outcome.
    #[cfg(test)]
    pub(crate) attempts: Option<mpsc::Sender<Attempt>>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            quiet: Duration::from_millis(200),
            first_retry: Duration::from_secs(60),
            last_retry: Duration::from_mins(30),
            free_space: crate::platform::free_space,
            #[cfg(test)]
            attempts: None,
        }
    }
}

/// What a purge attempt came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// A run was active, so it waits for the next op.
    Busy,
    /// An op arrived during the copy, or after it, so the copy was thrown away.
    Stale,
    /// A reader kept the old file open.
    Blocked,
    /// The volume hasn't room for the copies and the foreground's reserve.
    NoRoom,
    /// SQLite or the file system failed.
    Failed,
    /// The clean copy replaced the store and every purge file is gone.
    Done,
}

/// The one purge job running on a helper thread. Only one runs at a time, so a removal never
/// takes a later copy's file.
enum Job {
    /// A copy; any op stops it.
    Copy {
        stop: Arc<AtomicBool>,
        helper: JoinHandle<Result<Option<Copied>, StoreError>>,
    },
    /// Removing the purge's files: the old store after a swap, an unused copy, or what a crash
    /// left. Ops don't stop it, and it runs during runs too: until it's done, the old store's
    /// deleted text is still on disk.
    Remove {
        helper: JoinHandle<io::Result<()>>,
        /// What the attempt came to once the files are gone; `None` goes on to an owed copy.
        then: Option<Attempt>,
    },
}

impl Job {
    fn is_finished(&self) -> bool {
        match self {
            Self::Copy { helper, .. } => helper.is_finished(),
            Self::Remove { helper, .. } => helper.is_finished(),
        }
    }

    /// Waits for the helper; false when it panicked.
    fn join(self) -> bool {
        match self {
            Self::Copy { helper, .. } => helper.join().is_ok(),
            Self::Remove { helper, .. } => helper.join().is_ok(),
        }
    }
}

/// The purge's side of the thread: what's running and when to try next.
struct Purger {
    schedule: Schedule,
    next: Option<Instant>,
    retry: Duration,
    job: Option<Job>,
}

impl Purger {
    /// Due at once when a purge is owed or its files are left from an earlier session.
    fn new(schedule: Schedule, store: &Store) -> Self {
        let due = store.purge_pending() || store.leftovers().exist();
        Self {
            retry: schedule.first_retry,
            next: due.then(Instant::now),
            schedule,
            job: None,
        }
    }

    /// How long the thread may wait for an op before the purge needs it.
    fn timeout(&self) -> Option<Duration> {
        if self.job.is_some() {
            return Some(POLL);
        }
        self.next
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// An op is about to run, so a running copy is stale: it stops before the op takes the disk.
    fn op_arriving(&self) {
        if let Some(Job::Copy { stop, .. }) = &self.job {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// An op ran: an owed purge waits for quiet again, but no sooner than a retry already set.
    fn op_arrived(&mut self, store: &Store) {
        if self.job.is_none() && store.purge_pending() {
            let quiet = Instant::now() + self.schedule.quiet;
            self.next = Some(self.next.map_or(quiet, |next| next.max(quiet)));
        }
    }

    /// The wait for an op ran out: finishes a job that's done, or starts one that's due.
    fn tick(&mut self, store: &mut Store) {
        let Some(job) = self.job.take_if(|job| job.is_finished()) else {
            if self.job.is_none() && self.next.is_some_and(|at| at <= Instant::now()) {
                self.start(store);
            }
            return;
        };
        match job {
            Job::Copy { helper, .. } => match helper.join() {
                Ok(Ok(Some(copied))) => {
                    let attempt = match store.swap(&copied) {
                        Ok(Swap::Done) => Attempt::Done,
                        Ok(Swap::Stale) => Attempt::Stale,
                        Ok(Swap::Blocked) => Attempt::Blocked,
                        Err(error) => failed(&error),
                    };
                    // The old store, or the copy the swap didn't use, is left to remove.
                    self.remove(store, Some(attempt));
                }
                Ok(Ok(None)) => self.settle(Attempt::Stale, store),
                Ok(Err(error)) => self.settle(failed(&error), store),
                Err(_) => {
                    tracing::error!("the purge's helper panicked");
                    self.settle(Attempt::Failed, store);
                }
            },
            Job::Remove { helper, then } => match (helper.join(), then) {
                (Ok(Ok(())), Some(attempt)) => self.settle(attempt, store),
                (Ok(Ok(())), None) if store.purge_pending() => self.next = Some(Instant::now()),
                (Ok(Ok(())), None) => self.settle(Attempt::Done, store),
                (Ok(Err(error)), _) => {
                    tracing::warn!(%error, "a purge file couldn't be removed; it's retried later");
                    self.settle(Attempt::Failed, store);
                }
                (Err(_), _) => {
                    tracing::error!("the purge's helper panicked");
                    self.settle(Attempt::Failed, store);
                }
            },
        }
    }

    /// Removes what an earlier attempt left, or else copies the store if a purge is owed, no
    /// run is active and the volume has room.
    fn start(&mut self, store: &mut Store) {
        if let Err(error) = store.reconnect() {
            return self.settle(failed(&error), store);
        }
        if store.leftovers().exist() {
            return self.remove(store, None);
        }
        let owed = match store.owed() {
            Ok(Some(owed)) => owed,
            // The swap went through but a failed reopen kept it from settling.
            Ok(None) if !store.purge_pending() => return self.settle(Attempt::Done, store),
            Ok(None) => return self.settle(Attempt::Busy, store),
            Err(error) => return self.settle(failed(&error), store),
        };
        match (self.schedule.free_space)(owed.folder()) {
            Ok(free) if free >= owed.needs().saturating_add(RESERVE) => {}
            Ok(_) => return self.settle(Attempt::NoRoom, store),
            Err(error) => return self.settle(failed(&StoreError::Purge(error)), store),
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        match thread::Builder::new()
            .name("purge".to_owned())
            .spawn(move || owed.copy(&stopping))
        {
            Ok(helper) => self.job = Some(Job::Copy { stop, helper }),
            Err(error) => {
                tracing::error!(%error, "the purge's helper didn't start");
                self.settle(Attempt::Failed, store);
            }
        }
    }

    fn remove(&mut self, store: &Store, then: Option<Attempt>) {
        let leftovers = store.leftovers();
        match thread::Builder::new()
            .name("purge".to_owned())
            .spawn(move || leftovers.remove())
        {
            Ok(helper) => self.job = Some(Job::Remove { helper, then }),
            Err(error) => {
                tracing::error!(%error, "the purge's helper didn't start");
                self.settle(Attempt::Failed, store);
            }
        }
    }

    fn settle(&mut self, attempt: Attempt, store: &Store) {
        let now = Instant::now();
        self.next = match attempt {
            // A deletion while the files were removed owes another purge.
            Attempt::Done => store.purge_pending().then(|| now + self.schedule.quiet),
            Attempt::Busy => None,
            Attempt::Stale | Attempt::Blocked | Attempt::NoRoom | Attempt::Failed => {
                let at = now + self.retry;
                self.retry = (self.retry * 2).min(self.schedule.last_retry);
                Some(at)
            }
        };
        if attempt == Attempt::Done {
            self.retry = self.schedule.first_retry;
        }
        #[cfg(test)]
        if let Some(attempts) = &self.schedule.attempts {
            attempts.send(attempt).ok();
        }
    }

    /// Tells a running copy to stop, and hands back the job to wait for.
    fn stop(self) -> Option<Job> {
        self.op_arriving();
        self.job
    }
}

fn failed(error: &StoreError) -> Attempt {
    tracing::error!(%error, "the purge failed; it's retried later");
    Attempt::Failed
}

#[derive(Debug)]
pub(crate) struct StoreThread {
    ops: mpsc::Sender<StoreOp>,
    /// Fires when the thread has run every queued op and closed the store.
    done: oneshot::Receiver<()>,
}

impl StoreThread {
    pub(crate) fn spawn(
        mut store: Store,
        acks: UnboundedSender<Input>,
        schedule: Schedule,
    ) -> std::io::Result<Self> {
        let (ops_tx, ops) = mpsc::channel();
        let (finished, done) = oneshot::channel();
        thread::Builder::new()
            .name("store".to_owned())
            .spawn(move || {
                // Every queued op runs, even with nobody left to hear the acknowledgement.
                // shortcut: in a debug build a panic here leaves queued ops unanswered, so their runs
                // never leave the coordinator and new turns stop once 16 pile up; release builds
                // abort on panic instead. Restart the brain on a store panic once it has a supervisor.
                let mut purger = Purger::new(schedule, &store);
                loop {
                    let op = match purger.timeout() {
                        None => ops.recv().map_err(|_| RecvTimeoutError::Disconnected),
                        Some(timeout) => ops.recv_timeout(timeout),
                    };
                    match op {
                        Ok(op) => {
                            purger.op_arriving();
                            if acks.send(apply(&mut store, op)).is_err() {
                                tracing::debug!("an acknowledgement had no listener");
                            }
                            purger.op_arrived(&store);
                        }
                        Err(RecvTimeoutError::Timeout) => purger.tick(&mut store),
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                let job = purger.stop();
                drop(store);
                if finished.send(()).is_err() {
                    tracing::debug!("nobody waited for the store to close");
                }
                // After the store closed: a copy's sync and a removal can't be stopped, and close
                // needn't wait for them. Files the exit cuts short are removed at the next start.
                if job.is_some_and(|job| !job.join()) {
                    tracing::error!("the purge's helper panicked");
                }
            })?;
        Ok(Self { ops: ops_tx, done })
    }

    /// Queues an operation. False when the thread has gone, which only a panic there causes.
    pub(crate) fn send(&self, op: StoreOp) -> bool {
        self.ops.send(op).is_ok()
    }

    /// Lets the queued operations finish, then closes the store; a running copy stops, and the
    /// purge stays owed. The receiver fires when it has, and fails if the thread panicked;
    /// nothing blocks on the thread itself.
    pub(crate) fn close(self) -> oneshot::Receiver<()> {
        drop(self.ops);
        self.done
    }
}

/// Runs one operation in one transaction and answers it. Errors are logged by kind and run id
/// only, never content.
fn apply(store: &mut Store, op: StoreOp) -> Input {
    let run = op.run();
    // After a swap's reopen failed: fails closed until the file opens again.
    if let Err(error) = store.reconnect() {
        tracing::warn!(%error, run = run.0, "the store is unavailable");
        return Input::StoreFailed { run };
    }
    let (result, stored, what) = match op {
        StoreOp::BeginTurn {
            thread,
            title,
            run,
            text,
        } => (
            store.begin_turn(thread, title.as_deref(), run, &text),
            Input::TurnStored { run },
            "turn",
        ),
        StoreOp::CommitStep {
            thread,
            run,
            text,
            index,
            step,
        } => {
            let body = serde_json::json!({ "index": index, "label": step.label }).to_string();
            let mut events = Vec::with_capacity(2);
            if let Some(text) = &text {
                events.push(NewEvent {
                    kind: EventKind::AssistantText,
                    text: Some(text.as_str()),
                    body: None,
                });
            }
            events.push(NewEvent {
                kind: EventKind::SceneStep,
                text: Some(step.label.as_str()),
                body: Some(body.as_str()),
            });
            (
                store.append(thread, run, &events),
                Input::StepStored { run, index, step },
                "step",
            )
        }
        StoreOp::EndRun {
            thread,
            run,
            text,
            revealed,
            end,
        } => {
            let revealed = revealed.map(|steps| serde_json::json!(steps).to_string());
            let mut events = Vec::with_capacity(2);
            if let Some(text) = &text {
                events.push(NewEvent {
                    kind: EventKind::AssistantText,
                    text: Some(text.as_str()),
                    body: None,
                });
            }
            if let Some(revealed) = &revealed {
                events.push(NewEvent {
                    kind: EventKind::Revealed,
                    text: None,
                    body: Some(revealed.as_str()),
                });
            }
            (
                store.end_run(thread, run, &events, end),
                Input::RunStored { run },
                "outcome",
            )
        }
    };
    match result {
        Ok(()) => stored,
        Err(error) => {
            tracing::warn!(%error, run = run.0, what, "a store operation failed");
            Input::StoreFailed { run }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc as std_mpsc;
    use std::time::{Duration, Instant};

    use capsula_protocol::message::{RunId, Step, ThreadId};
    use rusqlite::Connection;
    use tokio::sync::mpsc;

    use super::{Attempt, Purger, Schedule, StoreThread};
    use crate::coordinator::{Input, StoreOp};
    use crate::store::{RunEnd, Store, Swap};
    use crate::test_support::Scratch;

    /// A store owing the purge of its deleted thread 1.
    fn owing(scratch: &Scratch) -> Store {
        let mut store = Store::open(&scratch.db()).expect("store");
        store
            .begin_turn(ThreadId(1), Some("t"), RunId(1), "doomed")
            .expect("turn");
        store.begin_delete(ThreadId(1)).expect("begin");
        store.finish_delete(ThreadId(1)).expect("finish");
        store
    }

    /// The store's thread with short waits, and every purge attempt it makes.
    fn spawn(store: Store) -> (StoreThread, std_mpsc::Receiver<Attempt>) {
        spawn_with_room(store, |_| Ok(u64::MAX))
    }

    fn spawn_with_room(
        store: Store,
        free_space: fn(&Path) -> std::io::Result<u64>,
    ) -> (StoreThread, std_mpsc::Receiver<Attempt>) {
        let (acks, _replies) = mpsc::unbounded_channel();
        let (heard, attempts) = std_mpsc::channel();
        let schedule = Schedule {
            quiet: Duration::from_millis(1),
            first_retry: Duration::from_millis(20),
            last_retry: Duration::from_millis(20),
            free_space,
            attempts: Some(heard),
        };
        let thread = StoreThread::spawn(store, acks, schedule).expect("thread");
        (thread, attempts)
    }

    fn next(attempts: &std_mpsc::Receiver<Attempt>) -> Attempt {
        attempts
            .recv_timeout(Duration::from_secs(10))
            .expect("the store's thread tried the purge")
    }

    /// Waits out repeats a retry already under way can add, then expects the purge to finish.
    fn done_after(attempts: &std_mpsc::Receiver<Attempt>, repeats: &[Attempt]) {
        let mut next_one = next(attempts);
        while repeats.contains(&next_one) {
            next_one = next(attempts);
        }
        assert_eq!(next_one, Attempt::Done);
    }

    fn owed(scratch: &Scratch) -> i64 {
        Connection::open(scratch.db())
            .expect("watcher")
            .query_row("SELECT count(*) FROM purge_pending", [], |row| row.get(0))
            .expect("count")
    }

    #[test]
    fn an_owed_purge_waits_for_the_active_run_and_then_runs_in_session() {
        let scratch = Scratch::new("idle-purge");
        let mut store = owing(&scratch);
        store
            .begin_turn(ThreadId(2), Some("kept"), RunId(2), "busy")
            .expect("an active run");
        let (thread, attempts) = spawn(store);
        assert_eq!(next(&attempts), Attempt::Busy);
        assert_eq!(owed(&scratch), 1);
        assert!(thread.send(StoreOp::EndRun {
            thread: ThreadId(2),
            run: RunId(2),
            text: None,
            revealed: None,
            end: RunEnd::Done,
        }));
        assert_eq!(next(&attempts), Attempt::Done);
        thread.close().blocking_recv().expect("closed");
        assert_eq!(owed(&scratch), 0);
    }

    #[test]
    fn a_purge_a_reader_blocked_is_retried_in_session() {
        let scratch = Scratch::new("blocked-purge");
        let mut store = owing(&scratch);
        let reader = Connection::open(scratch.db()).expect("reader");
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM events;")
            .expect("snapshot");
        // A write past the reader's snapshot, so the WAL can't be emptied under it.
        store
            .begin_turn(ThreadId(2), Some("later"), RunId(2), "later")
            .expect("turn");
        store
            .end_run(ThreadId(2), RunId(2), &[], RunEnd::Done)
            .expect("ended");
        let (thread, attempts) = spawn(store);
        assert_eq!(next(&attempts), Attempt::Blocked);
        drop(reader);
        // A retry racing the reader's close can find its file still held.
        done_after(&attempts, &[Attempt::Blocked, Attempt::Failed]);
        thread.close().blocking_recv().expect("closed");
        assert_eq!(owed(&scratch), 0);
    }

    #[test]
    fn a_failed_purge_is_retried_in_session() {
        let scratch = Scratch::new("failed-purge");
        let store = owing(&scratch);
        // The copy carries the trigger, so clearing its owed row fails.
        store
            .conn_for_tests()
            .execute_batch(
                "CREATE TRIGGER keep_owed BEFORE DELETE ON purge_pending BEGIN
                     SELECT RAISE(ABORT, 'kept');
                 END;",
            )
            .expect("trigger");
        let (thread, attempts) = spawn(store);
        assert_eq!(next(&attempts), Attempt::Failed);
        Connection::open(scratch.db())
            .expect("fixer")
            .execute_batch("DROP TRIGGER keep_owed")
            .expect("drop");
        done_after(&attempts, &[Attempt::Failed]);
        thread.close().blocking_recv().expect("closed");
        assert_eq!(owed(&scratch), 0);
    }

    /// `db` with `suffix` appended.
    fn beside(db: &Path, suffix: &str) -> PathBuf {
        let mut path = db.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path)
    }

    /// The purge's files left in the store's folder.
    fn purge_files(scratch: &Scratch) -> Vec<PathBuf> {
        std::fs::read_dir(scratch.db().parent().expect("a folder"))
            .expect("folder")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.to_string_lossy().contains(".purge-"))
            .collect()
    }

    /// A store whose swap finished, with the old store still beside it, as a crash right after
    /// the swap leaves it.
    fn swapped_with_old_left(scratch: &Scratch) -> Store {
        let mut store = owing(scratch);
        let copied = store
            .owed()
            .expect("owed")
            .expect("idle")
            .copy(&Arc::new(AtomicBool::new(false)))
            .expect("copy")
            .expect("not stopped");
        assert_eq!(store.swap(&copied).expect("swap"), Swap::Done);
        assert_eq!(
            purge_files(scratch),
            vec![beside(&scratch.db(), ".purge-old")]
        );
        store
    }

    /// Opens `path` so it can't be deleted until the handle drops.
    fn hold(path: &Path) -> File {
        const FILE_SHARE_READ: u32 = 1;
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
            .expect("held")
    }

    #[test]
    fn an_old_store_left_by_a_crash_is_removed_at_start() {
        let scratch = Scratch::new("old-left");
        drop(swapped_with_old_left(&scratch));
        let store = Store::open(&scratch.db()).expect("reopen");
        assert!(
            !store.purge_pending(),
            "the swap finished; only the file is owed"
        );
        let (thread, attempts) = spawn(store);
        assert_eq!(next(&attempts), Attempt::Done);
        thread.close().blocking_recv().expect("closed");
        assert_eq!(purge_files(&scratch), Vec::<PathBuf>::new());
    }

    #[test]
    fn an_old_store_that_cant_be_removed_is_retried_in_session_and_at_the_next_start() {
        let scratch = Scratch::new("old-held");
        let store = swapped_with_old_left(&scratch);
        let holder = hold(&beside(&scratch.db(), ".purge-old"));
        let (thread, attempts) = spawn(store);
        assert_eq!(next(&attempts), Attempt::Failed);
        assert_eq!(next(&attempts), Attempt::Failed, "retried in session");
        thread.close().blocking_recv().expect("closed");
        assert_eq!(
            purge_files(&scratch),
            vec![beside(&scratch.db(), ".purge-old")]
        );

        // The next start owes the removal even though no purge row is left.
        let store = Store::open(&scratch.db()).expect("reopen");
        assert!(!store.purge_pending());
        let (thread, attempts) = spawn(store);
        assert_eq!(next(&attempts), Attempt::Failed);
        drop(holder);
        done_after(&attempts, &[Attempt::Failed]);
        thread.close().blocking_recv().expect("closed");
        assert_eq!(purge_files(&scratch), Vec::<PathBuf>::new());
    }

    #[test]
    fn a_crash_right_after_the_link_keeps_the_store_and_the_purge_finishes() {
        let scratch = Scratch::new("crash-linked");
        let store = owing(&scratch);
        let copied = store
            .owed()
            .expect("owed")
            .expect("idle")
            .copy(&Arc::new(AtomicBool::new(false)))
            .expect("copy")
            .expect("not stopped");
        drop((copied, store));
        // The brain dies between the link and the move: two names for the old store.
        std::fs::hard_link(scratch.db(), beside(&scratch.db(), ".purge-old")).expect("linked");

        let store = Store::open(&scratch.db()).expect("reopen");
        assert!(store.purge_pending());
        let (thread, attempts) = spawn(store);
        assert_eq!(next(&attempts), Attempt::Done);
        thread.close().blocking_recv().expect("closed");
        assert_eq!(purge_files(&scratch), Vec::<PathBuf>::new());
        assert_eq!(owed(&scratch), 0);
    }

    static ROOM: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn a_copy_waits_until_the_volume_has_room_for_it_and_the_foreground() {
        let scratch = Scratch::new("no-room");
        let store = owing(&scratch);
        let bytes: i64 = store
            .conn_for_tests()
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |row| row.get(0),
            )
            .expect("size");
        let needs = 2 * u64::try_from(bytes).expect("a size") + super::RESERVE;
        ROOM.store(needs - 1, Ordering::SeqCst);
        let (thread, attempts) = spawn_with_room(store, |_| Ok(ROOM.load(Ordering::SeqCst)));
        assert_eq!(next(&attempts), Attempt::NoRoom);
        assert_eq!(
            purge_files(&scratch),
            Vec::<PathBuf>::new(),
            "nothing was written"
        );
        assert_eq!(owed(&scratch), 1);
        ROOM.store(needs, Ordering::SeqCst);
        done_after(&attempts, &[Attempt::NoRoom]);
        thread.close().blocking_recv().expect("closed");
        assert_eq!(owed(&scratch), 0);
    }

    #[test]
    fn a_purge_found_finished_resets_the_backoff() {
        let scratch = Scratch::new("found-done");
        let mut store = Store::open(&scratch.db()).expect("store");
        let schedule = Schedule {
            first_retry: Duration::from_secs(60),
            last_retry: Duration::from_secs(240),
            ..Schedule::default()
        };
        let mut purger = Purger::new(schedule, &store);
        purger.retry = Duration::from_secs(240);
        purger.start(&mut store);
        assert_eq!(purger.retry, Duration::from_secs(60));
        assert!(purger.next.is_none());
    }

    #[test]
    fn copies_made_stale_back_off_until_one_finishes() {
        let scratch = Scratch::new("stale-backoff");
        let store = owing(&scratch);
        let schedule = Schedule {
            first_retry: Duration::from_secs(60),
            last_retry: Duration::from_secs(240),
            ..Schedule::default()
        };
        let mut purger = Purger::new(schedule, &store);
        for wait in [60, 120, 240, 240] {
            let before = Instant::now();
            purger.settle(Attempt::Stale, &store);
            let at = purger.next.expect("a retry");
            let wait = Duration::from_secs(wait);
            assert!(at >= before + wait && at <= Instant::now() + wait);
        }
        purger.settle(Attempt::Done, &store);
        assert_eq!(purger.retry, Duration::from_secs(60));
    }

    #[test]
    fn every_operation_gets_one_reply_in_order() {
        let scratch = Scratch::new("replies");
        let store = Store::open(&scratch.db()).expect("store");
        let (thread, run) = (ThreadId(1), RunId(1));
        let (acks, mut replies) = mpsc::unbounded_channel();
        let store = StoreThread::spawn(store, acks, Schedule::default()).expect("thread");
        for op in [
            StoreOp::BeginTurn {
                thread,
                title: Some("t".to_owned()),
                run,
                text: "hi".to_owned(),
            },
            StoreOp::CommitStep {
                thread: ThreadId(2),
                run,
                text: None,
                index: 0,
                step: Step {
                    label: "s".to_owned(),
                },
            },
            StoreOp::EndRun {
                thread,
                run,
                text: None,
                revealed: None,
                end: RunEnd::Done,
            },
        ] {
            assert!(store.send(op));
        }
        store.close().blocking_recv().expect("closed");
        let mut got = Vec::new();
        while let Ok(reply) = replies.try_recv() {
            got.push(reply);
        }
        assert_eq!(
            got,
            [
                Input::TurnStored { run },
                Input::StoreFailed { run },
                Input::RunStored { run },
            ]
        );
    }
}
