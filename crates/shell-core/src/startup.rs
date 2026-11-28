//! How a shell starts before its frontend runs, shared by every shell. Off macOS, [`start`] admits
//! the lock before the log opens, logs bad launch flags once, then takes the lock; macOS composes
//! [`acquire`] and [`logged_flags`] in its own order, lock first.

use std::fmt::Display;
use std::process::ExitCode;
use std::time::Duration;

use crate::instance::{InstanceLock, SessionLockError};
use crate::logging::LoggingError;

/// Why a start stopped before the frontend ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Another shell holds this session's lock.
    AnotherShell,
    /// Admission, logging, the launch flags or the lock failed.
    Failed,
}

impl Refused {
    /// Success when another shell already runs, failure otherwise.
    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        match self {
            Self::AnotherShell => ExitCode::SUCCESS,
            Self::Failed => ExitCode::FAILURE,
        }
    }
}

/// Admits the lock with `admit`, starts logging, reports bad flags, then takes the lock.
pub fn start<F, E: Display>(
    admit: impl FnOnce() -> Result<(), SessionLockError>,
    flags: Result<F, E>,
    init_logging: impl FnOnce() -> Result<(), LoggingError>,
) -> Result<(F, InstanceLock), Refused> {
    start_with(admit, flags, init_logging, acquire)
}

/// [`start`] with its acquisition step, so tests can see whether a refusal reaches the lock.
fn start_with<F, E: Display, L>(
    admit: impl FnOnce() -> Result<(), SessionLockError>,
    flags: Result<F, E>,
    init_logging: impl FnOnce() -> Result<(), LoggingError>,
    acquire: impl FnOnce() -> Result<L, Refused>,
) -> Result<(F, L), Refused> {
    // The lock's folder and name are admitted before the log opens, so a refusal opens neither the
    // log nor the lock; admission itself may create the product folder.
    admit().map_err(|_| Refused::Failed)?;
    let flags = logged_flags(flags, init_logging).ok_or(Refused::Failed)?;
    Ok((flags, acquire()?))
}

/// Starts logging, then logs a flag error once by its fixed text; `None` when either failed.
/// The error's Display must be fixed text with no argument value; each shell's parser test holds it.
pub fn logged_flags<F, E: Display>(
    flags: Result<F, E>,
    init_logging: impl FnOnce() -> Result<(), LoggingError>,
) -> Option<F> {
    init_logging().ok()?;
    flags
        .inspect_err(|error| tracing::error!(%error, "invalid launch flags"))
        .ok()
}

/// The current session's lock, or why the shell must not run.
pub fn acquire() -> Result<InstanceLock, Refused> {
    held(InstanceLock::try_acquire_current_session())
}

fn held(lock: Result<Option<InstanceLock>, SessionLockError>) -> Result<InstanceLock, Refused> {
    match lock {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => {
            tracing::info!("another shell is running in this session");
            Err(Refused::AnotherShell)
        }
        Err(failure) => {
            if let Some(error) = std::error::Error::source(&failure) {
                tracing::error!(%error, "{failure}");
            } else {
                tracing::error!("{failure}");
            }
            Err(Refused::Failed)
        }
    }
}

