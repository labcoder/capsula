//! One link's output owner: each session's output, every output generation's tail debt, the
//! capture inhibit, and whether a microphone may start or collect (docs/development/speaker.md).
//! Each section under its lock is short and never blocks: opens, joins, pipe aborts and last
//! drops of a speaker happen after it.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::Instant;

use async_channel::{Receiver, Sender};
use kumochi_protocol::message::{GestureId, OutputGeneration, RunId};

use crate::platform::microphone::{Generation, MicOwner, Reserved, Running};
use crate::speaker::{Command, FAULT, RELEASED, Speaker, Tail, UNKNOWN};

use super::{HANDSHAKE_TIMEOUT, RETRY_DELAY};

mod queued;
pub(super) use queued::{NarratedReservation, Queued, RefusedTail};

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;

/// Output generations tracked until their tails settle; `LIVE` keeps one speaker thread at a
/// time, so this is a ceiling, not the safety argument.
const LEDGER: usize = 4;

/// Aborts one session's pipes.
pub type Abort = Arc<dyn Fn() + Send + Sync>;

/// A test's view of each speaker command as it is queued.
#[cfg(any(test, feature = "test-support"))]
pub type Witness = Box<dyn FnMut(&Command) + Send>;

/// The owner of a link's outputs and its capture inhibit.
// shortcut: tokens and claims carry no owner origin, which holds while one presence keeps one
// link and owner for its life; bind them to their owner once a presence opens a second link, or
// replaces its handle, even in turn, while deferred timers or completions survive.
pub struct OutputOwner {
    state: Mutex<State>,
    mic_retired: Condvar,
    changed: Sender<()>,
    changes: Receiver<()>,
    /// Runs as `opened` is entered, before its decision: a test's point between the link's own
    /// check and the owner's.
    #[cfg(any(test, feature = "test-support"))]
    before_opened: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// A test's contention: this many more non-waiting calls find the owner busy.
    #[cfg(any(test, feature = "test-support"))]
    refusals: std::sync::atomic::AtomicU32,
}

