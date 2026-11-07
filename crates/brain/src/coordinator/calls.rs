//! Tool calls from a run: resolved against the current run, handed to the dispatcher, and
//! answered on their write's acknowledgement (docs/development/tools.md).

use kumochi_protocol::message::{ControlToShell, ThreadId};

use super::admission::origin;
use super::{Coordinator, Effect, Run, Stale, StoreOp};
use crate::dispatch::{
    self, Caller, Dispatch, LedgerKey, LocalWrite, RunCtx, Step, Then, ToolError, ToolOutput,
};
use crate::engines::ToolName;
use crate::store::Failure;

impl Coordinator {
    pub(super) fn read_finished(
        &mut self,
        done: crate::dispatch::ReadFinished,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let pending = self.pending_deletes();
        let deleting = self
            .current
            .as_ref()
            .is_some_and(|run| self.deleting_thread(run.thread));
        let run = self.current(done.key.run)?;
        let Some(call) = run.ledger.read_call(&done.key) else {
            return Ok(());
        };
        if call.build != done.build {
            return Ok(());
        }
        let available = call.control.check();
        let current_build = run
            .build
            .as_ref()
            .filter(|build| build.key() == done.build && build.admitted());
        let mut voided = current_build.map_or_default(|build| build.voided(build.sources()));
        if let (Some(build), Ok((_, sources))) = (current_build, &done.result) {
            voided.extend(build.voided(sources));
        }
        if deleting
            || run.finishing
            || current_build.is_none()
            || !run.tools.waiting(&done.key.call)
            || !voided.is_empty()
        {
            run.ledger.refused(&done.key, ToolError::Ended);
            if !voided.is_empty() {
                run.tools.drop_round();
                run.unstored.clear();
                effects.push(Effect::CancelEngine { run: done.key.run });
                self.rebuild(done.build, &voided, &pending, effects);
            }
            return Ok(());
        }
        if done.result.is_ok()
            && let Err(error) = available
        {
            return self.call_refused(&done.key, error.into(), effects);
        }
        let output = match done.result {
            Ok((selection, _)) => {
                if let Err(error) = selection.validate() {
                    return self.call_refused(&done.key, error.into(), effects);
                }
                ToolOutput::Recall(selection)
            }
            Err(crate::recall::Error::Read | crate::recall::Error::Handler) => {
                run.ledger.refused(&done.key, ToolError::Ended);
                return self.fail(done.key.run, Failure::Store, effects);
            }
            Err(error) => return self.call_refused(&done.key, error.into(), effects),
        };
        if output.stored().len() > dispatch::MAX_OUTPUT {
            return self.call_refused(&done.key, ToolError::TooLarge, effects);
        }
        if let Some(call) = run.ledger.read_finished(&done.key, output) {
            effects.push(Effect::Store(StoreOp::CommitCall(call)));
        }
        Ok(())
    }

    /// Dispatches one call. The tool loop (slice 3's 13b-2) and the MCP listener (slice 8's C4b-2)
    /// hand calls here; a call whose run isn't the current, started one, or whose thread is being
    /// deleted, runs nothing. Nor does a call from a build that rendered a thread a delete has
    /// reached since: its response is dropped and the round is built again, as for a voided send.
    pub(crate) fn tool_call(
        &mut self,
        caller: Caller,
        tool: ToolName,
        args: &[u8],
        effects: &mut Vec<Effect>,
    ) -> Dispatch {
        let ended = Dispatch::Answered(ToolOutput::Error(ToolError::Ended));
        let pending = self.pending_deletes();
        let Some(thread) = self.current.as_ref().map(|current| current.thread) else {
            return ended;
        };
        if self.deleting_thread(thread) {
            return ended;
        }
        let main = self.main_thread;
        let fresh = ThreadId(self.last_thread.0 + 1);
        let reach = origin(thread, main);
        let Ok(run) = self.current(caller.run()) else {
            return ended;
        };
        let Some(build) = run.build.as_ref().filter(|build| build.admitted()) else {
            return ended;
        };
        if run.finishing {
            return ended;
        }
        let voided = build.voided(build.sources());
        if !voided.is_empty() {
            let key = build.key();
            run.tools.drop_round();
            run.unstored.clear();
            effects.push(Effect::CancelEngine { run: key.run });
            self.rebuild(key, &voided, &pending, effects);
            return ended;
        }
        let mut ctx = RunCtx {
            build: build.key(),
            end: run.control.0.end,
            control: build.control.0.clone(),
            thread,
            main,
            fresh,
            reach,
            offered: build.offered(),
            mcp: run.mcp,
            ledger: &mut run.ledger,
        };
        match dispatch::call(&mut ctx, caller, tool, args) {
            Step::Answer(output) => Dispatch::Answered(output),
            Step::Join => Dispatch::Joined,
            Step::Read(call) => {
                effects.push(Effect::Store(StoreOp::ReadCall(call)));
                Dispatch::Committing
            }
            Step::Screen { key, draw } => {
                if !self.drawing_available() {
                    self.drawn(&key, ToolOutput::Error(ToolError::NotHere), effects);
                    return Dispatch::Committing;
                }
                let current = self.current.as_mut().expect("the resolved run is current");
                if current.draw_calls.len()
                    > usize::try_from(super::MAX_CALLS).expect("the call budget fits usize")
                {
                    self.drawn(&key, ToolOutput::Error(ToolError::TooLarge), effects);
                    return Dispatch::Committing;
                }
                let call =
                    crate::engines::CallId::new(&format!("draw-{}", current.draw_calls.len()))
                        .expect("a bounded host-generated ASCII id");
                current.draw_calls.push((call.clone(), key.clone()));
                let input = match draw {
                    Ok(draw) => crate::drawing::Input::Call {
                        thread,
                        run: key.run,
                        call,
                        args: draw.args,
                    },
                    Err(error) => crate::drawing::Input::Rejected {
                        thread,
                        run: key.run,
                        call,
                        error,
                    },
                };
                if self.drawing_input(input, effects).is_err() {
                    self.drawn(&key, ToolOutput::Error(ToolError::Ended), effects);
                }
                Dispatch::Committing
            }
            Step::Commit(call) => {
                // Advanced at dispatch: a candidate whose reserve never commits was never seen.
                if let LocalWrite::CreateTask { thread, .. } = &call.write {
                    self.last_thread = *thread;
                }
                effects.push(Effect::Store(StoreOp::CommitCall(call)));
                Dispatch::Committing
            }
        }
    }

