//! File logging through `tracing`, to `%LOCALAPPDATA%\\<product>\\logs`, one file a day for a week.
//!
//! Content (prompts, transcripts, screen text, images, tool arguments) is never logged, so nothing
//! needs redacting after the fact. Secrets are [`kumochi_protocol::Secret`]s and print as `***`.

use tracing::Level;
use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};

#[expect(
    dead_code,
    reason = "dormant owner awaits coordinated startup and Quit integration"
)]
pub(crate) mod owner {
    use std::cell::Cell;
    use std::fmt;
    use std::io::{self, Write};
    use std::panic::Location;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
    use std::thread::{self, JoinHandle};

    use time::{Date, OffsetDateTime};
    use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter, format};
    use tracing_subscriber::registry::LookupSpan;

    use crate::platform::logging::{Error, Log, Retention, Store};

    /// Proposed queued-copy bounds, not measured responsiveness or formatting-allocation bounds.
    const QUEUE_RECORDS: usize = 64;
    const RECORD_BYTES: usize = 4096;

    #[derive(Debug, Clone, Copy)]
    pub(crate) enum Count {
        Submitted,
        Written,
        Full,
        Contended,
        Oversize,
        Closed,
        Refused,
        Disconnected,
        Poisoned,
        Discarded,
        Abandoned,
        Format,
        Write,
        Flush,
        Open,
        Prune,
        PruneSkipped,
        PruneIncomplete,
        PanicOmitted,
        WorkerPanic,
    }

    impl Count {
        #[expect(clippy::as_conversions, reason = "fieldless counter indices fit usize")]
        const fn index(self) -> usize {
            self as usize
        }
    }
    const COUNTS: usize = Count::WorkerPanic.index() + 1;

    #[derive(Debug, Default)]
    struct Counts([AtomicUsize; COUNTS]);
    impl Counts {
        fn add(&self, kind: Count, n: usize) {
            let _previous =
                self.0[kind.index()].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                    Some(old.saturating_add(n))
                });
        }
        fn snapshot(&self) -> Statistics {
            Statistics(std::array::from_fn(|i| self.0[i].load(Ordering::Relaxed)))
        }
    }

    #[derive(Debug)]
    pub(crate) struct Statistics([usize; COUNTS]);
    impl Statistics {
        pub(crate) fn get(&self, kind: Count) -> usize {
            self.0[kind.index()]
        }
    }

    struct Record {
        bytes: Vec<u8>,
        counts: Arc<Counts>,
        handled: bool,
    }
    impl Record {
        fn finish(&mut self, kind: Count) {
            self.counts.add(kind, 1);
            self.handled = true;
        }
    }
    impl Drop for Record {
        fn drop(&mut self) {
            if !self.handled {
                self.counts.add(Count::Abandoned, 1);
            }
        }
    }
    impl fmt::Debug for Record {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Record")
                .field("bytes", &self.bytes.len())
                .field("handled", &self.handled)
                .finish()
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum State {
        Open,
        SecurityRefused,
        Closing,
        Closed,
    }
    struct Current<L> {
        state: State,
        log: Option<Arc<L>>,
    }
    struct WriterCell<L> {
        ingress: Mutex<Option<SyncSender<Record>>>,
        current: Mutex<Current<L>>,
        stop: AtomicBool,
        refused: AtomicBool,
        counts: Arc<Counts>,
    }
    impl<L> WriterCell<L> {
        fn current(&self) -> Option<MutexGuard<'_, Current<L>>> {
            match self.current.lock() {
                Ok(current) => Some(current),
                Err(_) => {
                    self.counts.add(Count::Poisoned, 1);
                    self.stop.store(true, Ordering::Release);
                    None
                }
            }
        }
        fn failure(&self, error: Error, ordinary: Count) {
            if error != Error::SecurityRefused {
                self.counts.add(ordinary, 1);
                return;
            }
            self.refused.store(true, Ordering::Release);
            if let Some(mut current) = self.current() {
                if current.state == State::Open && !self.stop.load(Ordering::Acquire) {
                    current.state = State::SecurityRefused;
                }
            }
        }
        fn retention(&self, result: Result<Retention, Error>) {
            match result {
                Ok(r) => {
                    self.counts.add(Count::PruneSkipped, r.skipped);
                    self.counts
                        .add(Count::PruneIncomplete, usize::from(r.incomplete));
                }
                Err(error) => self.failure(error, Count::Prune),
            }
        }
    }

    pub(crate) struct Writer<L>(Arc<WriterCell<L>>);
    impl<L> Clone for Writer<L> {
        fn clone(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
    }
    impl<L> Writer<L> {
        pub(crate) fn stop(&self) {
            self.0.stop.store(true, Ordering::Release);
        }
        pub(crate) fn statistics(&self) -> Statistics {
            self.0.counts.snapshot()
        }
        fn submit(&self, bytes: &[u8]) {
            let cell = &self.0;
            let loss = if cell.stop.load(Ordering::Acquire) {
                Some(Count::Closed)
            } else if cell.refused.load(Ordering::Acquire) {
                Some(Count::Refused)
            } else if bytes.len() > RECORD_BYTES {
                Some(Count::Oversize)
            } else {
                None
            };
            if let Some(loss) = loss {
                cell.counts.add(loss, 1);
                return;
            }
            let sender = match cell.ingress.try_lock() {
                Ok(sender) => sender,
                Err(TryLockError::WouldBlock) => {
                    cell.counts.add(Count::Contended, 1);
                    return;
                }
                Err(TryLockError::Poisoned(_)) => {
                    cell.counts.add(Count::Poisoned, 1);
                    self.stop();
                    return;
                }
            };
            if cell.stop.load(Ordering::Acquire) {
                cell.counts.add(Count::Closed, 1);
                return;
            }
            let Some(sender) = sender.as_ref() else {
                cell.counts.add(Count::Closed, 1);
                return;
            };
            let record = Record {
                bytes: bytes.to_vec(),
                counts: Arc::clone(&cell.counts),
                handled: false,
            };
            match sender.try_send(record) {
                Ok(()) => cell.counts.add(Count::Submitted, 1),
                Err(TrySendError::Full(mut record)) => record.finish(Count::Full),
                Err(TrySendError::Disconnected(mut record)) => record.finish(Count::Disconnected),
            }
        }
    }
    impl<L> Write for &Writer<L> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.submit(bytes);
            Ok(bytes.len())
        }
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.submit(bytes);
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl<'a, L: 'a> MakeWriter<'a> for Writer<L> {
        type Writer = &'a Self;
        fn make_writer(&'a self) -> Self::Writer {
            self
        }
    }

    thread_local! { static IN_SINK: Cell<bool> = const { Cell::new(false) }; }
    struct Entered;
    impl Entered {
        fn enter() -> Option<Self> {
            IN_SINK.with(|entered| {
                if entered.replace(true) {
                    None
                } else {
                    Some(Self)
                }
            })
        }
    }
    impl Drop for Entered {
        fn drop(&mut self) {
            IN_SINK.with(|entered| entered.set(false));
        }
    }

    impl<L: Log> Writer<L> {
        fn panic_at(&self, at: Option<&Location<'_>>) {
            let cell = &self.0;
            let Some(_entered) = Entered::enter() else {
                cell.counts.add(Count::PanicOmitted, 1);
                return;
            };
            if cell.stop.load(Ordering::Acquire) || cell.refused.load(Ordering::Acquire) {
                cell.counts.add(Count::PanicOmitted, 1);
                return;
            }
            let Some(current) = cell.current() else {
                return;
            };
            if current.state != State::Open
                || cell.stop.load(Ordering::Acquire)
                || cell.refused.load(Ordering::Acquire)
            {
                cell.counts.add(Count::PanicOmitted, 1);
                return;
            }
            if let Some(log) = &current.log {
                let location = at.map_or_else(
                    || "unknown".to_owned(),
                    |at| format!("{}:{}", at.file(), at.line()),
                );
                if log
                    .append(format!("panic location={location}\n").as_bytes())
                    .is_err()
                {
                    cell.counts.add(Count::Write, 1);
                }
            } else {
                cell.counts.add(Count::PanicOmitted, 1);
            }
        }
    }

    struct CountFormat<F> {
        inner: F,
        counts: Arc<Counts>,
    }
    impl<S, N, F> FormatEvent<S, N> for CountFormat<F>
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
        N: for<'a> FormatFields<'a> + 'static,
        F: FormatEvent<S, N>,
    {
        fn format_event(
            &self,
            cx: &FmtContext<'_, S, N>,
            writer: format::Writer<'_>,
            event: &tracing::Event<'_>,
        ) -> fmt::Result {
            let result = self.inner.format_event(cx, writer, event);
            if result.is_err() {
                self.counts.add(Count::Format, 1);
            }
            result
        }
    }

    #[derive(Debug, thiserror::Error)]
    pub(crate) enum StartError {
        #[error("initial log acquisition failed: {0}")]
        Storage(#[from] Error),
        #[error("log worker creation failed")]
        Spawn(io::ErrorKind),
        #[error("logging was already started")]
        Subscriber,
    }

    /// Blocking cleanup owner. Keep it pre-window or in retained off-UI cleanup, never a UI entity.
    /// Unwind can join; release panic=abort invokes the hook then exits without cleanup.
    #[must_use = "retain the worker owner until off-UI close completes"]
    pub(crate) struct Logging<B: Store> {
        worker: Option<JoinHandle<B>>,
        writer: Writer<B::Log>,
    }

    impl<B: Store> Logging<B> {
        pub(crate) fn start(store: B) -> Result<Self, StartError> {
            Self::spawn_with(store, OffsetDateTime::now_utc().date(), |work| {
                thread::Builder::new()
                    .name("shell-log".to_owned())
                    .spawn(work)
            })
        }
        fn spawn_with(
            mut store: B,
            day: Date,
            spawn: impl FnOnce(Box<dyn FnOnce() -> B + Send>) -> io::Result<JoinHandle<B>>,
        ) -> Result<Self, StartError> {
            let _entered = Entered::enter();
            let log = store.open(day)?;
            let counts = Arc::new(Counts::default());
            let retention = store.retain(&log);
            if retention
                .as_ref()
                .is_err_and(|e| *e == Error::SecurityRefused)
            {
                return Err(Error::SecurityRefused.into());
            }
            let (sender, receiver) = mpsc::sync_channel(QUEUE_RECORDS);
            let cell = Arc::new(WriterCell {
                ingress: Mutex::new(Some(sender)),
                current: Mutex::new(Current {
                    state: State::Open,
                    log: Some(Arc::new(log)),
                }),
                stop: AtomicBool::new(false),
                refused: AtomicBool::new(false),
                counts,
            });
            cell.retention(retention);
            let worker_cell = Arc::clone(&cell);
            let worker = spawn(Box::new(move || work(store, receiver, &worker_cell, day)))
                .map_err(|error| StartError::Spawn(error.kind()))?;
            Ok(Self {
                worker: Some(worker),
                writer: Writer(cell),
            })
        }
        pub(crate) fn writer(&self) -> Writer<B::Log> {
            self.writer.clone()
        }
        pub(crate) fn install(self) -> Result<Self, StartError>
        where
            B::Log: 'static,
        {
            let owner = self.publish(|dispatch| {
                tracing::dispatcher::set_global_default(dispatch)
                    .map_err(|_| StartError::Subscriber)
            })?;
            let writer = owner.writer();
            std::panic::set_hook(Box::new(move |info| writer.panic_at(info.location())));
            Ok(owner)
        }
        fn publish(
            self,
            install: impl FnOnce(tracing::Dispatch) -> Result<(), StartError>,
        ) -> Result<Self, StartError>
        where
            B::Log: 'static,
        {
            let subscriber = tracing_subscriber::fmt()
                .event_format(CountFormat {
                    inner: format::format(),
                    counts: Arc::clone(&self.writer.0.counts),
                })
                .with_writer(self.writer())
                .with_max_level(if cfg!(debug_assertions) {
                    tracing::Level::DEBUG
                } else {
                    tracing::Level::INFO
                })
                .log_internal_errors(false)
                .finish();
            if let Err(error) = install(tracing::Dispatch::new(subscriber)) {
                self.close();
                return Err(error);
            }
            Ok(self)
        }
        pub(crate) fn close(mut self) -> Statistics {
            self.retire();
            self.writer.statistics()
        }
        fn retire(&mut self) {
            let Some(worker) = self.worker.take() else {
                return;
            };
            let _entered = Entered::enter();
            self.writer.stop();
            let cell = &self.writer.0;
            cell.current
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .state = State::Closing;
            drop(
                cell.ingress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take(),
            );
            let backend = match worker.join() {
                Ok(backend) => Some(backend),
                Err(_) => {
                    cell.counts.add(Count::WorkerPanic, 1);
                    None
                }
            };
            let log = cell
                .current
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .log
                .take();
            if let Some(log) = log {
                if !cell.refused.load(Ordering::Acquire)
                    && !cell.current.is_poisoned()
                    && !cell.ingress.is_poisoned()
                    && log.flush().is_err()
                {
                    cell.counts.add(Count::Flush, 1);
                }
                drop(log);
            }
            drop(backend);
            cell.current
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .state = State::Closed;
        }
    }
    impl<B: Store> Drop for Logging<B> {
        fn drop(&mut self) {
            self.retire();
        }
    }

    fn rollover<B: Store>(
        store: &mut B,
        cell: &WriterCell<B::Log>,
        high_water: &mut Date,
        day: Date,
    ) {
        let _entered = Entered::enter();
        if day <= *high_water
            || cell.stop.load(Ordering::Acquire)
            || cell.refused.load(Ordering::Acquire)
        {
            return;
        }
        *high_water = day;
        let candidate = match store.open(day) {
            Ok(log) => Arc::new(log),
            Err(error) => {
                cell.failure(error, Count::Open);
                return;
            }
        };
        let old = {
            let Some(mut current) = cell.current() else {
                return;
            };
            if current.state != State::Open
                || cell.stop.load(Ordering::Acquire)
                || cell.refused.load(Ordering::Acquire)
            {
                return;
            }
            current.log.replace(Arc::clone(&candidate))
        };
        if let Some(old) = old {
            if !cell.stop.load(Ordering::Acquire)
                && !cell.refused.load(Ordering::Acquire)
                && old.flush().is_err()
            {
                cell.counts.add(Count::Flush, 1);
            }
            drop(old);
        }
        if !cell.stop.load(Ordering::Acquire) {
            cell.retention(store.retain(&candidate));
        }
    }
    fn work<B: Store>(
        mut store: B,
        receiver: Receiver<Record>,
        cell: &WriterCell<B::Log>,
        mut day: Date,
    ) -> B {
        while let Ok(mut record) = receiver.recv() {
            let _entered = Entered::enter();
            if cell.stop.load(Ordering::Acquire) {
                record.finish(Count::Discarded);
                continue;
            }
            rollover(&mut store, cell, &mut day, OffsetDateTime::now_utc().date());
            let Some(current) = cell.current() else {
                record.handled = true;
                continue;
            };
            let outcome = match current.state {
                State::Open if cell.refused.load(Ordering::Acquire) => Count::Refused,
                State::Open if !cell.stop.load(Ordering::Acquire) => {
                    if current
                        .log
                        .as_ref()
                        .is_some_and(|log| log.append(&record.bytes).is_ok())
                    {
                        Count::Written
                    } else {
                        Count::Write
                    }
                }
                State::SecurityRefused => Count::Refused,
                _ => Count::Discarded,
            };
            record.finish(outcome);
        }
        store
    }

    #[cfg(test)]
    mod tests {
        include!("logging/tests.rs");
    }
}

