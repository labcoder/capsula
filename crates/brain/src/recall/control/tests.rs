use std::sync::atomic::AtomicU8;
use std::sync::{Mutex, mpsc};

use super::*;

const SQL: &str = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) SELECT sum(x) FROM n";

#[derive(Debug)]
pub(crate) struct Gate {
    pub(crate) entered: mpsc::Sender<()>,
    pub(crate) release: mpsc::Receiver<()>,
}

#[derive(Debug, Default)]
pub(crate) struct State {
    pub(crate) waiting: AtomicBool,
    pub(crate) fault: AtomicU8,
    pub(crate) time: Mutex<Option<Instant>>,
    pub(crate) gate: Mutex<Option<Gate>>,
}

impl State {
    pub(super) fn now(&self) -> Instant {
        self.time
            .lock()
            .expect("clock")
            .unwrap_or_else(Instant::now)
    }

    pub(super) fn hook(&self, result: rusqlite::Result<()>, point: u8) -> rusqlite::Result<()> {
        if self
            .fault
            .compare_exchange(point, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        result
    }

    pub(super) fn progress(&self) -> bool {
        let gate = self.gate.lock().expect("gate").take();
        gate.is_some_and(|gate| {
            gate.entered.send(()).is_err()
                || gate.release.recv_timeout(Duration::from_secs(10)).is_err()
        })
    }
}

fn control() -> Control {
    Control::new(Instant::now() + Duration::from_secs(60))
}

fn sum(conn: &Connection) -> Result<i64, Error> {
    conn.query_row(SQL, [], |row| row.get(0))
        .map_err(|_| Error::Read)
}

#[test]
fn owners_cancel_children_and_deadlines_never_extend() {
    let start = Instant::now();
    let end = start + Duration::from_secs(60);
    let early = end - READ_TIME;
    let owner = Control::new(end);
    let child = owner.child();
    let shortened = child.until(early);
    let extended = child.until(end + READ_TIME);

    *child.test.time.lock().unwrap() = Some(early - Duration::from_millis(1));
    assert_eq!(shortened.check(), Ok(()));
    *child.test.time.lock().unwrap() = Some(early);
    assert_eq!(shortened.check(), Err(Error::Timeout));
    assert_eq!(extended.check(), Ok(()));
    *child.test.time.lock().unwrap() = Some(end - Duration::from_millis(1));
    assert_eq!(extended.check(), Ok(()));
    *child.test.time.lock().unwrap() = Some(end);
    assert_eq!(extended.check(), Err(Error::Timeout));

    owner.cancel();
    assert_eq!(child.check(), Err(Error::Ended));
    assert_eq!(extended.check(), Err(Error::Ended));
    assert_eq!(child.until(end + READ_TIME).check(), Err(Error::Ended));
}

#[test]
fn one_scope_keeps_one_absolute_deadline_across_queries() {
    let mut conn = Connection::open_in_memory().unwrap();
    let end = Instant::now() + READ_TIME;
    let control = control().until(end);
    let mut ran = false;
    let result = control.with_handler(&mut conn, |conn, scope| {
        assert_eq!(scope.control.end, end);
        assert_eq!(scope.query(conn, || Ok(7)), Ok(7));
        *control.test.time.lock().unwrap() = Some(end);
        scope.query(conn, || {
            ran = true;
            Ok(8)
        })
    });
    assert_eq!(result, Ok(Err(Error::Timeout)));
    assert!(!ran);
}

#[test]
fn cancel_and_deadline_interrupt_sql_and_restore_the_connection() {
    for timeout in [false, true] {
        let mut conn = Connection::open_in_memory().expect("connection");
        conn.busy_timeout(Duration::from_millis(321))
            .expect("timeout");
        let control = control();
        let mut interrupted = false;
        let result = control.with_handler(&mut conn, |conn, scope| {
            scope.query(conn, || {
                assert_eq!(conn.query_row::<i64, _, _>("PRAGMA busy_timeout", [], |row| row.get(0)).unwrap(), 0);
                if timeout { *control.test.time.lock().expect("clock") = Some(control.end); }
                else { control.cancel(); }
                let result = conn.query_row::<i64, _, _>(SQL, [], |row| row.get(0));
                interrupted = matches!(&result, Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::OperationInterrupted);
                result.map_err(|_| Error::Read)
            })
        });
        assert_eq!(
            result,
            Ok(Err(if timeout {
                Error::Timeout
            } else {
                Error::Ended
            })),
            "timeout={timeout}"
        );
        assert!(
            interrupted,
            "SQLite stopped rather than completing after the deadline"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("PRAGMA busy_timeout", [], |row| row.get(0))
                .expect("timeout"),
            321
        );
        assert_eq!(sum(&conn), Ok(500_000_500_000));
    }
}

#[test]
fn foreign_interrupt_is_not_relabelled_as_our_timeout() {
    let mut conn = Connection::open_in_memory().expect("connection");
    let control = control();
    let interrupt = conn.get_interrupt_handle();
    let (entered, heard) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *control.test.gate.lock().expect("gate") = Some(Gate {
        entered,
        release: released,
    });
    let worker = std::thread::spawn(move || {
        heard
            .recv_timeout(Duration::from_secs(10))
            .expect("SQLite running");
        interrupt.interrupt();
        release.send(()).expect("resume SQLite");
    });
    let result = control.with_handler(&mut conn, |conn, scope| {
        scope.query(conn, || {
            let result = sum(conn);
            *control.test.time.lock().expect("clock") = Some(control.end);
            result
        })
    });
    worker.join().expect("interrupt worker");
    assert_eq!(result, Ok(Err(Error::Read)));
    assert_eq!(sum(&conn), Ok(500_000_500_000));
}

#[test]
fn inactive_handlers_leave_non_recall_queries_unchanged() {
    let mut conn = Connection::open_in_memory().expect("connection");
    let control = control();
    control.cancel();
    assert_eq!(
        control.with_handler(&mut conn, |conn, _| sum(conn)),
        Ok(Ok(500_000_500_000))
    );
}

#[test]
fn each_hook_failure_is_typed_and_nested_cleanup_failure_escapes() {
    for point in [1, 2, 3] {
        let mut conn = Connection::open_in_memory().expect("connection");
        let control = control();
        control.test.fault.store(point, Ordering::SeqCst);
        assert_eq!(
            control.with_handler(&mut conn, |conn, scope| scope.query(conn, || Ok(7))),
            Err(Error::Handler),
            "hook point {point}"
        );
        conn.progress_handler(0, None::<fn() -> bool>)
            .expect("fixture cleanup");
    }
}

#[test]
fn query_error_deactivates_its_scope_before_non_recall_sql() {
    let mut conn = Connection::open_in_memory().unwrap();
    let control = control();
    let result = control.with_handler(&mut conn, |conn, scope| {
        let result = scope.query(conn, || Err::<i64, _>(Error::Read));
        control.cancel();
        assert_eq!(sum(conn), Ok(500_000_500_000));
        result
    });
    assert_eq!(result, Ok(Err(Error::Read)));
}

#[test]
fn cleanup_failure_stays_terminal_after_a_valid_query() {
    let mut conn = Connection::open_in_memory().unwrap();
    let control = control();
    control.test.fault.store(3, Ordering::SeqCst);
    let result = control.with_handler(&mut conn, |conn, scope| {
        assert_eq!(scope.query(conn, || Ok(7)), Err(Error::Handler));
        scope.query(conn, || Ok(9))
    });
    assert_eq!(result, Err(Error::Handler));
}

#[test]
fn unwind_restores_timeout_and_removes_the_handler() {
    let mut conn = Connection::open_in_memory().expect("connection");
    conn.busy_timeout(Duration::from_millis(123))
        .expect("timeout");
    let control = control();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let result = control.with_handler(&mut conn, |conn, scope| {
            scope.query::<()>(conn, || panic!("fixture panic"))
        });
        panic!("expected unwind, got {result:?}");
    }));
    assert!(panic.is_err());
    control.cancel();
    assert_eq!(
        conn.query_row::<i64, _, _>("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("timeout"),
        123
    );
    assert_eq!(sum(&conn), Ok(500_000_500_000));
}
