use super::*;
use std::sync::mpsc::{Sender, TryRecvError};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);

struct Pause {
    entered: Sender<()>,
    release: Receiver<()>,
}
fn pause() -> (Pause, Receiver<()>, Sender<()>) {
    let (entered, arrived) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    (
        Pause {
            entered,
            release: resume,
        },
        arrived,
        release,
    )
}
impl Pause {
    fn wait(self) {
        self.entered
            .send(())
            .expect("fixture has a waiting observer");
        self.release
            .recv_timeout(WAIT)
            .expect("fixture resumes entered work");
    }
}

#[derive(Default)]
struct Fixture {
    files: AtomicUsize,
    namespaces: AtomicUsize,
    retired: Mutex<Vec<&'static str>>,
    generations: AtomicUsize,
    opens: Mutex<Vec<Date>>,
    bytes: Mutex<Vec<u8>>,
    open_error: Mutex<Option<Error>>,
    retention_error: Mutex<Option<Error>>,
    open_pause: Mutex<Option<Pause>>,
    write_pause: Mutex<Option<Pause>>,
    partial: AtomicBool,
    panic_write: AtomicBool,
    flush_error: AtomicBool,
    flushes: AtomicUsize,
    skipped: AtomicUsize,
}
struct TestNamespace(Arc<Fixture>);
struct TestStore(Arc<Fixture>, Arc<TestNamespace>);
struct TestLog {
    fixture: Arc<Fixture>,
    generation: usize,
    _namespace: Arc<TestNamespace>,
}
impl TestStore {
    fn new(fixture: &Arc<Fixture>) -> Self {
        fixture.namespaces.fetch_add(1, Ordering::SeqCst);
        Self(
            Arc::clone(fixture),
            Arc::new(TestNamespace(Arc::clone(fixture))),
        )
    }
}
impl Drop for TestNamespace {
    fn drop(&mut self) {
        self.0.namespaces.fetch_sub(1, Ordering::SeqCst);
        self.0.retired.lock().unwrap().push("namespace");
    }
}
impl Drop for TestLog {
    fn drop(&mut self) {
        self.fixture.files.fetch_sub(1, Ordering::SeqCst);
        self.fixture.retired.lock().unwrap().push("file");
    }
}
impl Store for TestStore {
    type Log = TestLog;
    fn open(&mut self, day: Date) -> Result<TestLog, Error> {
        self.0.opens.lock().unwrap().push(day);
        let paused = self.0.open_pause.lock().unwrap().take();
        if let Some(paused) = paused {
            paused.wait();
        }
        if let Some(error) = self.0.open_error.lock().unwrap().take() {
            return Err(error);
        }
        self.0.files.fetch_add(1, Ordering::SeqCst);
        let generation = self.0.generations.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(TestLog {
            fixture: Arc::clone(&self.0),
            generation,
            _namespace: Arc::clone(&self.1),
        })
    }
    fn retain(&mut self, _: &TestLog) -> Result<Retention, Error> {
        if let Some(error) = self.0.retention_error.lock().unwrap().take() {
            return Err(error);
        }
        Ok(Retention {
            skipped: self.0.skipped.load(Ordering::SeqCst),
            incomplete: true,
        })
    }
}
impl Log for TestLog {
    fn append(&self, bytes: &[u8]) -> io::Result<()> {
        let paused = self.fixture.write_pause.lock().unwrap().take();
        if let Some(paused) = paused {
            paused.wait();
        }
        assert!(
            !self.fixture.panic_write.load(Ordering::SeqCst),
            "worker unwind fixture"
        );
        let mut output = self.fixture.bytes.lock().unwrap();
        if self.fixture.partial.load(Ordering::SeqCst) {
            output.extend_from_slice(&bytes[..bytes.len().min(2)]);
            return Err(io::ErrorKind::WriteZero.into());
        }
        output.extend_from_slice(bytes);
        Ok(())
    }
    fn flush(&self) -> io::Result<()> {
        self.fixture.flushes.fetch_add(1, Ordering::SeqCst);
        if self.fixture.flush_error.load(Ordering::SeqCst) {
            Err(io::ErrorKind::Other.into())
        } else {
            Ok(())
        }
    }
}
fn today() -> Date {
    OffsetDateTime::now_utc().date()
}
fn start(fixture: &Arc<Fixture>, day: Date) -> Logging<TestStore> {
    Logging::spawn_with(TestStore::new(fixture), day, |work| {
        thread::Builder::new().spawn(work)
    })
    .unwrap()
}
fn ingress(fixture: &Arc<Fixture>, capacity: usize) -> (Writer<TestLog>, Receiver<Record>) {
    ingress_on(fixture, capacity, today())
}
fn ingress_on(
    fixture: &Arc<Fixture>,
    capacity: usize,
    day: Date,
) -> (Writer<TestLog>, Receiver<Record>) {
    let mut store = TestStore::new(fixture);
    let log = store.open(day).unwrap();
    let (sender, receiver) = mpsc::sync_channel(capacity);
    (
        Writer(Arc::new(WriterCell {
            ingress: Mutex::new(Some(sender)),
            current: Mutex::new(Current {
                state: State::Open,
                log: Some(Arc::new(log)),
            }),
            stop: AtomicBool::new(false),
            refused: AtomicBool::new(false),
            counts: Arc::new(Counts::default()),
        })),
        receiver,
    )
}