impl Default for OutputOwner {
    fn default() -> Self {
        let (sender, changes) = async_channel::bounded(1);
        Self {
            state: Mutex::default(),
            mic_retired: Condvar::new(),
            changed: sender,
            changes,
            #[cfg(any(test, feature = "test-support"))]
            before_opened: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            refusals: std::sync::atomic::AtomicU32::default(),
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
    queued: Option<Queued>,
    output: Output,
    commands: Option<SyncSender<Command>>,
    abort: Option<Abort>,
    ledger: [Option<Entry>; LEDGER],
    inhibit: Option<Inhibit>,
    off: bool,
    /// Sees each speaker command as it is queued: a test's point to check what came before.
    #[cfg(any(test, feature = "test-support"))]
    witness: Option<Witness>,
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
    Queued(OutputGeneration),
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
    retiring: bool,
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
    /// The exact registered queued output when replies are silent.
    SilentQueued(OutputGeneration),
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
        #[cfg(any(test, feature = "test-support"))]
        if let Some(witness) = self.witness.as_mut() {
            witness(&command);
        }
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

    fn tail(&self, generation: OutputGeneration) -> Option<&Arc<Tail>> {
        self.ledger
            .iter()
            .flatten()
            .find(|entry| entry.generation == generation)
            .map(|entry| &entry.tail)
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
            (OutputKey::SilentQueued(key), Output::Queued(_))
                if self.silent_queued() == Some(key) =>
            {
                Ok(())
            }
            (OutputKey::NoOutput(key), Output::Quiet { epoch, .. }) if key == epoch => Ok(()),
            (OutputKey::Generation(key), Output::Live(generation)) if key == generation => {
                let now = self
                    .tail(generation)
                    .ok_or(Denied::Stale)?
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
        #[cfg(any(test, feature = "test-support"))]
        if self
            .refusals
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(Denied::Busy);
        }
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

    pub(super) fn begin_queued(
        &self,
        session: u64,
        abort: Abort,
        reconnect_by: Instant,
        cancelled: impl Fn() -> bool,
        queued: impl FnOnce(Instant) -> Queued,
    ) -> Option<Instant> {
        let mut state = self.lock();
        let mut waiting = None;
        let generation = loop {
            if Instant::now() >= reconnect_by
                || state.session.is_some()
                || state.off
                || state.ledger.iter().any(Option::is_some)
                || state.queued.as_ref().is_some_and(Queued::owes)
            {
                return None;
            }
            let generation = state.generations.checked_add(1)?;
            let Some(inhibit) = &state.inhibit else {
                break generation;
            };
            if !inhibit.retiring
                || waiting.is_some_and(|serial| serial != inhibit.serial)
                || cancelled()
            {
                return None;
            }
            let left = reconnect_by.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            waiting = Some(inhibit.serial);
            (state, _) = self
                .mic_retired
                .wait_timeout(state, left.min(RETRY_DELAY))
                .unwrap_or_else(PoisonError::into_inner);
            state.settle();
        };
        let startup = Instant::now();
        if startup >= reconnect_by || cancelled() {
            return None;
        }
        let registration_by = startup + HANDSHAKE_TIMEOUT;
        state.generations = generation;
        state.session = Some(session);
        state.output = Output::Queued(OutputGeneration(generation));
        state.abort = Some(abort);
        state.queued = Some(queued(registration_by));
        Some(registration_by)
    }

    /// The link thread's open of `generation` returned, before its reader starts. Under the owner's
    /// lock it reads whether Quit began and whether its session is still `alive`. These reads
    /// decide whether the opening wins against Quit or session loss. A refused speaker announces
    /// nothing and is let go after the lock. A kept speaker is fenced first: withdrawn when talk-back went off,
    /// else paused under a held inhibit, and only then accepted, so its announcement can't
    /// precede its fence. An `Err` speaker couldn't be fenced: the link drops it and ends the
    /// session. A stream that retired before this keeps its entry until now, so the session never
    /// goes live without it.
    ///
    /// # Errors
    /// The speaker, when it couldn't be fenced.
    pub(super) fn opened(
        &self,
        generation: OutputGeneration,
        speaker: Option<Speaker>,
        quitting: impl FnOnce() -> bool,
        alive: &AtomicBool,
    ) -> Result<Option<Speaker>, Speaker> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(hook) = self
            .before_opened
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            hook();
        }
        let mut state = self.lock();
        let opening = state.output == Output::Opening(generation);
        let current = !quitting() && alive.load(Ordering::Acquire);
        let mut refused = None;
        let opened = match speaker {
            None => {
                if opening {
                    state.quiet(Why::Refused);
                }
                Ok(None)
            }
            Some(speaker) if !current => {
                if opening {
                    state.quiet(Why::Refused);
                }
                refused = Some(speaker);
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
        drop(refused);
        self.wake();
        opened
    }

    /// The session was lost, as the caller already published: waits for any opening decision.
    pub(super) fn cancelled(&self) {
        drop(self.state.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// Runs `hook` as the next `opened` is entered.
    #[cfg(any(test, feature = "test-support"))]
    pub fn before_opened(&self, hook: Box<dyn FnOnce() + Send>) {
        *self
            .before_opened
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    #[cfg(any(test, feature = "test-support"))]
    #[cfg(all(windows, debug_assertions))]
    pub(super) fn with_locked<T>(&self, run: impl FnOnce() -> T) -> T {
        let _held = self.lock();
        run()
    }

    /// Ends `session`, from [`LinkHandle::end`] or first in the link thread's teardown, whichever
    /// comes first: claims stop matching, a bound microphone is cancelled, and the speaker's queue
    /// and the abort are let go here. Pending tails stay in the ledger; the inhibit stays for the
    /// next session.
    pub(super) fn end(&self, session: u64) {
        let mut state = self.lock();
        if state.session != Some(session) {
            return;
        }
        let queued = matches!(state.output, Output::Queued(_));
        state.session = None;
        state.output = Output::Closed;
        if let Some(owner) = &mut state.queued {
            owner.close();
        }
        if let Some(inhibit) = &mut state.inhibit
            && let Some(mic) = &inhibit.mic
        {
            inhibit.retiring = true;
            mic.cancel();
        }
        let released = (state.commands.take(), state.abort.take());
        drop(state);
        if queued {
            self.after(released.1);
        } else {
            drop(released);
            self.wake();
        }
    }

    /// Takes the capture inhibit for `gesture` and pauses a live speaker under its serial. Never
    /// waits: contention is `Busy`, and the gesture gets nothing.
    ///
    /// # Errors
    /// `Busy`, `Inhibited` while one is held, `Overflow`, or `Lost` when the speaker
    /// couldn't take the pause, which ends the session.
    pub fn acquire(&self, gesture: GestureId) -> Result<InhibitToken, Denied> {
        let mut state = self.try_lock()?;
        if state.inhibit.is_some()
            || (matches!(state.output, Output::Queued(_)) && state.silent_queued().is_none())
            || state.queued.as_ref().is_some_and(Queued::owes)
        {
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
                retiring: false,
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

    /// Binds the inhibit, once, to its gesture's microphone generation. Never waits.
    ///
    /// # Errors
    /// `Busy`, `Stale` for another token or gesture, `Inhibited` once bound.
    pub fn bind_mic(&self, token: &InhibitToken, mic: &Generation) -> Result<(), Denied> {
        let mut state = self.try_lock()?;
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
    /// `Busy`, or `Opening` until the open is settled: wait for a wake and claim again.
    /// Otherwise what the check refuses.
    pub fn claim(&self, token: &InhibitToken, session: u64) -> Result<Claim, Denied> {
        let state = self.try_lock()?;
        let key = match state.output {
            Output::Live(generation) => OutputKey::Generation(generation),
            Output::Quiet { epoch, .. } => OutputKey::NoOutput(epoch),
            Output::Opening(_) => return Err(Denied::Opening),
            Output::Closed | Output::Lost(_) => return Err(Denied::Stale),
            Output::Queued(_) => {
                OutputKey::SilentQueued(state.silent_queued().ok_or(Denied::Unqualified)?)
            }
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
    /// `Busy`, what the check refuses, or `Stale` when the generation can't start.
    pub fn start_mic(&self, claim: &Claim) -> Result<Reserved, Denied> {
        let state = self.try_lock()?;
        state.check(claim)?;
        state
            .inhibit
            .as_ref()
            .and_then(|inhibit| inhibit.mic.as_ref())
            .ok_or(Denied::Inhibited)?
            .reserve()
            .map_err(|_| Denied::Stale)
    }

    /// Accepts `mic`'s running receipt under `claim`, with its copies still closed: the caller
    /// publishes the hold's state, then opens them with [`Self::collect`].
    ///
    /// # Errors
    /// `Busy`, what the check refuses, or `Stale` for another microphone, another gesture's
    /// receipt or a generation no longer running, such as one native input already released.
    pub fn accept(&self, claim: &Claim, mic: &MicOwner, running: Running) -> Result<(), Denied> {
        let state = self.try_lock()?;
        Self::runs_bound(&state, claim, mic, running)?;
        if !mic.still_running() {
            return Err(Denied::Stale);
        }
        Ok(())
    }

    /// Checks `claim` again and, in the same step, opens copies on the accepted receipt, only
    /// when `mic` runs the bound generation itself.
    ///
    /// # Errors
    /// `Busy`, what the check refuses, or `Stale` for another microphone, another gesture's
    /// receipt or a generation that can't collect.
    pub fn collect(&self, claim: &Claim, mic: &MicOwner, running: Running) -> Result<(), Denied> {
        let state = self.try_lock()?;
        Self::runs_bound(&state, claim, mic, running)?;
        mic.accept_running(running).map_err(|_| Denied::Stale)
    }

    fn runs_bound(
        state: &State,
        claim: &Claim,
        mic: &MicOwner,
        running: Running,
    ) -> Result<(), Denied> {
        state.check(claim)?;
        let bound = state
            .inhibit
            .as_ref()
            .and_then(|inhibit| inhibit.mic.as_ref())
            .ok_or(Denied::Inhibited)?;
        if !mic.runs(bound) || running.gesture != claim.owner.gesture {
            return Err(Denied::Stale);
        }
        Ok(())
    }

    /// Lets go of the inhibit on a proof the owner checks on the bound microphone, and resumes
    /// the speaker. A refused proof hands `token` back, so a later one can still retire it.
    ///
    /// # Errors
    /// `Busy`, `Stale` for a token that isn't current, `Inhibited` for a proof that doesn't
    /// hold.
    pub fn retire(
        &self,
        token: InhibitToken,
        how: Retirement,
    ) -> Result<(), (InhibitToken, Denied)> {
        let mut state = match self.try_lock() {
            Ok(state) => state,
            Err(denied) => return Err((token, denied)),
        };
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
        self.mic_retired.notify_one();
        self.after(abort);
        Ok(())
    }

    /// The run whose content `session`'s live output committed last and hasn't been stopped:
    /// positive evidence that it is speaking, even once the device has drained. Never waits: a
    /// busy owner or a tail mid-publish is no evidence.
    pub fn spoken(&self, session: u64) -> Option<RunId> {
        let state = self.try_lock().ok()?;
        if state.session != Some(session) {
            return None;
        }
        let now = state.tail(state.live()?)?.read()?;
        (now.spoken != 0).then_some(RunId(now.spoken))
    }

    /// Queues the speaker's `Stop` of `run` on `session` only, after the brain accepted its
    /// own stop. Never waits: [`Denied::Busy`] leaves the stop to the caller to offer again.
    ///
    /// # Errors
    /// [`Denied::Busy`] on contention, [`Denied::Stale`] when `session` isn't current, and
    /// [`Denied::Lost`] when the speaker couldn't take it. Queued admission closes immediately;
    /// its caller schedules the session teardown off the UI thread.
    pub fn stop(&self, session: u64, run: RunId) -> Result<(), Denied> {
        let mut state = self.try_lock()?;
        if state.session != Some(session) {
            return Err(Denied::Stale);
        }
        if matches!(state.output, Output::Queued(_))
            && let Some(queued) = &mut state.queued
        {
            if queued.stop(run, self::queued::StopOrigin::Local) {
                return Ok(());
            }
            queued.refuse();
            return Err(Denied::Lost);
        }
        let abort = state.command(Command::Stop(run));
        drop(state);
        let stopped = if abort.is_none() {
            Ok(())
        } else {
            Err(Denied::Lost)
        };
        self.after(abort);
        stopped
    }

    /// Opens `session`'s output with no speaker, as a reconnect's link thread does.
    #[cfg(any(test, feature = "test-support"))]
    pub fn begin_quiet(&self, session: u64) {
        let _opened = self.begin(session, None);
    }

    /// Makes the next `calls` non-waiting calls find the owner busy.
    #[cfg(any(test, feature = "test-support"))]
    pub fn refuse(&self, calls: u32) {
        self.refusals.store(calls, Ordering::Release);
    }

    /// Talk-back off: a live speaker is withdrawn, an opening one when it opens, and later
    /// sessions get none.
    pub fn withdraw(&self) {
        let mut state = self.lock();
        if std::mem::replace(&mut state.off, true) {
            return;
        }
        if matches!(state.output, Output::Queued(_))
            && let Some(session) = state.session
        {
            drop(state);
            self.end(session);
            return;
        }
        let abort = state.command(Command::Withdraw);
        drop(state);
        self.after(abort);
    }

    /// The current output, settled. Never waits.
    ///
    /// # Errors
    /// `Busy` on contention: no view, never a stale one.
    pub fn view(&self) -> Result<OutputView, Denied> {
        let state = self.try_lock()?;
        let (key, why) = match state.output {
            Output::Live(generation) => (Some(OutputKey::Generation(generation)), None),
            Output::Quiet { epoch, why } => (Some(OutputKey::NoOutput(epoch)), Some(why)),
            _ => (None, None),
        };
        let mut entries = state.ledger.iter().flatten();
        Ok(OutputView {
            key,
            why,
            retiring: entries
                .clone()
                .any(|entry| !entry.terminal && Some(entry.generation) != state.live()),
            terminal: entries.any(|entry| entry.terminal),
        })
    }

    /// Runs `witness` on each speaker command as it is queued.
    #[cfg(any(test, feature = "test-support"))]
    pub fn witness(&self, witness: Witness) {
        self.lock().witness = Some(witness);
    }

    /// Makes `session`'s output a live generation with no speaker behind it: its commands come
    /// out of the receiver, and the test publishes its tail.
    #[cfg(any(test, feature = "test-support"))]
    pub fn scripted(&self, session: u64) -> (std::sync::mpsc::Receiver<Command>, Arc<Tail>) {
        let (commands, receiver) = std::sync::mpsc::sync_channel(64);
        let tail = Arc::<Tail>::default();
        let mut state = self.lock();
        let generation = OutputGeneration(state.generations + 1);
        state.generations = generation.0;
        let free = state
            .ledger
            .iter()
            .position(Option::is_none)
            .expect("a free slot");
        state.ledger[free] = Some(Entry {
            generation,
            tail: Arc::clone(&tail),
            terminal: false,
        });
        state.session = Some(session);
        state.output = Output::Live(generation);
        state.commands = Some(commands);
        drop(state);
        (receiver, tail)
    }
}
