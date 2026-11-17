use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect, PointerPath};
use kumochi_protocol::message::{
    CaptureFor, Content, ControlToBrain, GestureId, HoldFailure, HoldStatus, MAX_TRANSCRIPT_BYTES,
    ModelName, Phase, RunId, RunOrigin, ThreadId,
};

use gpui_kit::Window;

use super::ink::Sampler;
use super::overlay_visibility::Visibility;
use super::{Context, Presence, Session};
use crate::input::hold::Intent;
use crate::input::path::PathRecorder;
use crate::platform::microphone::{Generation, Mode};
use crate::runs::{HoldRelease, ScreenNotice};
use crate::strings::{Strings, Text};

mod source;

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
    PacketCountMismatch,
    RunBeforeStop,
    NoSession,
    PendingHold,
    StaleContext,
    InputFence,
    GenerationNotPreparing,
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
    token: Generation,
    stamp: Stamp,
    session: Session,
    source: Source,
    released: Option<u64>,
    /// A hold the brain was told shows the screen, until it asks for that release: once, when
    /// the keys come up or when the notice stops being `On` before they do.
    screen: bool,
    terminal: Option<Text>,
    /// Later run states carry no origin, so they match the hold by the run its first one named.
    run: Option<RunId>,
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
    pub(super) status: Option<Text>,
    pub(super) recovery: Option<Recovery>,
    /// What a lost brain did with the held words; reconnecting doesn't clear it.
    pub(super) lost: Option<Text>,
    #[cfg(test)]
    pub(super) start: Option<Start>,
}

