//! Each gesture's capture inhibit, from key-down until its retirement is proved (voice.md).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use gpui_kit::{BackgroundExecutor, Task};
use kumochi_protocol::message::{ControlToBrain, GestureId, NoticeRevision, RunId, ThreadId};
use kumochi_shell_core::link::output::{Claim, Denied, InhibitToken, OutputOwner, Retirement};

use super::{Context, FailureReason, FailureStage, Presence, Session, Source};
use crate::platform::microphone::{Generation, Mode, Reserved};
use crate::strings::Text;

#[cfg(test)]
mod tests;

/// Rechecks a busy owner gets per operation: an attempt budget, not a latency promise.
const RECHECKS: u8 = 3;
/// The fallback wake after a busy owner; any earlier wake retries first.
pub(super) const RECHECK: Duration = Duration::from_millis(10);
/// How long an admitted hold waits for the output to go quiet [guess].
const QUIET_WAIT: Duration = Duration::from_secs(2);

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
    /// Admitted: the quiet deadline and its wake until the start is reserved, then the claim
    /// its collection is checked under.
    Admitted {
        deadline: Option<(Instant, Task<()>)>,
        claim: Option<Claim>,
    },
    /// Over: the dispatcher retires it on its proof.
    Ending,
}

/// The screen offer a hold's first input is seen under: the brain's revision, bound to the
/// connection, choice revision and target it was published for. `epoch` counts every change the
/// shell published, so an offer that went and came back is another one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::presence) struct Offer {
    pub(in crate::presence) session: u64,
    pub(in crate::presence) choice: u64,
    pub(in crate::presence) target: (Option<ThreadId>, u64),
    pub(in crate::presence) revision: NoticeRevision,
    pub(in crate::presence) epoch: u64,
}

struct Gate {
    gesture: GestureId,
    /// The link generation and choice revision at key-down.
    session: u64,
    choice: u64,
    /// The offer published at key-down; recognition and release compare it whole with the
    /// offer published then.
    offer: Option<Offer>,
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
    tell: Option<Session>,
    received: bool,
}

