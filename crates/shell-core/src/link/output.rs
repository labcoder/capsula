//! One link's output owner: each session's output, every output generation's tail debt, the
//! capture inhibit, and whether a microphone may start or collect (docs/development/speaker.md).
//! Each section under its lock is short and never blocks: opens, joins, pipe aborts and last
//! drops of a speaker happen after it.

use std::fmt;
use std::sync::atomic::{Ordering, fence};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use async_channel::{Receiver, Sender};
use kumochi_protocol::message::{GestureId, OutputGeneration, RunId};

use crate::platform::microphone::{Generation, MicOwner, Reserved, Running};
use crate::speaker::{Command, FAULT, RELEASED, Speaker, Tail, UNKNOWN};

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;

/// Output generations tracked until their tails settle; `LIVE` keeps one speaker thread at a
/// time, so this is a ceiling, not the safety argument.
const LEDGER: usize = 4;

/// Aborts one session's pipes.
pub type Abort = Arc<dyn Fn() + Send + Sync>;

/// The owner of a link's outputs and its capture inhibit.
pub struct OutputOwner {
    state: Mutex<State>,
    changed: Sender<()>,
    changes: Receiver<()>,
}

impl Default for OutputOwner {
    fn default() -> Self {
        let (sender, changes) = async_channel::bounded(1);
        Self {
            state: Mutex::default(),
            changed: sender,
            changes,
        }
    }
}

impl fmt::Debug for OutputOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputOwner").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    session: Option<u64>,
    output: Output,
    commands: Option<SyncSender<Command>>,
    abort: Option<Abort>,
    ledger: [Option<Entry>; LEDGER],
    inhibit: Option<Inhibit>,
    off: bool,
    generations: u32,
    serials: u32,
    epochs: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Output {
    #[default]
    Closed,
    Quiet {
        epoch: u64,
        why: Why,
    },
    Opening(OutputGeneration),
    Live(OutputGeneration),
    Lost(OutputGeneration),
}

/// Why a session has no output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// No opener: every normal launch.
    NoOpener,
    /// Talk-back is off.
    Off,
    /// The open was refused, or no generation or ledger slot was left.
    Refused,
    /// Its output was withdrawn or failed, and its tail settled clean.
    Retired,
}

struct Entry {
    generation: OutputGeneration,
    tail: Arc<Tail>,
    terminal: bool,
}

struct Inhibit {
    serial: u32,
    gesture: GestureId,
    revision: u32,
    mic: Option<Generation>,
}

impl Inhibit {
    fn token(&self) -> InhibitToken {
        InhibitToken {
            serial: self.serial,
            gesture: self.gesture,
            revision: self.revision,
        }
    }
}

/// Holds the capture inhibit; only its holder can move or retire it.
#[derive(Debug, PartialEq, Eq)]
pub struct InhibitToken {
    serial: u32,
    gesture: GestureId,
    revision: u32,
}

/// An output the owner named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKey {
    /// A live output generation.
    Generation(OutputGeneration),
    /// A session's no-output epoch.
    NoOutput(u64),
}

/// One session's claim to start and collect a microphone under the inhibit, checked again at
/// each microphone transition.
#[derive(Debug)]
pub struct Claim {
    session: u64,
    key: OutputKey,
    owner: InhibitToken,
}

/// The proof that lets go of the inhibit; each is checked on the bound microphone generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retirement {
    /// No microphone was bound.
    NoMic,
    /// The bound generation never started: the owner discards it.
    Unstarted,
    /// The bound generation's gate shows it retired.
    Retired,
}

/// Why the owner said no.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    /// Contention on the native path, or a tail mid-publish.
    Busy,
    /// A key, owner or session that isn't current.
    Stale,
    /// An inhibit is held, a microphone is bound, or a proof failed.
    Inhibited,
    /// The session's output is still opening.
    Opening,
    /// Another generation's tail is still pending.
    Debt,
    /// Some tail can't be shown played: until restart.
    Terminal,
    /// The live generation hasn't gone quiet under this pause.
    NotQuiet,
    /// The live generation exposed content, and D1 qualifies no route.
    Unqualified,
    /// The speaker couldn't take a command, so the session is ending.
    Lost,
    /// A serial or revision ran out.
    Overflow,
}

