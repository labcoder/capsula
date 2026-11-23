//! File logging through `tracing`, to `%LOCALAPPDATA%\\<product>\\logs`, one file a day for a week.
//!
//! Content (prompts, transcripts, screen text, images, tool arguments) is never logged, so nothing
//! needs redacting after the fact. Secrets are [`kumochi_protocol::Secret`]s and print as `***`.

use tracing::Level;
use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt as _;

/// Daily files kept before the oldest is deleted.
const KEEP_DAYS: usize = 7;

pub(crate) fn filter(level: Level) -> Targets {
    Targets::new()
        .with_default(level)
        .with_target("hyper_util", LevelFilter::OFF)
        // Spoken replies' dependencies: their events, errors included, can carry paths or text.
        .with_target("ptts", LevelFilter::OFF)
        .with_target("xn", LevelFilter::OFF)
        .with_target("xn_gemm", LevelFilter::OFF)
        .with_target("tokenizers", LevelFilter::OFF)
}

/// Why logging couldn't start.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    /// This build doesn't admit native file logging.
    #[error("native file logging is unavailable on this platform")]
    NativeRuntime(#[source] std::io::Error),
    /// The per-user data folder isn't known.
    #[error("{} is not set to an absolute path", crate::platform::DATA_ROOT)]
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
/// Fails when native logging isn't admitted, the log folder can't be found or created,
/// or logging already started.
pub fn init(prefix: &str) -> Result<(), LoggingError> {
    crate::platform::require_native_runtime().map_err(LoggingError::NativeRuntime)?;
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
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(appender))
        .with(filter(level));
    tracing::subscriber::set_global_default(subscriber)?;
    install_panic_hook();
    Ok(())
}

/// Replaces the default panic output, which prints the payload, with the location alone.
pub fn install_panic_hook() {
    // The payload can hold any value an assertion printed, so only the location is logged.
    std::panic::set_hook(Box::new(|info| {
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()));
        tracing::error!(location = at.as_deref().unwrap_or("unknown"), "panic");
    }));
}
