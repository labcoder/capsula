use std::io::{self, Write};
use std::time::{Duration, Instant};

use super::signin::session;
use super::{Brain, with_cleanup};
use crate::slice2::client::{Client, Frame};
use anyhow::{Result, anyhow, ensure};
use kumochi_protocol::message::{
    AccountView, ConnectionKind, ControlToBrain, ControlToShell, DataToBrain, DataToShell, Deleted,
    ErrorKey, ModelChoice, ModelInfo, Phase, PlanState, RunId, RunOrigin, SceneId, ScreenChoice,
    ThreadId,
};

const PROMPT: &str =
    "Use draw exactly once to draw one small rectangle, with begin, one step, and end.";
const READY: Duration = Duration::from_secs(45);
const RESPONSE: Duration = Duration::from_secs(180);

pub(super) fn inner() -> Result<()> {
    let brain = Brain::start(3)?;
    let (result, sent) = session(Client::connect(), &mut io::stdout().lock(), drive);
    let cleanup = if sent { brain.quit() } else { brain.stop() };
    with_cleanup(result, cleanup)
}

#[derive(Default)]
struct Counts {
    active: Option<RunId>,
    thread: Option<ThreadId>,
    terminal_phase: Option<Phase>,
    error_key: Option<ErrorKey>,
    run_end: bool,
    deadline: Option<Instant>,
    data_frames: usize,
    data_bytes: u64,
    drawn_scenes: Vec<SceneId>,
    text_frames: usize,
    text_bytes: usize,
    steps: usize,
    complete_ends: usize,
    incomplete_ends: usize,
    scene_bytes: u64,
}

impl Counts {
    fn finished(&self) -> bool {
        self.run_end && self.terminal_phase.is_some()
    }

    fn expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn observe(&mut self, frame: &Frame) -> Option<Result<bool>> {
        match frame {
            Frame::Control(ControlToShell::Account(view)) => {
                if let Err(error) = account(view) {
                    return Some(Err(error));
                }
            }
            Frame::Control(ControlToShell::RunState {
                run,
                thread,
                phase,
                error,
                origin,
            }) => {
                if matches!(origin, Some(RunOrigin::Typed { turn: 1 })) {
                    if self.active.is_some_and(|active| active != *run) {
                        return Some(Err(anyhow!("unexpected run before typed admission")));
                    }
                    self.active = Some(*run);
                    self.thread = Some(*thread);
                }
                if self.thread.is_some()
                    && self.active == Some(*run)
                    && matches!(phase, Phase::Done | Phase::Failed | Phase::Interrupted)
                    && self.terminal_phase != Some(Phase::Failed)
                {
                    self.terminal_phase = Some(*phase);
                    self.error_key = error
                        .as_ref()
                        .map(kumochi_protocol::message::UserError::key);
                }
            }
            Frame::Data(frame, bytes) if !self.run_end => {
                self.data_frames += 1;
                self.data_bytes += u64::from(*bytes);
                let run = match frame {
                    DataToShell::AssistantText { run, .. }
                    | DataToShell::SceneStepCommit { run, .. }
                    | DataToShell::SceneEnd { run, .. }
                    | DataToShell::RunEnd { run } => Some(*run),
                    _ => None,
                };
                if self.active.is_none() {
                    self.active = run;
                }
                if run.is_some() && run == self.active {
                    self.data(frame, *bytes);
                    self.run_end = matches!(frame, DataToShell::RunEnd { .. });
                }
            }
            _ => {}
        }
        if self.thread.is_some() && self.complete_ends != 0 && self.steps != 0 {
            Some(Ok(true))
        } else if self.thread.is_some() && self.terminal_phase.is_some() && self.run_end {
            Some(Ok(false))
        } else {
            None
        }
    }

    fn data(&mut self, frame: &DataToShell, bytes: u32) {
        match frame {
            DataToShell::AssistantText { delta, .. } => {
                self.text_frames += 1;
                self.text_bytes += delta.len();
            }
            DataToShell::SceneStepCommit { scene, .. } => {
                if !self.drawn_scenes.contains(scene) {
                    self.drawn_scenes.push(*scene);
                }
                self.steps += 1;
                self.scene_bytes += u64::from(bytes);
            }
            DataToShell::SceneEnd {
                scene,
                complete: true,
                ..
            } => self.complete_ends += usize::from(self.drawn_scenes.contains(scene)),
            DataToShell::SceneEnd {
                complete: false, ..
            } => self.incomplete_ends += 1,
            _ => {}
        }
    }