#[test]
fn ingress_accepts_whole_limit_records_and_attributes_each_rejection_once() {
    let fixture = Arc::new(Fixture::default());
    let (writer, receiver) = ingress(&fixture, QUEUE_RECORDS);
    let mut output = &writer;
    output.write_all(&vec![b'x'; RECORD_BYTES + 1]).unwrap();
    for _ in 0..QUEUE_RECORDS {
        output.write_all(&vec![b'x'; RECORD_BYTES]).unwrap();
    }
    output.write_all(b"full").unwrap();
    let held = writer.0.ingress.lock().unwrap();
    output.write_all(b"contended").unwrap();
    drop(held);
    writer.stop();
    output.write_all(b"stopped").unwrap();
    assert_eq!(writer.statistics().get(Count::Submitted), 64);
    for kind in [
        Count::Oversize,
        Count::Full,
        Count::Contended,
        Count::Closed,
    ] {
        assert_eq!(writer.statistics().get(kind), 1, "{kind:?}");
    }
    for _ in 0..QUEUE_RECORDS {
        let mut record = receiver.try_recv().unwrap();
        assert_eq!(record.bytes, vec![b'x'; 4096]);
        record.finish(Count::Discarded);
    }
    assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
    assert_eq!(writer.statistics().get(Count::Abandoned), 0);
}

#[test]
fn receiver_destruction_and_disconnection_do_not_double_count_losses() {
    let fixture = Arc::new(Fixture::default());
    let (writer, receiver) = ingress(&fixture, 2);
    writer.submit(b"first");
    writer.submit(b"second");
    drop(receiver);
    writer.submit(b"disconnected");
    drop(writer.0.ingress.lock().unwrap().take());
    assert_eq!(writer.statistics().get(Count::Abandoned), 2);
    assert_eq!(writer.statistics().get(Count::Disconnected), 1);
}

#[test]
fn empty_output_is_one_whole_record() {
    let fixture = Arc::new(Fixture::default());
    let (writer, receiver) = ingress(&fixture, 1);
    (&writer).write_all(b"").unwrap();
    let mut record = receiver.try_recv().unwrap();
    assert_eq!(record.bytes, b"");
    record.finish(Count::Discarded);
    assert_eq!(writer.statistics().get(Count::Submitted), 1);
    assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
}

struct BadFormat;
impl<S, N> FormatEvent<S, N> for BadFormat
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _: &FmtContext<'_, S, N>,
        mut writer: format::Writer<'_>,
        _: &tracing::Event<'_>,
    ) -> fmt::Result {
        fmt::Write::write_str(&mut writer, "FORMAT_CONTENT_MARKER")?;
        Err(fmt::Error)
    }
}
#[test]
fn formatter_error_counts_without_publishing_partial_content() {
    let fixture = Arc::new(Fixture::default());
    let (writer, receiver) = ingress(&fixture, 1);
    let subscriber = subscriber(writer.clone(), BadFormat);
    tracing::subscriber::with_default(subscriber, || tracing::info!("fixture"));
    assert_eq!(writer.statistics().get(Count::Format), 1);
    assert_eq!(writer.statistics().get(Count::Submitted), 0);
    assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
}