impl Gate {
    fn new(
        gesture: GestureId,
        session: u64,
        choice: u64,
        offer: Option<Offer>,
        authority: Authority,
    ) -> Self {
        Self {
            gesture,
            session,
            choice,
            offer,
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

    /// Spends one recheck and arms its fallback wake; false once the budget is spent.
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
    /// The offer presence published last, and how many changes it has published.
    offer: Rc<Cell<(Option<Offer>, u64)>>,
    /// Undecided Esc deliveries and refused stops' session ends: no gate retires while one is.
    held_back: Rc<Cell<usize>>,
    poke: Sender<()>,
    pokes: Receiver<()>,
}

/// A hold on every retirement: an Esc delivery's until its decision is applied, or a refused
/// stop's until its session end has run.
pub(in crate::presence) struct HoldBack(Gates);

impl Drop for HoldBack {
    fn drop(&mut self) {
        self.0
            .held_back
            .set(self.0.held_back.get().saturating_sub(1));
        self.0.poke();
    }
}

impl Default for Gates {
    fn default() -> Self {
        let (poke, pokes) = async_channel::bounded(1);
        Self {
            list: Rc::default(),
            on: Rc::default(),
            offer: Rc::default(),
            held_back: Rc::default(),
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

    /// Publishes `offer`, whose epoch is ignored, for the next key-down, and returns it as
    /// published: unchanged, it keeps its epoch; changed, it takes the next one.
    pub(in crate::presence) fn publish(&self, offer: Option<Offer>) -> Option<Offer> {
        let (last, epoch) = self.offer.get();
        let same = |offer: Offer| Offer { epoch: 0, ..offer };
        if last.map(same) == offer.map(same) {
            return last;
        }
        let epoch = epoch + 1;
        let offer = offer.map(|offer| Offer { epoch, ..offer });
        self.offer.set((offer, epoch));
        offer
    }

    /// The offer `gesture`'s newest key-down was seen under, if any.
    pub(in crate::presence) fn offer_of(&self, gesture: GestureId) -> Option<Offer> {
        self.list
            .borrow()
            .iter()
            .rev()
            .find(|gate| gate.gesture == gesture)
            .and_then(|gate| gate.offer)
    }

    /// Holds every retirement until the returned hold is dropped.
    pub(in crate::presence) fn hold_back(&self) -> Rc<HoldBack> {
        self.held_back.set(self.held_back.get() + 1);
        Rc::new(HoldBack(self.clone()))
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
        let offer = self.offer.get().0;
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
        gates.push(Gate::new(gesture, session, choice, offer, authority));
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

    /// Accounts for a skipped recognition without touching a gate that already owns its mic.
    pub(in crate::presence) fn skip_recognition(&self, mic: Generation) -> bool {
        let mut gates = self.list.borrow_mut();
        match gates.iter_mut().find(|gate| gate.gesture == mic.gesture()) {
            Some(gate) if gate.mic.is_some() => false,
            Some(gate) if matches!(gate.stage, Stage::Recognized | Stage::Ending) => {
                gate.mic = Some(mic);
                gate.stage = Stage::Ending;
                drop(gates);
                self.poke();
                true
            }
            _ => {
                drop(gates);
                mic.discard_unstarted().is_ok()
            }
        }
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
        gate.rechecks = RECHECKS;
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

    /// The brain admitted `gesture`'s hold: its quiet deadline is set, once.
    pub(super) fn admit(&self, gesture: GestureId, executor: &BackgroundExecutor) -> bool {
        let mut gates = self.list.borrow_mut();
        let Some(gate) = gates.iter_mut().find(|gate| {
            gate.gesture == gesture && matches!(gate.stage, Stage::Recognized) && gate.mic.is_some()
        }) else {
            return false;
        };
        gate.stage = Stage::Admitted {
            deadline: Some((
                executor.now() + QUIET_WAIT,
                self.after(executor, QUIET_WAIT),
            )),
            claim: None,
        };
        true
    }

    /// Runs one owner call under `gesture`'s claim: the receipt's acceptance or its copies.
    ///
    /// # Errors
    /// `Busy` while a recheck is left: offer the same receipt again after [`RECHECK`]. Any other
    /// refusal, or a busy owner with no recheck left, is final.
    pub(super) fn claimed(
        &self,
        gesture: GestureId,
        call: impl FnOnce(&Claim) -> Result<(), Denied>,
    ) -> Result<(), Denied> {
        let mut gates = self.list.borrow_mut();
        let Some(gate) = gates.iter_mut().find(|gate| gate.gesture == gesture) else {
            return Err(Denied::Stale);
        };
        let Stage::Admitted {
            claim: Some(claim), ..
        } = &gate.stage
        else {
            return Err(Denied::Stale);
        };
        match call(claim) {
            Err(Denied::Busy) => {
                tracing::debug!(stage = ?FailureStage::Inhibit, left = gate.rechecks, "held speech input owner busy");
                gate.rechecks = gate.rechecks.checked_sub(1).ok_or(Denied::Overflow)?;
                Err(Denied::Busy)
            }
            collected => collected,
        }
    }

    /// Ends `mic`'s gate unstarted; false when no gate holds it, so the caller discards it.
    pub(super) fn unused(&self, mic: &Generation, tell: Option<&Session>) -> bool {
        let mut found = false;
        for gate in self
            .list
            .borrow_mut()
            .iter_mut()
            .filter(|gate| gate.holds(mic))
        {
            gate.stage = Stage::Ending;
            gate.tell = tell.cloned();
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
    End(GestureId, Text),
    Retired(GestureId),
    Start(Reserved),
    Stopped(GestureId, Session),
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
                After::End(gesture, text) => {
                    if self
                        .voice
                        .pending
                        .as_ref()
                        .is_some_and(|hold| hold.token.gesture() == gesture)
                    {
                        tracing::debug!(stage = ?FailureStage::Inhibit, reason = ?text, "held speech input refused");
                        self.end_voice(text, cx);
                    }
                }
                After::Start(reserved) => self.open_microphone(reserved, cx),
                After::Retired(gesture) => {
                    if let Some(hold) = self
                        .voice
                        .pending
                        .as_mut()
                        .filter(|hold| hold.token.gesture() == gesture)
                    {
                        hold.unretired = false;
                        self.finish_voice();
                    }
                }
                // The link keeps the proof owed, and replays it on the next connection.
                After::Stopped(gesture, session) => {
                    if !self.link.is_quitting()
                        && !self
                            .link
                            .send_control(&session, ControlToBrain::VoiceHoldStopped { gesture })
                        && self
                            .session
                            .as_ref()
                            .is_some_and(|current| current.generation() == session.generation())
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
                        after.push(After::End(gate.gesture, Text::TurnBusy));
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
                    after.push(After::End(gate.gesture, Text::TurnBusy));
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
                            after.push(After::End(gate.gesture, Text::TurnBusy));
                        }
                    }
                }
                if gate.bound
                    && let Err(text) = self.wait(gate, &token, gates, executor, after)
                {
                    after.push(After::End(gate.gesture, text));
                }
                gate.authority = Authority::Held(token);
            }
            Authority::None => {}
        }
        true
    }

    /// One check of an admitted hold: its deadline, today's checks, its claim and its start.
    /// `Err` ends the hold with that notice. Wakes never refill the budget or move the deadline.
    fn wait(
        &self,
        gate: &mut Gate,
        token: &InhibitToken,
        gates: &Gates,
        executor: &BackgroundExecutor,
        after: &mut Vec<After>,
    ) -> Result<(), Text> {
        let Stage::Admitted {
            deadline: Some((at, _)),
            ..
        } = &gate.stage
        else {
            return Ok(());
        };
        if executor.now() >= *at {
            return Err(Text::HoldCancelled);
        }
        let Some(hold) = self.voice.pending.as_ref().filter(|hold| {
            hold.token.gesture() == gate.gesture
                && matches!(hold.source, Source::Waiting)
                && hold.terminal.is_none()
        }) else {
            return Ok(());
        };
        if !gates.on.get()
            || !self.voice_current(hold.stamp)
            || !self.link.input_is(gate.gesture)
            || hold.token.mode(&hold.session.alive(), Instant::now()) != Ok(Mode::Preparing)
        {
            return Err(Text::HoldCancelled);
        }
        let output = self.link.output();
        let claimed = match output.claim(token, hold.session.generation()) {
            Ok(claimed) => claimed,
            Err(Denied::Opening | Denied::NotQuiet) => return Ok(()),
            Err(Denied::Busy) if gate.recheck(gates, executor) => return Ok(()),
            Err(_) => return Err(Text::TurnBusy),
        };
        match output.start_mic(&claimed) {
            Ok(reserved) => {
                gate.stage = Stage::Admitted {
                    deadline: None,
                    claim: Some(claimed),
                };
                after.push(After::Start(reserved));
                Ok(())
            }
            Err(Denied::Busy) if gate.recheck(gates, executor) => Ok(()),
            Err(_) => Err(Text::TurnBusy),
        }
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
        // A resume never overtakes a speaker stop a busy owner hasn't taken, an undecided Esc,
        // or the end of a session whose brain refused a stop.
        if !self.pending_stops.is_empty() || gates.held_back.get() > 0 {
            gate.authority = Authority::Held(token);
            return true;
        }
        match self.link.output().retire(token, how) {
            Ok(()) => {
                after.push(After::Retired(gate.gesture));
                forget(gate, after);
                false
            }
            // A stale token's owner let it go: its unstarted generation is still discarded here.
            Err((_, Denied::Stale)) => {
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
        .is_none_or(|mic| mic.discard_unstarted().is_ok() || mic.finished());
    if let (Some(session), true) = (&gate.tell, discarded) {
        after.push(After::Stopped(gate.gesture, session.clone()));
    }
}
