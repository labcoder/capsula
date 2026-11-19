//! Each gesture's capture inhibit, from key-down until its retirement is proved (voice.md).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use gpui_kit::{BackgroundExecutor, Task};
use kumochi_protocol::message::{ControlToBrain, GestureId, RunId};
use kumochi_shell_core::link::output::{Denied, InhibitToken, OutputOwner, Retirement};

use super::{Context, FailureReason, FailureStage, Presence};
use crate::platform::microphone::Generation;
use crate::strings::Text;

#[cfg(test)]
mod tests;

/// Rechecks a busy owner gets per operation: an attempt budget, not a latency promise.
const RECHECKS: u8 = 3;
const RECHECK: Duration = Duration::from_millis(10);

enum Authority {
    /// Busy at key-down: the dispatcher acquires.
    Acquiring,
    /// Busy moving a first tap's token here: it stays good until the dispatcher moves it.
    Transferring(InhibitToken),
    Held(InhibitToken),
    /// Refused: nothing to retire.
    None,
}

enum Stage {
    /// Key-down: the gesture is engaged and wasn't recognized.
    Native,
    /// Recognized: its deferred delivery, then its hold, carries it.
    Recognized,
    /// Over: the dispatcher retires it on its proof.
    Ending,
}

struct Gate {
    gesture: GestureId,
    /// The link generation and choice revision at key-down.
    session: u64,
    choice: u64,
    authority: Authority,
    /// The run to fence, taken once when `Warm` is applied.
    run: Option<(u64, RunId)>,
    /// The generation recognition armed: retirement is proved on it.
    mic: Option<Generation>,
    bound: bool,
    stage: Stage,
    rechecks: u8,
    timer: Option<Task<()>>,
    /// The session to tell `VoiceHoldStopped` once its unstarted generation is discarded.
    tell: Option<u64>,
    received: bool,
}

impl Gate {
    fn new(gesture: GestureId, session: u64, choice: u64, authority: Authority) -> Self {
        Self {
            gesture,
            session,
            choice,
            authority,
            run: None,
            mic: None,
            bound: false,
            stage: Stage::Native,
            rechecks: RECHECKS,
            timer: None,
            tell: None,
            received: false,
        }
    }

    fn holds(&self, mic: &Generation) -> bool {
        self.mic
            .as_ref()
            .is_some_and(|held| held.gesture() == mic.gesture())
    }

    /// Spends one recheck and arms its wake; false once the budget is spent.
    fn recheck(&mut self, gates: &Gates, executor: &BackgroundExecutor) -> bool {
        tracing::debug!(stage = ?FailureStage::Inhibit, left = self.rechecks, "held speech input owner busy");
        let Some(left) = self.rechecks.checked_sub(1) else {
            return false;
        };
        self.rechecks = left;
        self.timer = Some(gates.after(executor, RECHECK));
        true
    }
}

/// Every gesture's gate, shared by the native callback and presence on the UI thread.
#[derive(Clone)]
pub(in crate::presence) struct Gates {
    list: Rc<RefCell<Vec<Gate>>>,
    /// Whether hold-to-talk can take a hold, mirrored from settings.
    on: Rc<Cell<bool>>,
    poke: Sender<()>,
    pokes: Receiver<()>,
}

impl Default for Gates {
    fn default() -> Self {
        let (poke, pokes) = async_channel::bounded(1);
        Self {
            list: Rc::default(),
            on: Rc::default(),
            poke,
            pokes,
        }
    }
}

