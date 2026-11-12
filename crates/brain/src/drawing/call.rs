//! Draw calls: complete-only arguments, closed records and each run's budgets. Every record
//! takes the same path, one at a time, through the call's guard, admission and the store.

use std::mem;
use std::sync::Arc;

use kumochi_protocol::message::{
    DataToShell, MAX_DRAW_STEPS_PER_RUN, RunId, SceneHeader, SceneId, ThreadId,
};
use kumochi_scene::{
    Admission, Begin, Budget, CallState, ErrorCode, FieldPath, Record, SceneState, Step, StepError,
    TokenError, Tokenizer, WireId, parse_draw_record,
};
use serde::Deserialize;

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
/// Follows a model error while the run can still make its repair call.
const RETRY_HINT: &str = "; steps not in drawn were not shown; correct this and call draw again";
/// Follows any early stop once the run accepts no further draw call.
const SPENT_HINT: &str = "; no further draw call will be accepted in this turn";

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
    /// The run accepts no further draw call.
    pub spent: bool,
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
        if let Some(error) = &self.error {
            text.push_str("; stopped: ");
            text.push_str(&match error {
                CallError::Step(error) => error.tool_text(),
                error => error.to_string(),
            });
            if self.spent {
                text.push_str(SPENT_HINT);
            } else if matches!(error, CallError::Step(_) | CallError::Token(_)) {
                text.push_str(RETRY_HINT);
            }
        }
        text
    }
}

/// One draw call of an open run.
pub(super) struct Call {
    pub(super) run: RunId,
    id: CallId,
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
    steps: usize,
    rejected: u8,
    /// A call failed, so the next calls are the repair round.
    failed: bool,
    exhausted: bool,
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
            Ok(records) => {
                for bytes in records {
                    self.intake(at, &bytes, out);
                }
                let job = Job::Finish {
                    run,
                    call: id,
                    error: None,
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
    /// budget, which ends it at once.
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
            return if self.calls[at].done {
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
                spent: true,
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
            Wait::Step(write) if write.run == run && write.call == *id && write.interrupted.is_some()
        )) || self.queue.iter().any(|job| {
            matches!(job, Job::Finish { run: owner, call, .. } if *owner == run && call == id)
        })
    }

