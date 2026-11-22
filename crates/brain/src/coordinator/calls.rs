//! Tool calls from a run: resolved against the current run, handed to the dispatcher, and
//! answered on their write's acknowledgement (docs/development/tools.md).

use std::time::Instant;

use kumochi_protocol::message::{ControlToShell, RunId, ThreadId};

use super::admission::origin;
use super::{BuildKey, Coordinator, Effect, Run, Stale, StoreOp};
use crate::bridge::listener::{McpOrigin, McpReply};
use crate::bridge::{CallKey, LaunchId};
use crate::dispatch::{
    self, Caller, Dispatch, LedgerKey, LocalWrite, Route, RunCtx, Step, Then, ToolError, ToolOutput,
};
use crate::drawing::CallError;
use crate::engines::ToolName;
use crate::engines::claude_code::audit::{self, AuditGate};
use crate::engines::claude_code::launch::{Bind, Offer};
use crate::store::Failure;

/// What keeps a run's bound launch authorized, frozen at its bind: the build it serves, its own
/// gate, its effective deadline, and the tools it offers. The gate is pending until the launch's
/// post-seed audit passes, and fails once that audit fails or the launch is revoked, replaced or
/// torn down; dropping the authority fails it too, so the listener stops releasing the moment the
/// core lets go.
#[derive(Debug)]
pub(super) struct McpAuthority {
    build: BuildKey,
    gate: AuditGate,
    until: Instant,
    tools: Vec<ToolName>,
}

impl Drop for McpAuthority {
    fn drop(&mut self) {
        self.gate.fail();
    }
}

/// Where a bound launch stands with the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    Callable,
    /// Bound, its post-seed audit still to pass: no work, but the binding stays.
    Pending,
    /// Lost for good.
    Revoked,
}

