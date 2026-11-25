//! The fixture engine: replays a recorded script of text and steps with delays, so runs stream
//! across the real pipes with no network.
//!
//! One line per event: `<ms> text <words>`, `<ms> fill <bytes>` (that many letters, for
//! saturation tests), `<ms> step <label>`, `<ms> fail`, and `late <label>`: a step the engine
//! still sends after it's cancelled, as a slow tool call would; the round's pump has closed by
//! then, so it reaches nobody. Blank lines and `#` comments are skipped. The round completes
//! after the last line.
//!
//! shortcut: steps stream as one `draw` call of newline-terminated labels: the first step starts
//! it, each step is one argument delta, and the script's end readies it with the joined lines.
//! Slice 4's B3 replaces this with recorded draw arguments.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::{
    CallId, Engine, EngineFailure, Finish, Round, RunRequest, SendGate, ToolName, TurnEvent,
};
use crate::coordinator::MAX_TEXT;

/// A sample script, which a dev build plays when [`constants::FIXTURE_ENV`] names its file.
///
/// [`constants::FIXTURE_ENV`]: kumochi_protocol::constants::FIXTURE_ENV
#[cfg(test)]
pub(crate) const DEFAULT: &str = include_str!("../../fixtures/default.txt");

/// The id of the fixture's one draw call.
const CALL: &str = "fixture-1";

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
    lines: Arc<[Line]>,
    late: Option<String>,
}