struct FailedField {
    calls: AtomicUsize,
    panic: bool,
}
impl FailedField {
    fn error() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            panic: false,
        }
    }
}
impl fmt::Display for FailedField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        f.write_str("PRIVATE_FORMAT_PREFIX")?;
        assert!(!(first && self.panic), "producer formatting fixture");
        if first { Err(fmt::Error) } else { Ok(()) }
    }
}
fn plain_subscriber(writer: &Writer<TestLog>) -> impl tracing::Subscriber + Send + Sync + 'static {
    subscriber(
        writer.clone(),
        format::format()
            .without_time()
            .with_level(false)
            .with_target(false),
    )
}
fn accepted_bytes(receiver: &Receiver<Record>) -> Vec<u8> {
    let mut record = receiver.try_recv().unwrap();
    record.finish(Count::Discarded);
    std::mem::take(&mut record.bytes)
}

#[test]
fn new_span_field_error_never_reformats_raw_attributes_or_accepts_partial_fields() {
    let fixture = Arc::new(Fixture::default());
    let (writer, receiver) = ingress(&fixture, 1);
    let field = FailedField::error();
    tracing::subscriber::with_default(plain_subscriber(&writer), || {
        let span = tracing::info_span!(
            "fixture_span",
            value = tracing::field::display(&field),
            following = "RAW_ATTRIBUTE_SECRET"
        );
        span.in_scope(|| tracing::info!("valid"));
    });
    assert_eq!(field.calls.load(Ordering::SeqCst), 1);
    assert_eq!(accepted_bytes(&receiver), b"fixture_span: valid\n");
    assert_eq!(writer.statistics().get(Count::FieldFormat), 1);
    assert_eq!(writer.statistics().get(Count::Format), 0);
    assert_eq!(writer.statistics().get(Count::Submitted), 1);
    assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
}

#[test]
fn failed_span_field_update_preserves_only_the_prior_committed_text() {
    let fixture = Arc::new(Fixture::default());
    let (writer, receiver) = ingress(&fixture, 1);
    let field = FailedField::error();
    tracing::subscriber::with_default(plain_subscriber(&writer), || {
        let span = tracing::info_span!("fixture_span", safe = 1_u64, value = tracing::field::Empty);
        span.record("value", tracing::field::display(&field));
        span.in_scope(|| tracing::info!("valid"));
    });
    assert_eq!(field.calls.load(Ordering::SeqCst), 1);
    assert_eq!(accepted_bytes(&receiver), b"fixture_span{safe=1}: valid\n");
    assert_eq!(writer.statistics().get(Count::FieldFormat), 1);
    assert_eq!(writer.statistics().get(Count::Format), 0);
    assert_eq!(writer.statistics().get(Count::Submitted), 1);
}

#[test]
fn event_field_error_rejects_the_whole_event_and_allows_a_later_valid_event() {
    let fixture = Arc::new(Fixture::default());
    let (writer, receiver) = ingress(&fixture, 1);
    let field = FailedField::error();
    tracing::subscriber::with_default(plain_subscriber(&writer), || {
        tracing::info!(value = tracing::field::display(&field), "failed");
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
        tracing::info!("valid");
    });
    assert_eq!(accepted_bytes(&receiver), b"valid\n");
    assert_eq!(writer.statistics().get(Count::FieldFormat), 1);
    assert_eq!(writer.statistics().get(Count::Format), 1);
    assert_eq!(writer.statistics().get(Count::Submitted), 1);
    assert_eq!(writer.statistics().get(Count::Abandoned), 0);
}

