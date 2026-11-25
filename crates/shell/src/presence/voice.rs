use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect, PointerPath};
use kumochi_protocol::message::{
    CaptureFor, ControlToBrain, GestureId, HoldStatus, LeaseGeneration, ModelName, Phase, RunId,
    RunOrigin, ThreadId,
};

use gpui_kit::Window;
use kumochi_shell_core::link::InputCancel;
use kumochi_shell_core::presence::teardown::{cancel_tracked_input, drain_hold_cancel};
pub(super) use kumochi_shell_core::voice::Stamp;
use kumochi_shell_core::voice::{
    self, Cancel, Discard, ReceiptStep, Release, RunStep, Source, State, StatusStep, VoiceOnly,
};

use super::gesture::ReleaseInput;
use super::ink::Sampler;
use super::overlay_visibility::Visibility;
use super::{Context, Presence, Session};
#[cfg(test)]
use crate::input::TalkHeld;
use crate::input::hold::Intent;
use crate::input::path::PathRecorder;
use crate::platform::microphone::{Generation, Mode};
use crate::runs::{HoldRelease, ScreenNotice};
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
    RunningRejected,
    WarmSend,
    StopSend,
    ReleaseSend,
    StaleReceipt,
    CaptureClockUnavailable,
    ScreenReleaseSend,
}

#[cfg(test)]
use crate::platform::microphone::{Finalization, MicOwner};
#[cfg(test)]
use kumochi_protocol::message::DataToBrain;
#[cfg(test)]
use kumochi_protocol::message::{HoldFailure, MAX_TRANSCRIPT_BYTES};
#[cfg(test)]
use kumochi_shell_core::voice::Hold;
#[cfg(test)]
use kumochi_shell_core::voice::Recovery;

#[cfg(test)]
#[path = "voice_source_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
#[path = "screen_marks_tests.rs"]
mod marks_tests;

#[derive(Default)]
pub(super) struct Voice {
    pub(super) target: Rc<Cell<(Option<ThreadId>, u64)>>,
    /// Set while the onboarding window shows; the native fence drops holds while it's set.
    pub(super) paused: Rc<Cell<bool>>,
    /// The pending hold, its notice, kept words and last ended hold, decided in the core.
    pub(super) state: State,
    /// The hold the screen watch was last told was released (`marks.rs`).
    pub(super) watched: Option<super::marks::Watched>,
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

impl Presence {
    pub(super) fn voice_working(&self) -> bool {
        self.voice_escape_owner().is_some()
    }

    pub(super) fn voice_escape_owner(&self) -> Option<GestureId> {
        self.voice.state.escape_owner()
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
        match self
            .voice
            .state
            .run_state(run, thread, origin, phase, applied)
        {
            RunStep::None => {}
            RunStep::Refused { gesture, session } => {
                let deferred = self.link.deferred_input_cancel(gesture, session);
                self.end_voice_after_cancel(Text::HoldFailed, deferred.as_ref(), cx);
            }
            RunStep::BeforeStop => {
                tracing::warn!(stage = ?FailureStage::Run, reason = ?FailureReason::RunBeforeStop, "held speech input failed");
                self.end_voice(Text::HoldFailed, cx);
                if let Some(session) = &self.session {
                    self.link.end(session);
                }
            }
            RunStep::Thinking => {
                self.voice_notice(Text::HoldThinking, cx);
                if self.show_voice_answer(cx).is_err() {
                    tracing::error!("can't show the voice answer");
                }
            }
            RunStep::Answered(gesture) => {
                self.link.microphone_gate().cancel(gesture);
                self.link.clear_input(gesture);
                self.refresh_dock(cx);
            }
        }
    }

    fn voice_current(&self, stamp: Stamp) -> bool {
        let target = (self.runs.thread(), self.runs.revision());
        stamp.current(target, self.link.choice_revision(), self.session.as_ref())
    }

    pub(super) fn voice_notice(&mut self, text: Text, cx: &mut Context<'_, Self>) {
        tracing::debug!(stage = ?FailureStage::Notice, reason = ?text, "held speech input notice");
        // Every change to the pending hold ends in a notice, so the chord follows its keys here.
        if !self.voice.state.notice(text) {
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
        let gesture = token.gesture();
        match voice::discard(token, session) {
            Discard::Nothing => {}
            Discard::Cancel => self.link.microphone_gate().cancel(gesture),
            Discard::Stop(session) => {
                if !self
                    .link
                    .send_control(session, ControlToBrain::VoiceHoldStopped { gesture })
                {
                    tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::StopSend, "held speech input failed");
                    self.link.end(session);
                }
            }
        }
    }

