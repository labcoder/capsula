//! What a held-key voice turn decides, apart from the windows, tasks and microphone that apply
//! it: what each admission, brain status, release, run state, physical receipt, end and lost
//! brain does to the pending hold, which notice it leaves, and which run a native Esc press may
//! still stop. The shell
//! samples its owners, applies each returned step, and reads this state again after anything
//! that changes it.

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use kumochi_protocol::geometry::PhysicalPoint;
use kumochi_protocol::message::{
    AudioSession, Content, ControlToBrain, GestureId, HoldFailure, HoldStatus,
    MAX_TRANSCRIPT_BYTES, NoticeRevision, Phase, RunId, RunOrigin, ThreadId,
};

use crate::link::output::Denied;
use crate::link::{InputCancel, Session};
use crate::platform::microphone::{Finalization, Generation, MicError, Mode};
use crate::runs::{Applied, Refused as RunRefused, RunTail};
use crate::strings::Text;

/// The target, connection and choice a hold's key-down was seen under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub target: (Option<ThreadId>, u64),
    pub session: u64,
    pub choice: u64,
}

impl Stamp {
    /// Whether the target, choice and open connection are still the ones it was seen under.
    pub fn current(
        self,
        target: (Option<ThreadId>, u64),
        choice: u64,
        session: Option<&Session>,
    ) -> bool {
        self.target == target
            && self.choice == choice
            && session.is_some_and(|s| s.generation() == self.session && s.open())
    }
}

/// The screen offer a hold's first input is seen under: the brain's revision, bound to the
/// connection, choice revision and target it was published for. `epoch` counts every change the
/// shell published, so an offer that went and came back is another one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    pub session: u64,
    pub choice: u64,
    pub target: (Option<ThreadId>, u64),
    pub revision: NoticeRevision,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum Source {
    Waiting,
    Recording,
    Joined(u32),
    Sent,
}

/// The pending hold. Every field changes only here.
pub struct Hold {
    token: Generation,
    stamp: Stamp,
    session: Session,
    source: Source,
    /// The owner accepted this hold's collection: only then can its release send words.
    collected: bool,
    released: Option<u64>,
    /// A hold the brain was told shows the screen, until it asks for that release: once, when
    /// the keys come up or when the notice stops being `On` before they do.
    screen: bool,
    /// The offer such a hold's first input was seen under; its release needs it still current.
    offer: Option<Offer>,
    terminal: Option<Text>,
    /// Later run states carry no origin, so they match the hold by the run its first one named.
    run: Option<RunTail>,
    /// A speaking queued hold's release waits for its own inhibit's retirement: its reply can
    /// start before then.
    unretired: bool,
}

impl Hold {
    pub fn token(&self) -> &Generation {
        &self.token
    }

    pub fn stamp(&self) -> Stamp {
        self.stamp
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn offer(&self) -> Option<Offer> {
        self.offer
    }

    pub fn terminal(&self) -> Option<Text> {
        self.terminal
    }

    pub fn run(&self) -> Option<RunTail> {
        self.run
    }

    pub fn source(&self) -> Source {
        self.source
    }

    pub fn collected(&self) -> bool {
        self.collected
    }

    pub fn released(&self) -> Option<u64> {
        self.released
    }

    pub fn unretired(&self) -> bool {
        self.unretired
    }

    /// The release that sends the hold's words: once its keys came up, its source joined every
    /// packet it forwarded, and its own inhibit retired.
    fn key_up(&self) -> Option<ControlToBrain> {
        let (Source::Joined(chunks), Some(t_qpc), false) =
            (self.source, self.released, self.unretired)
        else {
            return None;
        };
        Some(ControlToBrain::KeyUp {
            gesture: self.token.gesture(),
            t_qpc,
            chunks,
        })
    }
}

/// Fixtures that put a hold where a test needs it without driving its capture.
#[cfg(any(test, feature = "test-support"))]
impl Hold {
    pub fn set_source(&mut self, source: Source) {
        self.source = source;
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "a fixture sets the collection either way"
    )]
    pub fn set_collected(&mut self, collected: bool) {
        self.collected = collected;
    }