impl Gates {
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the setting it mirrors is one bool"
    )]
    pub(super) fn follow(&self, on: bool) {
        self.on.set(on);
    }

    /// Wakes the dispatcher; a full channel already holds a wake.
    pub(in crate::presence) fn poke(&self) {
        let _woken = self.poke.try_send(());
    }

    fn after(&self, executor: &BackgroundExecutor, delay: Duration) -> Task<()> {
        let (poke, timer) = (self.poke.clone(), executor.timer(delay));
        executor.spawn(async move {
            timer.await;
            let _woken = poke.try_send(());
        })
    }

    /// Key-down, in the native callback: moves a first tap's token here or takes the inhibit.
    pub(in crate::presence) fn warm(
        &self,
        output: &OutputOwner,
        gesture: GestureId,
        session: u64,
        choice: u64,
    ) {
        if !self.on.get() {
            return;
        }
        let mut gates = self.list.borrow_mut();
        let native = gates.iter().position(|gate| {
            matches!(gate.stage, Stage::Native)
                && matches!(
                    gate.authority,
                    Authority::Held(_) | Authority::Transferring(_)
                )
        });
        for gate in gates.iter_mut() {
            if matches!(gate.stage, Stage::Native) {
                gate.stage = Stage::Ending;
            }
        }
        let moving = native.map(|at| {
            (
                at,
                std::mem::replace(&mut gates[at].authority, Authority::None),
            )
        });
        let authority = match moving {
            Some((at, Authority::Held(token) | Authority::Transferring(token))) => {
                match output.transfer(&token, gesture) {
                    Ok(moved) => {
                        gates.remove(at);
                        Authority::Held(moved)
                    }
                    Err(Denied::Busy) => {
                        gates.remove(at);
                        Authority::Transferring(token)
                    }
                    // The old gate keeps its token and retires it; this press gets nothing.
                    Err(_) => {
                        gates[at].authority = Authority::Held(token);
                        Authority::None
                    }
                }
            }
            // Inhibited: an older gate still holds it, and the dispatcher retires that first.
            _ => match output.acquire(gesture) {
                Ok(token) => Authority::Held(token),
                Err(Denied::Busy | Denied::Inhibited) => Authority::Acquiring,
                Err(_) => Authority::None,
            },
        };
        gates.push(Gate::new(gesture, session, choice, authority));
        drop(gates);
        self.poke();
    }

    /// After a native reduction that left no gesture engaged: its native gate ends.
    pub(in crate::presence) fn disengaged(&self) {
        let mut ended = false;
        for gate in self.list.borrow_mut().iter_mut() {
            if matches!(gate.stage, Stage::Native) {
                gate.stage = Stage::Ending;
                ended = true;
            }
        }
        if ended {
            self.poke();
        }
    }

    /// The native callback recognized `gesture`'s hold, with an armed generation or not.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "whether the native gate armed a generation"
    )]
    pub(in crate::presence) fn recognized(&self, gesture: GestureId, armed: bool) {
        for gate in self.list.borrow_mut().iter_mut() {
            if gate.gesture == gesture && matches!(gate.stage, Stage::Native) {
                gate.stage = if armed {
                    Stage::Recognized
                } else {
                    Stage::Ending
                };
            }
        }
        self.poke();
    }

    /// `gesture`'s `Warm` was applied with `run` speaking: what its recognition fences.
    pub(super) fn capture(&self, gesture: GestureId, run: Option<(u64, RunId)>) {
        for gate in self.list.borrow_mut().iter_mut() {
            if gate.gesture == gesture && gate.mic.is_none() {
                gate.run = run;
            }
        }
    }

    /// Esc: no gate fences a run any more.
    pub(in crate::presence) fn forget_runs(&self) {
        for gate in self.list.borrow_mut().iter_mut() {
            gate.run = None;
        }
    }

    /// Binds `mic` to the gesture's token, now or in the dispatcher, and hands back its run.
    ///
    /// # Errors
    /// No token, or a refused bind: the gate ends, and the caller discards `mic`.
    pub(super) fn recognize(
        &self,
        output: &OutputOwner,
        gesture: GestureId,
        mic: &Generation,
    ) -> Result<Option<(u64, RunId)>, Denied> {
        let mut gates = self.list.borrow_mut();
        let gate = gates
            .iter_mut()
            .find(|gate| {
                gate.gesture == gesture
                    && matches!(gate.stage, Stage::Recognized)
                    && gate.mic.is_none()
            })
            .ok_or(Denied::Stale)?;
        let bound = match &gate.authority {
            Authority::Held(token) => output.bind_mic(token, mic),
            // Authority still coming: the dispatcher binds once it has it.
            Authority::Acquiring | Authority::Transferring(_) => Err(Denied::Busy),
            Authority::None => Err(Denied::Stale),
        };
        match bound {
            Ok(()) => gate.bound = true,
            Err(Denied::Busy) => {}
            Err(denied) => {
                gate.stage = Stage::Ending;
                drop(gates);
                self.poke();
                return Err(denied);
            }
        }
        gate.mic = Some(mic.clone());
        let run = gate.run.take();
        drop(gates);
        self.poke();
        Ok(run)
    }

    /// Whether a gate holds `mic` bound: only then may it start.
    pub(super) fn bound(&self, mic: &Generation) -> bool {
        self.list
            .borrow()
            .iter()
            .any(|gate| gate.holds(mic) && gate.bound)
    }

    /// Ends `mic`'s gate unstarted; false when no gate holds it, so the caller discards it.
    pub(super) fn unused(&self, mic: &Generation, tell: Option<u64>) -> bool {
        let mut found = false;
        for gate in self
            .list
            .borrow_mut()
            .iter_mut()
            .filter(|gate| gate.holds(mic))
        {
            gate.stage = Stage::Ending;
            gate.tell = tell;
            found = true;
        }
        if found {
            self.poke();
        }
        found
    }

    /// The physical receipt of `mic`'s worker came, whether or not its hold is still there.
    pub(super) fn received(&self, mic: &Generation) {
        for gate in self
            .list
            .borrow_mut()
            .iter_mut()
            .filter(|gate| gate.holds(mic))
        {
            gate.stage = Stage::Ending;
            gate.received = true;
        }
        self.poke();
    }
}

