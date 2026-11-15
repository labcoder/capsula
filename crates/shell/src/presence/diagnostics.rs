//! `kumochi --input-diagnostics=<file>`: one line per input milestone, for the input-session
//! harness. Lines carry a performance-counter stamp and durations, never keys or text. A normal
//! run creates no file.

use std::fmt::Arguments;
use std::fs::File;
use std::io::Write as _;
use std::time::Duration;

use gpui_kit::{App, Global};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};

use crate::platform;

/// The diagnostics writer, kept as a GPUI global only in diagnostic runs. Its worker thread
/// writes and flushes each batch, so the UI thread only queues lines.
pub(crate) struct Diagnostics {
    writer: NonBlocking,
    guard: Option<WorkerGuard>,
    /// `--settle-after-ms=<n>`: when the settled interval starts, counted from launch.
    settle: Option<Duration>,
    /// Dock frames painted since launch.
    paints: u64,
    /// `paints` when the settled interval started.
    settled: Option<u64>,
}

impl Global for Diagnostics {}

impl Diagnostics {
    /// Reads `--input-diagnostics=<file>`, then creates or truncates the file and starts its
    /// writer thread. Called before GPUI starts, so the UI thread never touches the disk.
    ///
    /// # Errors
    /// Fails when the file can't be created.
    pub(crate) fn prepare() -> std::io::Result<Option<Self>> {
        let Some(path) = std::env::args().find_map(|arg| {
            arg.strip_prefix("--input-diagnostics=")
                .map(std::path::PathBuf::from)
        }) else {
            return Ok(None);
        };
        let settle = std::env::args()
            .find_map(|arg| arg.strip_prefix("--settle-after-ms=")?.parse().ok())
            .map(Duration::from_millis);
        let (writer, guard) = tracing_appender::non_blocking(File::create(path)?);
        Ok(Some(Self {
            writer,
            guard: Some(guard),
            settle,
            paints: 0,
            settled: None,
        }))
    }
}

/// Queues `qpc=<counter> <line>` when diagnostics are on. The queue drops lines when full
/// rather than blocking, so a stalled disk never stalls the UI.
pub(super) fn record(cx: &mut App, line: Arguments<'_>) {
    if !cx.has_global::<Diagnostics>() {
        return;
    }
    let line = format!("qpc={} {line}\n", platform::performance_counter());
    if let Err(error) = cx
        .global_mut::<Diagnostics>()
        .writer
        .write_all(line.as_bytes())
    {
        tracing::warn!(%error, "can't queue input diagnostics");
    }
}

/// Counts one dock or peek frame, for the settled-interval paint check.
pub(super) fn painted(cx: &mut App) {
    if cx.has_global::<Diagnostics>() {
        cx.global_mut::<Diagnostics>().paints += 1;
    }
}

/// When the settled interval starts, if this run checks it.
pub(super) fn settle_delay(cx: &App) -> Option<Duration> {
    cx.has_global::<Diagnostics>()
        .then(|| cx.global::<Diagnostics>().settle)
        .flatten()
}

/// Starts the settled interval: dock and peek frames from here on should be none at rest.
pub(super) fn settle(cx: &mut App) {
    if cx.has_global::<Diagnostics>() {
        let diagnostics = cx.global_mut::<Diagnostics>();
        diagnostics.settled = Some(diagnostics.paints);
    }
    record(cx, format_args!("dock settled"));
}

/// At Quit, records how many dock frames the settled interval painted, if it started.
pub(super) fn settled_paints(cx: &mut App) {
    if !cx.has_global::<Diagnostics>() {
        return;
    }
    let diagnostics = cx.global::<Diagnostics>();
    if let Some(settled) = diagnostics.settled {
        let paints = diagnostics.paints - settled;
        record(cx, format_args!("dock paints settled={paints}"));
    }
}

/// Compares the draft a typing panel was dismissed with to what it holds on reopening:
/// `[nonempty, preserved]`. An empty draft is never preserved, so it can't pass as kept.
pub(super) fn draft(saved: &str, current: &str) -> [bool; 2] {
    let nonempty = !saved.is_empty();
    [nonempty, nonempty && saved == current]
}

/// Takes the writer's guard, so Quit drops it off the UI thread: dropping it waits for the
/// worker to write what's queued.
pub(crate) fn release(cx: &mut App) -> Option<WorkerGuard> {
    cx.has_global::<Diagnostics>()
        .then(|| cx.global_mut::<Diagnostics>().guard.take())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::draft;

    #[test]
    fn only_an_unchanged_nonempty_draft_is_preserved() {
        assert_eq!(draft("", ""), [false, false]);
        assert_eq!(draft("note", "note"), [true, true]);
        assert_eq!(draft("note", ""), [true, false]);
        assert_eq!(draft("note", "not"), [true, false]);
        assert_eq!(draft("", "typed after"), [false, false]);
    }
}
