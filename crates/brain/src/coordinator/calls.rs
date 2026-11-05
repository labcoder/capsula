//! Tool calls from a run: resolved against the current run, handed to the dispatcher, and
//! answered on their write's acknowledgement (docs/development/tools.md).

use super::admission::origin;
use super::{Coordinator, Effect, Stale, StoreOp};
use crate::dispatch::{self, Caller, Dispatch, LedgerKey, RunCtx, Step, ToolError, ToolOutput};
use crate::engines::ToolName;
use crate::store::Failure;

impl Coordinator {
    /// Dispatches one call. The tool loop (slice 3's 13b-2) and the MCP listener (slice 8's C4b-2)
    /// hand calls here; a call whose run isn't the current, started one, or whose thread is being
    /// deleted, runs nothing.
    pub(crate) fn tool_call(
        &mut self,
        caller: Caller,
        tool: ToolName,
        args: &[u8],
        effects: &mut Vec<Effect>,
    ) -> Dispatch {
        let ended = Dispatch::Answered(ToolOutput::Error(ToolError::Ended));
        let Some(thread) = self.current.as_ref().map(|current| current.thread) else {
            return ended;
        };
        if self.deleting_thread(thread) {
            return ended;
        }
        let reach = origin(thread, self.main_thread);
        let Ok(run) = self.current(caller.run()) else {
            return ended;
        };
        let Some(build) = run.build.as_ref().filter(|build| build.admitted()) else {
            return ended;
        };
        if run.finishing {
            return ended;
        }
        let mut ctx = RunCtx {
            thread,
            reach,
            offered: build.offered(),
            mcp: run.mcp,
            ledger: &mut run.ledger,
        };
        match dispatch::call(&mut ctx, caller, tool, args) {
            Step::Answer(output) => Dispatch::Answered(output),
            Step::Join => Dispatch::Joined,
            Step::Commit(call) => {
                effects.push(Effect::Store(StoreOp::CommitCall(call)));
                Dispatch::Committing
            }
        }
    }

    /// A call's write committed. Its callers are answered only while their run is current and,
    /// for MCP, its launch is still the authorized one; a model call answers its round's call,
    /// and the round is stored once every call has its answer.
    pub(super) fn call_committed(
        &mut self,
        key: &LedgerKey,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let run = self.current(key.run)?;
        let answers = run.ledger.committed(key);
        let authorized = |caller: &Caller| match caller {
            Caller::Model { .. } => true,
            Caller::Mcp { launch, .. } => run.mcp == Some(*launch),
        };
        for (caller, output) in answers.into_iter().filter(|(caller, _)| authorized(caller)) {
            let taken = match &caller {
                Caller::Model { call, .. } => run.tools.answer(call, output.clone()),
                Caller::Mcp { .. } => false,
            };
            if !taken {
                run.answered.push((caller, output));
            }
        }
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