    pub fn set_released(&mut self, qpc: u64) {
        self.released = Some(qpc);
    }
}

/// An accepted hold's words, kept until the brain says its turn is stored, so a lost brain
/// can give them back as a draft.
pub struct Recovery {
    pub session: u64,
    pub gesture: GestureId,
    /// The held run, once Thinking names it, so its failure drops the words after the hold ends.
    pub run: Option<RunId>,
    pub target: (Option<ThreadId>, u64),
    pub picture: bool,
    pub words: Option<Content>,
}

impl std::fmt::Debug for Recovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recovery").finish_non_exhaustive()
    }
}

/// What a run state does to the pending hold once its receipts and words have followed it.
#[derive(Debug, PartialEq, Eq)]
pub enum RunStep {
    None,
    /// The brain refused the hold's input: it fails after the cancel its session deferred.
    Refused {
        gesture: GestureId,
        session: u64,
    },
    /// The hold's run came before its release was sent: it fails, and the connection ends.
    BeforeStop,
    Thinking,
    /// The run took the hold's input over: the hold is gone, and its gate and input clear.
    Answered(GestureId),
}

/// What ending the pending hold leaves to apply.
pub struct Ended {
    /// The hold torn down, whose marks, gate and input go.
    pub gesture: Option<GestureId>,
    /// Its unstarted generation, to discard and tell its session about.
    pub unused: Option<(Generation, Session)>,
    pub notice: Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cancel {
    End,
    Refresh,
    Idle,
}

/// A native Esc press that revoked a hold's input, until that hold's marks and cancel apply.
#[must_use]
pub struct Revoked {
    gesture: GestureId,
    tail: Option<RunTail>,
}

impl Revoked {
    /// After its marks and cancel: forgets the hold this press ended, and names the run that
    /// hold became only while it is still the one moving.
    pub fn settle(self, state: &mut State, moving: Option<RunId>) -> Option<RunTail> {
        state.ended.take_if(|(hold, _)| *hold == self.gesture);
        self.tail.filter(|tail| moving == Some(tail.run))
    }
}

/// Why a hold step was refused, named as the shell's diagnostics name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    PendingHold,
    StaleContext,
    InputFence,
    GenerationNotPreparing,
    EscapeLease,
    NoPendingHold,
    StaleStatus,
    TerminalStatus,
    PointerUnread,
}

/// A recognition's first refusal in its fixed order, and whether it still fences the spoken run.
#[derive(Debug, PartialEq, Eq)]
pub struct Admission {
    pub refused: Option<Refused>,
    /// A valid recognition still stops the earlier spoken run if Windows refused its Esc
    /// hotkey; any other refusal leaves it alone.
    pub fence: bool,
}

pub struct Recognition<'a> {
    pub screen: bool,
    pub notice: Text,
    pub unsent: Unsent<'a>,
}

pub enum Unsent<'a> {
    DiscardRun,
    StopOn(&'a Session),
}

