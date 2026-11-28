//! How a shell starts before its frontend runs, shared by every shell: the lock is admitted before
//! the log opens, bad launch flags are logged once, then the lock is taken.

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
    // The lock's folder and name are admitted before the log opens, so a refusal writes nothing.
    admit().map_err(|_| Refused::Failed)?;
    let flags = logged_flags(flags, init_logging).ok_or(Refused::Failed)?;
    Ok((flags, acquire()?))
}

/// Starts logging, then logs a flag error once by its fixed text; `None` when either failed.
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

    #[test]
    fn an_admission_refusal_fails_before_logging_starts() {
        let mut logging_started = false;
        let started = start(
            || Err(SessionLockError::DataDirectory),
            Ok::<(), Flaw>(()),
            || {
                logging_started = true;
                Ok(())
            },
        );
        assert_eq!(started.err(), Some(Refused::Failed), "admission refuses");
        assert!(!logging_started, "nothing is written, the log included");
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
        assert_eq!(log.lines().count(), 1, "expected one startup diagnostic");
        assert!(
            log.contains("invalid launch flags"),
            "the diagnostic names flags"
        );
        assert!(
            log.contains("--quit-after-ms must be between 1 and 120000"),
            "the diagnostic says how to fix them"
        );
        assert!(!log.contains(MARKER), "no argument value is logged");
        assert!(
            !bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
            "no argument value is logged, as bytes"
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
            log.trim_end(),
            " INFO another shell is running in this session",
            "one fixed line"
        );
    }

    #[test]
    fn a_lock_that_cannot_open_fails_with_exit_failure() {
        let dir = scratch::dir("startup-broken");
        fs::create_dir_all(dir.join("shell-1.lock")).expect("a directory where the file goes");
        let refused = held(
            InstanceLock::try_acquire(&dir.join("shell-1.lock")).map_err(SessionLockError::Lock),
        )
        .err();
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