#[test]
#[cfg(panic = "unwind")]
fn caught_producer_formatter_unwind_cannot_prefix_the_next_event() {
    let fixture = Arc::new(Fixture::default());
    let (writer, receiver) = ingress(&fixture, 1);
    let field = FailedField {
        calls: AtomicUsize::new(0),
        panic: true,
    };
    tracing::subscriber::with_default(plain_subscriber(&writer), || {
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracing::info!(value = tracing::field::display(&field), "failed");
        }));
        assert!(failed.is_err());
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
        assert_eq!(FIELD_FAILED.with(Cell::get), None);
        tracing::info!("valid");
    });
    assert_eq!(field.calls.load(Ordering::SeqCst), 1);
    assert_eq!(accepted_bytes(&receiver), b"valid\n");
    assert_eq!(writer.statistics().get(Count::FieldFormat), 1);
    assert_eq!(writer.statistics().get(Count::Format), 1);
    assert_eq!(writer.statistics().get(Count::WorkerPanic), 0);
    assert_eq!(writer.statistics().get(Count::Submitted), 1);
    assert_eq!(writer.statistics().get(Count::Abandoned), 0);
}

#[test]
fn record_debug_and_errors_do_not_expose_content_or_foreign_error_text() {
    let marker = b"PRIVATE_CONTENT_MARKER".to_vec();
    let mut record = Record {
        bytes: marker.clone(),
        counts: Arc::new(Counts::default()),
        handled: false,
    };
    let debug = format!("{record:?}");
    assert!(!debug.contains("PRIVATE_CONTENT_MARKER"));
    assert!(!debug.contains(&format!("{marker:?}")));
    record.finish(Count::Discarded);
    assert_eq!(
        format!("{}", StartError::Spawn(io::ErrorKind::PermissionDenied)),
        "log worker creation failed"
    );
    assert_eq!(
        format!("{}", Error::Open(io::ErrorKind::PermissionDenied)),
        "log object operation failed"
    );
}

#[test]
fn panic_output_is_location_only_and_recursion_does_not_clear_the_outer_guard() {
    let fixture = Arc::new(Fixture::default());
    let (writer, _receiver) = ingress(&fixture, 1);
    let outer = Entered::enter().unwrap();
    writer.panic_at(None);
    writer.panic_at(None);
    assert!(IN_SINK.with(Cell::get));
    assert_eq!(writer.statistics().get(Count::PanicOmitted), 2);
    assert!(fixture.bytes.lock().unwrap().is_empty());
    drop(outer);
    writer.panic_at(None);
    assert_eq!(&*fixture.bytes.lock().unwrap(), b"panic location=unknown\n");
    writer.stop();
    writer.panic_at(Some(Location::caller()));
    assert_eq!(writer.statistics().get(Count::PanicOmitted), 3);
    assert_eq!(&*fixture.bytes.lock().unwrap(), b"panic location=unknown\n");
}

#[test]
fn failed_open_advances_utc_high_water_and_rollback_cannot_retry() {
    let fixture = Arc::new(Fixture::default());
    let mut store = TestStore::new(&fixture);
    let initial = Date::from_calendar_date(2026, time::Month::November, 8).unwrap();
    let (writer, _receiver) = ingress_on(&fixture, 1, initial);
    let tomorrow = initial.next_day().unwrap();
    let mut high_water = initial;
    *fixture.open_error.lock().unwrap() = Some(Error::Open(io::ErrorKind::StorageFull));
    rollover(&mut store, &writer.0, &mut high_water, tomorrow);
    rollover(&mut store, &writer.0, &mut high_water, initial);
    rollover(&mut store, &writer.0, &mut high_water, tomorrow);
    assert_eq!(high_water, tomorrow);
    assert_eq!(writer.statistics().get(Count::Open), 1);
    assert_eq!(fixture.generations.load(Ordering::SeqCst), 1);
    let later = tomorrow.next_day().unwrap();
    rollover(&mut store, &writer.0, &mut high_water, later);
    assert_eq!(fixture.generations.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.files.load(Ordering::SeqCst), 1);
    assert_eq!(&fixture.opens.lock().unwrap()[1..], &[tomorrow, later]);
}

