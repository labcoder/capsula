//! Draw calls: complete-only arguments, closed records and each run's budgets. Every record
//! takes the same path, one at a time, through the call's guard, admission and the store.

use std::mem;
use std::sync::Arc;

use kumochi_protocol::message::{
    DataToShell, MAX_DRAW_STEPS_PER_RUN, RunId, SceneHeader, SceneId, ThreadId,
};
use kumochi_scene::{
    Admission, Begin, Budget, CallState, End, ErrorCode, FieldPath, Record, SceneState, Step,
    StepError, TokenError, Tokenizer, WireId, parse_draw_record, parse_screen_draw_record,
};
use serde::Deserialize;

use kumochi_scene::record::Surface;
use kumochi_scene::screen::without_boxes;

use super::io::{Target, Write};
use super::{Drawing, Job, LAYOUT_VERSION, Output, Pend, StepWait, Wait};
use crate::coordinator::{Effect, Stale};
use crate::engines::CallId;

/// Rejected records one run may make [guess, architecture v1 §4.5].
const MAX_REJECTED: u8 = 8;
/// Draw calls one run may make (guess).
pub(super) const MAX_CALLS_PER_RUN: usize = 16;
/// Records one call may make: a `begin`, the run's whole step budget and an `end`. The next
/// one ends the call as exhausted, after the records before it.
pub(super) const MAX_CALL_RECORDS: usize = MAX_DRAW_STEPS_PER_RUN + 2;
/// Follows a model error while a corrected call can still draw.
const RETRY_HINT: &str = "; drawn lists what this call added, and earlier steps stay as they were; correct this and call draw again";
/// Follows a model error after the call drew steps, so a correction doesn't redraw them.
const DRAWN_RETRY_HINT: &str = "; the steps in drawn are on screen, and earlier steps stay as they were; correct this by drawing only what is missing, with new step and object ids, and call draw again";
/// Follows a complete call whose arguments went on after its records array.
const AFTER_RECORDS_NOTE: &str = "; text after the records array was ignored";
/// Follows a model error once the run drew its whole step budget but still takes calls.
const FULL_HINT: &str = "; no further step can be drawn in this turn";
/// What everything on screen together can paint, in shapes a model can plan with. The draw
/// prompt says the same, and `the_paint_budget_matches_the_prompt_and_the_paint_caps` ties both
/// numbers to the paint caps.
macro_rules! paint_budget {
    () => {
        "everything on screen can hold about 40 filled circles or ellipses, or a diagram of about 30 boxes with labelled arrows"
    };
}
#[cfg(test)]
pub(super) const PAINT_BUDGET: &str = paint_budget!();
/// Follows a step too large to paint while the run can still draw and earlier steps stay shown.
const PAINT_HINT: &str = concat!(
    "; the picture stays at its last shown step; ",
    paint_budget!(),
    "; use fewer or simpler shapes and call draw again"
);
/// Follows a step too large to paint that left its scene with nothing shown, so it was removed.
const CLEARED_HINT: &str = concat!(
    "; nothing of this scene was shown, so it was removed; ",
    paint_budget!(),
    "; begin a new scene with a new name and fewer or simpler shapes"
);
/// Follows any early stop once the run accepts no further draw call.
const SPENT_HINT: &str = "; no further draw call will be accepted in this turn";
/// Follows each refusal of a screen scene, which no correction of the call can draw.
const NO_PICTURE_NOTE: &str = "; no picture of the user's screen was sent in this turn, so the screen can't be marked; explain in words or draw a card";
const OTHER_DISPLAY_NOTE: &str =
    "; the picture shows a display that can't be marked; explain in words or draw a card";
const SCREEN_OFF_NOTE: &str =
    "; the user turned off sharing the screen; explain in words or draw a card";
const SCREEN_CHANGED_NOTE: &str = "; the user's screen may have changed after the picture was taken, so these marks weren't shown and the screen can't be marked again in this turn; explain in words or draw a card";
/// Ends every result of a call in a screen scene, whose boxes are never stored.
const NOT_KEPT_NOTE: &str = "; the marks' positions are not kept after this turn";

/// Why a call ended incomplete. `Display` carries no drawing content or model id.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// A record was rejected; its own step id goes only into the tool result.
    #[error(transparent)]
    Step(StepError),
    /// The arguments weren't one well-formed envelope.
    #[error(transparent)]
    Token(TokenError),
    /// A store operation failed; the steps before it stay.
    #[error("drawing.StoreFailed")]
    Store,
    /// The run's rejected records, its repair round or its calls are spent.
    #[error("drawing.BudgetSpent")]
    Exhausted,
    /// The shell couldn't prepare a step of the call's scene.
    #[error("drawing.PreparationFailed")]
    Failed,
    /// The owner stopped; submitted writes still settle without accepting queued records.
    #[error("drawing.Interrupted")]
    Interrupted,
    /// The shell couldn't paint a step of the call's scene: it held too many shapes.
    #[error("drawing.TooMuchToPaint")]
    TooMuchToPaint {
        /// Nothing of the scene was shown, so it was removed.
        cleared: bool,
    },
    /// A screen scene, but no picture of the user's screen went with the run.
    #[error("drawing.NoScreenPicture")]
    NoScreenPicture,
    /// A screen scene, but the run's picture shows a display the overlay doesn't cover.
    #[error("drawing.OtherDisplay")]
    OtherDisplay,
    /// A screen scene, but the user turned off sharing the screen.
    #[error("drawing.ScreenOff")]
    ScreenOff,
    /// A screen scene the shell skipped, since the user's screen may have changed after the
    /// picture.
    #[error("drawing.ScreenChanged")]
    ScreenChanged,
}

