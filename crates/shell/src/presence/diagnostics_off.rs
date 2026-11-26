//! A build without the `gate-recorder` feature writes no input diagnostics. Launch refuses
//! `--input-diagnostics`, so none are ever made, and every hook here does nothing.

use std::fmt::Arguments;
use std::time::Duration;

use gpui_kit::{App, Global};
use tracing_appender::non_blocking::WorkerGuard;

/// Never made in this build.
#[derive(Debug)]
pub(crate) enum Diagnostics {}

impl Global for Diagnostics {}

impl Diagnostics {
    /// No diagnostics in this build.
    ///
    /// # Errors
    /// Never.
    #[expect(clippy::unnecessary_wraps, reason = "the recorder's own signature")]
    pub(crate) fn prepare() -> std::io::Result<Option<Self>> {
        Ok(None)
    }
}

pub(super) fn active(_: &App) -> bool {
    false
}

pub(super) fn record(_: &mut App, _: Arguments<'_>) {}

pub(super) fn settle_delay(_: &App) -> Option<Duration> {
    None
}

pub(super) fn settle(_: &mut App) {}

pub(super) fn settled_paints(_: &mut App) {}

/// Only called on a draft that diagnostics saved, which this build never does.
pub(super) fn draft(_: &str, _: &str) -> [bool; 2] {
    [false; 2]
}

pub(crate) fn release(_: &mut App) -> Option<WorkerGuard> {
    None
}