    /// Mirrors whether hold-to-talk can take a hold for the native callback.
    pub(super) fn follow_voice_input(&self, cx: &gpui_kit::App) {
        self.voice
            .gates
            .follow(self.settings.read(cx).hold_to_talk_notice().is_none());
    }

    fn drain_voice_cancel(&mut self, covered: Option<&InputCancel>) -> bool {
        let sent = drain_hold_cancel(&self.link, self.session.as_ref(), &mut self.runs, covered);
        if !sent && let Some(session) = &self.session {
            tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::CancelDrain, "held speech input failed");
            self.link.end(session);
        }
        sent
    }

    fn end_voice(&mut self, text: Text, cx: &mut Context<'_, Self>) {
        self.end_voice_after_cancel(text, None, cx);
    }

    fn end_voice_after_cancel(
        &mut self,
        text: Text,
        covered: Option<&InputCancel>,
        cx: &mut Context<'_, Self>,
    ) {
        self.stop_line(cx);
        let ended = self.voice.state.end(text);
        if let Some(gesture) = ended.gesture {
            self.marks_dropped(gesture, cx);
            self.link.microphone_gate().cancel(gesture);
            self.link.clear_input(gesture);
        }
        if let Some((token, session)) = &ended.unused {
            self.unused_voice(token, Some(session));
        }
        self.runs.cancel_hold();
        self.drain_voice_cancel(covered);
        self.voice_notice(ended.notice, cx);
    }

    pub(super) fn cancel_voice(&mut self, cx: &mut Context<'_, Self>) {
        let (action, queued) = self.voice.state.cancel(|| cancel_tracked_input(&self.link));
        match action {
            Cancel::End => self.end_voice_after_cancel(Text::HoldCancelled, queued.as_ref(), cx),
            Cancel::Refresh => {
                self.refresh_dock(cx);
                self.refresh_conversation(cx);
            }
            Cancel::Idle => {}
        }
        self.drain_voice_cancel(queued.as_ref());
    }

