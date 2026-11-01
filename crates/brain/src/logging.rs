//! File logging through `tracing`, to `%LOCALAPPDATA%\\<product>\\logs`, one file a day for a week.
//!
//! Content (prompts, transcripts, screen text, images, tool arguments) is never logged, so nothing
//! needs redacting after the fact. Secrets are [`capsula_protocol::Secret`]s and print as `***`.

use std::path::PathBuf;

use capsula_protocol::constants;
use tracing::Level;
use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};

/// Daily files kept before the oldest is deleted.
const KEEP_DAYS: usize = 7;

/// Why logging couldn't start.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    /// The per-user data folder isn't known.
    #[error("%LOCALAPPDATA% is not set")]
    NoLocalAppData,
    /// The log folder or file couldn't be created.
    #[error("can't open the log file")]
    Appender(#[from] InitError),
    /// Something already installed a global subscriber.
    #[error("logging was already started")]
    AlreadyStarted(#[from] tracing::subscriber::SetGlobalDefaultError),
}

/// Starts logging to files named `<prefix>.<date>.log` and routes panics there.
///
/// Writes are synchronous: there are a few lines per state change and none per frame, and a
/// panic message has to reach the file before a release build aborts.
///
/// # Errors
/// Fails when the log folder can't be found or created, or logging already started.
pub fn init(prefix: &str) -> Result<(), LoggingError> {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or(LoggingError::NoLocalAppData)?
        .join(constants::DATA_DIR)
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
    std::panic::set_hook(Box::new(|info| tracing::error!(%info, "panic")));
    Ok(())
}