/// What the UI may show about outputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputView {
    /// The current output's key.
    pub key: Option<OutputKey>,
    /// Why the session has no output.
    pub why: Option<Why>,
    /// An earlier generation is still settling.
    pub retiring: bool,
    /// A tail couldn't be shown played.
    pub terminal: bool,
}

enum Verdict {
    Pending,
    Clean,
    Terminal,
}

/// A generation's verdict from its own tail. Only the ledger holding it means no worker, engine
/// or thread can write it again.
fn verdict(tail: &Arc<Tail>) -> Verdict {
    let gone = Arc::strong_count(tail) == 1;
    fence(Ordering::Acquire);
    let Some(now) = tail.read() else {
        return if gone {
            Verdict::Terminal
        } else {
            Verdict::Pending
        };
    };
    let owed = now.owed();
    if gone || now.flags & RELEASED != 0 {
        if owed {
            Verdict::Terminal
        } else {
            Verdict::Clean
        }
    } else if now.flags & FAULT != 0 && owed {
        Verdict::Terminal
    } else {
        Verdict::Pending
    }
}

impl State {
    fn settle(&mut self) {
        let Self {
            ledger,
            output,
            epochs,
            ..
        } = self;
        for slot in ledger.iter_mut() {
            // An opening generation settles only once its open's outcome is applied.
            let Some(entry) = slot
                .as_mut()
                .filter(|entry| !entry.terminal && *output != Output::Opening(entry.generation))
            else {
                continue;
            };
            let live = *output == Output::Live(entry.generation);
            match verdict(&entry.tail) {
                Verdict::Pending => {}
                Verdict::Clean => {
                    *slot = None;
                    if live {
                        *epochs += 1;
                        *output = Output::Quiet {
                            epoch: *epochs,
                            why: Why::Retired,
                        };
                    }
                }
                Verdict::Terminal => {
                    entry.terminal = true;
                    if live {
                        *output = Output::Lost(entry.generation);
                    }
                }
            }
        }
    }

    fn quiet(&mut self, why: Why) {
        self.epochs += 1;
        self.output = Output::Quiet {
            epoch: self.epochs,
            why,
        };
    }

    /// Queues a command on the speaker. A failed enqueue drops the output and hands back the
    /// session's abort.
    fn command(&mut self, command: Command) -> Option<Abort> {
        if self.commands.as_ref()?.try_send(command).is_ok() {
            return None;
        }
        self.commands = None;
        self.output = match self.output {
            Output::Opening(generation) | Output::Live(generation) | Output::Lost(generation) => {
                Output::Lost(generation)
            }
            _ => Output::Closed,
        };
        self.abort.clone()
    }

    fn held(&mut self, token: &InhibitToken) -> Result<&mut Inhibit, Denied> {
        self.inhibit
            .as_mut()
            .filter(|inhibit| inhibit.token() == *token)
            .ok_or(Denied::Stale)
    }

    fn live(&self) -> Option<OutputGeneration> {
        match self.output {
            Output::Live(generation) => Some(generation),
            _ => None,
        }
    }

    /// Everything a claim needs, now.
    fn check(&self, claim: &Claim) -> Result<(), Denied> {
        if self.session != Some(claim.session) {
            return Err(Denied::Stale);
        }
        let inhibit = self
            .inhibit
            .as_ref()
            .filter(|inhibit| inhibit.token() == claim.owner)
            .ok_or(Denied::Stale)?;
        if inhibit.mic.is_none() {
            return Err(Denied::Inhibited);
        }
        let mut entries = self.ledger.iter().flatten();
        if entries.clone().any(|entry| entry.terminal) {
            return Err(Denied::Terminal);
        }
        if entries.any(|entry| Some(entry.generation) != self.live()) {
            return Err(Denied::Debt);
        }
        match (claim.key, self.output) {
            (OutputKey::NoOutput(key), Output::Quiet { epoch, .. }) if key == epoch => Ok(()),
            (OutputKey::Generation(key), Output::Live(generation)) if key == generation => {
                let now = self
                    .ledger
                    .iter()
                    .flatten()
                    .find(|entry| entry.generation == generation)
                    .ok_or(Denied::Stale)?
                    .tail
                    .read()
                    .ok_or(Denied::Busy)?;
                if now.exposed > 0 || now.flags & UNKNOWN != 0 {
                    Err(Denied::Unqualified)
                } else if now.quiet != claim.owner.serial || now.owed() {
                    Err(Denied::NotQuiet)
                } else {
                    Ok(())
                }
            }
            _ => Err(Denied::Stale),
        }
    }
}