    pub(super) fn stop_voice_for_quit(&mut self) {
        self.voice.paused.set(true);
        self.voice.gates.follow(false);
        let unused = self.voice.state.quit(|| {
            self.voice.watched = None;
            drop(self.ink.borrow_mut().take());
        });
        if let Some(hold) = unused {
            self.unused_voice(hold.token(), None);
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
        let pending = self.voice.state.owns(gesture);
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
        let Some(gesture) = self.voice.state.take_screen() else {
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
        let held = self.voice.state.pending.as_ref().is_some_and(|hold| {
            hold.token().gesture() == gesture
                && hold.released().is_none()
                && hold.terminal().is_none()
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
        let Some(input) = input.filter(|input| self.release_current(gesture, input, None)) else {
            self.runs.hold_screen_refused(gesture);
            return;
        };
        let (own_model, default) = self.screen_inputs(cx);
        if !self.runs.releases_screen(gesture, own_model, default) {
            return;
        }
        let show = self.settings.read(cx).show_screen_choice();
        let pointer = match self.voice.state.screen_release(
            show,
            || self.publish_offer(cx),
            pointer,
        ) {
            Ok(pointer) => pointer,
            Err(VoiceOnly::TurnedOff) => {
                self.runs.hold_screen_turned_off();
                return;
            }
            Err(VoiceOnly::Refused(reason)) => {
                tracing::debug!(stage = ?FailureStage::Release, ?reason, "screen release refused; voice only");
                self.runs.hold_screen_refused(gesture);
                return;
            }
        };
        let counter = crate::platform::INPUT_COUNTER;
        #[cfg(not(test))]
        let read = counter;
        #[cfg(test)]
        let fixture = self.counter_fixture.clone();
        #[cfg(test)]
        let read = fixture
            .as_deref()
            .or(counter.as_ref().map(|read| -> &dyn Fn() -> i64 { read }));
        let Some(read) = read else {
            tracing::debug!(stage = ?FailureStage::Release, reason = ?FailureReason::CaptureClockUnavailable, "screen release refused; voice only");
            self.runs.hold_screen_refused(gesture);
            return;
        };
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
            if !self.release_current(gesture, input, Some(lease)) {
                return false;
            }
            // The watch starts before the message is written: the brain takes the picture only
            // once it reads it, so the sources are listening before the picture exists.
            self.marks_released(gesture, input.held(), cx);
            self.release_current(gesture, input, Some(lease))
                && self.link.send_control(session, message)
        });
        if sent {
            self.screen_lease = Some(lease);
        } else {
            tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::ScreenReleaseSend, "held speech input failed");
            self.screen_release_failed(gesture, lease, cx);
        }
    }

    fn release_current(
        &self,
        gesture: GestureId,
        input: &ReleaseInput,
        lease: Option<LeaseGeneration>,
    ) -> bool {
        self.voice.state.permits(
            gesture,
            || input.continuous() && self.native.escape_trusted() && self.link.input_is(gesture),
            || {
                (
                    (self.runs.thread(), self.runs.revision()),
                    self.link.choice_revision(),
                    self.session.as_ref(),
                )
            },
            || lease.is_none_or(|lease| self.exclusion.live(lease)),
        )
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
                self.voice.state.lost = None;
                self.recognize_voice(gesture, generation, stamp, qpc, pointer, cx);
            }
            Intent::End { gesture, cancelled } => {
                match self.voice.state.release(gesture, cancelled, qpc, stamp) {
                    Release::Refused(reason) => {
                        tracing::debug!(stage = ?FailureStage::Release, ?reason, "held speech input refused");
                    }
                    Release::Cancel => self.end_voice(Text::HoldCancelled, cx),
                    Release::Released { screen } => {
                        if let Some((session, same)) = screen {
                            let input = input.filter(|_| same);
                            self.release_screen(gesture, &session, qpc, pointer, input, cx);
                        }
                        if self.voice.state.live(gesture) {
                            self.voice_notice(Text::HoldStopping, cx);
                            self.finish_voice();
                        }
                    }
                }
            }
            Intent::Unavailable => {
                self.voice.state.lost = None;
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
            let notice = self.voice.state.unarmed_notice();
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
            if matches!(voice::discard(&token, None), Discard::Cancel) {
                self.link.microphone_gate().cancel(gesture);
            }
            self.link.clear_input(gesture);
            if let Some(notice) = self
                .voice
                .state
                .no_inhibit(|| self.settings.read(cx).hold_to_talk_notice())
            {
                self.voice_notice(notice, cx);
            }
            return;
        };
        let audio = self.link.audio_session();
        let settings = self.settings.read(cx);
        if let Some(notice) = State::held_back(
            audio,
            || {
                self.link.admits_voice(stamp.session)
                    && settings.view().is_some()
                    && !settings.show_screen_pending()
            },
            || settings.hold_to_talk_notice(),
            || self.link.voice_stop_pending(),
        ) {
            tracing::debug!(stage = ?FailureStage::Admission, reason = ?notice, "held speech input refused");
            self.unused_voice(&token, self.session.as_ref());
            self.link.clear_input(gesture);
            self.voice.state.admission_refused(notice);
            self.refresh_dock(cx);
            self.refresh_conversation(cx);
            return;
        }
        if let Some(notice) = self.voice.state.replace_hold() {
            self.end_voice(notice, cx);
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
        let admission = self.voice.state.admission(
            || {
                let target = (self.runs.thread(), self.runs.revision());
                stamp.current(target, self.link.choice_revision(), self.session.as_ref())
            },
            || self.link.input_is(gesture),
            || token.mode(&session.alive(), Instant::now()) == Ok(Mode::Preparing),
            || {
                self.talk_escape
                    .acquire(|| self.native.register_talk_escape())
                    .is_ok()
            },
        );
        // A refused brain stop ends the connection before any microphone can open.
        if admission.fence
            && let Some((_, run)) = spoken.filter(|(at, _)| *at == stamp.session)
            && !self.fence_spoken(run, &session, cx)
        {
            tracing::warn!(stage = ?FailureStage::Admission, reason = ?FailureReason::FenceSend, "held speech input failed");
            self.unused_voice(&token, None);
            self.link.clear_input(gesture);
            self.voice_notice(Text::HoldFailed, cx);
            return;
        }
        if let Some(reason) = admission.refused {
            tracing::debug!(stage = ?FailureStage::Admission, ?reason, "held speech input refused");
            self.unused_voice(&token, Some(&session));
            self.link.clear_input(gesture);
            self.voice_notice(Text::HoldFailed, cx);
            return;
        }
        // Nothing of the screen is taken now, after every refusal above. The runs model reads
        // the notice the dock's icon shows, and its answer says which hold this is.
        // Only the offer its first input was seen under, and only while it is still published.
        let (offer, request) = {
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
        let recognition = voice::Recognition::new(&session, &request);
        self.link.watch_revoke(gesture, stamp.session);
        let sent = match request {
            Err(reason) => {
                tracing::warn!(stage = ?FailureStage::Admission, ?reason, "held speech input failed");
                false
            }
            Ok(_) if !self.drain_voice_cancel(None) => {
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
            let screen = recognition.screen;
            // The keys of a new hold are down: marks of the answer before it go now.
            self.marks_changed(cx);
            // The line starts only once the brain was told this hold shows the screen, and
            // only from where the pointer was read: without that the picture goes alone.
            if screen && let Some(origin) = pointer {
                self.start_line(gesture, origin, cx);
            }
            let unretired = audio == kumochi_protocol::message::AudioSession::QueuedRequired
                && self.link.output().queued_speaking(session.generation());
            self.voice
                .state
                .begin(token, stamp, session, screen, offer, unretired);
            self.chat_notice.clear();
            self.error = None;
            self.refusal = None;
            self.voice_notice(Text::HoldPreparing, cx);
        } else {
            let session = match recognition.unsent {
                voice::Unsent::DiscardRun => {
                    self.runs.discard_unpublished_hold(gesture);
                    None
                }
                voice::Unsent::StopOn(session) => Some(session),
            };
            self.unused_voice(&token, session);
            self.link.clear_input(gesture);
            self.voice_notice(recognition.notice, cx);
        }
    }

    fn finish_voice(&mut self) {
        let link = &self.link;
        let Some(hold) = self
            .voice
            .state
            .finish(|session, message| link.send_control(session, message))
        else {
            return;
        };
        if link.audio_session() == kumochi_protocol::message::AudioSession::Legacy
            || link.input_is(hold.token().gesture())
        {
            tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::ReleaseSend, "held speech input failed");
            link.end(hold.session());
        }
    }

    /// The brain was lost with a hold's turn unstored: its words go back into an empty composer
    /// on the hold's own target, without showing it, and the notice says what happened.
    pub(super) fn recover_hold(&mut self, cx: &mut Context<'_, Self>) {
        let target = (self.runs.thread(), self.runs.revision());
        let (link, text) = (&self.link, &self.text);
        let restore = |words| {
            text.as_ref().is_some_and(|text| {
                text.window
                    .update(cx, |_, window, cx| {
                        text.view
                            .update(cx, |view, cx| view.restore(words, window, cx))
                    })
                    .unwrap_or_else(|_| {
                        tracing::warn!("can't restore the held words");
                        false
                    })
            })
        };
        if self
            .voice
            .state
            .lose(|gesture| link.was_revoked(gesture), target, restore)
        {
            self.refresh_dock(cx);
            self.refresh_conversation(cx);
        }
    }

    pub(super) fn voice_status(
        &mut self,
        generation: u64,
        gesture: GestureId,
        status: HoldStatus,
        cx: &mut Context<'_, Self>,
    ) {
        let speech = || self.settings.read(cx).speech_input_status();
        match self.voice.state.status(generation, gesture, status, speech) {
            StatusStep::Refused(reason) => {
                tracing::debug!(stage = ?FailureStage::Brain, ?reason, "held speech input refused");
            }
            StatusStep::Ignored => {}
            // The waiter is installed, then checked at once.
            StatusStep::Admit => {
                if self.voice.gates.admit(gesture, cx.background_executor()) {
                    self.dispatch(cx);
                }
            }
            StatusStep::Notice(text) => self.voice_notice(text, cx),
            StatusStep::End(text) => self.end_voice(text, cx),
            StatusStep::Failed(reason, text) => {
                tracing::warn!(stage = ?FailureStage::Brain, ?reason, "held speech input failed");
                self.end_voice(text, cx);
            }
        }
    }
}