/// A call that ended: the steps it drew and, unless it completed, why the rest didn't.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallEnd {
    /// The run.
    pub run: RunId,
    /// The provider's call id.
    pub call: CallId,
    /// The scene it drew in, once it had one.
    pub scene: Option<SceneId>,
    /// The step ids it committed, in order.
    pub drawn: Vec<WireId>,
    /// Why it stopped early.
    pub error: Option<CallError>,
    /// What the run still accepts.
    pub remaining: Remaining,
    /// Text after its closed records array was ignored.
    pub after_records: bool,
    /// The picture's size, when the call began a screen scene on it.
    pub picture: Option<[u32; 2]>,
}

/// What a run still accepts once one of its calls ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remaining {
    /// Calls that may draw new steps.
    Steps,
    /// Calls, but no new step: the run drew its whole step budget.
    Calls,
    /// No further draw call: the run closed or spent its rejected records, repair round or calls.
    Nothing,
}

impl CallEnd {
    /// Every record committed and the call ended properly.
    pub fn complete(&self) -> bool {
        self.error.is_none()
    }

    /// The tool result for the model: step ids, a field path and a code, never field values.
    /// It carries the model's ids, so it's never logged.
    pub fn tool_text(&self) -> String {
        let drawn: Vec<&str> = self.drawn.iter().map(WireId::as_str).collect();
        let mut text = format!("drawn: [{}]", drawn.join(", "));
        if self.after_records {
            text.push_str(AFTER_RECORDS_NOTE);
        }
        if let Some(error) = &self.error {
            let cleared =
                matches!(error, CallError::Step(step) if step.code == ErrorCode::SceneCleared);
            text.push_str("; stopped: ");
            if matches!(
                error,
                CallError::NoScreenPicture
                    | CallError::OtherDisplay
                    | CallError::ScreenOff
                    | CallError::ScreenChanged
            ) {
                text.push_str("surface: ");
            }
            text.push_str(&match error {
                CallError::Step(error) => error.tool_text(),
                error => error.to_string(),
            });
            text.push_str(match error {
                CallError::NoScreenPicture => NO_PICTURE_NOTE,
                CallError::OtherDisplay => OTHER_DISPLAY_NOTE,
                CallError::ScreenOff => SCREEN_OFF_NOTE,
                CallError::ScreenChanged => SCREEN_CHANGED_NOTE,
                _ => "",
            });
            if let (Some([width, height]), CallError::Step(step)) = (self.picture, error)
                && step.code == ErrorCode::SizeNotPicture
            {
                text = format!("{text}; the picture is {width} by {height} pixels");
            }
            text.push_str(match (self.remaining, error) {
                (Remaining::Nothing, _) => SPENT_HINT,
                (
                    Remaining::Calls,
                    CallError::Step(_) | CallError::Token(_) | CallError::TooMuchToPaint { .. },
                ) => FULL_HINT,
                (Remaining::Steps, CallError::Step(_) | CallError::Token(_))
                    if self.drawn.is_empty() || cleared =>
                {
                    RETRY_HINT
                }
                (Remaining::Steps, CallError::Step(_) | CallError::Token(_)) => DRAWN_RETRY_HINT,
                (Remaining::Steps, CallError::TooMuchToPaint { cleared: false }) => PAINT_HINT,
                (Remaining::Steps, CallError::TooMuchToPaint { cleared: true }) => CLEARED_HINT,
                _ => "",
            });
        }
        if self.picture.is_some() {
            text.push_str(NOT_KEPT_NOTE);
        }
        text
    }
}