    /// Parses a record and queues it behind any pending write.
    fn intake(&mut self, at: usize, bytes: &[u8], out: &mut Output) {
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
        let job = match parse_draw_record(bytes) {
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

    /// A call's `begin`: a scene this thread holds continues with a typed-equal header; any
    /// other name is new only if the store never began it, cleared or not.
    fn begin(&mut self, at: usize, begin: Begin, out: &mut Output) -> Result<(), StepError> {
        let (run, thread) = (self.calls[at].run, self.calls[at].thread);
        let named = self.scenes.iter().find(|held| {
            !held.gone && held.thread == thread && held.state.header().scene == begin.scene
        });
        if let Some(held) = named {
            let call = &mut self.calls[at];
            call.guard
                .accept(&Record::Begin(begin), held.state.header())?;
            call.scene = Bound::Held(held.scene);
            return Ok(());
        }
        let name = begin.scene.clone();
        let wait = Wait::Lookup {
            run,
            call: self.calls[at].id.clone(),
            begin: Box::new(begin),
        };
        self.write(wait, Write::Lookup { thread, name }, out);
        Ok(())
    }

    /// The store said whether the call's new scene name was ever used in its thread.
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
        let write = Write::Step {
            thread,
            run,
            target,
            clear,
            revision,
            layout_version,
            step: Arc::clone(&step),
        };
        let wait = StepWait {
            run,
            call: self.calls[at].id.clone(),
            thread,
            scene,
            step,
            fenced: false,
            clear_requested: false,
            interrupted: None,
        };
        self.write(Wait::Step(Box::new(wait)), write, out);
        Ok(())
    }

    /// A step's write committed: the call drew it, in the scene it now holds.
    pub(super) fn drew(&mut self, run: RunId, id: &CallId, scene: SceneId, step: &Step) {
        if let Some(ledger) = self.runs.iter_mut().find(|ledger| ledger.run == run) {
            ledger.steps += 1;
        }
        if let Some(at) = self.open_call(run, id) {
            let call = &mut self.calls[at];
            call.drawn.push(step.id.clone());
            call.scene = Bound::Held(scene);
        }
    }

    /// A new scene was stored. The thread's other calls that began its name while it wasn't
    /// stored yet continue it, as a later call would, or are refused if their header differs,
    /// so a name is never begun twice.
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
                !call.done && call.thread == thread && state.header().scene == header.scene
            })
            .collect();
        for at in rivals {
            let Bound::New(state) = &self.calls[at].scene else {
                continue;
            };
            if *state.header() == header {
                self.calls[at].scene = Bound::Held(scene);
            } else {
                let error = StepError::new(ErrorCode::HeaderMismatch);
                self.fail(at, CallError::Step(error), out);
            }
        }
    }

    /// The call's arguments ended: complete if its guard saw `end`.
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
        let ended = match error {
            Some(error) => Err(error),
            None => self.calls[at].guard.finish().map_err(CallError::Step),
        };
        match ended {
            Ok(()) => self.end(at, None, out),
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
        let call = &self.calls[at];
        let scene = match call.scene {
            Bound::Held(scene) => Some(scene),
            Bound::None | Bound::New(_) => None,
        };
        if let Some(scene) = scene.filter(|scene| self.live(*scene).is_some()) {
            out.effects.push(Effect::Data(DataToShell::SceneEnd {
                run: call.run,
                scene,
                complete: error.is_none(),
            }));
        }
        let spent = self.spent(self.calls[at].run);
        let call = &mut self.calls[at];
        call.done = true;
        out.calls.push(CallEnd {
            run: call.run,
            call: call.id.clone(),
            scene,
            drawn: mem::take(&mut call.drawn),
            error,
            spent,
        });
    }

    /// Ends every open call drawing in `scene`, whose step the shell couldn't prepare.
    pub(super) fn end_calls_in(&mut self, scene: SceneId, out: &mut Output) {
        let failing: Vec<usize> = (0..self.calls.len())
            .filter(|&at| {
                let call = &self.calls[at];
                !call.done && matches!(call.scene, Bound::Held(bound) if bound == scene)
            })
            .collect();
        for at in failing {
            self.end(at, Some(CallError::Failed), out);
        }
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
                } if *owner == run && call == id => Some(error.clone()),
                _ => None,
            })
            .unwrap_or(CallError::Interrupted);
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
        if let Some(super::Pending {
            wait: Wait::Step(write),
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

    pub(super) fn close(&mut self, run: RunId, out: &mut Output) {
        self.closed = self.closed.max(run);
        let closed = self.closed;
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
            Job::Record { run, .. } | Job::Reject { run, .. } | Job::Finish { run, .. } => {
                *run > closed
            }
            Job::Cut { .. } | Job::Shown { .. } | Job::Unseen { .. } | Job::ClearVisible => true,
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

    /// The run accepts no further draw call: it spent its rejected records, repair round or calls.
    // shortcut: the coordinator's round and tool-call budgets aren't seen here, so a refusal in a
    // run's last allowed round still invites a retry; pass them in if that shows up.
    fn spent(&self, run: RunId) -> bool {
        self.runs.iter().any(|ledger| {
            ledger.run == run && (ledger.exhausted || ledger.calls >= MAX_CALLS_PER_RUN)
        })
    }

    pub(super) fn open_call(&self, run: RunId, id: &CallId) -> Option<usize> {
        self.calls
            .iter()
            .position(|call| call.run == run && call.id == *id && !call.done)
    }

    fn ledger(&mut self, run: RunId) -> &mut RunLedger {
        let at = self
            .runs
            .iter()
            .position(|ledger| ledger.run == run)
            .unwrap_or_else(|| {
                self.runs.push(RunLedger {
                    run,
                    calls: 0,
                    steps: 0,
                    rejected: 0,
                    failed: false,
                    exhausted: false,
                });
                self.runs.len() - 1
            });
        &mut self.runs[at]
    }
}

/// The closed records of one whole envelope, or why it isn't one.
fn envelope(args: &[u8]) -> Result<Vec<Vec<u8>>, TokenError> {
    let mut tokenizer = Tokenizer::new();
    let mut input = args;
    let mut records = Vec::new();
    while let Some(record) = tokenizer.next(&mut input)? {
        records.push(record.bytes.to_vec());
    }
    tokenizer.finish()?;
    Ok(records)
}

fn in_step(step: &Step, code: ErrorCode) -> StepError {
    StepError {
        step: Some(step.id.clone()),
        path: FieldPath::default(),
        code,
    }
}
