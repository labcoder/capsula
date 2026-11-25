//! What a held-key voice turn's lifecycle decides, apart from the windows, tasks and microphone
//! that apply it: what each brain status, run state, end and lost brain does to the pending
//! hold, which notice it leaves, and which run a native Esc press may still stop. The shell
//! samples its owners, applies each returned step, and reads this state again after anything
//! that changes it.

use kumochi_protocol::message::{
    Content, GestureId, HoldFailure, HoldStatus, MAX_TRANSCRIPT_BYTES, NoticeRevision, Phase,
    RunId, RunOrigin, ThreadId,
};

use crate::link::Session;
use crate::platform::microphone::Generation;
use crate::runs::{Applied, RunTail};
use crate::strings::Text;

#[derive(Debug)]
pub enum FailureStage {
    Input,
    Inhibit,
    Admission,
    CaptureStart,
    CaptureReceipt,
    Release,
    Run,
    Brain,
    Notice,
}

#[derive(Debug)]
pub enum FailureReason {
    GestureUnavailable,
    DismissPanels,
    CancelDrain,
    RecognitionSend,
    FenceSend,
    PacketCountMismatch,
    RunBeforeStop,
    NoSession,
    NoInhibit,
    PendingHold,
    StaleContext,
    InputFence,
    GenerationNotPreparing,
    EscapeLease,
    NoPendingHold,
    StaleStatus,
    TerminalStatus,
    RunningRejected,
    WarmSend,
    StopSend,
    ReleaseSend,
    StaleReceipt,
    PointerUnread,
    CaptureClockUnavailable,
    ScreenReleaseSend,
}

/// The target, connection and choice a hold's key-down was seen under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub target: (Option<ThreadId>, u64),
    pub session: u64,
    pub choice: u64,
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

/// The pending hold. Its identity, run and terminal notice change only here; the capture
/// adapter moves its source, collection, release, screen ask and retirement wait.
pub struct Hold {
    token: Generation,
    stamp: Stamp,
    session: Session,
    pub source: Source,
    /// The owner accepted this hold's collection: only then can its release send words.
    pub collected: bool,
    pub released: Option<u64>,
    /// A hold the brain was told shows the screen, until it asks for that release: once, when
    /// the keys come up or when the notice stops being `On` before they do.
    pub screen: bool,
    /// The offer such a hold's first input was seen under; its release needs it still current.
    offer: Option<Offer>,
    terminal: Option<Text>,
    /// Later run states carry no origin, so they match the hold by the run its first one named.
    run: Option<RunTail>,
    /// A speaking queued hold's release waits for its own inhibit's retirement: its reply can
    /// start before then.
    pub unretired: bool,
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

/// What a brain status does to the hold it names.
#[derive(Debug, PartialEq, Eq)]
pub enum StatusStep {
    Ignored,
    /// Install the admitted hold's waiter, then check it at once.
    Admit,
    Notice(Text),
    End(Text),
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

    /// Whether a hold's keys are down: recognized, and neither released nor ended.
    pub fn held(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|hold| hold.released.is_none() && hold.terminal.is_none())
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
                ended.notice = if terminal == Text::HoldUnavailable || text == Text::HoldUnavailable
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
            tracing::warn!(stage = ?FailureStage::Run, reason = ?FailureReason::RunBeforeStop, "held speech input failed");
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
            tracing::debug!(stage = ?FailureStage::Brain, reason = ?FailureReason::StaleStatus, "held speech input refused");
            return StatusStep::Ignored;
        };
        if hold.terminal.is_some() {
            tracing::debug!(stage = ?FailureStage::Brain, reason = ?FailureReason::TerminalStatus, "held speech input refused");
            return StatusStep::Ignored;
        }
        match status {
            HoldStatus::Admitted if matches!(hold.source, Source::Waiting) => StatusStep::Admit,
            HoldStatus::Admitted => StatusStep::Ignored,
            HoldStatus::Loading => StatusStep::Notice(Text::VoiceLoading),
            HoldStatus::Transcribing => StatusStep::Notice(Text::HoldTranscribing),
            HoldStatus::Empty => StatusStep::End(Text::HoldEmpty),
            HoldStatus::Cancelled => StatusStep::End(Text::HoldCancelled),
            HoldStatus::Failed(reason) => {
                tracing::warn!(stage = ?FailureStage::Brain, ?reason, "held speech input failed");
                StatusStep::End(match reason {
                    HoldFailure::NotLoaded => match speech() {
                        text @ (Text::VoiceNotInstalled | Text::VoiceUnloaded) => text,
                        _ => Text::VoiceStatusUnknown,
                    },
                    HoldFailure::Busy => Text::TurnBusy,
                    _ => Text::HoldFailed,
                })
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
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