/// Daily files kept before the oldest is deleted.
const KEEP_DAYS: usize = 7;

/// Why logging couldn't start.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LoggingError {
    /// The per-user data folder isn't known.
    #[error("%LOCALAPPDATA% is not set to an absolute path")]
    NoLocalAppData,
    /// The log folder or file couldn't be created.
    #[error("can't open the log file")]
    Appender(#[from] InitError),
    /// Something already installed a global subscriber.
    #[error("logging was already started")]
    AlreadyStarted(#[from] tracing::subscriber::SetGlobalDefaultError),
}

/// Starts logging to files named `<prefix>.<date>.log` and records panics there.
///
/// Writes are synchronous: there are a few lines per state change and none per frame, and a
/// panic message has to reach the file before a release build aborts.
///
/// # Errors
/// Fails when the log folder can't be found or created, or logging already started.
pub(crate) fn init(prefix: &str) -> Result<(), LoggingError> {
    let dir = crate::platform::data_dir()
        .ok_or(LoggingError::NoLocalAppData)?
        .join("logs");
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .filename_suffix("log")
        .max_log_files(KEEP_DAYS)
        .build(dir)?;
    let level = if cfg!(debug_assertions) {
        Level::DEBUG
    } else {
        Level::INFO
    };
    let subscriber = tracing_subscriber::fmt()
        .with_writer(appender)
        .with_max_level(level)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;
    // The payload can hold any value an assertion printed, so only the location is logged.
    std::panic::set_hook(Box::new(|info| {
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()));
        tracing::error!(location = at.as_deref().unwrap_or("unknown"), "panic");
    }));
    Ok(())
}