/// One draw call of an open run.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is its own fact about the call, not one state spread over flags"
)]
pub(super) struct Call {
    pub(super) run: RunId,
    pub(super) id: CallId,
    thread: ThreadId,
    guard: CallState,
    scene: Bound,
    /// Records taken, so a repeated or skipped record is refused.
    records: u32,
    drawn: Vec<WireId>,
    /// It started after a failed call of its run: the run's one repair round.
    repair: bool,
    /// A record didn't parse or was past [`MAX_CALL_RECORDS`]: later ones are skipped unparsed.
    refused: bool,
    /// It ended; kept so a repeat of it changes nothing.
    pub(super) done: bool,
    /// The picture's size once its `begin` was a screen scene's: its scene is a screen scene.
    pub(super) picture: Option<[u32; 2]>,
    /// The lifetime index of the newest step it committed in its scene.
    pub(super) last: Option<u32>,
    /// It finished and waits for the shell to answer for this step of its scene.
    pub(super) awaits: Option<u32>,
    /// The newest step of its scene the shell prepared or showed while it was open.
    pub(super) answered: Option<u32>,
    /// The shell answered for its scene since the last [`super::READY_WAIT`] tick.
    pub(super) heard: bool,
    /// Text after its records array was ignored, told with its result.
    pub(super) after_records: bool,
    /// The shell refused one of its steps before the queued cut reached it.
    pub(super) refusal: Option<CallError>,
}

impl Call {
    /// The visible scene it drew steps in.
    pub(super) fn drew_in(&self) -> Option<SceneId> {
        match self.scene {
            Bound::Held(scene) if !self.drawn.is_empty() => Some(scene),
            Bound::None | Bound::New(_) | Bound::Held(_) => None,
        }
    }
}

/// The scene a call draws in.
enum Bound {
    /// No `begin` yet.
    None,
    /// A new scene, until its first step is written.
    New(Box<SceneState>),
    /// A visible scene.
    Held(SceneId),
}

/// One open run's draw budgets.
pub(super) struct RunLedger {
    pub(super) run: RunId,
    calls: usize,
    pub(super) steps: usize,
    /// Committed steps a failed cut then dropped; `steps` keeps them, so no budget comes back.
    refused: usize,
    rejected: u8,
    /// A call failed, so the next calls are the repair round.
    failed: bool,
    exhausted: bool,
    /// A replacement it drew failed before anything of it was shown, so this thread's earlier
    /// drawings are gone; told once the run can't repair it, and dropped when the user ended it.
    pub(super) loss: Option<ThreadId>,
    /// What its screen scenes may be drawn on; `None` while `draw` can't mark the screen.
    pub(super) canvas: Option<Result<crate::context::Geometry, crate::context::NoCanvas>>,
    pub(super) picture_recipient: Option<crate::context::PictureRecipient>,
}

impl RunLedger {
    // shortcut: a step refused after its run closed finds no ledger and adds no notice; carry
    // the count past the run's end if a late refusal shows up in practice.
    pub(super) fn drew_nothing(&self) -> bool {
        self.calls > 0 && self.steps == self.refused
    }

    pub(super) fn refuse_step(&mut self) {
        self.refused = (self.refused + 1).min(self.steps);
    }
}

/// The step id a record that didn't parse still names, for the tool result.
#[derive(Deserialize)]
struct Named {
    step: NamedStep,
}

#[derive(Deserialize)]
struct NamedStep {
    id: WireId,
}

impl Drawing {
    pub(super) fn rejected_call(
        &mut self,
        thread: ThreadId,
        run: RunId,
        id: &CallId,
        error: CallError,
        out: &mut Output,
    ) -> Result<(), Stale> {
        if let Some(at) = self.call_at(thread, run, id, out)? {
            self.fail(at, error, out);
        }
        Ok(())
    }

    /// A complete-only call: nothing runs unless the whole envelope is well formed, then its
    /// records take the streamed path in order.
    pub(super) fn complete_call(
        &mut self,
        thread: ThreadId,
        run: RunId,
        id: CallId,
        args: &[u8],
        out: &mut Output,
    ) -> Result<(), Stale> {
        if self
            .calls
            .iter()
            .any(|call| call.run == run && call.id == id)
        {
            return Err(Stale);
        }
        let Some(at) = self.call_at(thread, run, &id, out)? else {
            return Ok(());
        };
        match envelope(args) {
            Err(error) => self.fail(at, CallError::Token(error), out),
            Ok((records, after_records)) => {
                for bytes in records {
                    self.intake(at, &bytes, out);
                }
                let job = Job::Finish {
                    run,
                    call: id,
                    error: after_records.then_some(CallError::Token(TokenError::AfterRecords)),
                };
                self.enqueue(job, out);
            }
        }
        Ok(())
    }

    /// One closed record of a streamed call.
    pub(super) fn closed_record(
        &mut self,
        thread: ThreadId,
        run: RunId,
        id: &CallId,
        index: u32,
        bytes: &[u8],
        out: &mut Output,
    ) -> Result<(), Stale> {
        let taken = self
            .calls
            .iter()
            .find(|call| call.run == run && call.id == *id)
            .map_or(0, |call| call.records);
        if index != taken || self.finishing(run, id) {
            return Err(Stale);
        }
        if let Some(at) = self.call_at(thread, run, id, out)? {
            self.intake(at, bytes, out);
        }
        Ok(())
    }