#[cfg(test)]
type Start = Box<
    dyn FnOnce(
        Generation,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<MicOwner, crate::platform::microphone::MicError>,
>;

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
        origin: Option<RunOrigin>,
        phase: Phase,
        applied: &crate::runs::Applied,
        cx: &mut Context<'_, Self>,
    ) {
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
                    None => hold.run == Some(run),
                }
        }) else {
            return;
        };
        hold.run = Some(run);
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
                self.voice.status = None;
                self.refresh_dock(cx);
            }
            _ => {}
        }
    }

    fn voice_current(&self, stamp: Stamp) -> bool {
        stamp.target == (self.runs.thread(), self.runs.revision())
            && stamp.choice == self.link.choice_revision()
            && self.session.as_ref().is_some_and(|s| {
                s.generation() == stamp.session && s.alive().load(Ordering::Acquire)
            })
    }

    fn voice_notice(&mut self, text: Text, cx: &mut Context<'_, Self>) {
        tracing::debug!(stage = ?FailureStage::Notice, reason = ?text, "held speech input notice");
        self.voice.status = Some(text);
        // A microphone that may still be open matters more than the lost brain's notice.
        if text == Text::HoldUnavailable {
            self.voice.lost = None;
        }
        // Every change to the pending hold ends in a notice, so the chord follows its keys here.
        if self.voice_held() {
            let native = &self.native;
            self.talk_escape.acquire(|| native.register_talk_escape());
        } else {
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

    fn unused_voice(&self, token: &Generation, session: &Session) {
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
        self.voice.recovery = None;
        self.stop_line(cx);
        let mut notice = text;
        if let Some(mut hold) = self.voice.pending.take() {
            let gesture = hold.token.gesture();
            self.link.microphone_gate().cancel(gesture);
            self.link.clear_input(gesture);
            if matches!(hold.source, Source::Waiting) {
                self.unused_voice(&hold.token, &hold.session);
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
        // shortcut: a thread whose row was never read counts as on the default model (OSC-763), upgrade before any view sends `SetModel`.
        let own = self
            .runs
            .thread()
            .and_then(|thread| self.threads.read(cx).row(thread))
            .and_then(|row| row.choice.as_ref());
        (settings.own_model(own), settings.default_model_name())
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
    fn line_display(&mut self, cx: &mut Context<'_, Self>) -> Option<(PhysicalRect, f32)> {
        // The overlay sits on the primary display, so its scale is that display's; the dock may
        // be on another one, and stands in only until the overlay first opens.
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
    /// covered, then sends the one `ScreenRelease`. Whatever stops it short leaves the hold
    /// voice only, and its line says why.
    fn release_screen(
        &mut self,
        gesture: GestureId,
        session: &Session,
        t_qpc: u64,
        pointer: Option<PhysicalPoint>,
        cx: &mut Context<'_, Self>,
    ) {
        let sampler = self.stop_line(cx);
        let (own_model, default) = self.screen_inputs(cx);
        if !self.runs.releases_screen(gesture, own_model, default) {
            return;
        }
        // An unchecked box that is still saving is already the user's no.
        if self.settings.read(cx).show_screen_choice() == Some(false) {
            self.runs.hold_screen_turned_off();
            return;
        }
        // The pointer picks the display, so a release that couldn't read it shows none.
        let Some(pointer) = pointer else {
            tracing::debug!(stage = ?FailureStage::Release, reason = ?FailureReason::PointerUnread, "screen release refused; voice only");
            self.runs.hold_screen_refused();
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
                self.runs.hold_screen_refused();
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
        let sent = self
            .runs
            .screen_release(gesture, own_model, default, release)
            .is_some_and(|message| self.link.send_control(session, message));
        if sent {
            self.screen_lease = Some(lease);
        } else {
            tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::ScreenReleaseSend, "held speech input failed");
            // The brain never saw this lease, so releasing it is no lapse.
            self.revoke_exclusion(cx);
        }
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
        let shown = self.runs.hold_screen_text(notice);
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

    /// `pointer` is where the pointer was when the native callback recognized a hold or saw its
    /// keys come up.
    pub(super) fn voice_intent(
        &mut self,
        intent: Intent,
        stamp: Stamp,
        qpc: u64,
        pointer: Option<PhysicalPoint>,
        cx: &mut Context<'_, Self>,
    ) {
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
                let Some(hold) = self
                    .voice
                    .pending
                    .as_mut()
                    .filter(|hold| hold.token.gesture() == gesture)
                else {
                    tracing::debug!(stage = ?FailureStage::Release, reason = ?FailureReason::NoPendingHold, "held speech input refused");
                    return;
                };
                if cancelled || self.voice.status != Some(Text::HoldListening) {
                    self.end_voice(Text::HoldCancelled, cx);
                } else {
                    hold.released = Some(qpc);
                    if std::mem::take(&mut hold.screen) {
                        let session = hold.session.clone();
                        self.release_screen(gesture, &session, qpc, pointer, cx);
                    }
                    self.voice_notice(Text::HoldStopping, cx);
                    self.finish_voice();
                }
            }
            Intent::Unavailable => {
                self.voice.lost = None;
                tracing::warn!(stage = ?FailureStage::Input, reason = ?FailureReason::GestureUnavailable, "held speech input failed");
                self.voice_notice(Text::HoldFailed, cx);
            }
        }
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
        if let Some(notice) = self
            .settings
            .read(cx)
            .hold_to_talk_notice()
            .or_else(|| self.link.voice_stop_pending().then_some(Text::TurnBusy))
        {
            tracing::debug!(stage = ?FailureStage::Admission, reason = ?notice, "held speech input refused");
            if let Some(session) = &self.session {
                self.unused_voice(&token, session);
            } else if token.discard_unstarted().is_err() {
                self.link.microphone_gate().cancel(gesture);
            }
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
            if token.discard_unstarted().is_err() {
                self.link.microphone_gate().cancel(gesture);
            }
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
            self.unused_voice(&token, &session);
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
        } else {
            None
        };
        if let Some(reason) = refused {
            tracing::debug!(stage = ?FailureStage::Admission, ?reason, "held speech input refused");
            self.unused_voice(&token, &session);
            self.link.clear_input(gesture);
            self.voice_notice(Text::HoldFailed, cx);
            return;
        }
        // Nothing of the screen is taken now, after every refusal above. The runs model reads
        // the notice the dock's icon shows, and its answer says which hold this is.
        let (own_model, default) = self.screen_inputs(cx);
        let request = self
            .runs
            .begin_screen_hold(gesture, qpc, own_model, default);
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
                released: None,
                screen,
                terminal: None,
                run: None,
            });
            self.chat_notice.clear();
            self.error = None;
            self.refusal = None;
            self.voice_notice(Text::HoldPreparing, cx);
        } else {
            self.unused_voice(&token, &session);
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
            hold.source = Source::Sent;
            if !self.link.send_control(
                &hold.session,
                ControlToBrain::KeyUp {
                    gesture: hold.token.gesture(),
                    t_qpc,
                    chunks,
                },
            ) {
                tracing::warn!(stage = ?FailureStage::Release, reason = ?FailureReason::ReleaseSend, "held speech input failed");
                self.link.end(&hold.session);
            }
        }
    }

    /// Keeps the accepted hold's words, from its own connection, until its receipt.
    pub(super) fn hold_words(&mut self, generation: u64, gesture: GestureId, words: Content) {
        if words.len() > MAX_TRANSCRIPT_BYTES {
            tracing::warn!(len = words.len(), "held words over the bound");
            return;
        }
        if let Some(slot) = self.voice.recovery.as_mut().filter(|slot| {
            slot.session == generation && slot.gesture == gesture && slot.words.is_none()
        }) {
            slot.words = Some(words);
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
            HoldStatus::Admitted if matches!(hold.source, Source::Waiting) => {
                self.open_microphone(cx);
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