/// The deadline `--quit-after-ms=<value>` names: 1 to 120000 milliseconds.
#[must_use]
pub fn quit_after_ms(value: &str) -> Option<Duration> {
    let millis = value.parse::<u64>().ok()?;
    (1..=120_000)
        .contains(&millis)
        .then_some(Duration::from_millis(millis))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fmt;
    use std::fs;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::scratch;

    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A subscriber that writes plain lines into the returned buffer.
    fn capturing() -> (Arc<Mutex<Vec<u8>>>, impl tracing::Subscriber + Send + Sync) {
        let out = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_writer({
                let out = Arc::clone(&out);
                move || Captured(Arc::clone(&out))
            })
            .finish();
        (out, subscriber)
    }

    /// What `run` logs while a capturing subscriber is the default.
    fn logged(run: impl FnOnce()) -> String {
        let (out, subscriber) = capturing();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = out.lock().unwrap();
        String::from_utf8(bytes.clone()).unwrap()
    }

    /// A flag error whose Debug holds the argument and whose Display doesn't.
    #[derive(Debug)]
    struct Flaw {
        _value: &'static str,
    }

    impl fmt::Display for Flaw {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("--quit-after-ms must be between 1 and 120000")
        }
    }

    const MARKER: &str = "private-fixture-value";

    /// The steps `start_with` took, in order.
    type Steps = RefCell<Vec<&'static str>>;

    #[test]
    fn a_refusal_never_reaches_the_lock() {
        type Admit = fn() -> Result<(), SessionLockError>;
        type Log = fn() -> Result<(), LoggingError>;
        let admitted: Admit = || Ok(());
        let refused: Admit = || Err(SessionLockError::DataDirectory);
        let opened: Log = || Ok(());
        let unopened: Log = || Err(LoggingError::NoLocalAppData);
        for (case, admit, flags, init_logging, expected) in [
            ("admission refused", refused, Ok(()), opened, vec!["admit"]),
            (
                "bad flags",
                admitted,
                Err(Flaw { _value: MARKER }),
                opened,
                vec!["admit", "log"],
            ),
            (
                "logging failed",
                admitted,
                Ok(()),
                unopened,
                vec!["admit", "log"],
            ),
        ] {
            let steps = Steps::default();
            let started = start_with(
                || {
                    steps.borrow_mut().push("admit");
                    admit()
                },
                flags,
                || {
                    steps.borrow_mut().push("log");
                    init_logging()
                },
                || {
                    steps.borrow_mut().push("acquire");
                    Ok("lock")
                },
            );
            assert_eq!(started.err(), Some(Refused::Failed), "{case} refuses");
            assert_eq!(*steps.borrow(), expected, "{case} stops before the lock");
        }
    }

    #[test]
    fn an_admitted_start_takes_the_lock_once_and_last() {
        let steps = Steps::default();
        let started = start_with(
            || {
                steps.borrow_mut().push("admit");
                Ok(())
            },
            Ok::<u8, Flaw>(7),
            || {
                steps.borrow_mut().push("log");
                Ok(())
            },
            || {
                steps.borrow_mut().push("acquire");
                Ok("lock")
            },
        );
        assert_eq!(started, Ok((7, "lock")), "the flags and the lock come back");
        assert_eq!(
            *steps.borrow(),
            ["admit", "log", "acquire"],
            "the lock is taken once, last"
        );
    }

    #[test]
    fn invalid_launch_flags_get_one_actionable_redacted_diagnostic() {
        let (out, subscriber) = capturing();
        let mut logger = None;
        let flags = logged_flags(Err::<(), _>(Flaw { _value: MARKER }), || {
            logger = Some(tracing::subscriber::set_default(subscriber));
            Ok(())
        });
        drop(logger);
        let bytes = out.lock().unwrap();
        let log = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(flags, None, "bad flags stop the start");
        assert_eq!(
            log, "ERROR invalid launch flags error=--quit-after-ms must be between 1 and 120000\n",
            "one fixed diagnostic"
        );
        assert!(!log.contains(MARKER), "no argument value is logged");
        assert!(
            !log.contains(&format!("{:?}", MARKER.as_bytes())),
            "no argument value is logged as a byte list"
        );
    }

    #[test]
    fn valid_flags_pass_silently_and_a_logging_failure_stops_the_start() {
        let mut flags = None;
        let log = logged(|| flags = logged_flags(Ok::<u8, Flaw>(7), || Ok(())));
        assert_eq!(flags, Some(7), "valid flags pass through");
        assert_eq!(log, "", "nothing is logged for valid flags");
        assert_eq!(
            logged_flags(Ok::<u8, Flaw>(7), || Err(LoggingError::NoLocalAppData)),
            None,
            "no shell runs without its log"
        );
    }

    #[test]
    fn a_held_lock_is_another_shell_and_exits_successfully() {
        let dir = scratch::dir("startup-held");
        let path = dir.join("shell-1.lock");
        let _first = InstanceLock::try_acquire(&path)
            .expect("the lock opens")
            .expect("the first shell holds it");
        let mut refused = None;
        let log = logged(|| {
            refused = held(InstanceLock::try_acquire(&path).map_err(SessionLockError::Lock)).err();
        });
        assert_eq!(
            refused,
            Some(Refused::AnotherShell),
            "the second shell defers"
        );
        assert_eq!(
            Refused::AnotherShell.exit_code(),
            ExitCode::SUCCESS,
            "deferring is a success"
        );
        assert_eq!(
            log, " INFO another shell is running in this session\n",
            "one fixed line"
        );
    }

    #[test]
    fn a_lock_that_cannot_open_fails_with_exit_failure() {
        let dir = scratch::dir("startup-broken");
        fs::create_dir_all(dir.join("shell-1.lock")).expect("a directory where the file goes");
        let mut refused = None;
        let log = logged(|| {
            refused = held(
                InstanceLock::try_acquire(&dir.join("shell-1.lock"))
                    .map_err(SessionLockError::Lock),
            )
            .err();
        });
        assert_eq!(
            refused,
            Some(Refused::Failed),
            "a broken lock is not another shell"
        );
        assert_eq!(
            Refused::Failed.exit_code(),
            ExitCode::FAILURE,
            "a failure exits with failure"
        );
        // The OS error's text varies by system and language, so only its line's start is fixed.
        assert!(
            log.starts_with("ERROR can't take the single-instance lock error="),
            "one error line names the failure: {log:?}"
        );
        assert_eq!(log.lines().count(), 1, "one line");
    }

    #[test]
    fn quit_after_ms_takes_one_to_120000() {
        for (value, expected) in [
            ("1", Some(Duration::from_millis(1))),
            ("120000", Some(Duration::from_millis(120_000))),
            ("0", None),
            ("120001", None),
            ("", None),
            ("x", None),
            ("-1", None),
            ("5ms", None),
        ] {
            assert_eq!(quit_after_ms(value), expected, "--quit-after-ms={value}");
        }
    }
}