    /// A streamed call's arguments ended.
    pub(super) fn call_finished(
        &mut self,
        thread: ThreadId,
        run: RunId,
        id: CallId,
        error: Option<CallError>,
        out: &mut Output,
    ) -> Result<(), Stale> {
        if matches!(error, Some(CallError::Interrupted)) {
            if !self
                .calls
                .iter()
                .any(|call| call.run == run && call.id == id)
                && self.call_at(thread, run, &id, out)?.is_none()
            {
                return Ok(());
            }
            self.interrupt(run, &id, out);
            return Ok(());
        }
        if self.finishing(run, &id) {
            return Err(Stale);
        }
        if self.call_at(thread, run, &id, out)?.is_some() {
            let job = Job::Finish {
                run,
                call: id,
                error,
            };
            self.enqueue(job, out);
        }
        Ok(())
    }

    /// The open call `id`, started if it's new. `None` when a new call is refused for its run's
    /// budget, which ends it at once. A call whose arguments ended and whose result waits for
    /// the shell takes no more of them.
    fn call_at(
        &mut self,
        thread: ThreadId,
        run: RunId,
        id: &CallId,
        out: &mut Output,
    ) -> Result<Option<usize>, Stale> {
        if run <= self.closed {
            return Err(Stale);
        }
        if let Some(at) = self
            .calls
            .iter()
            .position(|call| call.run == run && call.id == *id)
        {
            return if self.calls[at].done || self.calls[at].awaits.is_some() {
                Err(Stale)
            } else {
                Ok(Some(at))
            };
        }
        let ledger = self.ledger(run);
        if ledger.calls > MAX_CALLS_PER_RUN {
            return Err(Stale);
        }
        ledger.calls += 1;
        let (repair, exhausted) = (ledger.failed, ledger.exhausted);
        if ledger.calls > MAX_CALLS_PER_RUN {
            // Told once; the run's later calls are stale.
            out.calls.push(CallEnd {
                run,
                call: id.clone(),
                scene: None,
                drawn: Vec::new(),
                error: Some(CallError::Exhausted),
                remaining: Remaining::Nothing,
                after_records: false,
                picture: None,
            });
            return Ok(None);
        }
        self.calls.push(Call {
            run,
            id: id.clone(),
            thread,
            guard: CallState::default(),
            scene: Bound::None,
            records: 0,
            drawn: Vec::new(),
            repair,
            refused: false,
            done: false,
            picture: None,
            last: None,
            awaits: None,
            answered: None,
            heard: false,
            after_records: false,
            refusal: None,
        });
        let at = self.calls.len() - 1;
        if exhausted {
            self.end(at, Some(CallError::Exhausted), out);
            return Ok(None);
        }
        Ok(Some(at))
    }

    /// The call's arguments already ended and wait in the queue, so nothing more of it counts.
    fn finishing(&self, run: RunId, id: &CallId) -> bool {
        self.pending.as_ref().is_some_and(|pending| matches!(&pending.wait,
            Wait::Step(write) | Wait::Prepare { step: write, .. }
                if write.run == run && write.call == *id && write.interrupted.is_some()
        )) || self.queue.iter().any(|job| {
            matches!(job, Job::Finish { run: owner, call, .. } if *owner == run && call == id)
        })
    }

    /// Parses a record and queues it behind any pending write.
    fn intake(&mut self, at: usize, bytes: &[u8], out: &mut Output) {
        let run = self.calls[at].run;
        let screen = self
            .runs
            .iter()
            .any(|ledger| ledger.run == run && ledger.canvas.is_some());
        let call = &mut self.calls[at];
        call.records += 1;
        if call.refused {
            return;
        }
        let (run, id) = (call.run, call.id.clone());
        if usize::try_from(call.records).unwrap_or(usize::MAX) > MAX_CALL_RECORDS {
            call.refused = true;
            let job = Job::Reject {
                run,
                call: id,
                error: CallError::Exhausted,
            };
            self.enqueue(job, out);
            return;
        }
        let parsed = if screen {
            parse_screen_draw_record(bytes)
        } else {
            parse_draw_record(bytes)
        };
        let job = match parsed {
            Ok(record) => Job::Record {
                run,
                call: id,
                record: Box::new(record),
            },
            Err(mut error) => {
                call.refused = true;
                if error.step.is_none() {
                    error.step = serde_json::from_slice::<Named>(bytes)
                        .ok()
                        .map(|named| named.step.id);
                }
                Job::Reject {
                    run,
                    call: id,
                    error: CallError::Step(error),
                }
            }
        };
        self.enqueue(job, out);
    }

    /// Runs one record of an open call.
    pub(super) fn record(&mut self, run: RunId, id: &CallId, record: Record, out: &mut Output) {
        let Some(at) = self.open_call(run, id) else {
            return;
        };
        if self.unavailable {
            self.end(at, Some(CallError::Store), out);
            return;
        }
        if self.exhausted(run) {
            self.end(at, Some(CallError::Exhausted), out);
            return;
        }
        let record = match record {
            Record::Begin(begin) if begin.surface == Surface::Screen => {
                match self.stamped(at, begin) {
                    Ok(begin) => Record::Begin(begin),
                    Err(error) => return self.fail(at, error, out),
                }
            }
            record => record,
        };
        if let Err(error) = self.apply(at, record, out) {
            self.fail(at, CallError::Step(error), out);
        }
    }