/// What the dispatcher does once no gate is borrowed.
enum After {
    End(GestureId),
    Stopped(GestureId, u64),
    Unconfirmed,
}

impl Presence {
    /// Starts the one dispatcher, which runs after whatever delivery poked it.
    pub(in crate::presence) fn dispatcher(&mut self, cx: &mut Context<'_, Self>) {
        let pokes = self.voice.gates.pokes.clone();
        cx.spawn(async move |this, cx| {
            while pokes.recv().await.is_ok() {
                if this.update(cx, Self::dispatch).is_err() {
                    break;
                }
            }
        })
        .detach();
        self.forward_changes(cx);
    }

    /// The only reader of the owner's changes, which are hints: each pokes the dispatcher.
    pub(in crate::presence) fn forward_changes(&self, cx: &mut Context<'_, Self>) {
        let (changes, poke) = (self.link.output().changes(), self.voice.gates.poke.clone());
        cx.background_executor()
            .spawn(async move {
                while changes.recv().await.is_ok() {
                    let _woken = poke.try_send(());
                }
            })
            .detach();
    }

    /// Steps every gate once; the only place a gate retires.
    pub(super) fn dispatch(&mut self, cx: &mut Context<'_, Self>) {
        let executor = cx.background_executor().clone();
        let current = (self.link.link_generation(), self.link.choice_revision());
        let gates = self.voice.gates.clone();
        let mut after = Vec::new();
        gates
            .list
            .borrow_mut()
            .retain_mut(|gate| self.step(gate, &gates, &executor, current, &mut after));
        for step in after {
            match step {
                After::End(gesture) => {
                    if self
                        .voice
                        .pending
                        .as_ref()
                        .is_some_and(|hold| hold.token.gesture() == gesture)
                    {
                        tracing::debug!(stage = ?FailureStage::Inhibit, reason = ?FailureReason::NoInhibit, "held speech input refused");
                        self.end_voice(Text::TurnBusy, cx);
                    }
                }
                After::Stopped(gesture, generation) => {
                    let Some(session) = self
                        .session
                        .clone()
                        .filter(|session| session.generation() == generation)
                    else {
                        continue;
                    };
                    if !self
                        .link
                        .send_control(&session, ControlToBrain::VoiceHoldStopped { gesture })
                    {
                        tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::StopSend, "held speech input failed");
                        self.end_session(&session, cx).detach();
                    }
                }
                // A hold's own failure notice stays until the hold is gone.
                After::Unconfirmed => {
                    if self.voice.pending.is_none()
                        && self.voice.status != Some(Text::HoldUnconfirmed)
                    {
                        self.voice_notice(Text::HoldUnconfirmed, cx);
                    }
                }
            }
        }
    }

    fn step(
        &self,
        gate: &mut Gate,
        gates: &Gates,
        executor: &BackgroundExecutor,
        current: (u64, u64),
        after: &mut Vec<After>,
    ) -> bool {
        let output = self.link.output();
        gate.timer = None;
        if matches!(gate.stage, Stage::Ending) {
            return self.retire_gate(gate, gates, executor, after);
        }
        match std::mem::replace(&mut gate.authority, Authority::None) {
            Authority::Acquiring => {
                // Only for the original gesture, on its own link and settings.
                if (gate.session, gate.choice) != current || !gates.on.get() {
                    return false;
                }
                gate.authority = match output.acquire(gate.gesture) {
                    // A recognition waiting on it binds on the next step.
                    Ok(token) => {
                        gates.poke();
                        Authority::Held(token)
                    }
                    Err(Denied::Busy | Denied::Inhibited) if gate.recheck(gates, executor) => {
                        Authority::Acquiring
                    }
                    Err(_) => {
                        after.push(After::End(gate.gesture));
                        Authority::None
                    }
                };
            }
            Authority::Transferring(token) => match output.transfer(&token, gate.gesture) {
                Ok(moved) => {
                    gates.poke();
                    gate.authority = Authority::Held(moved);
                }
                Err(Denied::Busy) if gate.recheck(gates, executor) => {
                    gate.authority = Authority::Transferring(token);
                }
                // The first tap's token retires, and this gesture gets nothing.
                Err(_) => {
                    gate.authority = Authority::Held(token);
                    gate.stage = Stage::Ending;
                    gates.poke();
                    after.push(After::End(gate.gesture));
                }
            },
            Authority::Held(token) => {
                if let (false, Some(mic)) = (gate.bound, &gate.mic) {
                    match output.bind_mic(&token, mic) {
                        Ok(()) => gate.bound = true,
                        Err(Denied::Busy) if gate.recheck(gates, executor) => {}
                        Err(_) => {
                            gate.stage = Stage::Ending;
                            gates.poke();
                            after.push(After::End(gate.gesture));
                        }
                    }
                }
                gate.authority = Authority::Held(token);
            }
            Authority::None => {}
        }
        true
    }

    /// False once the gate is gone.
    fn retire_gate(
        &self,
        gate: &mut Gate,
        gates: &Gates,
        executor: &BackgroundExecutor,
        after: &mut Vec<After>,
    ) -> bool {
        let token = match std::mem::replace(&mut gate.authority, Authority::None) {
            Authority::Held(token) | Authority::Transferring(token) => token,
            Authority::Acquiring | Authority::None => {
                forget(gate, after);
                return false;
            }
        };
        let how = match (&gate.mic, gate.bound) {
            (Some(mic), true) if mic.finished() || gate.received => Retirement::Retired,
            (Some(_), true) => Retirement::Unstarted,
            _ => Retirement::NoMic,
        };
        // A resume never overtakes a speaker stop a busy owner hasn't taken: its drain pokes.
        if !self.pending_stops.is_empty() {
            gate.authority = Authority::Held(token);
            return true;
        }
        match self.link.output().retire(token, how) {
            // A stale token's owner let it go: its unstarted generation is still discarded here.
            Ok(()) | Err((_, Denied::Stale)) => {
                forget(gate, after);
                false
            }
            Err((token, Denied::Busy)) => {
                // The receipt may already have come, and nothing else will poke.
                gate.authority = Authority::Held(token);
                gate.timer = Some(gates.after(executor, RECHECK));
                true
            }
            // Started, so not discarded: the physical receipt pokes.
            Err((token, Denied::Inhibited)) if matches!(how, Retirement::Unstarted) => {
                gate.authority = Authority::Held(token);
                true
            }
            Err((token, _)) => {
                gate.authority = Authority::Held(token);
                after.push(After::Unconfirmed);
                true
            }
        }
    }
}

/// A gone gate's unstarted generation is discarded, once, and its session told.
fn forget(gate: &Gate, after: &mut Vec<After>) {
    let discarded = gate
        .mic
        .as_ref()
        .is_none_or(|mic| mic.discard_unstarted().is_ok() || mic.retired());
    if let (Some(session), true) = (gate.tell, discarded) {
        after.push(After::Stopped(gate.gesture, session));
    }
}