#[test]
fn candidate_skips_are_counted_but_parent_refusal_is_terminal() {
    let fixture = Arc::new(Fixture::default());
    let mut store = TestStore::new(&fixture);
    let (writer, _receiver) = ingress(&fixture, 1);
    fixture.skipped.store(3, Ordering::SeqCst);
    let current = Arc::clone(writer.0.current.lock().unwrap().log.as_ref().unwrap());
    writer.0.retention(store.retain(&current));
    assert_eq!(writer.statistics().get(Count::PruneSkipped), 3);
    assert_eq!(writer.statistics().get(Count::PruneIncomplete), 1);
    writer.0.retention(Err(Error::SecurityRefused));
    let mut day = today();
    rollover(&mut store, &writer.0, &mut day, today().next_day().unwrap());
    writer.submit(b"valid after refusal");
    assert_eq!(fixture.generations.load(Ordering::SeqCst), 1);
    assert_eq!(
        writer.0.current.lock().unwrap().state,
        State::SecurityRefused
    );
    assert_eq!(writer.statistics().get(Count::Refused), 1);
    assert!(fixture.bytes.lock().unwrap().is_empty());
}

#[test]
fn closing_rejects_an_already_preparing_candidate_without_replacing_current() {
    let fixture = Arc::new(Fixture::default());
    let store = TestStore::new(&fixture);
    let (writer, _receiver) = ingress(&fixture, 1);
    let (paused, arrived, release) = pause();
    *fixture.open_pause.lock().unwrap() = Some(paused);
    let worker_writer = writer.clone();
    let worker = thread::spawn(move || {
        let mut store = store;
        let mut day = today();
        rollover(
            &mut store,
            &worker_writer.0,
            &mut day,
            today().next_day().unwrap(),
        );
        store
    });
    arrived.recv_timeout(WAIT).unwrap();
    writer.stop();
    writer.0.current.lock().unwrap().state = State::Closing;
    release.send(()).unwrap();
    drop(worker.join().unwrap());
    let current = writer.0.current.lock().unwrap();
    assert_eq!(current.state, State::Closing);
    assert_eq!(current.log.as_ref().unwrap().generation, 1);
    assert_eq!(fixture.files.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.generations.load(Ordering::SeqCst), 2);
}

#[test]
fn active_file_admission_refusal_is_not_a_retention_candidate_skip() {
    let fixture = Arc::new(Fixture::default());
    let mut store = TestStore::new(&fixture);
    let (writer, _receiver) = ingress(&fixture, 1);
    let mut day = today();
    #[cfg(windows)]
    let refusal =
        crate::platform::logging::active_file(Ok(()), |_| Err(io::ErrorKind::InvalidData.into()))
            .unwrap_err();
    #[cfg(not(windows))]
    let refusal = Error::SecurityRefused;
    *fixture.open_error.lock().unwrap() = Some(refusal);
    rollover(&mut store, &writer.0, &mut day, today().next_day().unwrap());
    assert_eq!(
        writer.0.current.lock().unwrap().state,
        State::SecurityRefused
    );
    rollover(
        &mut store,
        &writer.0,
        &mut day,
        today().next_day().unwrap().next_day().unwrap(),
    );
    writer.submit(b"valid after active-file refusal");
    assert_eq!(fixture.generations.load(Ordering::SeqCst), 1);
    assert_eq!(writer.statistics().get(Count::PruneSkipped), 0);
    assert_eq!(writer.statistics().get(Count::Refused), 1);
    assert!(fixture.bytes.lock().unwrap().is_empty());
}

#[test]
fn close_waits_for_entered_io_and_releases_last_owners_before_unlock_or_quit() {
    let fixture = Arc::new(Fixture::default());
    let owner = start(&fixture, today());
    let writer = owner.writer();
    let (paused, arrived, release) = pause();
    *fixture.write_pause.lock().unwrap() = Some(paused);
    writer.submit(b"entered");
    arrived.recv_timeout(WAIT).unwrap();
    writer.submit(b"queued");
    let (quit, finished) = mpsc::channel();
    let unlocked = Arc::new(AtomicBool::new(false));
    let instance = Arc::clone(&unlocked);
    let closed_fixture = Arc::clone(&fixture);
    let closer = thread::spawn(move || {
        let counts = owner.close();
        assert_eq!(closed_fixture.files.load(Ordering::SeqCst), 0);
        assert_eq!(closed_fixture.namespaces.load(Ordering::SeqCst), 0);
        assert_eq!(
            &*closed_fixture.retired.lock().unwrap(),
            &["file", "namespace"]
        );
        instance.store(true, Ordering::SeqCst);
        quit.send(counts).unwrap();
    });
    writer.stop();
    assert!(!unlocked.load(Ordering::SeqCst));
    assert_eq!(fixture.files.load(Ordering::SeqCst), 1);
    assert_eq!(finished.try_recv().unwrap_err(), TryRecvError::Empty);
    release.send(()).unwrap();
    let counts = finished.recv_timeout(WAIT).unwrap();
    closer.join().unwrap();
    assert_eq!(counts.get(Count::Written), 1);
    assert_eq!(counts.get(Count::Discarded), 1);
    assert_eq!(counts.get(Count::WorkerPanic), 0);
    writer.submit(b"late");
    writer.panic_at(None);
    assert_eq!(writer.statistics().get(Count::Closed), 1);
    assert_eq!(writer.0.current.lock().unwrap().state, State::Closed);
    assert_eq!(&*fixture.bytes.lock().unwrap(), b"entered");
}