impl<'a> Recognition<'a> {
    pub fn new(session: &'a Session, request: &Result<ControlToBrain, RunRefused>) -> Self {
        let (notice, unsent) = match request {
            Ok(_) => (Text::HoldFailed, Unsent::DiscardRun),
            Err(RunRefused::Busy) => (Text::TurnBusy, Unsent::StopOn(session)),
            Err(_) => (Text::HoldFailed, Unsent::StopOn(session)),
        };
        Self {
            screen: matches!(request, Ok(ControlToBrain::ScreenHoldRecognized { .. })),
            notice,
            unsent,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Collection {
    Complete,
    Retry,
    Cancel,
}

impl Collection {
    pub fn after(result: Result<(), Denied>) -> Self {
        match result {
            Ok(()) => Self::Complete,
            Err(Denied::Busy) => Self::Retry,
            Err(_) => Self::Cancel,
        }
    }
}

/// What a brain status does to the hold it names.
#[derive(Debug, PartialEq, Eq)]
pub enum StatusStep {
    Refused(Refused),
    Ignored,
    /// Install the admitted hold's waiter, then check it at once.
    Admit,
    Notice(Text),
    End(Text),
    /// The brain failed the hold for `reason`: it ends with the notice.
    Failed(HoldFailure, Text),
}

/// What a release delivery does.
pub enum Release {
    Refused(Refused),
    /// Anything but an accepted collection's first release cancels.
    Cancel,
    /// The release is stamped. A hold that shows the screen asks once on its own session, with
    /// release authority only when it was recognized under the delivery's stamp.
    Released {
        screen: Option<(Session, bool)>,
    },
}

/// Why a screen release goes voice only.
#[derive(Debug, PartialEq, Eq)]
pub enum VoiceOnly {
    TurnedOff,
    Refused(Refused),
}

/// What the physical receipt of a hold's worker does to the hold.
#[derive(Debug, PartialEq, Eq)]
pub enum ReceiptStep {
    /// The hold is gone: the gate took its proof already, and no notice is published.
    Stale,
    End(Text),
    /// Every forwarded packet joined: its release can send the words.
    Finish,
}

/// What discarding an unused generation leaves to apply once no gate took it.
pub enum Discard<'a> {
    Nothing,
    /// A start already claimed it and no session waits: its microphone gate cancels it.
    Cancel,
    /// Its session hears that the hold stopped.
    Stop(&'a Session),
}

/// Discards `token` unstarted where no gate holds it, the gate being the one authority while it
/// does. With `session`, a generation discarded now or already retired tells it the hold stopped.
pub fn discard<'a>(token: &Generation, session: Option<&'a Session>) -> Discard<'a> {
    let discarded = token.discard_unstarted().is_ok();
    match session {
        None if discarded => Discard::Nothing,
        None => Discard::Cancel,
        Some(session) if discarded || token.retired() => Discard::Stop(session),
        Some(_) => Discard::Nothing,
    }
}

/// A hold's target: the thread and its revision.
type Target = (Option<ThreadId>, u64);

#[derive(Default)]
pub struct State {
    pub pending: Option<Hold>,
    pub status: Option<Text>,
    pub recovery: Option<Recovery>,
    /// The pending hold torn down last and the run it became, if one was named, until the next
    /// native Esc press is applied: the press that revoked that hold's input acts on that hold
    /// and that run only.
    pub ended: Option<(GestureId, Option<RunTail>)>,
    /// What a lost brain did with the held words; reconnecting doesn't clear it.
    pub lost: Option<Text>,
}

impl State {
    fn receipts(&mut self) -> impl Iterator<Item = (GestureId, &mut Option<RunTail>)> {
        self.pending
            .as_mut()
            .map(|hold| (hold.token.gesture(), &mut hold.run))
            .into_iter()
            .chain(self.ended.as_mut().map(|(gesture, run)| (*gesture, run)))
    }

    /// The hold Esc belongs to: pending, and not ended with a notice.
    pub fn escape_owner(&self) -> Option<GestureId> {
        self.pending
            .as_ref()
            .filter(|hold| hold.terminal.is_none())
            .map(|hold| hold.token.gesture())
    }

