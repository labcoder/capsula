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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "13b-2's tool loop and C4b-2's listener call it")
    )]
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
        let ctx = RunCtx {
            thread,
            reach,
            offered: build.offered(),
            mcp: run.mcp,
            ledger: &mut run.ledger,
        };
        match dispatch::call(ctx, caller, tool, args) {
            Step::Answer(output) => Dispatch::Answered(output),
            Step::Join => Dispatch::Joined,
            Step::Commit(call) => {
                effects.push(Effect::Store(StoreOp::CommitCall(call)));
                Dispatch::Committing
            }
        }
    }

    /// A call's write was acknowledged. Its callers are answered only while their run is current
    /// and, for MCP, its launch is still the authorized one; a failed write fails the run.
    pub(super) fn call_committed(
        &mut self,
        key: &LedgerKey,
        committed: bool,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let run = self.current(key.run)?;
        let answers = run.ledger.committed(key, committed);
        if !committed {
            return self.fail(key.run, Failure::Store, effects);
        }
        let authorized = |caller: &Caller| match caller {
            Caller::Model { .. } => true,
            Caller::Mcp { launch, .. } => run.mcp == Some(*launch),
        };
        let answers: Vec<_> = answers
            .into_iter()
            .filter(|(caller, _)| authorized(caller))
            .collect();
        run.answered.extend(answers);
        Ok(())
    }
}