impl OutputOwner {
    fn lock(&self) -> MutexGuard<'_, State> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.settle();
        state
    }

    fn try_lock(&self) -> Result<MutexGuard<'_, State>, Denied> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(state)) => state.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(Denied::Busy),
        };
        state.settle();
        Ok(state)
    }

    fn wake(&self) {
        // A full channel already holds a wake.
        let _woken = self.changed.try_send(());
    }

    /// Ends a session whose speaker couldn't take a command, then wakes waiting claims.
    fn after(&self, abort: Option<Abort>) {
        if let Some(abort) = abort {
            abort();
        }
        self.wake();
    }

    /// Wakes once per change that can grant or refuse; read [`OutputOwner::view`] or claim
    /// again after each.
    pub fn changes(&self) -> Receiver<()> {
        self.changes.clone()
    }

    /// For a speaker's worker, which wakes the owner without locking it.
    pub(super) fn waker(&self) -> Sender<()> {
        self.changed.clone()
    }

    /// The link thread starts `session`, before it says Connected. With an opener (`abort`
    /// ends the session's pipes) and talk-back on, it reserves a ledger slot and a generation
    /// before the opener runs and returns them; without a slot or a generation it refuses.
    pub(super) fn begin(
        &self,
        session: u64,
        abort: Option<Abort>,
    ) -> Option<(OutputGeneration, Arc<Tail>)> {
        let mut state = self.lock();
        state.session = Some(session);
        let opener = abort.is_some();
        state.abort = abort;
        let free = state.ledger.iter().position(Option::is_none);
        let next = state.generations.checked_add(1);
        let begun = match (opener, state.off, free, next) {
            (false, ..) => {
                state.quiet(Why::NoOpener);
                None
            }
            (true, true, ..) => {
                state.quiet(Why::Off);
                None
            }
            (true, false, Some(free), Some(next)) => {
                let generation = OutputGeneration(next);
                let tail = Arc::<Tail>::default();
                state.generations = next;
                state.ledger[free] = Some(Entry {
                    generation,
                    tail: Arc::clone(&tail),
                    terminal: false,
                });
                state.output = Output::Opening(generation);
                Some((generation, tail))
            }
            _ => {
                state.quiet(Why::Refused);
                None
            }
        };
        drop(state);
        self.wake();
        begun
    }

    /// The link thread's open of `generation` returned, before its reader starts. A kept speaker
    /// is fenced first: withdrawn when talk-back went off, else paused under a held inhibit, and
    /// only then accepted, so its announcement can't precede its fence. An
    /// `Err` speaker couldn't be fenced: the link drops it and ends the session. A stream that
    /// retired before this keeps its entry until now, so the session never goes live without it.
    ///
    /// # Errors
    /// The speaker, when it couldn't be fenced.
    pub(super) fn opened(
        &self,
        generation: OutputGeneration,
        speaker: Option<Speaker>,
    ) -> Result<Option<Speaker>, Speaker> {
        let mut state = self.lock();
        let opening = state.output == Output::Opening(generation);
        let opened = match speaker {
            None => {
                if opening {
                    state.quiet(Why::Refused);
                }
                Ok(None)
            }
            Some(speaker) => {
                let fence = if state.off {
                    Some(Command::Withdraw)
                } else {
                    state
                        .inhibit
                        .as_ref()
                        .map(|inhibit| Command::Pause(inhibit.serial))
                };
                if opening && fence.is_none_or(|fence| speaker.command(fence)) {
                    state.commands = speaker.commands();
                    state.output = Output::Live(generation);
                    speaker.accept();
                    Ok(Some(speaker))
                } else {
                    if opening {
                        state.output = Output::Lost(generation);
                    }
                    Err(speaker)
                }
            }
        };
        drop(state);
        self.wake();
        opened
    }

    /// The link thread ends `session`, first in teardown: claims stop matching, a bound
    /// microphone is cancelled, and the speaker's queue and the abort are let go here. Pending
    /// tails stay in the ledger; the inhibit stays for the next session.
    pub(super) fn end(&self, session: u64) {
        let mut state = self.lock();
        if state.session != Some(session) {
            return;
        }
        state.session = None;
        state.output = Output::Closed;
        if let Some(mic) = state
            .inhibit
            .as_ref()
            .and_then(|inhibit| inhibit.mic.as_ref())
        {
            mic.cancel();
        }
        let released = (state.commands.take(), state.abort.take());
        drop(state);
        drop(released);
        self.wake();
    }

    /// Takes the capture inhibit for `gesture` and pauses a live speaker under its serial. Never
    /// waits: contention is `Busy`, and the gesture gets nothing.
    ///
    /// # Errors
    /// `Busy`, `Inhibited` while one is held, `Overflow`, or `Lost` when the speaker
    /// couldn't take the pause, which ends the session.
    pub fn acquire(&self, gesture: GestureId) -> Result<InhibitToken, Denied> {
        let mut state = self.try_lock()?;
        if state.inhibit.is_some() {
            return Err(Denied::Inhibited);
        }
        let serial = state.serials.checked_add(1).ok_or(Denied::Overflow)?;
        state.serials = serial;
        let abort = state.command(Command::Pause(serial));
        let acquired = if abort.is_some() {
            Err(Denied::Lost)
        } else {
            let inhibit = Inhibit {
                serial,
                gesture,
                revision: 0,
                mic: None,
            };
            let token = inhibit.token();
            state.inhibit = Some(inhibit);
            Ok(token)
        };
        drop(state);
        self.after(abort);
        acquired
    }

    /// Moves the inhibit to gesture `to` under the same serial, before a microphone is bound.
    /// Never waits; on any refusal `token` stays good.
    ///
    /// # Errors
    /// `Busy`, `Stale` for a token that isn't current, `Inhibited` once a microphone is
    /// bound, or `Overflow`.
    pub fn transfer(&self, token: &InhibitToken, to: GestureId) -> Result<InhibitToken, Denied> {
        let mut state = self.try_lock()?;
        let inhibit = state.held(token)?;
        if inhibit.mic.is_some() {
            return Err(Denied::Inhibited);
        }
        inhibit.revision = inhibit.revision.checked_add(1).ok_or(Denied::Overflow)?;
        inhibit.gesture = to;
        Ok(inhibit.token())
    }

    /// Binds the inhibit, once, to its gesture's microphone generation.
    ///
    /// # Errors
    /// `Stale` for another token or gesture, `Inhibited` once bound.
    pub fn bind_mic(&self, token: &InhibitToken, mic: &Generation) -> Result<(), Denied> {
        let mut state = self.lock();
        let inhibit = state.held(token)?;
        if inhibit.mic.is_some() {
            return Err(Denied::Inhibited);
        }
        if mic.gesture() != inhibit.gesture {
            return Err(Denied::Stale);
        }
        inhibit.mic = Some(mic.clone());
        Ok(())
    }

    /// A claim on `session`'s current output under `token`, with the owner's key for it.
    ///
    /// # Errors
    /// `Opening` until the open is settled: wait for a wake and claim again. Otherwise what
    /// the check refuses.
    pub fn claim(&self, token: &InhibitToken, session: u64) -> Result<Claim, Denied> {
        let state = self.lock();
        let key = match state.output {
            Output::Live(generation) => OutputKey::Generation(generation),
            Output::Quiet { epoch, .. } => OutputKey::NoOutput(epoch),
            Output::Opening(_) => return Err(Denied::Opening),
            Output::Closed | Output::Lost(_) => return Err(Denied::Stale),
        };
        let claim = Claim {
            session,
            key,
            owner: InhibitToken {
                serial: token.serial,
                gesture: token.gesture,
                revision: token.revision,
            },
        };
        state.check(&claim)?;
        Ok(claim)
    }

    /// Checks `claim` and, in the same step, claims the bound microphone's single start; the
    /// caller then starts it with [`MicOwner::start_reserved`].
    ///
    /// # Errors
    /// What the check refuses, or `Stale` when the generation can't start.
    pub fn start_mic(&self, claim: &Claim) -> Result<Reserved, Denied> {
        let state = self.lock();
        state.check(claim)?;
        state
            .inhibit
            .as_ref()
            .and_then(|inhibit| inhibit.mic.as_ref())
            .ok_or(Denied::Inhibited)?
            .reserve()
            .map_err(|_| Denied::Stale)
    }

    /// Checks `claim` and, in the same step, lets `mic` collect on its running receipt, only
    /// when `mic` runs the bound generation itself.
    ///
    /// # Errors
    /// What the check refuses, or `Stale` for another microphone, another gesture's receipt or
    /// a generation that can't collect.
    pub fn collect(&self, claim: &Claim, mic: &MicOwner, running: Running) -> Result<(), Denied> {
        let state = self.lock();
        state.check(claim)?;
        let bound = state
            .inhibit
            .as_ref()
            .and_then(|inhibit| inhibit.mic.as_ref())
            .ok_or(Denied::Inhibited)?;
        if !mic.runs(bound) || running.gesture != claim.owner.gesture {
            return Err(Denied::Stale);
        }
        mic.accept_running(running).map_err(|_| Denied::Stale)
    }

    /// Lets go of the inhibit on a proof the owner checks on the bound microphone, and resumes
    /// the speaker. A refused proof hands `token` back, so a later one can still retire it.
    ///
    /// # Errors
    /// `Stale` for a token that isn't current, `Inhibited` for a proof that doesn't hold.
    pub fn retire(
        &self,
        token: InhibitToken,
        how: Retirement,
    ) -> Result<(), (InhibitToken, Denied)> {
        let mut state = self.lock();
        let proved = match state.held(&token) {
            Err(denied) => return Err((token, denied)),
            Ok(inhibit) => match (how, &inhibit.mic) {
                (Retirement::NoMic, None) => true,
                (Retirement::Unstarted, Some(mic)) => mic.discard_unstarted().is_ok(),
                (Retirement::Retired, Some(mic)) => mic.finished(),
                _ => false,
            },
        };
        if !proved {
            return Err((token, Denied::Inhibited));
        }
        state.inhibit = None;
        let abort = state.command(Command::Resume(token.serial));
        drop(state);
        self.after(abort);
        Ok(())
    }

    /// Queues the speaker's `Stop` of `run` on `session` only, after the brain accepted its
    /// own stop. False when `session` isn't current, or the speaker couldn't take it, which ends
    /// the session.
    pub fn stop(&self, session: u64, run: RunId) -> bool {
        let mut state = self.lock();
        if state.session != Some(session) {
            return false;
        }
        let abort = state.command(Command::Stop(run));
        drop(state);
        let stopped = abort.is_none();
        self.after(abort);
        stopped
    }

    /// Talk-back off: a live speaker is withdrawn, an opening one when it opens, and later
    /// sessions get none.
    pub fn withdraw(&self) {
        let mut state = self.lock();
        if std::mem::replace(&mut state.off, true) {
            return;
        }
        let abort = state.command(Command::Withdraw);
        drop(state);
        self.after(abort);
    }

    /// The current output, settled.
    pub fn view(&self) -> OutputView {
        let state = self.lock();
        let (key, why) = match state.output {
            Output::Live(generation) => (Some(OutputKey::Generation(generation)), None),
            Output::Quiet { epoch, why } => (Some(OutputKey::NoOutput(epoch)), Some(why)),
            _ => (None, None),
        };
        let mut entries = state.ledger.iter().flatten();
        OutputView {
            key,
            why,
            retiring: entries
                .clone()
                .any(|entry| !entry.terminal && Some(entry.generation) != state.live()),
            terminal: entries.any(|entry| entry.terminal),
        }
    }
}