impl Coordinator {
    fn current_call(&mut self, key: &LedgerKey) -> Result<&mut Run, Stale> {
        let run = self.current(key.run)?;
        if let dispatch::Route::ModelRound(round) = key.route
            && run
                .build
                .as_ref()
                .is_none_or(|build| build.key().round != round)
        {
            return Err(Stale);
        }
        Ok(run)
    }

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
            effects.push(Effect::Store(StoreOp::CommitCall(run.stored_call(call))));
        }
        Ok(())
    }

    /// Authorizes `bind`'s launch, asked for `thread` and holding `gate`, for the calls of its
    /// build while that's the current run's admitted build for that thread and origin, offers
    /// every tool the launch does, and has no launch yet. Its deadline is the earliest of the
    /// launch's, its question's and the build's, which the listener releases by; `None` leaves
    /// the run's authorized launch as it was.
    pub(crate) fn mcp_bound(
        &mut self,
        bind: &Bind,
        thread: ThreadId,
        gate: AuditGate,
    ) -> Option<Instant> {
        let main = self.main_thread;
        let Offer::Mcp(asked, tools, _) = &bind.offer else {
            return None;
        };
        let run = self.current(bind.build.run).ok()?;
        let current = run
            .build
            .as_ref()
            .filter(|current| current.key() == bind.build && current.admitted())?;
        let offers = tools.iter().all(|tool| current.offered().contains(tool));
        let until = bind.until.min(current.control.0.end);
        if run.finishing
            || run.mcp.is_some()
            || run.thread != thread
            || *asked != origin(run.thread, main)
            || !offers
        {
            return None;
        }
        run.mcp = Some(bind.launch);
        run.mcp_authority = Some(McpAuthority {
            build: bind.build,
            gate,
            until,
            tools: tools.clone(),
        });
        Some(until)
    }

    /// Where `launch`'s binding for `build` stands, and the tools it offers: revoked unless it's
    /// the run's bound launch for its current admitted build, its effective deadline ahead, the
    /// build's control live, the run not finishing, and no source of it deleted or voided; then
    /// as its gate says, read once.
    fn mcp_standing(&self, build: BuildKey, launch: LaunchId) -> (Standing, &[ToolName]) {
        let revoked = (Standing::Revoked, &[][..]);
        let Some(run) = self.current.as_ref().filter(|run| run.id == build.run) else {
            return revoked;
        };
        let Some(authority) = run
            .mcp_authority
            .as_ref()
            .filter(|authority| run.mcp == Some(launch) && authority.build == build)
        else {
            return revoked;
        };
        let live = Instant::now() < authority.until
            && !run.finishing
            && !self.deleting_thread(run.thread)
            && run
                .build
                .as_ref()
                .filter(|current| current.key() == build && current.admitted())
                .is_some_and(|current| {
                    current.control.0.check().is_ok()
                        && current.voided(current.sources()).is_empty()
                });
        let standing = match authority.gate.standing() {
            _ if !live => Standing::Revoked,
            audit::Standing::Failed => Standing::Revoked,
            audit::Standing::Pending => Standing::Pending,
            audit::Standing::Passed => Standing::Callable,
        };
        (standing, &authority.tools)
    }

    /// Whether `launch`'s work for `build` may start, for `tool` when one is named, or its
    /// result leave: callable, and `tool` among the launch's own. The one check wherever MCP
    /// work starts or content leaves the core: [`ToolError::NotHere`] for a tool the launch
    /// doesn't offer, and [`ToolError::Ended`] for anything else, a pending binding's included.
    pub(crate) fn mcp_authorized(
        &self,
        build: BuildKey,
        launch: LaunchId,
        tool: Option<ToolName>,
    ) -> Result<(), ToolError> {
        let (standing, tools) = self.mcp_standing(build, launch);
        if standing != Standing::Callable {
            return Err(ToolError::Ended);
        }
        if tool.is_some_and(|tool| !tools.contains(&tool)) {
            return Err(ToolError::NotHere);
        }
        Ok(())
    }

    /// Revokes the run's bound launch once its binding is lost for good, before anything more of
    /// its work can start; a pending one stays bound.
    pub(super) fn fence_mcp(&mut self, effects: &mut Vec<Effect>) {
        let Some(run) = &self.current else {
            return;
        };
        let (Some(launch), Some(authority)) = (run.mcp, &run.mcp_authority) else {
            return;
        };
        let (id, build) = (run.id, authority.build);
        if self.mcp_standing(build, launch).0 == Standing::Revoked {
            self.revoke_mcp(id, effects);
        }
    }

    /// The one way a run's bound launch stops: dropping its authority fails its gate first, so the
    /// listener releases nothing more of it, and no waiting call of it is answered; then its own
    /// draws are interrupted by their host call ids, so the drawing submits nothing more of
    /// theirs. A step already issued settles, and its answer can't reach the next launch.
    pub(super) fn revoke_mcp(&mut self, run: RunId, effects: &mut Vec<Effect>) {
        let Ok(current) = self.current(run) else {
            return;
        };
        current.mcp_authority = None;
        let Some(launch) = current.mcp.take() else {
            return;
        };
        let thread = current.thread;
        let calls: Vec<_> = current
            .draw_calls
            .iter()
            .filter(|call| {
                call.key.route == Route::Mcp(launch) && current.ledger.drawing(&call.key)
            })
            .map(|call| call.call.clone())
            .collect();
        for call in calls {
            let interrupted = crate::drawing::Input::CallFinished {
                thread,
                run,
                call,
                error: Some(CallError::Interrupted),
            };
            if let Ok(output) = self.drawing.handle(interrupted) {
                self.drawing_output(output, effects);
            }
        }
    }

    /// An MCP call from `launch` for `build`, dispatched only while the launch is authorized for
    /// `tool`, checked before any charge or ledger replay. A key the run hasn't seen costs one of
    /// the run's calls, and one past its budget ends the run; a refused call costs nothing, so
    /// stale work never ends a run on its budget.
    pub(crate) fn mcp_call(
        &mut self,
        build: BuildKey,
        launch: LaunchId,
        key: CallKey,
        (tool, args): (ToolName, &[u8]),
        effects: &mut Vec<Effect>,
    ) -> Dispatch {
        self.fence_mcp(effects);
        if let Err(error) = self.mcp_authorized(build, launch, Some(tool)) {
            return Dispatch::Answered(ToolOutput::Error(error));
        }
        let caller = Caller::Mcp {
            run: build.run,
            launch,
            key,
        };
        let run = self
            .current(build.run)
            .expect("an authorized launch's run is current");
        if !run.ledger.has(&caller.key()) && !run.tools.charge() {
            self.fail(build.run, Failure::Budget, effects)
                .expect("the run is current");
            return Dispatch::Answered(ToolOutput::Error(ToolError::Ended));
        }
        self.tool_call(caller, tool, args, effects)
    }

    /// Takes the settled answer to `launch`'s call `key` for `build`: `None` until it has one,
    /// and [`Stale`] once the launch is no longer authorized.
    pub(crate) fn take_mcp_answer(
        &mut self,
        build: BuildKey,
        launch: LaunchId,
        key: &CallKey,
    ) -> Result<Option<ToolOutput>, Stale> {
        self.mcp_authorized(build, launch, None)
            .map_err(|_| Stale)?;
        let run = self.current(build.run)?;
        let at = run.answered.iter().position(|(caller, _)| {
            matches!(caller, Caller::Mcp { launch: from, key: called, .. } if *from == launch && called == key)
        });
        Ok(at.map(|at| run.answered.remove(at).1))
    }

    /// What the model reads over MCP for `origin`'s `output`: its text, or a closed error, and
    /// only `Ended` once its launch is no longer authorized. A draw that stopped early reads as an
    /// error, text included, as the ledger keeps it for the call and any replay of it.
    pub(crate) fn mcp_reply(&mut self, origin: &McpOrigin, output: &ToolOutput) -> McpReply {
        let ended = ToolOutput::Error(ToolError::Ended);
        let output = match self.mcp_authorized(origin.build, origin.launch, None) {
            Ok(()) => output,
            Err(_) => &ended,
        };
        let (text, error) = match output {
            ToolOutput::Text(text) => (text.clone(), false),
            ToolOutput::Error(error) => (error.text().to_owned(), true),
            // References are re-read at each model build; nothing over MCP renders them yet.
            ToolOutput::History(_) | ToolOutput::Recall(_) => {
                (ToolError::NotHere.text().to_owned(), true)
            }
        };
        let caller = Caller::Mcp {
            run: origin.build.run,
            launch: origin.launch,
            key: origin.key.clone(),
        };
        let stopped = self
            .current(origin.build.run)
            .is_ok_and(|run| run.ledger.stopped(&caller.key()));
        McpReply {
            text,
            error: error || stopped,
        }
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
        self.tool_input(caller, tool, dispatch::CallInput::Complete(args), effects)
    }

    pub(super) fn tool_input(
        &mut self,
        caller: Caller,
        tool: ToolName,
        input: dispatch::CallInput<'_>,
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
            effects.push(Effect::CancelEngine { run: key.run });
            self.rebuild(key, &voided, &pending, effects);
            return ended;
        }
        if matches!(
            input,
            dispatch::CallInput::Draw(dispatch::DrawAction::Start)
        ) && !self.drawing_available()
        {
            return Dispatch::Joined;
        }
        let run = self
            .current(caller.run())
            .expect("the resolved run remains current");
        let build = run
            .build
            .as_ref()
            .expect("the admitted build remains current");
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
        match dispatch::call(&mut ctx, caller, tool, input) {
            Step::Answer(output) => Dispatch::Answered(output),
            Step::Join => Dispatch::Joined,
            Step::Read(call) => {
                effects.push(Effect::Store(StoreOp::ReadCall(call)));
                Dispatch::Committing
            }
            Step::Screen { key, draw } => self.drawing_call(thread, &key, draw, effects),
            Step::Commit(call) => {
                let call = run.stored_call(call);
                // Advanced at dispatch: a candidate whose reserve never commits was never seen.
                if let LocalWrite::CreateTask { thread, .. } = &call.write {
                    self.last_thread = *thread;
                }
                effects.push(Effect::Store(StoreOp::CommitCall(call)));
                Dispatch::Committing
            }
        }
    }

    pub(super) fn drawn(
        &mut self,
        key: &LedgerKey,
        end: impl Into<dispatch::DrawEnd>,
        effects: &mut Vec<Effect>,
    ) {
        let Ok(current) = self.current(key.run) else {
            return;
        };
        let end = end.into();
        let output = end.output.clone();
        match current.ledger.drawn(key, end) {
            dispatch::Drawn::Refused(answers) => deliver(current, answers),
            dispatch::Drawn::Commit(hash) => {
                let call = current.stored_call(dispatch::CommitCall {
                    key: key.clone(),
                    thread: current.thread,
                    tool: "draw",
                    hash,
                    write: LocalWrite::None,
                    output,
                    then: None,
                });
                effects.push(Effect::Store(StoreOp::CommitCall(call)));
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
        let run = self.current_call(key)?;
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
        let run = self.current_call(key)?;
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
        self.current_call(key)?.ledger.failed(key);
        self.fail(key.run, Failure::Store, effects)
    }
}

impl Run {
    fn stored_call(&self, mut call: dispatch::CommitCall) -> dispatch::CommitCall {
        if self.pictured_key_model.is_some() {
            call.output = ToolOutput::Text(dispatch::PICTURE_RESULT.to_owned());
        }
        call
    }
}

/// Answers settled callers. A model call answers its round's call once, so the round stores one
/// result per call; any other answer waits in `answered`, which no round reads. An MCP caller
/// whose launch is no longer the authorized one gets nothing.
fn deliver(run: &mut Run, answers: Vec<(Caller, ToolOutput)>) {
    for (caller, output) in answers {
        if let Caller::Model { call, .. } | Caller::ModelRound { call, .. } = &caller
            && run.provider_calls.contains(&caller.key())
        {
            if run
                .tools
                .provider_registered(call, run.build.as_ref().map(super::admission::Build::key))
            {
                run.tools.answer(call, output);
            }
            continue;
        }
        let taken = match &caller {
            Caller::Model { call, .. } => run.tools.answer(call, output.clone()),
            Caller::ModelRound { round, call, .. }
                if run
                    .build
                    .as_ref()
                    .is_some_and(|build| build.key().round == *round) =>
            {
                run.tools.answer(call, output.clone())
            }
            Caller::Mcp { launch, .. } if run.mcp == Some(*launch) => false,
            Caller::ModelRound { .. } | Caller::Mcp { .. } => continue,
        };
        if !taken {
            run.answered.push((caller, output));
        }
    }
}