#[test]
fn close_joins_entered_maintenance_outside_the_writer_gate_before_namespace_release() {
    let fixture = Arc::new(Fixture::default());
    let owner = start(&fixture, today().previous_day().unwrap());
    let writer = owner.writer();
    let (paused, arrived, release) = pause();
    *fixture.open_pause.lock().unwrap() = Some(paused);
    writer.submit(b"due");
    arrived.recv_timeout(WAIT).unwrap();
    writer.stop();
    let (complete, finished) = mpsc::channel();
    let closer = thread::spawn(move || complete.send(owner.close()).unwrap());
    assert_eq!(fixture.namespaces.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.files.load(Ordering::SeqCst), 1);
    assert_eq!(finished.try_recv().unwrap_err(), TryRecvError::Empty);
    release.send(()).unwrap();
    let counts = finished.recv_timeout(WAIT).unwrap();
    closer.join().unwrap();
    assert_eq!(counts.get(Count::Discarded), 1);
    assert_eq!(counts.get(Count::Written), 0);
    assert_eq!(fixture.generations.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.namespaces.load(Ordering::SeqCst), 0);
    assert!(fixture.bytes.lock().unwrap().is_empty());
}

#[test]
fn partial_io_is_not_replayed_or_double_counted_as_abandonment() {
    let fixture = Arc::new(Fixture::default());
    fixture.partial.store(true, Ordering::SeqCst);
    fixture.flush_error.store(true, Ordering::SeqCst);
    let owner = start(&fixture, today());
    let writer = owner.writer();
    let (paused, arrived, release) = pause();
    *fixture.write_pause.lock().unwrap() = Some(paused);
    writer.submit(b"abcdef");
    arrived.recv_timeout(WAIT).unwrap();
    release.send(()).unwrap();
    let counts = owner.close();
    assert_eq!(counts.get(Count::Write), 1);
    assert_eq!(counts.get(Count::Abandoned), 0);
    assert_eq!(counts.get(Count::Flush), 1);
    assert_eq!(&*fixture.bytes.lock().unwrap(), b"ab");
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.namespaces.load(Ordering::SeqCst), 0);
}

#[test]
#[cfg(panic = "unwind")]
fn worker_unwind_attributes_inflight_and_queued_records_and_still_reclaims_current() {
    let fixture = Arc::new(Fixture::default());
    fixture.panic_write.store(true, Ordering::SeqCst);
    let owner = start(&fixture, today());
    let writer = owner.writer();
    let (paused, arrived, release) = pause();
    *fixture.write_pause.lock().unwrap() = Some(paused);
    writer.submit(b"inflight");
    arrived.recv_timeout(WAIT).unwrap();
    writer.submit(b"queued1");
    writer.submit(b"queued2");
    release.send(()).unwrap();
    let counts = owner.close();
    assert_eq!(counts.get(Count::Abandoned), 3);
    assert_eq!(counts.get(Count::WorkerPanic), 1);
    assert_eq!(counts.get(Count::Write), 0);
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.namespaces.load(Ordering::SeqCst), 0);
    assert_eq!(&*fixture.retired.lock().unwrap(), &["file", "namespace"]);
    assert!(writer.0.current.is_poisoned());
    assert!(
        writer
            .0
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .log
            .is_none()
    );
}

