//! The fixture engine: replays a recorded script of text and steps with delays, so runs stream
//! across the real pipes with no network.
//!
//! One line per event: `<ms> text <words>`, `<ms> fill <bytes>` (that many letters, for
//! saturation tests), `<ms> step <label>`, `<ms> fail`, and `late <label>`: a step the engine
//! still finishes after it's cancelled, as a slow tool call would. Blank lines and `#` comments
//! are skipped. The run is done after the last line.

use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::message::{RunId, Step};
use tokio::sync::{mpsc, oneshot};

use crate::coordinator::{EngineEvent, Input, MAX_TEXT};

/// The script runs use unless a dev build names another in [`constants::FIXTURE_ENV`].
///
/// [`constants::FIXTURE_ENV`]: kumochi_protocol::constants::FIXTURE_ENV
pub(crate) const DEFAULT: &str = include_str!("../../fixtures/default.txt");

/// A script line that doesn't parse.
#[derive(Debug, thiserror::Error)]
#[error("fixture line {line} isn't an event")]
pub(crate) struct FixtureError {
    /// The 1-based line number.
    pub(crate) line: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Line {
    Text(Duration, String),
    Step(Duration, String),
    Fail(Duration),
}

/// A parsed script.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Fixture {
    lines: Vec<Line>,
    late: Option<String>,
}

impl Fixture {
    /// Parses a script.
    ///
    /// # Errors
    /// Fails on the first line that isn't an event.
    pub(crate) fn parse(source: &str) -> Result<Self, FixtureError> {
        let mut fixture = Self {
            lines: Vec::new(),
            late: None,
        };
        for (index, raw) in source.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = FixtureError { line: index + 1 };
            if let Some(label) = line.strip_prefix("late ") {
                fixture.late = Some(label.to_owned());
                continue;
            }
            let (ms, rest) = line.split_once(' ').unwrap_or((line, ""));
            let after =
                Duration::from_millis(ms.parse().map_err(|_| FixtureError { line: index + 1 })?);
            let (kind, arg) = rest.split_once(' ').unwrap_or((rest, ""));
            fixture.lines.push(match kind {
                "text" => Line::Text(after, arg.to_owned()),
                "fill" => {
                    let bytes: usize = arg.parse().map_err(|_| bad)?;
                    if bytes > MAX_TEXT {
                        return Err(FixtureError { line: index + 1 });
                    }
                    Line::Text(after, "x".repeat(bytes))
                }
                "step" => Line::Step(after, arg.to_owned()),
                "fail" => Line::Fail(after),
                _ => return Err(bad),
            });
        }
        Ok(fixture)
    }
}

/// Plays `fixture` for `run`, reporting to `inputs`, and waits while `inputs` is full. Sending
/// on (or dropping) the returned sender cancels it.
pub(crate) fn start(
    fixture: Arc<Fixture>,
    run: RunId,
    inputs: mpsc::Sender<Input>,
) -> oneshot::Sender<()> {
    let (cancel, cancelled) = oneshot::channel();
    tokio::spawn(play(fixture, run, inputs, cancelled));
    cancel
}

async fn play(
    fixture: Arc<Fixture>,
    run: RunId,
    inputs: mpsc::Sender<Input>,
    mut cancelled: oneshot::Receiver<()>,
) {
    for line in &fixture.lines {
        let (after, event) = match line {
            Line::Text(after, text) => (*after, EngineEvent::Text(text.clone())),
            Line::Step(after, label) => (
                *after,
                EngineEvent::Step(Step {
                    label: label.clone(),
                }),
            ),
            Line::Fail(after) => (*after, EngineEvent::Failed),
        };
        let failed = event == EngineEvent::Failed;
        if !emit(
            &inputs,
            run,
            after,
            event,
            &mut cancelled,
            fixture.late.as_deref(),
        )
        .await
            || failed
        {
            return;
        }
    }
    let done = EngineEvent::Done;
    emit(
        &inputs,
        run,
        Duration::ZERO,
        done,
        &mut cancelled,
        fixture.late.as_deref(),
    )
    .await;
}

/// Waits `after`, then sends `event`. False when the run was cancelled first, after sending its
/// late step, or when nobody listens.
async fn emit(
    inputs: &mpsc::Sender<Input>,
    run: RunId,
    after: Duration,
    event: EngineEvent,
    cancelled: &mut oneshot::Receiver<()>,
    late: Option<&str>,
) -> bool {
    let sent = async {
        tokio::time::sleep(after).await;
        inputs.send(Input::Engine { run, event }).await.is_ok()
    };
    tokio::select! {
        sent = sent => sent,
        _ = cancelled => {
            if let Some(label) = late {
                let step = EngineEvent::Step(Step { label: label.to_owned() });
                // Never waits: a cancelled run's leftovers aren't worth holding a task for.
                let _dropped = inputs.try_send(Input::Engine { run, event: step });
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use kumochi_protocol::message::{RunId, Step};
    use tokio::sync::mpsc;

    use super::{DEFAULT, Fixture, Line, start};
    use crate::coordinator::{EngineEvent, Input};

    #[test]
    fn scripts_parse_and_bad_lines_are_named() {
        let fixture =
            Fixture::parse("# c\n\n5 text hi there\n0 fill 3\n7 step a b\nlate z\n1 fail\n")
                .expect("parses");
        assert_eq!(
            fixture.lines,
            vec![
                Line::Text(Duration::from_millis(5), "hi there".to_owned()),
                Line::Text(Duration::ZERO, "xxx".to_owned()),
                Line::Step(Duration::from_millis(7), "a b".to_owned()),
                Line::Fail(Duration::from_millis(1)),
            ]
        );
        assert_eq!(fixture.late.as_deref(), Some("z"));
        for bad in ["x text", "5 shout", "1 fill many", "1 fill 1048577"] {
            assert_eq!(
                Fixture::parse(&format!("0 text ok\n{bad}"))
                    .map(|_| ())
                    .unwrap_err()
                    .line,
                2
            );
        }
        Fixture::parse(DEFAULT).expect("the default script parses");
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_run_still_finishes_its_late_step_and_nothing_else() {
        let fixture =
            Arc::new(Fixture::parse("10 text a\n1000 step never\nlate closed").expect("parses"));
        let (inputs, mut events) = mpsc::channel(4);
        let cancel = start(fixture, RunId(3), inputs);
        let first = events.recv().await;
        assert_eq!(
            first,
            Some(Input::Engine {
                run: RunId(3),
                event: EngineEvent::Text("a".to_owned())
            })
        );
        cancel.send(()).expect("still playing");
        assert_eq!(
            events.recv().await,
            Some(Input::Engine {
                run: RunId(3),
                event: EngineEvent::Step(Step {
                    label: "closed".to_owned()
                })
            })
        );
        assert_eq!(events.recv().await, None, "the engine ended");
    }

    #[tokio::test(start_paused = true)]
    async fn a_script_ends_with_done() {
        let fixture = Arc::new(Fixture::parse("1 step s").expect("parses"));
        let (inputs, mut events) = mpsc::channel(4);
        let _cancel = start(fixture, RunId(1), inputs);
        assert_eq!(
            events.recv().await,
            Some(Input::Engine {
                run: RunId(1),
                event: EngineEvent::Step(Step {
                    label: "s".to_owned()
                })
            })
        );
        assert_eq!(
            events.recv().await,
            Some(Input::Engine {
                run: RunId(1),
                event: EngineEvent::Done
            })
        );
    }
}