impl Fixture {
    /// Parses a script.
    ///
    /// # Errors
    /// Fails on the first line that isn't an event.
    pub(crate) fn parse(source: &str) -> Result<Self, FixtureError> {
        let mut lines = Vec::new();
        let mut late = None;
        for (index, raw) in source.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = FixtureError { line: index + 1 };
            if let Some(label) = line.strip_prefix("late ") {
                late = Some(label.to_owned());
                continue;
            }
            let (ms, rest) = line.split_once(' ').unwrap_or((line, ""));
            let after =
                Duration::from_millis(ms.parse().map_err(|_| FixtureError { line: index + 1 })?);
            let (kind, arg) = rest.split_once(' ').unwrap_or((rest, ""));
            lines.push(match kind {
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
        Ok(Self {
            lines: lines.into(),
            late,
        })
    }
}

impl Engine for Fixture {
    fn start(
        &self,
        _request: RunRequest,
        events: mpsc::Sender<TurnEvent>,
        _gate: SendGate,
    ) -> Round {
        let out = Outbox {
            events,
            late: self.late.clone(),
        };
        Box::pin(play(Arc::clone(&self.lines), out))
    }
}

/// The round's sender. Dropped before the round ends, it still sends the late step.
struct Outbox {
    events: mpsc::Sender<TurnEvent>,
    late: Option<String>,
}

impl Outbox {
    /// False when nobody listens.
    async fn send(&self, event: TurnEvent) -> bool {
        self.events.send(event).await.is_ok()
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        if let Some(label) = self.late.take() {
            // Never waits: a cancelled round's leftovers aren't worth holding a task for.
            let _dropped = self.events.try_send(step(&label));
        }
    }
}

fn call() -> CallId {
    CallId::new(CALL).expect("the fixture's call id is short ASCII")
}

/// One step, as the draw call's next argument delta.
fn step(label: &str) -> TurnEvent {
    TurnEvent::ToolArgsDelta {
        call: call(),
        delta: format!("{label}\n"),
    }
}

async fn play(lines: Arc<[Line]>, mut out: Outbox) {
    let mut args = String::new();
    for line in lines.iter() {
        let (Line::Text(after, _) | Line::Step(after, _) | Line::Fail(after)) = line;
        tokio::time::sleep(*after).await;
        let sent = match line {
            Line::Text(_, text) => out.send(TurnEvent::TextDelta(text.clone())).await,
            Line::Step(_, label) => {
                let started = !args.is_empty()
                    || out
                        .send(TurnEvent::ToolCallStarted {
                            call: call(),
                            name: ToolName::Draw,
                        })
                        .await;
                args.push_str(label);
                args.push('\n');
                started && out.send(step(label)).await
            }
            Line::Fail(_) => {
                out.late = None;
                let failed = Finish::Failed(EngineFailure::Provider);
                out.send(TurnEvent::Finished(failed)).await;
                return;
            }
        };
        if !sent {
            return;
        }
    }
    if !args.is_empty()
        && !out
            .send(TurnEvent::ToolCallReady { call: call(), args })
            .await
    {
        return;
    }
    out.send(TurnEvent::Finished(Finish::Completed)).await;
    out.late = None;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kumochi_protocol::message::{ControlToShell, Phase, RunId, Step, ThreadId};
    use tokio::sync::mpsc;

    use super::{DEFAULT, Fixture, Line, call, step};
    use crate::context::Built;
    use crate::coordinator::{BuildKey, ContextBuilt, Coordinator, Effect, Input, StoreOp};
    use crate::engines::{
        Engine, EngineFailure, Finish, RunRequest, SendGate, ToolName, TurnEvent, start,
    };
    use crate::store::SceneTarget;

    fn request() -> RunRequest {
        RunRequest::for_tests(RunId(3), ThreadId(1), "go")
    }

    fn gate() -> SendGate {
        SendGate::closed(request().key)
    }

    #[test]
    fn scripts_parse_and_bad_lines_are_named() {
        let fixture =
            Fixture::parse("# c\n\n5 text hi there\n0 fill 3\n7 step a b\nlate z\n1 fail\n")
                .expect("parses");
        assert_eq!(
            fixture.lines.to_vec(),
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
        let fixture = Fixture::parse("10 text a\n1000 step never\nlate closed").expect("parses");
        let (events_tx, mut events) = mpsc::channel(4);
        let round = tokio::spawn(fixture.start(request(), events_tx, gate()));
        assert_eq!(
            events.recv().await,
            Some(TurnEvent::TextDelta("a".to_owned()))
        );
        round.abort();
        assert_eq!(events.recv().await, Some(step("closed")));
        assert_eq!(events.recv().await, None, "the engine ended");
    }

    #[tokio::test(start_paused = true)]
    async fn a_script_ends_with_done() {
        let fixture = Fixture::parse("1 step s\n1 step t").expect("parses");
        let (events_tx, mut events) = mpsc::channel(8);
        tokio::spawn(fixture.start(request(), events_tx, gate()));
        let mut seen = Vec::new();
        while let Some(event) = events.recv().await {
            seen.push(event);
        }
        assert_eq!(
            seen,
            [
                TurnEvent::ToolCallStarted {
                    call: call(),
                    name: ToolName::Draw
                },
                step("s"),
                step("t"),
                TurnEvent::ToolCallReady {
                    call: call(),
                    args: "s\nt\n".to_owned()
                },
                TurnEvent::Finished(Finish::Completed),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_script_fails_its_round_and_sends_no_late_step() {
        let fixture = Fixture::parse("0 text a\n0 fail\n0 text never\nlate z").expect("parses");
        let (events_tx, mut events) = mpsc::channel(8);
        tokio::spawn(fixture.start(request(), events_tx, gate()));
        let mut seen = Vec::new();
        while let Some(event) = events.recv().await {
            seen.push(event);
        }
        assert_eq!(
            seen,
            [
                TurnEvent::TextDelta("a".to_owned()),
                TurnEvent::Finished(Finish::Failed(EngineFailure::Provider)),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_step_is_committed_before_the_call_is_ready() {
        let fixture = Fixture::parse("0 step first\n1000 step second").expect("parses");
        let (inputs, mut received) = mpsc::channel(16);
        let _cancel = start(&fixture, request(), inputs, gate());
        let mut core = Coordinator::ready_for_tests(ThreadId(1), ThreadId(1), RunId(2), Vec::new());
        core.handle(Input::Turn {
            turn: 1,
            thread: Some(ThreadId(1)),
            text: "go".into(),
        })
        .expect("a turn is never stale");
        core.handle(Input::TurnStored { run: RunId(3) })
            .expect("current");
        let admitted = Input::ContextBuilt(ContextBuilt {
            key: BuildKey {
                run: RunId(3),
                round: 1,
                attempt: 1,
            },
            snapshot: None,
            result: Ok(Built {
                recovered_context: None,
                request: request().request,
                sources: vec![ThreadId(1)],
            }),
        });
        core.handle(admitted).expect("admitted");
        let started = received.recv().await.expect("the call starts");
        assert_eq!(core.handle(started), Ok(Vec::new()));
        let first = received.recv().await.expect("the first step");
        assert_eq!(
            core.handle(first),
            Ok(vec![
                Effect::Control(ControlToShell::RunState {
                    origin: None,
                    run: RunId(3),
                    thread: ThreadId(1),
                    phase: Phase::Answering,
                    error: None,
                }),
                Effect::Store(StoreOp::CommitStep {
                    thread: ThreadId(1),
                    run: RunId(3),
                    text: None,
                    scene: SceneTarget::New { evict: None },
                    index: 0,
                    step: Step {
                        label: "first".to_owned(),
                    },
                }),
            ])
        );
        assert!(
            received.is_empty(),
            "the second step and the ready call are still a second away"
        );
    }
}
