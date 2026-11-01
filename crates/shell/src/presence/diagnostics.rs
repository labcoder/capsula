//! `capsula --input-diagnostics=<file>`: one line per input milestone, for the input-session
//! harness. Lines carry a performance-counter stamp and durations, never keys or text. A normal
//! run creates no file.

use std::fmt::Arguments;
use std::fs::File;
use std::io::Write as _;
use std::time::Instant;

use gpui_kit::{App, Global};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};

use crate::input::{Effect, Gesture};
use crate::platform;

/// The Listening frame still waiting for its paint. It's set only while Listening is really
/// shown, so a press that ends or is superseded before the paint reports no paint at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PendingPaint(Option<Instant>);

impl PendingPaint {
    /// Follows one applied effect against the gesture's current state.
    pub(super) fn follow(&mut self, effect: Effect, gesture: &Gesture, at: Instant) {
        match effect {
            Effect::Listening if gesture.listening() => self.0 = Some(at),
            Effect::Listening | Effect::Resting => self.0 = None,
            Effect::HoldStarted | Effect::HoldEnded | Effect::OpenInput => {}
        }
    }

    /// The Listening time to report from this paint, once.
    pub(super) fn take(&mut self) -> Option<Instant> {
        self.0.take()
    }
}

/// The diagnostics writer, kept as a GPUI global only in diagnostic runs. Its worker thread
/// writes and flushes each batch, so the UI thread only queues lines.
pub(crate) struct Diagnostics {
    writer: NonBlocking,
    guard: Option<WorkerGuard>,
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
        let (writer, guard) = tracing_appender::non_blocking(File::create(path)?);
        Ok(Some(Self {
            writer,
            guard: Some(guard),
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

/// Takes the writer's guard, so Quit drops it off the UI thread: dropping it waits for the
/// worker to write what's queued.
pub(crate) fn release(cx: &mut App) -> Option<WorkerGuard> {
    cx.has_global::<Diagnostics>()
        .then(|| cx.global_mut::<Diagnostics>().guard.take())
        .flatten()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::PendingPaint;
    use crate::input::{Edge, Effect, Event, Gesture, Key};

    fn listening(at: Instant) -> Gesture {
        let mut gesture = Gesture::default();
        for key in [Key::LeftControl, Key::LeftAlt] {
            gesture.handle(
                Event::Key {
                    key,
                    edge: Edge::Down,
                },
                at,
            );
        }
        assert!(gesture.listening(), "the chord listens");
        gesture
    }

    #[test]
    fn a_press_that_rests_before_the_paint_reports_no_paint() {
        let at = Instant::now();
        let mut pending = PendingPaint::default();
        pending.follow(Effect::Listening, &listening(at), at);
        pending.follow(Effect::Resting, &Gesture::default(), at);
        assert_eq!(pending.take(), None);
    }

    #[test]
    fn a_superseded_listening_reports_no_paint() {
        let mut pending = PendingPaint::default();
        pending.follow(Effect::Listening, &Gesture::default(), Instant::now());
        assert_eq!(pending.take(), None);
    }

    #[test]
    fn a_shown_listening_reports_one_paint() {
        let at = Instant::now();
        let gesture = listening(at);
        let mut pending = PendingPaint::default();
        pending.follow(Effect::Listening, &gesture, at);
        pending.follow(Effect::HoldStarted, &gesture, at);
        assert_eq!(pending.take(), Some(at));
        assert_eq!(pending.take(), None);
    }
}
