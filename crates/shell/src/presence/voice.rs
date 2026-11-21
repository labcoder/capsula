use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect, PointerPath};
use kumochi_protocol::message::{
    CaptureFor, Content, ControlToBrain, GestureId, HoldFailure, HoldStatus, LeaseGeneration,
    MAX_TRANSCRIPT_BYTES, ModelName, Phase, RunId, RunOrigin, ThreadId,
};

use gpui_kit::Window;

use super::gesture::ReleaseInput;
use super::ink::Sampler;
use super::overlay_visibility::Visibility;
use super::{Context, Presence, Session};
#[cfg(test)]
use crate::input::TalkHeld;
use crate::input::hold::Intent;
use crate::input::path::PathRecorder;
use crate::platform::microphone::{Generation, Mode};
use crate::runs::{HoldRelease, RunTail, ScreenNotice};
use crate::strings::{Strings, Text};

mod inhibit;
mod source;

pub(super) use inhibit::{Gates, HoldBack};

#[cfg(test)]
mod recovery_tests;

/// A screen notice or line, naming its model when it has one.
pub(super) fn screen_text(strings: &Strings, text: Text, model: Option<&ModelName>) -> String {
    model.map_or_else(
        || strings.get(text),
        |model| strings.with_model(text, model.as_str()),
    )
}

#[derive(Debug)]
enum FailureStage {
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
enum FailureReason {
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
    ScreenReleaseSend,
}

#[cfg(test)]
use crate::platform::microphone::{Finalization, MicOwner};
#[cfg(test)]
use kumochi_protocol::message::DataToBrain;

#[cfg(test)]
#[path = "voice_source_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "screen_marks_tests.rs"]
mod marks_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    pub(super) target: (Option<ThreadId>, u64),
    pub(super) session: u64,
    pub(super) choice: u64,
}

#[derive(Clone, Copy, Debug)]
enum Source {
    Waiting,
    Recording,
    Joined(u32),
    Sent,
}

pub(super) struct Hold {
    pub(super) token: Generation,
    stamp: Stamp,
    session: Session,
    source: Source,
    /// The owner accepted this hold's collection: only then can its release send words.
    pub(super) collected: bool,
    released: Option<u64>,
    /// A hold the brain was told shows the screen, until it asks for that release: once, when
    /// the keys come up or when the notice stops being `On` before they do.
    screen: bool,
    /// The offer such a hold's first input was seen under; its release needs it still current.
    offer: Option<inhibit::Offer>,
    terminal: Option<Text>,
    /// Later run states carry no origin, so they match the hold by the run its first one named.
    pub(super) run: Option<RunTail>,
}

/// An accepted hold's words, kept until the brain says its turn is stored, so a lost brain
/// can give them back as a draft.
pub(super) struct Recovery {
    session: u64,
    gesture: GestureId,
    /// The held run, once Thinking names it, so its failure drops the words after the hold ends.
    run: Option<RunId>,
    target: (Option<ThreadId>, u64),
    picture: bool,
    words: Option<Content>,
}

impl std::fmt::Debug for Recovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recovery").finish_non_exhaustive()
    }
}

#[derive(Default)]
pub(super) struct Voice {
    pub(super) target: Rc<Cell<(Option<ThreadId>, u64)>>,
    /// Set while the onboarding window shows; the native fence drops holds while it's set.
    pub(super) paused: Rc<Cell<bool>>,
    pub(super) pending: Option<Hold>,
    /// The hold the screen watch was last told was released (`marks.rs`).
    pub(super) watched: Option<super::marks::Watched>,
    pub(super) status: Option<Text>,
    pub(super) recovery: Option<Recovery>,
    /// The pending hold torn down last and the run it became, if one was named, until the next
    /// native Esc press is applied: `native_escape` takes it, and the press that revoked that
    /// hold's input acts on that hold and that run only.
    pub(super) ended: Option<(GestureId, Option<RunTail>)>,
    /// What a lost brain did with the held words; reconnecting doesn't clear it.
    pub(super) lost: Option<Text>,
    /// Each gesture's hold on the capture inhibit, and the run its recognition fences.
    pub(super) gates: Gates,
    #[cfg(test)]
    pub(super) start: Option<Start>,
    /// Runs as Listening is published, before the source's copies open.
    #[cfg(test)]
    pub(super) publish: Option<Box<dyn FnMut()>>,
}