    pub fn owns(&self, gesture: GestureId) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|hold| hold.token.gesture() == gesture)
    }

    /// Records the notice and says whether the chord still follows a held key.
    pub fn notice(&mut self, text: Text) -> bool {
        self.status = Some(text);
        if text == Text::HoldUnavailable {
            self.lost = None;
        }
        self.pending
            .as_ref()
            .is_some_and(|hold| hold.released.is_none() && hold.terminal.is_none())
    }

    pub fn cancel(
        &mut self,
        revoke: impl FnOnce() -> Option<InputCancel>,
    ) -> (Cancel, Option<InputCancel>) {
        self.forget_recovery();
        let queued = revoke();
        let action = if self.pending.is_some() {
            Cancel::End
        } else if self.status.take().is_some() {
            Cancel::Refresh
        } else {
            Cancel::Idle
        };
        (action, queued)
    }

    pub fn quit(&mut self, drop_views: impl FnOnce()) -> Option<Hold> {
        self.forget_recovery();
        self.status = None;
        drop_views();
        self.pending
            .take()
            .filter(|hold| matches!(hold.source, Source::Waiting))
    }

    /// The admitted hold begins waiting for its source, with its words kept for a lost brain.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "whether it shows the screen and waits for its inhibit are separate brain facts"
    )]
    pub fn begin(
        &mut self,
        token: Generation,
        stamp: Stamp,
        session: Session,
        screen: bool,
        offer: Option<Offer>,
        unretired: bool,
    ) {
        self.recovery = Some(Recovery {
            session: stamp.session,
            gesture: token.gesture(),
            run: None,
            target: stamp.target,
            picture: screen,
            words: None,
        });
        self.pending = Some(Hold {
            token,
            stamp,
            session,
            source: Source::Waiting,
            collected: false,
            released: None,
            screen,
            offer: offer.filter(|_| screen),
            terminal: None,
            run: None,
            unretired,
        });
    }

    /// Ends the pending hold with `text`. A recording source keeps it until its receipt, with
    /// its first terminal notice; an unstarted one is handed back to discard.
    pub fn end(&mut self, text: Text) -> Ended {
        if let Some(hold) = &self.pending {
            self.discard_recovery(hold.token.gesture());
        }
        let mut ended = Ended {
            gesture: None,
            unused: None,
            notice: text,
        };
        let Some(mut hold) = self.pending.take() else {
            return ended;
        };
        let gesture = hold.token.gesture();
        // The Esc press that revoked this hold's input may be applied only after this.
        self.ended = Some((gesture, hold.run));
        ended.gesture = Some(gesture);
        match hold.source {
            Source::Waiting => ended.unused = Some((hold.token, hold.session)),
            Source::Recording => {
                let terminal = *hold.terminal.get_or_insert(text);
                let published = self.status;
                ended.notice = if [Some(terminal), Some(text), published]
                    .contains(&Some(Text::HoldUnavailable))
                {
                    Text::HoldUnavailable
                } else {
                    Text::HoldStopping
                };
                self.pending = Some(hold);
            }
            Source::Joined(_) | Source::Sent => {}
        }
        ended
    }

    /// Follows a run state into the receipts of the pending and last ended holds and into the
    /// kept words, then says what it does to the pending hold.
    pub fn run_state(
        &mut self,
        run: RunId,
        thread: ThreadId,
        origin: Option<RunOrigin>,
        phase: Phase,
        applied: &Applied,
    ) -> RunStep {
        let ours = |gesture: GestureId, named: Option<RunId>| match origin {
            Some(RunOrigin::Hold { gesture: owner }) => owner == gesture,
            Some(_) => false,
            None => named == Some(run),
        };
        for (gesture, receipt) in self.receipts() {
            if ours(gesture, receipt.map(|tail| tail.run))
                && receipt.is_none_or(|tail| tail.run == run)
            {
                let tail = receipt.get_or_insert(RunTail {
                    run,
                    thread,
                    control_closed: false,
                    data_ended: false,
                });
                tail.control_closed |=
                    matches!(phase, Phase::Done | Phase::Failed | Phase::Interrupted);
                tail.data_ended |= applied.input_ended;
            }
        }
        // A held run that failed or stopped mustn't give its words back at a later loss.
        if let Some(slot) = self.recovery.as_mut()
            && ours(slot.gesture, slot.run)
        {
            if matches!(phase, Phase::Failed | Phase::Interrupted) {
                self.recovery = None;
            } else {
                slot.run = Some(run);
            }
        }
        if let Some(gesture) = applied.refused_hold
            && let Some(hold) = self
                .pending
                .as_ref()
                .filter(|hold| hold.token.gesture() == gesture && hold.terminal.is_none())
        {
            return RunStep::Refused {
                gesture,
                session: hold.stamp.session,
            };
        }
        if !applied.visible {
            return RunStep::None;
        }
        let Some(hold) = self.pending.as_ref().filter(|hold| {
            hold.terminal.is_none() && ours(hold.token.gesture(), hold.run.map(|tail| tail.run))
        }) else {
            return RunStep::None;
        };
        if !matches!(hold.source, Source::Sent) {
            return RunStep::BeforeStop;
        }
        match phase {
            Phase::Thinking => RunStep::Thinking,
            // Speech input finished once the run exists; its own error or stop speaks for it.
            Phase::Answering | Phase::Done | Phase::Failed | Phase::Interrupted => {
                let gesture = hold.token.gesture();
                self.ended = Some((gesture, hold.run));
                self.pending = None;
                self.status = None;
                RunStep::Answered(gesture)
            }
            _ => RunStep::None,
        }
    }

    pub fn input_ended(&mut self, run: RunId) {
        for (_, receipt) in self.receipts() {
            if let Some(tail) = receipt.as_mut().filter(|tail| tail.run == run) {
                tail.data_ended = true;
            }
        }
    }

    pub fn thread_deleted(&mut self, thread: ThreadId) {
        for (_, receipt) in self.receipts() {
            if let Some(tail) = receipt.as_mut().filter(|tail| tail.thread == thread) {
                tail.control_closed = true;
            }
        }
    }

    /// Keeps the accepted hold's words, from its own connection, until its receipt.
    pub fn words(&mut self, generation: u64, gesture: GestureId, words: Content) {
        if words.len() > MAX_TRANSCRIPT_BYTES {
            tracing::warn!("held words over the bound");
            return;
        }
        if let Some(slot) = self.recovery.as_mut().filter(|slot| {
            slot.session == generation && slot.gesture == gesture && slot.words.is_none()
        }) {
            slot.words = Some(words);
        }
    }

    /// The brain stored the hold's turn, so its words aren't needed any more.
    pub fn stored(&mut self, generation: u64, gesture: GestureId) {
        if self
            .recovery
            .as_ref()
            .is_some_and(|slot| slot.session == generation && slot.gesture == gesture)
        {
            self.recovery = None;
        }
    }

    pub fn discard_recovery(&mut self, gesture: GestureId) {
        if self
            .recovery
            .as_ref()
            .is_some_and(|slot| slot.gesture == gesture)
        {
            self.recovery = None;
        }
    }

    pub fn forget_recovery(&mut self) {
        self.recovery = None;
    }

    pub fn forget_ended(&mut self) {
        self.ended = None;
    }

    /// The brain was lost with a hold's turn unstored. Words its Esc didn't revoke go to
    /// `restore` only on the hold's own target, and the notice says what happened. False when
    /// nothing was kept.
    pub fn lose(
        &mut self,
        revoked: impl FnOnce(GestureId) -> bool,
        target: (Option<ThreadId>, u64),
        restore: impl FnOnce(Content) -> bool,
    ) -> bool {
        let Some(slot) = self.recovery.take().filter(|slot| !revoked(slot.gesture)) else {
            return false;
        };
        let restored =
            slot.target.0.is_some() && slot.target == target && slot.words.is_some_and(restore);
        self.lost = Some(match (restored, slot.picture) {
            (true, false) => Text::HoldLostRestored,
            (true, true) => Text::HoldLostRestoredPictured,
            (false, false) => Text::HoldLost,
            (false, true) => Text::HoldLostPictured,
        });
        true
    }

    /// A native Esc press revoked `gesture`'s input: the hold it acts on, pending or ended last,
    /// with the run that hold became. Its kept words go; marks and the cancel apply next.
    pub fn revoked(&mut self, gesture: GestureId) -> Option<Revoked> {
        let ended = self
            .ended
            .take_if(|(hold, _)| *hold == gesture)
            .map(|(_, tail)| tail);
        let pending = self
            .pending
            .as_ref()
            .filter(|hold| hold.token.gesture() == gesture)
            .map(|hold| hold.run);
        let tail = ended.or(pending)?;
        self.discard_recovery(gesture);
        Some(Revoked { gesture, tail })
    }

    /// Whether `gesture` is the pending hold and has no terminal notice.
    pub fn live(&self, gesture: GestureId) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|hold| hold.token.gesture() == gesture && hold.terminal.is_none())
    }

    /// Whether `gesture` is the pending hold and its source still records.
    pub fn recording(&self, gesture: GestureId) -> bool {
        self.pending.as_ref().is_some_and(|hold| {
            hold.token.gesture() == gesture && matches!(hold.source, Source::Recording)
        })
    }

    /// Whether `gesture`'s screen release may still go, read fresh at its release, after the
    /// display shows and after the screen watch is told: continuous, trusted input on its own
    /// fence first, then the live hold and its original stamp, then the capture lease.
    pub fn permits<'a>(
        &self,
        gesture: GestureId,
        input: impl FnOnce() -> bool,
        current: impl FnOnce() -> (Target, u64, Option<&'a Session>),
        lease: impl FnOnce() -> bool,
    ) -> bool {
        input()
            && self.pending.as_ref().is_some_and(|hold| {
                if hold.token.gesture() != gesture || hold.terminal.is_some() {
                    return false;
                }
                let (target, choice, session) = current();
                hold.stamp.current(target, choice, session) && lease()
            })
    }

    /// Samples queued consent before settings, then a stop still owed only if neither refused.
    pub fn held_back(
        audio: AudioSession,
        consent: impl FnOnce() -> bool,
        off: impl FnOnce() -> Option<Text>,
        stop_owed: impl FnOnce() -> bool,
    ) -> Option<Text> {
        let consent_pending = audio == AudioSession::QueuedRequired && !consent();
        off().or_else(|| (consent_pending || stop_owed()).then_some(Text::TurnBusy))
    }

    pub fn admission_refused(&mut self, notice: Text) {
        self.status = Some(notice);
    }

    pub fn no_inhibit(&self, off: impl FnOnce() -> Option<Text>) -> Option<Text> {
        self.pending
            .is_none()
            .then(|| off().unwrap_or(Text::TurnBusy))
    }

    pub fn replace_hold(&self) -> Option<Text> {
        self.pending.as_ref().map(|_| Text::HoldCancelled)
    }

    /// The warning a listening hold gets near `token`'s deadline: still before it, its notice
    /// still Listening, still under its own stamp and input (`current`), and still collecting.
    pub fn near_limit(
        &self,
        token: &Generation,
        alive: &AtomicBool,
        now: Instant,
        current: impl FnOnce() -> bool,
    ) -> Option<Text> {
        (now < token.hold_deadline()
            && self.status == Some(Text::HoldListening)
            && current()
            && token.mode(alive, now) == Ok(Mode::Collecting))
        .then_some(Text::HoldNearLimit)
    }

    /// A recognition that brought no generation: busy, unless a microphone may still be open.
    pub fn unarmed_notice(&self) -> Text {
        if self.status == Some(Text::HoldUnavailable) {
            Text::HoldUnavailable
        } else {
            Text::TurnBusy
        }
    }

    /// A recognition's refusals in order, each fact read only when nothing before it refused:
    /// the stamp still current, the input fence still its gesture's, the generation still
    /// preparing, and the Esc hotkey leased last.
    pub fn admission(
        &self,
        current: impl FnOnce() -> bool,
        input: impl FnOnce() -> bool,
        preparing: impl FnOnce() -> bool,
        lease: impl FnOnce() -> bool,
    ) -> Admission {
        let refused = if self.pending.is_some() {
            Some(Refused::PendingHold)
        } else if !current() {
            Some(Refused::StaleContext)
        } else if !input() {
            Some(Refused::InputFence)
        } else if !preparing() {
            Some(Refused::GenerationNotPreparing)
        } else if !lease() {
            Some(Refused::EscapeLease)
        } else {
            None
        };
        let fence = matches!(refused, None | Some(Refused::EscapeLease));
        Admission { refused, fence }
    }

    /// What the brain's `status` for the hold of `generation` and `gesture` does. `speech`
    /// reads speech input's state, only for a model that wasn't loaded.
    pub fn status(
        &self,
        generation: u64,
        gesture: GestureId,
        status: HoldStatus,
        speech: impl FnOnce() -> Text,
    ) -> StatusStep {
        let Some(hold) = self
            .pending
            .as_ref()
            .filter(|h| h.stamp.session == generation && h.token.gesture() == gesture)
        else {
            return StatusStep::Refused(Refused::StaleStatus);
        };
        if hold.terminal.is_some() {
            return StatusStep::Refused(Refused::TerminalStatus);
        }
        match status {
            HoldStatus::Admitted if matches!(hold.source, Source::Waiting) => StatusStep::Admit,
            HoldStatus::Admitted => StatusStep::Ignored,
            HoldStatus::Loading => StatusStep::Notice(Text::VoiceLoading),
            HoldStatus::Transcribing => StatusStep::Notice(Text::HoldTranscribing),
            HoldStatus::Empty => StatusStep::End(Text::HoldEmpty),
            HoldStatus::Cancelled => StatusStep::End(Text::HoldCancelled),
            HoldStatus::Failed(reason) => StatusStep::Failed(
                reason,
                match reason {
                    HoldFailure::NotLoaded => match speech() {
                        text @ (Text::VoiceNotInstalled | Text::VoiceUnloaded) => text,
                        _ => Text::VoiceStatusUnknown,
                    },
                    HoldFailure::Busy => Text::TurnBusy,
                    _ => Text::HoldFailed,
                },
            ),
        }
    }

    /// The pending hold's source started: its microphone now records.
    pub fn record(&mut self) {
        if let Some(hold) = self.pending.as_mut() {
            hold.source = Source::Recording;
        }
    }

    /// Accepts this source once. Publish the returned notice before opening its copies.
    pub fn collecting(
        &mut self,
        gesture: GestureId,
        accept: impl FnOnce() -> Result<(), Denied>,
    ) -> Result<Option<Text>, Denied> {
        let hold = self
            .pending
            .as_mut()
            .filter(|hold| {
                hold.token.gesture() == gesture && matches!(hold.source, Source::Recording)
            })
            .ok_or(Denied::Stale)?;
        if hold.collected {
            return if hold.released.is_some() || hold.terminal.is_some() {
                Err(Denied::Stale)
            } else {
                Ok(None)
            };
        }
        accept()?;
        hold.collected = true;
        Ok(Some(Text::HoldListening))
    }

    /// The keys of `gesture` came up at `qpc`, or its gesture was cancelled.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "a cancelled End is the gesture's own fact, beside its release time"
    )]
    pub fn release(
        &mut self,
        gesture: GestureId,
        cancelled: bool,
        qpc: u64,
        stamp: Stamp,
    ) -> Release {
        let Some(hold) = self.pending.as_mut().filter(|hold| {
            hold.token.gesture() == gesture
                && hold.terminal.is_none()
                && (cancelled || hold.released.is_none())
        }) else {
            return Release::Refused(Refused::NoPendingHold);
        };
        // A terminal hold keeps its notice: only a cancel still ends it.
        if hold.terminal.is_some() && !cancelled {
            return Release::Refused(Refused::TerminalStatus);
        }
        // Only an accepted collection releases, and only once; anything else cancels.
        if cancelled || !hold.collected || hold.released.is_some() {
            return Release::Cancel;
        }
        hold.released = Some(qpc);
        let screen =
            std::mem::take(&mut hold.screen).then(|| (hold.session.clone(), hold.stamp == stamp));
        Release::Released { screen }
    }

    /// The screen notice stopped being `On` while a hold that shows it had its keys down: that
    /// refusal is the gesture's one ask, so its release sends no screen even if it comes back.
    pub fn take_screen(&mut self) -> Option<GestureId> {
        let hold = self.pending.as_mut()?;
        std::mem::take(&mut hold.screen).then(|| hold.token.gesture())
    }

    /// Whether a release the runs model still asks to show the screen goes on, checked in order
    /// before any lease is taken: the show-screen choice not turned off since, under the offer
    /// its first input was seen under and `publish` still publishes, and with a pointer to pick
    /// the display.
    pub fn screen_release(
        &self,
        show: Option<bool>,
        publish: impl FnOnce() -> Option<Offer>,
        pointer: Option<PhysicalPoint>,
    ) -> Result<PhysicalPoint, VoiceOnly> {
        // shortcut: the pending Off veto is local to offers and release; revisit save ordering if pictures are ever granted earlier.
        if show == Some(false) {
            return Err(VoiceOnly::TurnedOff);
        }
        let offer = self.pending.as_ref().and_then(|hold| hold.offer);
        if offer.is_none() || offer != publish() {
            return Err(VoiceOnly::Refused(Refused::StaleContext));
        }
        // The pointer picks the display, so a release that couldn't read it shows none.
        pointer.ok_or(VoiceOnly::Refused(Refused::PointerUnread))
    }

    /// `gesture`'s own inhibit retired: a speaking queued hold's release may go now. False when
    /// it isn't the pending hold.
    pub fn retired(&mut self, gesture: GestureId) -> bool {
        let Some(hold) = self
            .pending
            .as_mut()
            .filter(|hold| hold.token.gesture() == gesture)
        else {
            return false;
        };
        hold.unretired = false;
        true
    }

    /// Sends the pending hold's words through `send` once its release is due. The hold whose
    /// release couldn't be sent comes back, for its connection's end.
    pub fn finish(&mut self, send: impl FnOnce(&Session, ControlToBrain) -> bool) -> Option<&Hold> {
        let hold = self.pending.as_mut()?;
        let message = hold.key_up()?;
        if send(&hold.session, message) {
            hold.source = Source::Sent;
            return None;
        }
        Some(hold)
    }

    /// The physical receipt of `gesture`'s worker, after `count` packets were forwarded. Only the
    /// hold still pending takes it; a recorded end beats a clean receipt.
    pub fn receipt(
        &mut self,
        gesture: GestureId,
        receipt: Result<Finalization, MicError>,
        count: u32,
    ) -> ReceiptStep {
        let Some(hold) = self
            .pending
            .as_mut()
            .filter(|h| h.token.gesture() == gesture)
        else {
            return ReceiptStep::Stale;
        };
        let Ok(finalization) = receipt else {
            return ReceiptStep::End(Text::HoldUnavailable);
        };
        let notice = match finalization {
            Finalization::Discarded(MicError::Cancelled | MicError::SessionLost) => {
                Text::HoldCancelled
            }
            Finalization::Discarded(MicError::Unavailable | MicError::Format) => {
                Text::HoldMicrophoneUnavailable
            }
            _ => Text::HoldFailed,
        };
        hold.source = Source::Sent;
        if let Some(terminal) = hold.terminal {
            return ReceiptStep::End(if terminal == Text::HoldUnavailable {
                notice
            } else {
                terminal
            });
        }
        if finalization == (Finalization::Released { packets: count }) {
            hold.source = Source::Joined(count);
            return ReceiptStep::Finish;
        }
        ReceiptStep::End(notice)
    }
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