#[test]
fn spawn_and_subscriber_failure_revoke_and_release_unpublished_owners() {
    let fixture = Arc::new(Fixture::default());
    let failed = Logging::spawn_with(TestStore::new(&fixture), today(), |work| {
        drop(work);
        Err(io::ErrorKind::OutOfMemory.into())
    });
    assert!(matches!(
        failed,
        Err(StartError::Spawn(io::ErrorKind::OutOfMemory))
    ));
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.namespaces.load(Ordering::SeqCst), 0);
    let owner = start(&fixture, today());
    let late = owner.writer();
    let failed = owner.publish(|dispatch| {
        drop(dispatch);
        Err(StartError::Subscriber)
    });
    assert!(matches!(failed, Err(StartError::Subscriber)));
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.namespaces.load(Ordering::SeqCst), 0);
    late.submit(b"after failed install");
    assert_eq!(late.statistics().get(Count::Closed), 1);
}

#[test]
fn refused_sink_is_reclaimed_without_a_new_flush_or_panic_write() {
    let fixture = Arc::new(Fixture::default());
    let owner = start(&fixture, today());
    let writer = owner.writer();
    writer.0.failure(Error::SecurityRefused, Count::Open);
    writer.panic_at(None);
    let counts = owner.close();
    assert_eq!(counts.get(Count::PanicOmitted), 1);
    assert_eq!(fixture.flushes.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
    assert!(fixture.bytes.lock().unwrap().is_empty());
}

#[test]
fn initial_admission_failure_rolls_back_but_ordinary_retention_failure_keeps_current() {
    let fixture = Arc::new(Fixture::default());
    *fixture.open_error.lock().unwrap() = Some(Error::Open(io::ErrorKind::PermissionDenied));
    assert!(matches!(
        Logging::start(TestStore::new(&fixture)),
        Err(StartError::Storage(Error::Open(
            io::ErrorKind::PermissionDenied
        )))
    ));
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.namespaces.load(Ordering::SeqCst), 0);
    *fixture.retention_error.lock().unwrap() = Some(Error::SecurityRefused);
    assert!(matches!(
        Logging::start(TestStore::new(&fixture)),
        Err(StartError::Storage(Error::SecurityRefused))
    ));
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.namespaces.load(Ordering::SeqCst), 0);
    *fixture.retention_error.lock().unwrap() = Some(Error::Open(io::ErrorKind::PermissionDenied));
    let owner = start(&fixture, today());
    assert_eq!(owner.writer().statistics().get(Count::Prune), 1);
    assert_eq!(fixture.files.load(Ordering::SeqCst), 1);
    owner.close();
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
}

#[test]
fn off_ui_drop_fallback_joins_instead_of_detaching_a_worker() {
    let fixture = Arc::new(Fixture::default());
    let owner = start(&fixture, today());
    let late = owner.writer();
    thread::spawn(move || drop(owner)).join().unwrap();
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.namespaces.load(Ordering::SeqCst), 0);
    late.submit(b"late after owner drop");
    assert_eq!(late.statistics().get(Count::Closed), 1);
}

#[test]
#[cfg(panic = "unwind")]
fn poison_stops_production_and_cleanup_can_reclaim_without_resuming_writes() {
    let fixture = Arc::new(Fixture::default());
    let owner = start(&fixture, today());
    let writer = owner.writer();
    let poisoned = writer.clone();
    assert!(
        thread::spawn(move || {
            let _held = poisoned.0.ingress.lock().unwrap();
            panic!("ingress poison fixture");
        })
        .join()
        .is_err()
    );
    writer.submit(b"poisoned");
    writer.submit(b"valid after poison");
    let counts = owner.close();
    assert_eq!(counts.get(Count::Poisoned), 1);
    assert_eq!(counts.get(Count::Closed), 1);
    assert_eq!(counts.get(Count::Submitted), 0);
    assert_eq!(fixture.files.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.flushes.load(Ordering::SeqCst), 0);
    assert!(fixture.bytes.lock().unwrap().is_empty());
}

#[test]
fn loss_counters_saturate_instead_of_wrapping() {
    let counts = Counts::default();
    counts.add(Count::Full, usize::MAX);
    counts.add(Count::Full, 1);
    assert_eq!(counts.snapshot().get(Count::Full), usize::MAX);
}