    pub(super) fn drawn(&mut self, key: &LedgerKey, output: ToolOutput, effects: &mut Vec<Effect>) {
        let Ok(current) = self.current(key.run) else {
            return;
        };
        match current.ledger.drawn(key, output.clone()) {
            dispatch::Drawn::Refused(answers) => deliver(current, answers),
            dispatch::Drawn::Commit(hash) => {
                effects.push(Effect::Store(StoreOp::CommitCall(dispatch::CommitCall {
                    key: key.clone(),
                    thread: current.thread,
                    tool: "draw",
                    hash,
                    write: LocalWrite::None,
                    output,
                    then: None,
                })));
            }
            dispatch::Drawn::Stale => {}
        }
    }

    /// A call's write committed. Its callers are answered only while their run is current and,
    /// for MCP, its launch is still the authorized one; a model call answers its round's call,
    /// and the round is stored once every call has its answer. Its follow-up runs once, with
    /// the first acknowledgement, and only while the run is current.
    pub(super) fn call_committed(
        &mut self,
        key: &LedgerKey,
        then: Option<Then>,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let run = self.current(key.run)?;
        if let Some(answers) = run.ledger.committed(key) {
            deliver(run, answers);
            if let Some(
                Then::Focus(thread)
                | Then::Created {
                    thread,
                    focus: true,
                },
            ) = then
            {
                effects.push(Effect::Control(ControlToShell::Focus {
                    run: key.run,
                    thread,
                }));
            }
        }
        if !run.tools.dispatched() {
            return Ok(());
        }
        self.store_round(key.run, effects)
    }

    /// The store refused a call's write on its own check: every caller waiting on it is
    /// answered with the error, the key stays refused, and the run goes on.
    pub(super) fn call_refused(
        &mut self,
        key: &LedgerKey,
        error: ToolError,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let run = self.current(key.run)?;
        let answers = run
            .ledger
            .refused(key, error)
            .into_iter()
            .map(|caller| (caller, ToolOutput::Error(error)))
            .collect();
        deliver(run, answers);
        if !run.tools.dispatched() {
            return Ok(());
        }
        self.store_round(key.run, effects)
    }

    /// A call's write failed: its key stays refused and the run fails.
    pub(super) fn call_failed(
        &mut self,
        key: &LedgerKey,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        self.current(key.run)?.ledger.failed(key);
        self.fail(key.run, Failure::Store, effects)
    }
}

/// Answers settled callers. A model call answers its round's call once, so the round stores one
/// result per call; any other answer waits in `answered`, which no round reads. An MCP caller
/// whose launch is no longer the authorized one gets nothing.
fn deliver(run: &mut Run, answers: Vec<(Caller, ToolOutput)>) {
    for (caller, output) in answers {
        let taken = match &caller {
            Caller::Model { call, .. } => run.tools.answer(call, output.clone()),
            Caller::Mcp { launch, .. } if run.mcp == Some(*launch) => false,
            Caller::Mcp { .. } => continue,
        };
        if !taken {
            run.answered.push((caller, output));
        }
    }
}