    fn report(
        &self,
        out: &mut impl Write,
        status: &str,
        submitted: usize,
        schema_bytes: usize,
    ) -> Result<()> {
        writeln!(
            out,
            "status={status} inference_budget=3 submitted_turns={submitted} accepted_requests_lower={} inference_requests_upper={} draw_schema_bytes={schema_bytes} text_frames={} text_bytes={} committed_steps={} complete_draws={} incomplete_draws={} scene_bytes={} terminal_phase={:?} error_key={:?} run_end={} run_end_missing_at_deadline={} data_frames={} data_bytes={} task_delete_deferred={}",
            usize::from(self.text_bytes != 0 || self.steps != 0),
            submitted * 3,
            self.text_frames,
            self.text_bytes,
            self.steps,
            self.complete_ends,
            self.incomplete_ends,
            self.scene_bytes,
            self.terminal_phase,
            self.error_key,
            self.run_end,
            !self.run_end && self.expired(),
            self.data_frames,
            self.data_bytes,
            self.thread.is_some() && !self.run_end && !self.expired()
        )?;
        Ok(())
    }
}

fn account(view: &AccountView) -> Result<()> {
    ensure!(
        matches!(
            view,
            AccountView::SignedIn {
                plan: PlanState::Enabled,
                recovery: None,
                ..
            }
        ),
        "account unavailable or refresh/sign-in recovery required"
    );
    Ok(())
}

fn drive(client: &mut Client, out: &mut impl Write) -> Result<()> {
    let mut counts = Counts::default();
    let mut submitted = 0;
    let result = attempt(client, &mut counts, &mut submitted);
    let result = if matches!(result, Ok(true)) && !counts.finished() {
        let left = counts
            .deadline
            .expect("a submitted turn has a deadline")
            .saturating_duration_since(Instant::now());
        client
            .observe_unrecorded(left, |seen| {
                if let Some(Err(error)) = counts.observe(&seen.frame) {
                    return Some(Err(error));
                }
                counts.finished().then_some(Ok(true))
            })
            .and_then(std::convert::identity)
    } else {
        result
    };
    finish(&mut Unrecorded(client), &counts, result, out, submitted)
}

struct Unrecorded<'a>(&'a mut Client);

impl super::signin::Wire for Unrecorded<'_> {
    fn send(&mut self, message: &ControlToBrain) -> Result<()> {
        self.0.control(message).map(drop)
    }

    fn until<T>(
        &mut self,
        limit: Duration,
        mut pick: impl FnMut(&Frame) -> Option<T>,
    ) -> Result<T> {
        self.0.observe_unrecorded(limit, |seen| pick(&seen.frame))
    }
}

fn finish(
    wire: &mut impl super::signin::Wire,
    counts: &Counts,
    result: Result<bool>,
    out: &mut impl Write,
    submitted: usize,
) -> Result<()> {
    let schema_bytes = kumochi_scene::draw_schema().to_string().len();
    let status = match &result {
        Ok(true) => "complete_draw_observed",
        Ok(false) => "run_ended_without_complete_draw",
        Err(_) => "stopped_or_failed",
    };
    let report = counts.report(out, status, submitted, schema_bytes);
    let cleanup = if let Some(thread) = counts.thread.filter(|_| counts.run_end || counts.expired())
    {
        wire.send(&ControlToBrain::DeleteThread {
            thread,
            after_turns: 1,
            resend: false,
        })
        .map_err(|_| anyhow!("temporary task deletion send failed"))
        .and_then(|()| {
            wire.until(Duration::from_secs(15), |frame| match frame {
                Frame::Control(ControlToShell::ThreadDeleted {
                    thread: deleted,
                    outcome,
                    ..
                }) if *deleted == thread => Some(match outcome {
                    Deleted::Removed => Ok(()),
                    _ => Err(anyhow!("temporary task deletion was not durable")),
                }),
                _ => None,
            })
            .map_err(|_| anyhow!("temporary task deletion did not settle"))
            .and_then(std::convert::identity)
        })
    } else {
        Ok(())
    };
    with_cleanup(
        with_cleanup(
            result.and_then(|complete| {
                ensure!(complete, "no complete draw was observed");
                Ok(())
            }),
            report,
        ),
        cleanup,
    )
}