#[cfg(test)]
type Start = Box<
    dyn FnOnce(
        crate::platform::microphone::Reserved,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<MicOwner, crate::platform::microphone::MicError>,
>;

impl Voice {
    fn receipts(&mut self) -> impl Iterator<Item = (GestureId, &mut Option<RunTail>)> {
        self.pending
            .as_mut()
            .map(|hold| (hold.token.gesture(), &mut hold.run))
            .into_iter()
            .chain(self.ended.as_mut().map(|(gesture, run)| (*gesture, run)))
    }
}

impl Presence {
    pub(super) fn voice_working(&self) -> bool {
        self.voice
            .pending
            .as_ref()
            .is_some_and(|hold| hold.terminal.is_none())
    }

    /// Whether a hold's keys are down: recognized, and neither released nor ended.
    fn voice_held(&self) -> bool {
        self.voice
            .pending
            .as_ref()
            .is_some_and(|hold| hold.released.is_none() && hold.terminal.is_none())
    }

    pub(super) fn voice_run(
        &mut self,
        run: RunId,
        thread: ThreadId,
        origin: Option<RunOrigin>,
        phase: Phase,
        applied: &crate::runs::Applied,
        cx: &mut Context<'_, Self>,
    ) {
        for (gesture, receipt) in self.voice.receipts() {
            let matching = match origin {
                Some(RunOrigin::Hold { gesture: owner }) => owner == gesture,
                Some(_) => false,
                None => receipt.is_some_and(|tail| tail.run == run),
            };
            if matching && receipt.is_none_or(|tail| tail.run == run) {
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
        if let Some(slot) = self.voice.recovery.as_mut() {
            let held = match origin {
                Some(RunOrigin::Hold { gesture }) => slot.gesture == gesture,
                Some(_) => false,
                None => slot.run == Some(run),
            };
            if held && matches!(phase, Phase::Failed | Phase::Interrupted) {
                self.voice.recovery = None;
            } else if held {
                slot.run = Some(run);
            }
        }
        if !applied.visible {
            return;
        }
        let Some(hold) = self.voice.pending.as_mut().filter(|hold| {
            hold.terminal.is_none()
                && match origin {
                    Some(RunOrigin::Hold { gesture }) => hold.token.gesture() == gesture,
                    Some(_) => false,
                    None => hold.run.is_some_and(|tail| tail.run == run),
                }
        }) else {
            return;
        };
        let tail = hold.run;
        let gesture = hold.token.gesture();
        if !matches!(hold.source, Source::Sent) {
            tracing::warn!(stage = ?FailureStage::Run, reason = ?FailureReason::RunBeforeStop, "held speech input failed");
            self.end_voice(Text::HoldFailed, cx);
            if let Some(session) = &self.session {
                self.link.end(session);
            }
            return;
        }
        match phase {
            Phase::Thinking => {
                self.voice_notice(Text::HoldThinking, cx);
                if self.show_voice_answer(cx).is_err() {
                    tracing::error!("can't show the voice answer");
                }
            }
            // Speech input finished once the run exists; its own error or stop speaks for it.
            Phase::Answering | Phase::Done | Phase::Failed | Phase::Interrupted => {
                self.link.microphone_gate().cancel(gesture);
                self.link.clear_input(gesture);
                self.voice.pending = None;
                self.voice.ended = Some((gesture, tail));
                self.voice.status = None;
                self.refresh_dock(cx);
            }
            _ => {}
        }
    }

    fn voice_current(&self, stamp: Stamp) -> bool {
        stamp.target == (self.runs.thread(), self.runs.revision())
            && stamp.choice == self.link.choice_revision()
            && self
                .session
                .as_ref()
                .is_some_and(|s| s.generation() == stamp.session && s.open())
    }

    fn voice_notice(&mut self, text: Text, cx: &mut Context<'_, Self>) {
        tracing::debug!(stage = ?FailureStage::Notice, reason = ?text, "held speech input notice");
        self.voice.status = Some(text);
        // A microphone that may still be open matters more than the lost brain's notice.
        if text == Text::HoldUnavailable {
            self.voice.lost = None;
        }
        // Every change to the pending hold ends in a notice, so the chord follows its keys here.
        if !self.voice_held() {
            self.talk_escape.release();
        }
        self.refresh_dock(cx);
        if self
            .runs_changed(cx)
            .and_then(|()| self.pointer_changed(cx))
            .is_err()
        {
            tracing::error!("can't update voice input status");
        }
    }

    /// Discards an unstarted generation, and tells `session` the hold stopped. A gate that holds
    /// it is the one discard authority: its dispatcher discards it through the owner.
    fn unused_voice(&self, token: &Generation, session: Option<&Session>) {
        if self.voice.gates.unused(token, session) {
            return;
        }
        let Some(session) = session else {
            if token.discard_unstarted().is_err() {
                self.link.microphone_gate().cancel(token.gesture());
            }
            return;
        };
        if (token.discard_unstarted().is_ok() || token.retired())
            && !self.link.send_control(
                session,
                ControlToBrain::VoiceHoldStopped {
                    gesture: token.gesture(),
                },
            )
        {
            tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::StopSend, "held speech input failed");
            self.link.end(session);
        }
    }

    /// Mirrors whether hold-to-talk can take a hold for the native callback.
    pub(super) fn follow_voice_input(&self, cx: &gpui_kit::App) {
        self.voice
            .gates
            .follow(self.settings.read(cx).hold_to_talk_notice().is_none());
    }

    fn drain_voice_cancel(&mut self) -> bool {
        self.runs.take_hold_cancel().is_none_or(|message| {
            self.session.as_ref().is_some_and(|s| {
                let sent = self.link.send_control(s, message);
                if !sent {
                    tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::CancelDrain, "held speech input failed");
                    self.link.end(s);
                }
                sent
            })
        })
    }

    fn end_voice(&mut self, text: Text, cx: &mut Context<'_, Self>) {
        if let Some(hold) = &self.voice.pending {
            self.discard_voice_recovery(hold.token.gesture());
        }
        self.stop_line(cx);
        let mut notice = text;
        if let Some(mut hold) = self.voice.pending.take() {
            let gesture = hold.token.gesture();
            // The Esc press that revoked this hold's input may be applied only after this.
            self.voice.ended = Some((gesture, hold.run));
            self.marks_dropped(gesture, cx);
            self.link.microphone_gate().cancel(gesture);
            self.link.clear_input(gesture);
            if matches!(hold.source, Source::Waiting) {
                self.unused_voice(&hold.token, Some(&hold.session));
            } else if matches!(hold.source, Source::Recording) {
                let terminal = *hold.terminal.get_or_insert(text);
                notice = if terminal == Text::HoldUnavailable || text == Text::HoldUnavailable {
                    Text::HoldUnavailable
                } else {
                    Text::HoldStopping
                };
                self.voice.pending = Some(hold);
            }
        }
        self.runs.cancel_hold();
        self.drain_voice_cancel();
        self.voice_notice(notice, cx);
    }

    pub(super) fn cancel_voice(&mut self, cx: &mut Context<'_, Self>) {
        self.voice.recovery = None;
        self.link.cancel_input();
        if self.voice.pending.is_some() {
            self.end_voice(Text::HoldCancelled, cx);
        } else if self.voice.status.take().is_some() {
            self.refresh_dock(cx);
            self.refresh_conversation(cx);
        }
        self.drain_voice_cancel();
    }

    pub(super) fn stop_voice_for_quit(&mut self) {
        self.voice.paused.set(true);
        self.voice.gates.follow(false);
        self.voice.recovery = None;
        self.voice.status = None;
        self.voice.watched = None;
        drop(self.ink.borrow_mut().take());
        if let Some(hold) = self.voice.pending.take()
            && matches!(hold.source, Source::Waiting)
        {
            self.unused_voice(&hold.token, None);
        }
        self.talk_escape.release();
        self.runs.cancel_hold();
        let _cancel = self.runs.take_hold_cancel();
    }

    /// Ends the pending hold if `gesture` is it, and says whether it did.
    pub(super) fn cancel_voice_gesture(
        &mut self,
        gesture: GestureId,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let pending = self
            .voice
            .pending
            .as_ref()
            .is_some_and(|hold| hold.token.gesture() == gesture);
        if pending {
            self.end_voice(Text::HoldCancelled, cx);
        }
        pending
    }

    /// The standing notice's two inputs: whether the target thread runs on its own model, and
    /// the default model's name. The dock's icon and every question a hold asks about the
    /// screen read these, so the icon never names a model the picture won't reach.
    fn screen_inputs<'a>(&self, cx: &'a gpui_kit::App) -> (bool, Option<&'a ModelName>) {
        let settings = self.settings.read(cx);
        // shortcut: a row not read on this connection shows as on the default model (OSC-763); a hold on it is only a candidate the brain proves before capture.
        let own = self
            .runs
            .thread()
            .and_then(|thread| self.threads.read(cx).fresh_row(thread))
            .and_then(|row| row.choice.as_ref());
        (settings.own_model(own), settings.default_model_name())
    }

    /// Publishes [`Presence::current_offer`] and returns it.
    pub(super) fn publish_offer(&self, cx: &gpui_kit::App) -> Option<inhibit::Offer> {
        self.voice.gates.publish(self.current_offer(cx))
    }

    /// The offer a first input may be seen under now, read only: the brain's `On` for exactly
    /// the default model while the show-screen choice is on, for a target that is new or not read
    /// on this connection as pinned elsewhere. An unread row leaves a candidate only: the brain
    /// proves its recipient before any capture.
    pub(super) fn current_offer(&self, cx: &gpui_kit::App) -> Option<inhibit::Offer> {
        let session = self.session.as_ref().filter(|session| session.open())?;
        let Some(kumochi_protocol::message::ScreenHolds::On {
            connection,
            slug,
            revision,
            ..
        }) = self.runs.screen_holds()
        else {
            return None;
        };
        let settings = self.settings.read(cx);
        let default = settings.default_model_choice()?;
        let thread = self.runs.thread();
        let target = matches!(self.runs.target(), crate::runs::Target::New)
            || thread
                .and_then(|thread| self.threads.read(cx).fresh_row(thread))
                .is_none_or(|row| {
                    row.choice
                        .as_ref()
                        .is_none_or(|pin| pin.connection == *connection && pin.model == *slug)
                });
        (target
            && settings.show_screen_choice() == Some(true)
            && default.connection == *connection
            && default.model == *slug
            && matches!(self.screen_notice(cx), ScreenNotice::On(_)))
        .then(|| inhibit::Offer {
            session: session.generation(),
            choice: self.link.choice_revision(),
            target: (thread, self.runs.revision()),
            revision: *revision,
            epoch: 0,
        })
    }

    /// What a hold on the current target does with the screen.
    pub(super) fn screen_notice<'a>(&'a self, cx: &'a gpui_kit::App) -> ScreenNotice<'a> {
        let (own_model, default) = self.screen_inputs(cx);
        self.runs.screen_notice(own_model, default)
    }

    /// Follows the notice while a hold that shows the screen has its keys down. A notice that
    /// stops being `On` ends the line at once, and that refusal is the gesture's one ask: the
    /// hold's line says why, and its release sends nothing even if the notice comes back.
    pub(super) fn screen_changed(&mut self, cx: &mut Context<'_, Self>) {
        self.publish_offer(cx);
        if matches!(self.screen_notice(cx), ScreenNotice::On(_)) {
            return;
        }
        let Some(gesture) = self
            .voice
            .pending
            .as_mut()
            .and_then(|hold| std::mem::take(&mut hold.screen).then(|| hold.token.gesture()))
        else {
            return;
        };
        self.stop_line(cx);
        let (own_model, default) = self.screen_inputs(cx);
        self.runs.releases_screen(gesture, own_model, default);
    }

    /// The primary display in physical pixels, which is the one the overlay covers and so the
    /// one a hold's line is drawn on, with its scale.
    pub(super) fn line_display(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Option<(PhysicalRect, f32)> {
        // The overlay sits on the primary display, so its scale is that display's; the dock may
        // be on another one, and stands in while no overlay window is open.
        let window = self
            .overlay
            .window()
            .map_or(self.dock_window, |overlay| overlay.window);
        let scale = window
            .update(cx, |_, window, _| window.scale_factor())
            .ok()?;
        let size = super::overlay::primary(scale, cx)?.map(i32::unsigned_abs);
        Some((
            PhysicalRect {
                origin: [0, 0],
                size,
            },
            scale,
        ))
    }

    /// Starts a screen hold's line from where the pointer was at recognition: the recorder, and
    /// the frames that sample into it.
    fn start_line(
        &mut self,
        gesture: GestureId,
        origin: PhysicalPoint,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((display, scale)) = self.line_display(cx) else {
            return;
        };
        let recorder = PathRecorder::new(origin, scale, display);
        *self.ink.borrow_mut() = Some(Sampler::new(gesture, recorder));
        self.want_line_frame(gesture, None, cx);
    }

    /// Asks for the next frame of `gesture`'s line. Once the overlay shows, its own frames
    /// carry the line, so the pointer a frame reads is in that frame's paint. Until then the
    /// dock's do, because Windows paints no hidden window. `current` is the window whose
    /// frame is running: GPUI holds it, so a handle can't reach it.
    // shortcut: a dock that fails closed (presence.md) gets no frames, so a hold's line never starts while it is hidden; move these frames with the drawing's when that failure must keep drawing.
    fn want_line_frame(
        &mut self,
        gesture: GestureId,
        current: Option<&mut Window>,
        cx: &mut Context<'_, Self>,
    ) {
        let target = if let Visibility::Shown(overlay) = &self.overlay {
            overlay.window
        } else {
            self.dock_window
        };
        let owner = cx.weak_entity();
        let arm = move |window: &mut Window| {
            window.on_next_frame(move |window, cx| {
                let stepped = owner.update(cx, |this, cx| this.line_frame(gesture, window, cx));
                if stepped.is_err() {
                    tracing::debug!("the presence closed before a hold's next frame");
                }
            });
        };
        let armed = match current {
            Some(window) if window.window_handle() == target => {
                arm(window);
                Ok(())
            }
            _ => target.update(cx, |_, window, _| arm(window)),
        };
        if let Err(error) = armed {
            tracing::warn!(%error, "can't follow the pointer; the hold's line stays as it is");
        }
    }

    /// One frame of `gesture`'s line. The line belongs to the pending hold while its keys are
    /// down: a frame that finds that hold over clears what is left of its line, and one that
    /// finds another hold's line, or none, only ends.
    fn line_frame(&mut self, gesture: GestureId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let ours = move |this: &Self| {
            this.ink
                .borrow()
                .as_ref()
                .is_some_and(|ink| ink.gesture() == gesture)
        };
        if !ours(self) {
            return;
        }
        let held = self.voice.pending.as_ref().is_some_and(|hold| {
            hold.token.gesture() == gesture && hold.released.is_none() && hold.terminal.is_none()
        });
        if !held {
            Self::after_frame(cx, move |this, cx| {
                if ours(this) {
                    this.stop_line(cx);
                }
            });
            return;
        }
        // The read takes no lock and doesn't block; a failed one is skipped.
        let point = crate::platform::cursor_position().ok().map(PhysicalPoint);
        let now = cx.background_executor().now();
        let changed = point.is_some_and(|point| {
            self.ink
                .borrow_mut()
                .as_mut()
                .is_some_and(|ink| ink.sample(point, now))
        });
        if changed && matches!(self.overlay, Visibility::Shown(_)) {
            // A notify: this frame paints the line where the pointer is now.
            self.line_changed(cx);
        } else if changed {
            Self::after_frame(cx, Self::line_changed);
        }
        self.want_line_frame(gesture, Some(window), cx);
    }

    /// Runs `step` once the running update is over. Showing and hiding the overlay reach
    /// windows by their handles, and GPUI holds the window whose frame is running.
    pub(super) fn after_frame(
        cx: &mut Context<'_, Self>,
        step: impl FnOnce(&mut Self, &mut Context<'_, Self>) + 'static,
    ) {
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            if owner.update(cx, step).is_err() {
                tracing::debug!("the presence closed before a hold's frame ended");
            }
        });
    }

    /// Ends a screen hold's sampling and takes its line off the overlay.
    fn stop_line(&mut self, cx: &mut Context<'_, Self>) -> Option<Sampler> {
        let sampler = self.ink.borrow_mut().take()?;
        self.line_changed(cx);
        Some(sampler)
    }

    /// The keys of a hold that shows the screen came up. Asks once whether the release still
    /// shows it, stops the line, covers every presence surface and stamps when the last one was
    /// covered, tells the screen watch, then sends the one `ScreenRelease`. Whatever stops it
    /// short leaves the hold voice only, and its line says why.
    fn release_screen(
        &mut self,
        gesture: GestureId,
        session: &Session,
        t_qpc: u64,
        pointer: Option<PhysicalPoint>,
        input: Option<&ReleaseInput>,
        cx: &mut Context<'_, Self>,
    ) {
        let sampler = self.stop_line(cx);
        let Some(input) = input.filter(|input| self.release_current(gesture, input)) else {
            self.runs.hold_screen_refused(gesture);
            return;
        };
        let (own_model, default) = self.screen_inputs(cx);
        if !self.runs.releases_screen(gesture, own_model, default) {
            return;
        }
        // An unchecked box that is still saving is already the user's no.
        if self.settings.read(cx).show_screen_choice() == Some(false) {
            self.runs.hold_screen_turned_off();
            return;
        }
        // The offer its first input was seen under must still be the one published now.
        let offer = self.voice.pending.as_ref().and_then(|hold| hold.offer);
        if offer.is_none() || offer != self.publish_offer(cx) {
            tracing::debug!(stage = ?FailureStage::Release, reason = ?FailureReason::StaleContext, "screen release refused; voice only");
            self.runs.hold_screen_refused(gesture);
            return;
        }
        // The pointer picks the display, so a release that couldn't read it shows none.
        let Some(pointer) = pointer else {
            tracing::debug!(stage = ?FailureStage::Release, reason = ?FailureReason::PointerUnread, "screen release refused; voice only");
            self.runs.hold_screen_refused(gesture);
            return;
        };
        #[cfg(test)]
        let read = {
            let fixture = self.counter_fixture.clone();
            move || fixture.map_or_else(crate::platform::performance_counter, |read| read())
        };
        #[cfg(not(test))]
        let read = crate::platform::performance_counter;
        let stamped = self.acquire_stamped(session, CaptureFor::Gesture(gesture), read, cx);
        let (lease, excluded_qpc) = match stamped {
            Ok(stamped) => stamped,
            Err(reason) => {
                tracing::debug!(stage = ?FailureStage::Release, ?reason, "screen release refused; voice only");
                self.runs.hold_screen_refused(gesture);
                return;
            }
        };
        // The line was drawn on the overlay's display, so a picture of another one carries none.
        let path = sampler
            .filter(|sampler| {
                let size = sampler.display();
                [0, 1]
                    .into_iter()
                    .all(|axis| u32::try_from(pointer.0[axis]).is_ok_and(|at| at < size[axis]))
            })
            .map_or_else(
                || PointerPath(Vec::new()),
                |sampler| sampler.finish(pointer),
            );
        let (own_model, default) = self.screen_inputs(cx);
        let release = HoldRelease {
            pointer,
            path,
            lease,
            t_qpc,
            excluded_qpc,
        };
        let message = self
            .runs
            .screen_release(gesture, own_model, default, release);
        let sent = message.is_some_and(|message| {
            if self.presenter.watches_screen() {
                self.show_display(cx);
            }
            if !self.release_current(gesture, input) || !self.exclusion.live(lease) {
                return false;
            }
            // The watch starts before the message is written: the brain takes the picture only
            // once it reads it, so the sources are listening before the picture exists.
            self.marks_released(gesture, input.held(), cx);
            self.release_current(gesture, input)
                && self.exclusion.live(lease)
                && self.link.send_control(session, message)
        });
        if sent {
            self.screen_lease = Some(lease);
        } else {
            tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::ScreenReleaseSend, "held speech input failed");
            self.screen_release_failed(gesture, lease, cx);
        }
    }

    fn release_current(&self, gesture: GestureId, input: &ReleaseInput) -> bool {
        input.continuous()
            && self.native.escape_trusted()
            && self.link.input_is(gesture)
            && self.voice.pending.as_ref().is_some_and(|hold| {
                hold.token.gesture() == gesture
                    && hold.terminal.is_none()
                    && self.voice_current(hold.stamp)
            })
    }

    fn screen_release_failed(
        &mut self,
        gesture: GestureId,
        lease: LeaseGeneration,
        cx: &mut Context<'_, Self>,
    ) {
        self.runs.hold_screen_refused(gesture);
        self.marks_dropped(gesture, cx);
        self.revoke_exclusion_generation(lease, cx);
    }

    /// Follows the current hold's line in the status panel, and says once in the conversation
    /// when the default model starts or stops taking a hold's picture. A link event batch that
    /// changes neither costs two comparisons and no allocation.
    pub(super) fn refresh_screen(&self, cx: &mut Context<'_, Self>) {
        let notice = self.screen_notice(cx);
        let sees = match notice {
            ScreenNotice::On(model) => Some((Text::ScreenNoteCan, model)),
            ScreenNotice::ModelCantSee(model) => Some((Text::ScreenNoteCant, model)),
            _ => None,
        }
        .filter(|(sees, _)| self.chat.read(cx).sees != Some(*sees))
        .map(|(sees, model)| (sees, model.clone()));
        let shown = self
            .runs
            .hold_screen_text(notice)
            .or_else(|| self.marks_line().map(|text| (text, None)));
        let old = &self.panel.read(cx).screen_line;
        let line = (old.as_ref().map(|(text, model)| (*text, model.as_ref())) != shown)
            .then(|| shown.map(|(text, model)| (text, model.cloned())));
        if let Some((sees, model)) = sees {
            let thread = self.runs.thread();
            self.chat.update(cx, |chat, cx| {
                if chat.screen_sees(thread, sees, &model) {
                    cx.notify();
                }
            });
        }
        if let Some(screen_line) = line {
            self.panel.update(cx, |panel, cx| {
                panel.screen_line = screen_line;
                cx.notify();
            });
        }
    }

    /// `pointer` is the position at recognition or release, and `input` is release authority.
    pub(super) fn voice_delivery(
        &mut self,
        intent: Intent,
        stamp: Stamp,
        qpc: u64,
        pointer: Option<PhysicalPoint>,
        input: Option<&ReleaseInput>,
        cx: &mut Context<'_, Self>,
    ) {
        if self.link.is_quitting() {
            if let Intent::Recognized {
                generation: Ok(token),
                ..
            } = intent
            {
                self.unused_voice(&token, None);
            }
            return;
        }
        match intent {
            Intent::Warm(gesture) => {
                if let Some(reason) = self
                    .settings
                    .read(cx)
                    .hold_to_talk_notice()
                    .or_else(|| self.link.voice_stop_pending().then_some(Text::TurnBusy))
                {
                    tracing::debug!(stage = ?FailureStage::Admission, ?reason, "held speech input refused");
                    return;
                }
                if let Some(session) = self
                    .session
                    .as_ref()
                    .filter(|s| s.generation() == stamp.session)
                {
                    let run = self.link.output().spoken(stamp.session);
                    self.voice
                        .gates
                        .capture(gesture, run.map(|run| (stamp.session, run)));
                    if !self.link.send_control(
                        session,
                        ControlToBrain::KeyDown {
                            gesture,
                            t_qpc: qpc,
                        },
                    ) {
                        tracing::debug!(stage = ?FailureStage::Admission, reason = ?FailureReason::WarmSend, "held speech input refused");
                    }
                } else {
                    tracing::debug!(stage = ?FailureStage::Admission, reason = ?FailureReason::NoSession, "held speech input refused");
                }
            }
            Intent::Recognized {
                gesture,
                generation,
            } => {
                // A new attempt says how it went, accepted or refused.
                self.voice.lost = None;
                self.recognize_voice(gesture, generation, stamp, qpc, pointer, cx);
            }
            Intent::End { gesture, cancelled } => {
                let Some(hold) = self.voice.pending.as_mut().filter(|hold| {
                    hold.token.gesture() == gesture
                        && hold.terminal.is_none()
                        && (cancelled || hold.released.is_none())
                }) else {
                    tracing::debug!(stage = ?FailureStage::Release, reason = ?FailureReason::NoPendingHold, "held speech input refused");
                    return;
                };
                // A terminal hold keeps its notice: only a cancel still ends it.
                if hold.terminal.is_some() && !cancelled {
                    tracing::debug!(stage = ?FailureStage::Release, reason = ?FailureReason::TerminalStatus, "held speech input refused");
                    return;
                }
                // Only an accepted collection releases, and only once; anything else cancels.
                if cancelled || !hold.collected || hold.released.is_some() {
                    self.end_voice(Text::HoldCancelled, cx);
                } else {
                    hold.released = Some(qpc);
                    if std::mem::take(&mut hold.screen) {
                        let session = hold.session.clone();
                        let input = input.filter(|_| hold.stamp == stamp);
                        self.release_screen(gesture, &session, qpc, pointer, input, cx);
                    }
                    if self.voice.pending.as_ref().is_some_and(|hold| {
                        hold.token.gesture() == gesture && hold.terminal.is_none()
                    }) {
                        self.voice_notice(Text::HoldStopping, cx);
                        self.finish_voice();
                    }
                }
            }
            Intent::Unavailable => {
                self.voice.lost = None;
                tracing::warn!(stage = ?FailureStage::Input, reason = ?FailureReason::GestureUnavailable, "held speech input failed");
                self.voice_notice(Text::HoldFailed, cx);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn voice_intent(
        &mut self,
        intent: Intent,
        stamp: Stamp,
        qpc: u64,
        pointer: Option<PhysicalPoint>,
        held: TalkHeld,
        cx: &mut Context<'_, Self>,
    ) {
        self.voice_delivery(
            intent,
            stamp,
            qpc,
            pointer,
            Some(&ReleaseInput::fixture(held)),
            cx,
        );
    }

    #[expect(
        clippy::too_many_lines,
        reason = "admission keeps each fixed refusal diagnostic beside its cleanup"
    )]
    fn recognize_voice(
        &mut self,
        gesture: GestureId,
        generation: Result<Generation, crate::platform::microphone::MicError>,
        stamp: Stamp,
        qpc: u64,
        pointer: Option<PhysicalPoint>,
        cx: &mut Context<'_, Self>,
    ) {
        let Ok(token) = generation else {
            if let Err(reason) = generation {
                tracing::warn!(stage = ?FailureStage::Admission, ?reason, "held speech input refused");
            }
            let notice = if self.voice.status == Some(Text::HoldUnavailable) {
                Text::HoldUnavailable
            } else {
                Text::TurnBusy
            };
            self.voice_notice(notice, cx);
            return;
        };
        // A gesture holding no inhibit gets no hold, and leaves a pending one alone.
        let Ok(spoken) = self
            .voice
            .gates
            .recognize(self.link.output(), gesture, &token)
        else {
            tracing::debug!(stage = ?FailureStage::Admission, reason = ?FailureReason::NoInhibit, "held speech input refused");
            if token.discard_unstarted().is_err() {
                self.link.microphone_gate().cancel(gesture);
            }
            self.link.clear_input(gesture);
            if self.voice.pending.is_none() {
                let notice = self.settings.read(cx).hold_to_talk_notice();
                self.voice_notice(notice.unwrap_or(Text::TurnBusy), cx);
            }
            return;
        };
        let queued =
            self.link.audio_session() == kumochi_protocol::message::AudioSession::QueuedRequired;
        let settings = self.settings.read(cx);
        let consent_pending = queued
            && (!self.link.admits_voice(stamp.session)
                || settings
                    .view()
                    .is_none_or(|view| view.talk_back || view.show_screen)
                || settings.show_screen_pending()
                || self.voice.gates.offer_of(gesture).is_some());
        if let Some(notice) = settings.hold_to_talk_notice().or_else(|| {
            (consent_pending || self.link.voice_stop_pending()).then_some(Text::TurnBusy)
        }) {
            tracing::debug!(stage = ?FailureStage::Admission, reason = ?notice, "held speech input refused");
            self.unused_voice(&token, self.session.as_ref());
            self.link.clear_input(gesture);
            self.voice.status = Some(notice);
            self.refresh_dock(cx);
            self.refresh_conversation(cx);
            return;
        }
        if self.voice.pending.is_some() {
            self.end_voice(Text::HoldCancelled, cx);
        }
        let Some(session) = self.session.clone() else {
            tracing::debug!(stage = ?FailureStage::Admission, reason = ?FailureReason::NoSession, "held speech input refused");
            self.unused_voice(&token, None);
            self.link.clear_input(gesture);
            self.voice_notice(Text::HoldFailed, cx);
            return;
        };
        if self
            .dismiss_settings(cx)
            .and_then(|()| self.dismiss_threads(cx))
            .and_then(|()| self.dismiss_text(cx))
            .is_err()
        {
            tracing::warn!(stage = ?FailureStage::Admission, reason = ?FailureReason::DismissPanels, "held speech input failed");
            self.unused_voice(&token, Some(&session));
            self.link.clear_input(gesture);
            self.voice_notice(Text::HoldFailed, cx);
            return;
        }
        let refused = if self.voice.pending.is_some() {
            Some(FailureReason::PendingHold)
        } else if !self.voice_current(stamp) {
            Some(FailureReason::StaleContext)
        } else if !self.link.input_is(gesture) {
            Some(FailureReason::InputFence)
        } else if token.mode(&session.alive(), Instant::now()) != Ok(Mode::Preparing) {
            Some(FailureReason::GenerationNotPreparing)
        } else if self
            .talk_escape
            .acquire(|| self.native.register_talk_escape())
            .is_err()
        {
            Some(FailureReason::EscapeLease)
        } else {
            None
        };
        // A valid recognition still stops the earlier spoken run if Windows refused its Esc
        // hotkey. A refused brain stop ends the connection before any microphone can open.
        if matches!(refused, None | Some(FailureReason::EscapeLease))
            && let Some((_, run)) = spoken.filter(|(at, _)| *at == stamp.session)
            && !self.fence_spoken(run, &session, cx)
        {
            tracing::warn!(stage = ?FailureStage::Admission, reason = ?FailureReason::FenceSend, "held speech input failed");
            self.unused_voice(&token, None);
            self.link.clear_input(gesture);
            self.voice_notice(Text::HoldFailed, cx);
            return;
        }
        if let Some(reason) = refused {
            tracing::debug!(stage = ?FailureStage::Admission, ?reason, "held speech input refused");
            self.unused_voice(&token, Some(&session));
            self.link.clear_input(gesture);
            self.voice_notice(Text::HoldFailed, cx);
            return;
        }
        // Nothing of the screen is taken now, after every refusal above. The runs model reads
        // the notice the dock's icon shows, and its answer says which hold this is.
        // Only the offer its first input was seen under, and only while it is still published.
        let (offer, request) = if queued {
            (None, self.runs.begin_hold(gesture, None))
        } else {
            let offer = self
                .voice
                .gates
                .offer_of(gesture)
                .filter(|seen| Some(*seen) == self.publish_offer(cx));
            let (own_model, default) = self.screen_inputs(cx);
            let request = self.runs.begin_screen_hold(
                gesture,
                qpc,
                own_model,
                default,
                offer.map(|o| o.revision),
            );
            (offer, request)
        };
        let screen = matches!(request, Ok(ControlToBrain::ScreenHoldRecognized { .. }));
        let notice = if matches!(request, Err(crate::runs::Refused::Busy)) {
            Text::TurnBusy
        } else {
            Text::HoldFailed
        };
        let sent = match request {
            Err(reason) => {
                tracing::warn!(stage = ?FailureStage::Admission, ?reason, "held speech input failed");
                false
            }
            Ok(_) if !self.drain_voice_cancel() => {
                tracing::warn!(stage = ?FailureStage::Admission, reason = ?FailureReason::CancelDrain, "held speech input failed");
                false
            }
            Ok(request) => {
                let sent = self.link.send_control(&session, request);
                if !sent {
                    tracing::warn!(stage = ?FailureStage::Admission, reason = ?FailureReason::RecognitionSend, "held speech input failed");
                }
                sent
            }
        };
        if sent {
            // The keys of a new hold are down: marks of the answer before it go now.
            self.marks_changed(cx);
            // The line starts only once the brain was told this hold shows the screen, and
            // only from where the pointer was read: without that the picture goes alone.
            if screen && let Some(origin) = pointer {
                self.start_line(gesture, origin, cx);
            }
            self.voice.recovery = Some(Recovery {
                session: stamp.session,
                gesture,
                run: None,
                target: stamp.target,
                picture: screen,
                words: None,
            });
            self.voice.pending = Some(Hold {
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
            });
            self.chat_notice.clear();
            self.error = None;
            self.refusal = None;
            self.voice_notice(Text::HoldPreparing, cx);
        } else {
            self.unused_voice(&token, Some(&session));
            self.link.clear_input(gesture);
            self.runs.cancel_hold();
            self.drain_voice_cancel();
            self.voice_notice(notice, cx);
        }
    }

    fn finish_voice(&mut self) {
        let Some(hold) = self.voice.pending.as_mut() else {
            return;
        };
        if let (Source::Joined(chunks), Some(t_qpc)) = (hold.source, hold.released) {
            if self.link.send_control(
                &hold.session,
                ControlToBrain::KeyUp {
                    gesture: hold.token.gesture(),
                    t_qpc,
                    chunks,
                },
            ) {
                hold.source = Source::Sent;
            } else if self.link.audio_session() == kumochi_protocol::message::AudioSession::Legacy
                || self.link.input_is(hold.token.gesture())
            {
                tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::ReleaseSend, "held speech input failed");
                self.link.end(&hold.session);
            }
        }
    }

    /// Keeps the accepted hold's words, from its own connection, until its receipt.
    pub(super) fn hold_words(&mut self, generation: u64, gesture: GestureId, words: Content) {
        if words.len() > MAX_TRANSCRIPT_BYTES {
            tracing::warn!("held words over the bound");
            return;
        }
        if let Some(slot) = self.voice.recovery.as_mut().filter(|slot| {
            slot.session == generation && slot.gesture == gesture && slot.words.is_none()
        }) {
            slot.words = Some(words);
        }
    }

    pub(super) fn discard_voice_recovery(&mut self, gesture: GestureId) {
        if self
            .voice
            .recovery
            .as_ref()
            .is_some_and(|slot| slot.gesture == gesture)
        {
            self.voice.recovery = None;
        }
    }

    pub(super) fn voice_input_ended(&mut self, run: RunId) {
        for (_, receipt) in self.voice.receipts() {
            if let Some(tail) = receipt.as_mut().filter(|tail| tail.run == run) {
                tail.data_ended = true;
            }
        }
    }

    pub(super) fn voice_thread_deleted(&mut self, thread: ThreadId) {
        for (_, receipt) in self.voice.receipts() {
            if let Some(tail) = receipt.as_mut().filter(|tail| tail.thread == thread) {
                tail.control_closed = true;
            }
        }
    }

    /// The brain stored the hold's turn, so its words aren't needed any more.
    pub(super) fn hold_stored(&mut self, generation: u64, gesture: GestureId) {
        if self
            .voice
            .recovery
            .as_ref()
            .is_some_and(|slot| slot.session == generation && slot.gesture == gesture)
        {
            self.voice.recovery = None;
        }
    }

    /// The brain was lost with a hold's turn unstored: its words go back into an empty composer
    /// on the hold's own target, without showing it, and the notice says what happened.
    pub(super) fn recover_hold(&mut self, cx: &mut Context<'_, Self>) {
        let Some(slot) = self.voice.recovery.take() else {
            return;
        };
        let restored = if slot.target.0.is_some()
            && slot.target == (self.runs.thread(), self.runs.revision())
            && let Some(words) = slot.words
            && let Some(text) = &self.text
        {
            text.window
                .update(cx, |_, window, cx| {
                    text.view
                        .update(cx, |view, cx| view.restore(words, window, cx))
                })
                .unwrap_or_else(|_| {
                    tracing::warn!("can't restore the held words");
                    false
                })
        } else {
            false
        };
        self.voice.lost = Some(match (restored, slot.picture) {
            (true, false) => Text::HoldLostRestored,
            (true, true) => Text::HoldLostRestoredPictured,
            (false, false) => Text::HoldLost,
            (false, true) => Text::HoldLostPictured,
        });
        self.refresh_dock(cx);
        self.refresh_conversation(cx);
    }

    pub(super) fn voice_status(
        &mut self,
        generation: u64,
        gesture: GestureId,
        status: HoldStatus,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(hold) = self
            .voice
            .pending
            .as_ref()
            .filter(|h| h.stamp.session == generation && h.token.gesture() == gesture)
        else {
            tracing::debug!(stage = ?FailureStage::Brain, reason = ?FailureReason::StaleStatus, "held speech input refused");
            return;
        };
        if hold.terminal.is_some() {
            tracing::debug!(stage = ?FailureStage::Brain, reason = ?FailureReason::TerminalStatus, "held speech input refused");
            return;
        }
        match status {
            // The waiter is installed, then checked at once.
            HoldStatus::Admitted if matches!(hold.source, Source::Waiting) => {
                if self.voice.gates.admit(gesture, cx.background_executor()) {
                    self.dispatch(cx);
                }
            }
            HoldStatus::Admitted => {}
            HoldStatus::Loading => self.voice_notice(Text::VoiceLoading, cx),
            HoldStatus::Transcribing => self.voice_notice(Text::HoldTranscribing, cx),
            HoldStatus::Empty => self.end_voice(Text::HoldEmpty, cx),
            HoldStatus::Cancelled => self.end_voice(Text::HoldCancelled, cx),
            HoldStatus::Failed(reason) => {
                tracing::warn!(stage = ?FailureStage::Brain, ?reason, "held speech input failed");
                self.end_voice(
                    match reason {
                        HoldFailure::NotLoaded => {
                            match self.settings.read(cx).speech_input_status() {
                                text @ (Text::VoiceNotInstalled | Text::VoiceUnloaded) => text,
                                _ => Text::VoiceStatusUnknown,
                            }
                        }
                        HoldFailure::Busy => Text::TurnBusy,
                        _ => Text::HoldFailed,
                    },
                    cx,
                );
            }
        }
    }
}