    fn apply(&mut self, at: usize, record: Record, out: &mut Output) -> Result<(), StepError> {
        if let Record::Begin(begin) = record {
            if matches!(self.calls[at].scene, Bound::None) {
                return self.begin(at, begin, out);
            }
            return self.guard(at, &Record::Begin(begin));
        }
        self.guard(at, &record)?;
        match record {
            Record::Step(step) => self.step(at, step, out),
            Record::Begin(_) | Record::End(_) => Ok(()),
        }
    }

    /// Checks the record's place in the call against the scene's header.
    fn guard(&mut self, at: usize, record: &Record) -> Result<(), StepError> {
        let call = &mut self.calls[at];
        let header = match &call.scene {
            Bound::None => None,
            Bound::New(state) => Some(state.header()),
            Bound::Held(scene) => self
                .scenes
                .iter()
                .find(|held| held.scene == *scene && !held.gone)
                .map(|held| held.state.header()),
        };
        let Some(header) = header else {
            let code = if matches!(call.scene, Bound::None) {
                ErrorCode::BeforeBegin
            } else {
                ErrorCode::SceneCleared
            };
            let mut error = StepError::new(code);
            if let Record::Step(step) = record {
                error.step = Some(step.id.clone());
            }
            return Err(error);
        };
        call.guard.accept(record, header)
    }

    /// A call's `begin`: a held scene with this name on this surface continues with a typed-equal
    /// header; any other name is new only if the store never began it on that surface.
    fn begin(&mut self, at: usize, begin: Begin, out: &mut Output) -> Result<(), StepError> {
        let (run, thread) = (self.calls[at].run, self.calls[at].thread);
        let named = self.scenes.iter().find(|held| {
            !held.gone
                && held.thread == thread
                && held.state.header().surface == begin.surface
                && held.state.header().scene == begin.scene
        });
        if let Some(held) = named {
            let call = &mut self.calls[at];
            call.guard
                .accept(&Record::Begin(begin), held.state.header())?;
            call.scene = Bound::Held(held.scene);
            return Ok(());
        }
        let name = begin.scene.clone();
        let surface = begin.surface;
        let wait = Wait::Lookup {
            run,
            call: self.calls[at].id.clone(),
            begin: Box::new(begin),
        };
        self.write(
            wait,
            Write::Lookup {
                thread,
                surface,
                name,
            },
            out,
        );
        Ok(())
    }

    /// The store said whether the new name was ever used on this thread and surface.
    pub(super) fn looked_up(
        &mut self,
        run: RunId,
        id: &CallId,
        begin: Begin,
        found: Option<SceneId>,
        out: &mut Output,
    ) {
        let Some(at) = self.open_call(run, id) else {
            return;
        };
        let started = if found.is_some() {
            Err(StepError::new(ErrorCode::SceneCleared))
        } else {
            self.new_scene(at, begin)
        };
        if let Err(error) = started {
            self.fail(at, CallError::Step(error), out);
        }
    }

    fn new_scene(&mut self, at: usize, begin: Begin) -> Result<(), StepError> {
        let record = Record::Begin(begin.clone());
        let state = SceneState::begin(begin)?;
        let call = &mut self.calls[at];
        call.guard.accept(&record, state.header())?;
        call.scene = Bound::New(Box::new(state));
        Ok(())
    }