#[derive(Default)]
struct Readiness {
    account: Option<bool>,
    choice: Option<ModelChoice>,
    models: Option<Vec<ModelInfo>>,
}

impl Readiness {
    fn default_in_catalog(&self) -> bool {
        self.choice.as_ref().is_some_and(|choice| {
            self.models
                .as_ref()
                .is_some_and(|models| models.iter().any(|model| model.slug == choice.model))
        })
    }

    fn report(&self, out: &mut impl Write) -> Result<()> {
        writeln!(
            out,
            "account_seen={} account_usable={} default_set={} default_chatgpt={} catalog_seen={} default_in_catalog={}",
            self.account.is_some(),
            self.account == Some(true),
            self.choice.is_some(),
            self.choice
                .as_ref()
                .is_some_and(|choice| choice.connection == ConnectionKind::ChatGpt),
            self.models.is_some(),
            self.default_in_catalog()
        )?;
        Ok(())
    }
}

fn ready(client: &mut Client) -> Result<()> {
    let mut state = Readiness::default();
    let result = client
        .observe_unrecorded(READY, |seen| {
            let checked = match &seen.frame {
                Frame::Control(ControlToShell::Account(view)) => {
                    let checked = account(view);
                    state.account = Some(checked.is_ok());
                    checked
                }
                Frame::Control(ControlToShell::Settings { view, .. }) => {
                    state.choice.clone_from(&view.default_model);
                    if state
                        .choice
                        .as_ref()
                        .is_some_and(|choice| choice.connection != ConnectionKind::ChatGpt)
                    {
                        Err(anyhow!("default model is not ChatGPT"))
                    } else {
                        Ok(())
                    }
                }
                Frame::Data(
                    DataToShell::Models {
                        connection: ConnectionKind::ChatGpt,
                        models: catalog,
                    },
                    _,
                ) => {
                    state.models = Some(catalog.clone());
                    Ok(())
                }
                _ => Ok(()),
            };
            if let Err(error) = checked {
                return Some(Err(error));
            }
            (state.account == Some(true) && state.default_in_catalog()).then_some(Ok(()))
        })
        .and_then(std::convert::identity);
    if result.is_err() {
        state.report(&mut io::stdout().lock())?;
    }
    result
}

