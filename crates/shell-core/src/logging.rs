//! File logging through `tracing`, to the product's `logs` folder in the local application data
//! root, one file a day for a week.
//!
//! Content (prompts, transcripts, screen text, images, tool arguments) is never logged, so nothing
//! needs redacting after the fact. Secrets are [`kumochi_protocol::Secret`]s and print as `***`.

use std::path::Path;

use tracing::{Level, Subscriber};
use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};

/// Daily files kept before the oldest is deleted.
const KEEP_DAYS: usize = 7;

/// Why logging couldn't start.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    #[cfg(not(windows))]
    #[error("native file logging is unsupported on this platform")]
    Unsupported,
    /// The per-user data folder isn't known.
    #[error("the local application data folder is unavailable")]
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
pub fn init(prefix: &str) -> Result<(), LoggingError> {
    let dir = crate::platform::logging_data_dir()?.join(kumochi_protocol::constants::LOGS_DIR);
    tracing::subscriber::set_global_default(file_subscriber(&dir, prefix)?)?;
    // The payload can hold any value an assertion printed, so only the location is logged.
    std::panic::set_hook(Box::new(|info| {
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()));
        tracing::error!(location = at.as_deref().unwrap_or("unknown"), "panic");
    }));
    Ok(())
}

pub(crate) fn file_subscriber(dir: &Path, prefix: &str) -> Result<impl Subscriber, InitError> {
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
    Ok(tracing_subscriber::fmt()
        .with_writer(appender)
        .with_max_level(level)
        .finish())
}