    /// Admits a step and writes its first-step replacements in the same durable transaction.
    #[expect(
        clippy::too_many_lines,
        reason = "the stored write and its pending settlement share one read of the run's recipient"
    )]
    fn step(&mut self, at: usize, step: Step, out: &mut Output) -> Result<(), StepError> {
        let (run, thread) = (self.calls[at].run, self.calls[at].thread);
        let existing = match &self.calls[at].scene {
            Bound::Held(scene) => Some(*scene),
            Bound::None | Bound::New(_) => None,
        };
        let full = self.ledger(run).steps >= MAX_DRAW_STEPS_PER_RUN;
        if existing.is_none() {
            if full {
                return Err(in_step(&step, ErrorCode::OverBudget(Budget::Steps)));
            }
            if self.clearing {
                let job = Job::Record {
                    run,
                    call: self.calls[at].id.clone(),
                    record: Box::new(Record::Step(step)),
                };
                self.queue.push_front(job);
                return Ok(());
            }
        }
        kumochi_scene::refuse_three_dimensional(&step)?;
        let (scene, target, clear, revision, layout_version) = if let Some(scene) = existing {
            let held = &self.scenes[self
                .live(scene)
                .ok_or_else(|| in_step(&step, ErrorCode::SceneCleared))?];
            let Admission::New(admitted) = held.state.admit_checked(&step, &self.global)? else {
                return Ok(());
            };
            if full {
                return Err(in_step(&step, ErrorCode::OverBudget(Budget::Steps)));
            }
            (
                Pend::Held { scene, admitted },
                Target::Existing(scene),
                Vec::new(),
                held.state.revision() + 1,
                held.layout_version,
            )
        } else {
            let Bound::New(state) = mem::replace(&mut self.calls[at].scene, Bound::None) else {
                return Err(in_step(&step, ErrorCode::BeforeBegin));
            };
            let header = SceneHeader {
                begin: state.header().clone(),
                layout_version: LAYOUT_VERSION,
            };
            let victims = self.replacement_victims(thread, header.begin.others);
            let states: Vec<_> = self
                .scenes
                .iter()
                .filter(|held| victims.contains(&held.scene))
                .map(|held| &held.state)
                .collect();
            let admission =
                kumochi_scene::ReplacementAdmission::checked(*state, &step, &states, &self.global)?;
            (
                Pend::New {
                    admission: Box::new(admission),
                    victims: victims.clone(),
                },
                Target::New(header),
                victims,
                1,
                LAYOUT_VERSION,
            )
        };
        let step = Arc::new(step);
        let stored = if self.calls[at].picture.is_some() {
            Arc::new(without_boxes(&step))
        } else {
            Arc::clone(&step)
        };
        let recipient = self
            .runs
            .iter()
            .find(|ledger| ledger.run == run)
            .and_then(|ledger| ledger.picture_recipient.clone());
        let write = Write::Step {
            thread,
            run,
            target,
            clear,
            revision,
            layout_version,
            step: stored,
            protected: recipient.is_some(),
        };
        let wait = StepWait {
            run,
            call: self.calls[at].id.clone(),
            thread,
            scene,
            step,
            fenced: false,
            clear_requested: false,
            loss_dropped: false,
            interrupted: None,
            laid: None,
            recipient,
        };
        self.prepare_step(wait, write, out);
        Ok(())
    }

    /// A step's write committed: the call drew it, in the scene it now holds.
    pub(super) fn drew(&mut self, run: RunId, id: &CallId, scene: SceneId, step: &Step) {
        if let Some(ledger) = self.runs.iter_mut().find(|ledger| ledger.run == run) {
            ledger.steps += 1;
        }
        let last = self
            .live(scene)
            .and_then(|at| self.scenes[at].state.revision().checked_sub(1));
        if let Some(at) = self.open_call(run, id) {
            let call = &mut self.calls[at];
            call.drawn.push(step.id.clone());
            call.scene = Bound::Held(scene);
            call.last = last.or(call.last);
        }
    }

    /// A new scene was stored. The thread's other calls that began its name on the same surface
    /// continue it, as a later call would, or are refused if their header differs.
    pub(super) fn began(&mut self, thread: ThreadId, scene: SceneId, out: &mut Output) {
        let Some(header) = self
            .live(scene)
            .map(|at| self.scenes[at].state.header().clone())
        else {
            return;
        };
        let rivals: Vec<usize> = (0..self.calls.len())
            .filter(|&at| {
                let call = &self.calls[at];
                let Bound::New(state) = &call.scene else {
                    return false;
                };
                !call.done
                    && call.thread == thread
                    && state.header().surface == header.surface
                    && state.header().scene == header.scene
            })
            .collect();
        for at in rivals {
            let Bound::New(state) = &self.calls[at].scene else {
                continue;
            };
            if state.header().continues(&header) {
                self.calls[at].scene = Bound::Held(scene);
            } else {
                let error = StepError::new(ErrorCode::HeaderMismatch);
                self.fail(at, CallError::Step(error), out);
            }
        }
    }

    /// The call's arguments ended: complete if its guard saw `end`. A closed records array, with
    /// or without text after it, stands in for a missing `end` once all its records were accepted.
    pub(super) fn finish(
        &mut self,
        run: RunId,
        id: &CallId,
        error: Option<CallError>,
        out: &mut Output,
    ) {
        let Some(at) = self.open_call(run, id) else {
            return;
        };
        let after_records = error == Some(CallError::Token(TokenError::AfterRecords));
        if (error.is_none() || after_records) && self.calls[at].guard.finish().is_err() {
            self.record(run, id, Record::End(End {}), out);
        }
        let Some(at) = self.open_call(run, id) else {
            return;
        };
        let ended = match error {
            Some(error) if !after_records => Err(error),
            _ => self.calls[at].guard.finish().map_err(CallError::Step),
        };
        match ended {
            Ok(()) => self.finished(at, after_records, out),
            Err(error) => self.fail(at, error, out),
        }
    }

    /// A pending write of the call failed.
    pub(super) fn store_failed(&mut self, run: RunId, id: &CallId, out: &mut Output) {
        if let Some(at) = self.open_call(run, id) {
            self.fail(at, CallError::Store, out);
        }
    }

    /// Ends a call early. A model error spends the run's budgets: eight rejected records, and
    /// one repair round after a failed call.
    pub(super) fn fail(&mut self, at: usize, error: CallError, out: &mut Output) {
        let (run, repair) = (self.calls[at].run, self.calls[at].repair);
        // Spent budgets end every later model error as exhausted, whichever call it is in.
        let error = match error {
            CallError::Step(_) | CallError::Token(_) if self.exhausted(run) => CallError::Exhausted,
            error => error,
        };
        if let Some(ledger) = self.runs.iter_mut().find(|ledger| ledger.run == run) {
            if matches!(error, CallError::Step(_)) {
                ledger.rejected = ledger.rejected.saturating_add(1);
            }
            if matches!(error, CallError::Step(_) | CallError::Token(_)) {
                ledger.exhausted |= repair;
                ledger.failed = true;
            }
            ledger.exhausted |= ledger.rejected >= MAX_REJECTED;
        }
        self.end(at, Some(error), out);
    }

    /// Ends a call: its scene hears whether it completed, and the coordinator gets its result.
    pub(super) fn end(&mut self, at: usize, error: Option<CallError>, out: &mut Output) {
        let scene = match self.calls[at].scene {
            Bound::Held(scene) => Some(scene),
            Bound::None | Bound::New(_) => None,
        };
        if error
            .as_ref()
            .is_some_and(|error| *error != CallError::Interrupted)
            && let Some(scene) = scene
        {
            self.replacement_failed(scene, out);
        }
        let call = &self.calls[at];
        if let Some(scene) = scene.filter(|scene| self.live(*scene).is_some()) {
            out.effects.push(Effect::Data(DataToShell::SceneEnd {
                run: call.run,
                scene,
                complete: error.is_none(),
            }));
        }
        self.settle_loss(self.calls[at].run, out);
        let remaining = self.remaining(self.calls[at].run);
        let call = &mut self.calls[at];
        call.done = true;
        out.calls.push(CallEnd {
            run: call.run,
            call: call.id.clone(),
            scene,
            drawn: mem::take(&mut call.drawn),
            error,
            remaining,
            after_records: false,
            picture: call.picture,
        });
    }

    /// Ends every open call drawing in `scene`, whose step the shell couldn't prepare.
    pub(super) fn end_calls_in(&mut self, scene: SceneId, error: &CallError, out: &mut Output) {
        for at in self.open_in(scene) {
            self.end(at, Some(error.clone()), out);
        }
    }

    /// The open calls drawing in visible scene `scene`.
    pub(super) fn open_in(&self, scene: SceneId) -> Vec<usize> {
        (0..self.calls.len())
            .filter(|&at| {
                let call = &self.calls[at];
                !call.done && matches!(call.scene, Bound::Held(bound) if bound == scene)
            })
            .collect()
    }

    /// The run ended: its calls and queued records go; this and every earlier run is stale.
    pub(super) fn interrupt(&mut self, run: RunId, id: &CallId, out: &mut Output) {
        let error = self
            .queue
            .iter()
            .find_map(|job| match job {
                Job::Finish {
                    run: owner,
                    call,
                    error: Some(error),
                }
                | Job::Reject {
                    run: owner,
                    call,
                    error,
                } if *owner == run
                    && call == id
                    && *error != CallError::Token(TokenError::AfterRecords) =>
                {
                    Some(error.clone())
                }
                _ => None,
            })
            .unwrap_or(CallError::Interrupted);
        self.end_early(run, id, error, out);
    }

    /// Ends an open call with `error`: its queued records go, and a step of it still being laid
    /// out or written ends it when that settles.
    pub(super) fn end_early(
        &mut self,
        run: RunId,
        id: &CallId,
        error: CallError,
        out: &mut Output,
    ) {
        self.queue.retain(|job| {
            !matches!(job,
                Job::Record { run: owner, call, .. } | Job::Reject { run: owner, call, .. }
                | Job::Finish { run: owner, call, .. } if *owner == run && call == id
            )
        });
        let Some(at) = self.open_call(run, id) else {
            return;
        };
        self.calls[at].refused = true;
        self.cancel_preparation(|step| step.run == run && step.call == *id);
        if let Some(super::Pending {
            wait: Wait::Step(write) | Wait::Prepare { step: write, .. },
            ..
        }) = &mut self.pending
            && write.run == run
            && write.call == *id
        {
            if write.interrupted.is_none() {
                write.interrupted = Some(error);
            }
            return;
        }
        self.end(at, Some(error), out);
    }

    pub(super) fn clear_newcomers(&mut self, out: &mut Output) {
        if let Some(super::Pending {
            wait: Wait::Step(write),
            ..
        }) = &mut self.pending
            && matches!(write.scene, Pend::New { .. })
        {
            write.clear_requested = true;
        }
        let calls: Vec<_> = self
            .calls
            .iter()
            .filter(|call| !call.done && !matches!(call.scene, Bound::Held(_)))
            .map(|call| (call.run, call.id.clone()))
            .collect();
        for (run, call) in calls {
            self.interrupt(run, &call, out);
        }
    }

    pub(super) fn close(&mut self, run: RunId, loss: super::Loss, out: &mut Output) {
        self.closed = self.closed.max(run);
        let closed = self.closed;
        let mut ended: Vec<RunId> = self
            .runs
            .iter()
            .filter(|ledger| ledger.run <= closed && ledger.loss.is_some())
            .map(|ledger| ledger.run)
            .collect();
        if loss == super::Loss::Drop && !ended.contains(&run) {
            ended.push(run);
        }
        // shortcut: older runs' losses settle with the closing run's flag, which holds while runs
        // are sequential; give each run its own once OSC-56 lets runs overlap.
        for owner in ended {
            self.end_loss(owner, loss, out);
        }
        if loss == super::Loss::Drop
            && let Some(super::Pending {
                wait: Wait::Step(write) | Wait::Prepare { step: write, .. },
                ..
            }) = &mut self.pending
            && write.run <= closed
        {
            write.loss_dropped = true;
        }
        self.release_closed(closed, out);
        let calls: Vec<_> = self
            .calls
            .iter()
            .filter(|call| call.run <= closed && !call.done)
            .map(|call| (call.run, call.id.clone()))
            .collect();
        for (run, id) in calls {
            self.interrupt(run, &id, out);
        }
        self.calls.retain(|call| call.run > closed || !call.done);
        self.runs.retain(|ledger| ledger.run > closed);
        self.queue.retain(|job| match job {
            Job::ReplyText { key, .. } => key.run > closed,
            Job::Record { run, .. } | Job::Reject { run, .. } | Job::Finish { run, .. } => {
                *run > closed
            }
            Job::Cut { .. }
            | Job::Shown { .. }
            | Job::Ready { .. }
            | Job::Unseen { .. }
            | Job::ClearVisible => true,
        });
    }

    /// `job` would write a new scene's first step.
    pub(super) fn begins_scene(&self, job: &Job) -> bool {
        let Job::Record { run, call, record } = job else {
            return false;
        };
        matches!(**record, Record::Step(_))
            && self
                .open_call(*run, call)
                .is_some_and(|at| matches!(self.calls[at].scene, Bound::New(_)))
    }

    /// The run spent its rejected records or its repair round.
    fn exhausted(&self, run: RunId) -> bool {
        self.runs
            .iter()
            .any(|ledger| ledger.run == run && ledger.exhausted)
    }

    /// What the run still accepts from its own budgets and whether it closed.
    // shortcut: the coordinator's round and tool-call budgets aren't seen here, so a refusal in a
    // run's last allowed round still invites a retry; pass them in if that shows up.
    pub(super) fn remaining(&self, run: RunId) -> Remaining {
        let ledger = self.runs.iter().find(|ledger| ledger.run == run);
        if run <= self.closed
            || ledger.is_some_and(|ledger| ledger.exhausted || ledger.calls >= MAX_CALLS_PER_RUN)
        {
            Remaining::Nothing
        } else if ledger.is_some_and(|ledger| ledger.steps >= MAX_DRAW_STEPS_PER_RUN) {
            Remaining::Calls
        } else {
            Remaining::Steps
        }
    }

    pub(super) fn open_call(&self, run: RunId, id: &CallId) -> Option<usize> {
        self.calls
            .iter()
            .position(|call| call.run == run && call.id == *id && !call.done)
    }

    pub(super) fn ledger(&mut self, run: RunId) -> &mut RunLedger {
        let at = self
            .runs
            .iter()
            .position(|ledger| ledger.run == run)
            .unwrap_or_else(|| {
                self.runs.push(RunLedger {
                    run,
                    calls: 0,
                    steps: 0,
                    refused: 0,
                    rejected: 0,
                    failed: false,
                    exhausted: false,
                    loss: None,
                    canvas: None,
                    picture_recipient: None,
                });
                self.runs.len() - 1
            });
        &mut self.runs[at]
    }
}

/// The closed records of one whole envelope and whether text followed them, or why it isn't one.
fn envelope(args: &[u8]) -> Result<(Vec<Vec<u8>>, bool), TokenError> {
    let mut tokenizer = Tokenizer::new();
    let mut input = args;
    let mut records = Vec::new();
    loop {
        match tokenizer.next(&mut input) {
            Ok(Some(record)) => records.push(record.bytes.to_vec()),
            Ok(None) => break tokenizer.finish().map(|()| (records, false)),
            Err(TokenError::AfterRecords) => break Ok((records, true)),
            Err(error) => break Err(error),
        }
    }
}

fn in_step(step: &Step, code: ErrorCode) -> StepError {
    StepError {
        step: Some(step.id.clone()),
        path: FieldPath::default(),
        code,
    }
}