fn attempt(client: &mut Client, counts: &mut Counts, submitted: &mut usize) -> Result<bool> {
    ready(client)?;
    ensure!(*submitted == 0, "single-use turn already consumed");
    *submitted = 1;
    client
        .data(&DataToBrain::TextSubmit {
            turn: 1,
            screen: ScreenChoice::None,
            thread: None,
            text: PROMPT.into(),
        })
        .map_err(|_| anyhow!("single turn send failed"))?;
    counts.deadline = Some(Instant::now() + RESPONSE);
    client.observe_unrecorded(RESPONSE, |seen| counts.observe(&seen.frame))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Deletion {
        sent: Vec<(ThreadId, u32, bool)>,
        remaining: std::collections::VecDeque<Frame>,
        waits: Vec<Duration>,
    }

    impl super::super::signin::Wire for Deletion {
        fn send(&mut self, message: &ControlToBrain) -> Result<()> {
            if matches!(message, ControlToBrain::Quitting) {
                return Err(anyhow!("fixture quit send failed"));
            }
            let ControlToBrain::DeleteThread {
                thread,
                after_turns,
                resend,
            } = message
            else {
                return Err(anyhow!("only task deletion is expected"));
            };
            self.sent.push((*thread, *after_turns, *resend));
            Ok(())
        }

        fn until<T>(
            &mut self,
            limit: Duration,
            mut pick: impl FnMut(&Frame) -> Option<T>,
        ) -> Result<T> {
            self.waits.push(limit);
            while let Some(frame) = self.remaining.pop_front() {
                if let Some(value) = pick(&frame) {
                    return Ok(value);
                }
            }
            Err(anyhow!("fixture acknowledgment missing"))
        }
    }

    struct BrokenOutput;

    impl Write for BrokenOutput {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "output unavailable",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_quit_remains_failed_after_successful_drive_and_forced_stop() {
        let (result, quit_sent) = session(Ok(Deletion::default()), &mut Vec::new(), |_, _| Ok(()));
        assert!(!quit_sent);
        let stopped = Ok(());
        let error = with_cleanup(result, stopped).expect_err("failed graceful quit is retained");
        assert_eq!(
            error.to_string(),
            "couldn't send Quitting; the brain was ended"
        );
    }

    #[test]
    fn broken_output_still_deletes_a_fenced_task_and_awaits_its_acknowledgment() {
        let mut counts = admitted();
        assert_eq!(counts.observe(&end()).transpose().expect("fence"), None);
        assert_eq!(
            counts
                .observe(&control(Phase::Done, None))
                .transpose()
                .expect("terminal"),
            Some(false)
        );
        let deadline = Instant::now() + RESPONSE;
        counts.deadline = Some(deadline);
        let mut wire = Deletion::default();
        for thread in [ThreadId(9), ThreadId(2)] {
            wire.remaining
                .push_back(Frame::Control(ControlToShell::ThreadDeleted {
                    thread,
                    outcome: Deleted::Removed,
                    last_run: Some(RunId(1)),
                }));
        }
        let error = finish(&mut wire, &counts, Ok(true), &mut BrokenOutput, 1)
            .expect_err("output failure remains a failure");
        assert_eq!(wire.sent, [(ThreadId(2), 1, false)]);
        assert_eq!(wire.waits, [Duration::from_secs(15)]);
        assert!(
            wire.remaining.is_empty(),
            "matching durable acknowledgment awaited"
        );
        assert_eq!(error.to_string(), "output unavailable");
        assert_eq!(counts.deadline, Some(deadline));
    }

    #[test]
    fn broken_output_preserves_deferred_cleanup_without_a_fence_or_task_identity() {
        for (thread, run_end) in [(Some(ThreadId(2)), false), (None, true)] {
            let deadline = Instant::now() + RESPONSE;
            let counts = Counts {
                thread,
                run_end,
                deadline: Some(deadline),
                ..Counts::default()
            };
            let mut wire = Deletion::default();
            let error = finish(&mut wire, &counts, Ok(true), &mut BrokenOutput, 1)
                .expect_err("output failure remains a failure");
            assert_eq!(error.to_string(), "output unavailable");
            assert!(wire.sent.is_empty());
            assert!(wire.waits.is_empty());
            assert_eq!(counts.deadline, Some(deadline));
        }
    }

    #[test]
    fn attempt_output_and_durable_cleanup_failures_are_all_preserved() {
        let counts = Counts {
            thread: Some(ThreadId(2)),
            run_end: true,
            ..Counts::default()
        };
        let mut wire = Deletion::default();
        let error = finish(
            &mut wire,
            &counts,
            Err(anyhow!("attempt failed")),
            &mut BrokenOutput,
            1,
        )
        .expect_err("all failures remain");
        assert_eq!(wire.sent, [(ThreadId(2), 1, false)]);
        assert_eq!(wire.waits, [Duration::from_secs(15)]);
        assert_eq!(
            error.to_string(),
            "attempt failed; cleanup also failed: output unavailable; cleanup also failed: temporary task deletion did not settle"
        );
    }

    fn control(phase: Phase, key: Option<ErrorKey>) -> Frame {
        Frame::Control(ControlToShell::RunState {
            run: RunId(1),
            thread: ThreadId(2),
            phase,
            error: key.map(|key| {
                kumochi_protocol::message::UserError::new(key, Vec::new(), Vec::new())
                    .expect("closed error fixture")
            }),
            origin: (phase == Phase::Thinking).then_some(RunOrigin::Typed { turn: 1 }),
        })
    }

    fn admitted() -> Counts {
        let mut state = Counts::default();
        assert_eq!(
            state
                .observe(&control(Phase::Thinking, None))
                .transpose()
                .expect("admission"),
            None
        );
        state
    }

    fn text() -> Frame {
        Frame::Data(
            DataToShell::AssistantText {
                run: RunId(1),
                delta: "private-marker".into(),
            },
            100,
        )
    }

    fn end() -> Frame {
        Frame::Data(DataToShell::RunEnd { run: RunId(1) }, 20)
    }

    #[test]
    fn terminal_before_data_waits_for_the_data_fence() {
        let mut state = admitted();
        assert_eq!(
            state
                .observe(&control(Phase::Done, None))
                .transpose()
                .expect("terminal"),
            None
        );
        assert_eq!(state.observe(&text()).transpose().expect("data"), None);
        assert_eq!(
            state.observe(&end()).transpose().expect("fence"),
            Some(false)
        );
        assert_eq!(state.text_frames, 1);
        assert_eq!(state.text_bytes, 14);
        assert_eq!(state.data_frames, 2);
        assert_eq!(state.data_bytes, 120);
        assert_eq!(state.terminal_phase, Some(Phase::Done));
        assert_eq!(
            state.observe(&text()).transpose().expect("past fence"),
            Some(false)
        );
        assert_eq!(state.data_frames, 2);
        assert_eq!(state.text_bytes, 14);
    }

    #[test]
    fn data_before_terminal_waits_for_the_terminal_metadata() {
        let mut state = admitted();
        assert_eq!(state.observe(&text()).transpose().expect("data"), None);
        assert_eq!(state.observe(&end()).transpose().expect("fence"), None);
        assert_eq!(
            state
                .observe(&control(Phase::Done, None))
                .transpose()
                .expect("terminal"),
            Some(false)
        );
        assert_eq!(state.text_frames, 1);
        assert_eq!(state.text_bytes, 14);
        assert_eq!(state.data_frames, 2);
        assert_eq!(state.data_bytes, 120);
    }

    #[test]
    fn failed_without_payload_keeps_the_closed_error_key_until_run_end() {
        let mut state = admitted();
        assert_eq!(
            state
                .observe(&control(Phase::Failed, Some(ErrorKey::RequestRefused)))
                .transpose()
                .expect("failure"),
            None
        );
        assert_eq!(
            state
                .observe(&control(Phase::Done, None))
                .transpose()
                .expect("later terminal"),
            None
        );
        assert_eq!(
            state.observe(&end()).transpose().expect("fence"),
            Some(false)
        );
        assert_eq!(state.terminal_phase, Some(Phase::Failed));
        assert_eq!(state.error_key, Some(ErrorKey::RequestRefused));
        assert_eq!(state.text_frames, 0);
        assert_eq!(state.steps, 0);
        assert_eq!(state.data_frames, 1);
        assert_eq!(state.data_bytes, 20);
        let mut out = Vec::new();
        state.report(&mut out, "fixture", 1, 123).expect("report");
        assert_eq!(
            String::from_utf8(out).expect("UTF-8"),
            "status=fixture inference_budget=3 submitted_turns=1 accepted_requests_lower=0 inference_requests_upper=3 draw_schema_bytes=123 text_frames=0 text_bytes=0 committed_steps=0 complete_draws=0 incomplete_draws=0 scene_bytes=0 terminal_phase=Some(Failed) error_key=Some(RequestRefused) run_end=true run_end_missing_at_deadline=false data_frames=1 data_bytes=20 task_delete_deferred=false\n"
        );
    }

    #[test]
    fn a_missing_run_end_is_reported_only_after_the_deadline() {
        let mut state = admitted();
        state.deadline = Some(Instant::now() + Duration::from_secs(60));
        let mut before = Vec::new();
        state
            .report(&mut before, "fixture", 1, 123)
            .expect("report");
        assert!(String::from_utf8(before).expect("UTF-8").ends_with("run_end_missing_at_deadline=false data_frames=0 data_bytes=0 task_delete_deferred=true\n"));
        state.deadline = Some(Instant::now());
        let mut after = Vec::new();
        state.report(&mut after, "fixture", 1, 123).expect("report");
        assert!(String::from_utf8(after).expect("UTF-8").ends_with("run_end_missing_at_deadline=true data_frames=0 data_bytes=0 task_delete_deferred=false\n"));
    }

    #[test]
    fn complete_draw_keeps_the_early_acceptance_return() {
        let mut state = admitted();
        let kumochi_scene::Record::Step(step) =
            kumochi_scene::parse_record(br#"{"step":{"id":"s"}}"#).expect("step fixture")
        else {
            panic!("step fixture");
        };
        let step = Frame::Data(
            DataToShell::SceneStepCommit {
                run: RunId(1),
                scene: SceneId(1),
                index: 0,
                step,
            },
            100,
        );
        let complete = Frame::Data(
            DataToShell::SceneEnd {
                run: RunId(1),
                scene: SceneId(1),
                complete: true,
            },
            20,
        );
        assert_eq!(state.observe(&step).transpose().expect("step"), None);
        assert_eq!(
            state.observe(&complete).transpose().expect("complete draw"),
            Some(true)
        );
        assert!(!state.run_end);
        assert_eq!(state.terminal_phase, None);
        assert_eq!(state.data_frames, 2);
        assert_eq!(state.data_bytes, 120);
        assert!(!state.finished());
        state.observe(&end()).expect("fence").expect("draw");
        assert!(!state.finished());
        state
            .observe(&control(Phase::Done, None))
            .expect("terminal")
            .expect("draw");
        assert!(state.finished());
    }

    #[test]
    fn readiness_reports_only_presence_and_usability_booleans() {
        let mut state = Readiness::default();
        let mut out = Vec::new();
        state.report(&mut out).expect("report");
        assert_eq!(
            String::from_utf8(out).expect("UTF-8"),
            "account_seen=false account_usable=false default_set=false default_chatgpt=false catalog_seen=false default_in_catalog=false\n"
        );
        state.account = Some(true);
        state.choice = Some(ModelChoice {
            model: kumochi_protocol::message::ModelSlug::new("private-marker").expect("slug"),
            effort: kumochi_protocol::message::Effort::Low,
            connection: ConnectionKind::ChatGpt,
        });
        state.models = Some(vec![ModelInfo {
            slug: kumochi_protocol::message::ModelSlug::new("private-marker").expect("slug"),
            display_name: kumochi_protocol::message::ModelName::new("private-marker")
                .expect("name"),
        }]);
        let mut out = Vec::new();
        state.report(&mut out).expect("report");
        assert_eq!(
            String::from_utf8(out).expect("UTF-8"),
            "account_seen=true account_usable=true default_set=true default_chatgpt=true catalog_seen=true default_in_catalog=true\n"
        );
    }

    #[test]
    fn reporting_keeps_only_counts_and_lengths() {
        let mut counts = Counts::default();
        counts.data(
            &DataToShell::AssistantText {
                run: RunId(1),
                delta: "private-marker".into(),
            },
            100,
        );
        counts.data(
            &DataToShell::SceneEnd {
                run: RunId(1),
                scene: kumochi_protocol::message::SceneId(1),
                complete: true,
            },
            20,
        );
        let mut out = Vec::new();
        counts.report(&mut out, "fixture", 1, 123).expect("report");
        assert_eq!(
            String::from_utf8(out).expect("UTF-8"),
            "status=fixture inference_budget=3 submitted_turns=1 accepted_requests_lower=1 inference_requests_upper=3 draw_schema_bytes=123 text_frames=1 text_bytes=14 committed_steps=0 complete_draws=0 incomplete_draws=0 scene_bytes=0 terminal_phase=None error_key=None run_end=false run_end_missing_at_deadline=false data_frames=0 data_bytes=0 task_delete_deferred=false\n"
        );
    }

    #[test]
    fn a_complete_draw_must_end_the_scene_that_committed_a_step() {
        let kumochi_scene::Record::Step(step) =
            kumochi_scene::parse_record(br#"{"step":{"id":"s"}}"#).expect("step fixture")
        else {
            panic!("step fixture");
        };
        let mut counts = Counts::default();
        counts.data(
            &DataToShell::SceneStepCommit {
                run: RunId(1),
                scene: SceneId(1),
                index: 0,
                step,
            },
            100,
        );
        counts.data(
            &DataToShell::SceneEnd {
                run: RunId(1),
                scene: SceneId(2),
                complete: true,
            },
            20,
        );
        assert_eq!(counts.complete_ends, 0);
        counts.data(
            &DataToShell::SceneEnd {
                run: RunId(1),
                scene: SceneId(1),
                complete: true,
            },
            20,
        );
        assert_eq!(counts.complete_ends, 1);
        assert_eq!(counts.steps, 1);
        assert_eq!(counts.scene_bytes, 100);
    }

    #[test]
    fn signout_and_refresh_recovery_refuse_before_a_turn() {
        assert!(
            account(&AccountView::SignedOut {
                revocation_unconfirmed: false
            })
            .is_err()
        );
        assert!(
            account(&AccountView::SignedIn {
                email: None,
                plan: PlanState::Enabled,
                recovery: Some(kumochi_protocol::message::AccountRecovery::Retry)
            })
            .is_err()
        );
    }
}
