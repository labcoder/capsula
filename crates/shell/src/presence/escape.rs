//! Local Esc routing for the focused surface.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use super::drag::Event;
use super::{Activity, Context, Escape, EscapePanel, OpenError, Presence, Session, diagnostics};
pub(super) use crate::escape::local_decision;
use crate::escape::{
    self, AfterClear, AfterPull, Edit, Lead, MarkedAnswer, MovingStop, PlainMatch, SpeechPress,
    SpeechRoute, SpeechStop,
};
use crate::settings::Field;
use kumochi_protocol::message::{GestureId, RunId, ThreadId};

pub(super) type SpeechLease = Rc<Cell<Option<(u64, RunId, Instant)>>>;
pub(super) type NameEdit = Rc<EditSlot<Field>>;
pub(super) type RenameEdit = Rc<EditSlot<ThreadId>>;
pub(super) type PlainEscapeCell = Rc<RefCell<Option<PublishedEscape>>>;

pub(super) struct EditSlot<T>(Cell<(u64, Option<T>)>);

impl<T> Default for EditSlot<T> {
    fn default() -> Self {
        Self(Cell::new((0, None)))
    }
}

impl<T: Copy + Eq> EditSlot<T> {
    pub(super) fn get(&self) -> Option<T> {
        self.0.get().1
    }

    pub(super) fn set(&self, next: Option<T>) {
        let (generation, current) = self.0.get();
        if current != next {
            self.0.set((generation.wrapping_add(1), next));
        }
    }

    fn stamped(&self) -> Option<(u64, T)> {
        let (generation, value) = self.0.get();
        value.map(|value| (generation, value))
    }
}

enum PanelEdit {
    Name(NameEdit),
    Rename(RenameEdit),
}

/// The owners the aggregate plain Esc route may affect, with the shell's window and watch tokens.
pub(super) type PlainEscape = escape::PlainEscape<gpui_kit::WindowId, super::marks::Watched>;

pub(super) struct PublishedEscape {
    scope: PlainEscape,
    edit: Option<PanelEdit>,
}

impl PublishedEscape {
    fn capture(&self) -> PlainEscape {
        let edit = match &self.edit {
            Some(PanelEdit::Name(edit)) => edit.stamped().map(|(id, field)| Edit::Name(id, field)),
            Some(PanelEdit::Rename(edit)) => {
                edit.stamped().map(|(id, thread)| Edit::Rename(id, thread))
            }
            None => None,
        };
        self.scope.with_edit(edit)
    }
}

pub(super) struct Presses {
    pub(super) speech: SpeechLease,
    pub(super) escape_scope: Rc<Cell<super::gesture::EscapeScope>>,
    plain: PlainEscapeCell,
}

impl Presses {
    pub(super) fn plain(&self) -> Option<PlainEscape> {
        self.plain.borrow().as_ref().map(PublishedEscape::capture)
    }
}

impl Presence {
    pub(super) fn escape_presses(&self, cx: &gpui_kit::App) -> Presses {
        self.publish_plain_escape(cx);
        Presses {
            speech: Rc::clone(&self.speech),
            escape_scope: Rc::clone(&self.escape_scope),
            plain: Rc::clone(&self.plain_escape),
        }
    }

    pub(super) fn publish_plain_escape(&self, cx: &gpui_kit::App) {
        self.plain_escape.replace(self.plain_escape_snapshot(cx));
    }

    fn plain_escape_at(&self, cx: &gpui_kit::App) -> Option<PlainEscape> {
        let scope = self.plain_escape_snapshot(cx)?.capture();
        scope
            .onboarding_current(|| self.onboarding_name_edit(cx))
            .then_some(scope)
    }

    fn plain_escape_snapshot(&self, cx: &gpui_kit::App) -> Option<PublishedEscape> {
        let panel = self.plain_escape_panel();
        let pull = self.kumo.read(cx).pull_generation();
        self.plain_escape_scope(Self::activity(&self.runs, panel, &self.presenter), pull)
    }

    pub(super) fn publish_plain_escape_for(
        &self,
        activity: Activity,
        pull: Option<super::drag::Generation>,
    ) {
        self.plain_escape
            .replace(self.plain_escape_scope(activity, pull));
    }

    fn plain_escape_panel(&self) -> EscapePanel {
        if self.settings_focused() {
            EscapePanel::Settings
        } else if self.threads_focused() {
            EscapePanel::Threads
        } else if self.onboarding.is_some() {
            EscapePanel::Onboarding
        } else if self.text_open() {
            EscapePanel::Open
        } else {
            EscapePanel::Closed
        }
    }

    fn plain_escape_scope(
        &self,
        mut activity: Activity,
        pull: Option<super::drag::Generation>,
    ) -> Option<PublishedEscape> {
        let pair = match self.session.as_ref() {
            Some(session) if session.open() => Some(session.generation()),
            Some(_) => return None,
            None => None,
        };
        let scenes = [self.runs.escape_scenes()?, self.presenter.escape_scenes()?];
        let panel = self.plain_escape_panel();
        if panel != EscapePanel::Open {
            activity.drawing = self.presenter.drawn();
        }
        activity.pulling = pull.is_some();
        let action = local_decision(activity, panel, &self.presenter);
        let run = self.runs.stoppable();
        let hold = self.voice_escape_owner();
        let selection = self.presenter.input.selection();
        let marks = self.presenter.marks_owner().map(|owner| {
            MarkedAnswer::new(owner, action, run, hold, || {
                self.presenter.stoppable_run_excluding(Some(owner.1))
            })
        });
        let reads_edit = action == Escape::Dismiss;
        let (window, edit) = match panel {
            EscapePanel::Open => {
                let panel = self.text.as_ref()?;
                (Some((panel.window, panel.typing.opening())), None)
            }
            EscapePanel::Settings => {
                let panel = self.settings_panel.as_ref()?;
                let edit = reads_edit.then(|| PanelEdit::Name(Rc::clone(&panel.editing)));
                (Some((panel.window, panel.opening)), edit)
            }
            EscapePanel::Threads => {
                let panel = self.thread_panel.as_ref()?;
                let edit = reads_edit.then(|| PanelEdit::Rename(Rc::clone(&panel.rename)));
                (Some((panel.window, panel.typing.opening())), edit)
            }
            EscapePanel::Onboarding => {
                let panel = self.onboarding.as_ref()?;
                let edit = reads_edit.then(|| PanelEdit::Name(Rc::clone(&panel.escape_edit)));
                (Some((panel.window, 0)), edit)
            }
            _ => (None, None),
        };
        Some(PublishedEscape {
            scope: PlainEscape {
                pair,
                target: (self.runs.thread(), self.runs.revision()),
                frontier: (self.runs.newest_submission(), self.runs.newest_run()),
                moving: (run, self.presenter.stoppable_run()),
                action,
                scenes,
                hold,
                watch: self.voice.watched,
                marks,
                selection,
                pull,
                panel: window.map(|(window, opening)| (panel, window.window_id(), opening)),
                edit: None,
            },
            edit,
        })
    }

    pub(super) fn native_escape(
        &mut self,
        gesture: Option<GestureId>,
        speech: Option<SpeechPress>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.native_escape_after_wake(gesture, speech, None, cx)
    }

    pub(super) fn native_plain_escape(
        &mut self,
        press: &PlainEscape,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let Some(owners) = self
            .plain_escape_at(cx)
            .and_then(|live| press.matches(&live))
        else {
            return Ok(());
        };
        let mark = match owners {
            PlainMatch::Unchanged => None,
            PlainMatch::MarkGone(run) => Some(run),
        };
        self.native_escape_after_wake(None, None, mark, cx)
    }

    fn native_escape_after_wake(
        &mut self,
        gesture: Option<GestureId>,
        speech: Option<SpeechPress>,
        mark: Option<RunId>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        if gesture.is_none()
            && let Some(press) = speech
        {
            match press.route(self.open_pair()) {
                SpeechRoute::Outside => {
                    return if self.stop_speech(press, cx) {
                        self.runs_changed(cx)
                    } else {
                        Ok(())
                    };
                }
                SpeechRoute::Stale => return Ok(()),
                SpeechRoute::Current => {}
            }
        }
        self.voice.gates.forget_runs();
        // This press revoked the input of a hold that is already torn down: by its cancelled
        // `End`, in this delivery or at the raw Esc-down before it, or by the brain's reply or
        // the hold's own run, applied while the press waited. It is that hold's press only.
        // Marks that are up go, as at any press. The run the hold became is stopped if it is
        // what Esc would stop now. Nothing else: no local decision is left for it.
        if let Some(gesture) = gesture {
            let Some(revoked) = self.voice.state.revoked(gesture) else {
                return if self.voice_working() {
                    Ok(())
                } else {
                    self.escape_with_speech(speech, cx)
                };
            };
            self.marks_changed(cx);
            self.cancel_voice_gesture(gesture, cx);
            let now = std::time::Instant::now();
            let moving = self.moving_run(now);
            if let Some(tail) = revoked.settle(&mut self.voice.state, moving) {
                let message = self.runs.stop_owned(tail);
                let stop = escape::stop_owned(&mut self.presenter, tail.run, now);
                let valid = stop.effects.is_ok();
                self.scene_effects(stop.effects, cx);
                if !stop.scenes
                    && valid
                    && let Some(message) = message
                {
                    self.tell(message, cx);
                }
            }
            return self.runs_changed(cx);
        }
        self.voice.state.ended = None;
        // Marks that are up go at this press, and `escape` takes them before it ends a hold.
        // A hold is still pending here when its first mark came before its run's `Answering`.
        if self.presenter.marks_shown().is_some() || mark.is_some() {
            return self.escape_after_wake(speech, mark, cx);
        }
        if self.voice_working() {
            return Ok(());
        }
        self.escape_with_speech(speech, cx)
    }

    pub(super) fn speech_session(&self, press: SpeechPress) -> Option<&Session> {
        self.session.as_ref().filter(|_| press.on(self.open_pair()))
    }

    fn open_pair(&self) -> Option<u64> {
        self.session
            .as_ref()
            .filter(|session| session.open())
            .map(Session::generation)
    }

    /// Stops the press's exact run. Returns whether the run view needs refreshing.
    fn stop_speech(&mut self, press: SpeechPress, cx: &mut Context<'_, Self>) -> bool {
        let Some(current) = self.speech_session(press).cloned() else {
            return false;
        };
        match press.stop(&self.runs, self.moving_run(std::time::Instant::now())) {
            SpeechStop::Speaker => {
                // The lease observer follows the quiet, so no refresh here can fail the pair.
                self.stop_speech_locally(current, press.run, cx);
                false
            }
            SpeechStop::Run => {
                self.fence_spoken(press.run, &current, cx);
                true
            }
        }
    }

    /// Stops `run`, which was speaking when a hold's keys went down, before that hold starts:
    /// fenced here with Esc's priorities, then its `Stop`, or its scenes' `StopScenes`, queued
    /// for the brain, and the speaker's `Stop` only after that (`send_stop`). A run that has
    /// already ended or been replaced stops nothing. False when the brain's queue refused it,
    /// which ends the connection.
    pub(super) fn fence_spoken(
        &mut self,
        run: RunId,
        session: &Session,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let now = std::time::Instant::now();
        if self.moving_run(now) != Some(run) {
            return true;
        }
        let message = self.runs.fence_run(run);
        let stop = escape::stop_owned(&mut self.presenter, run, now);
        self.scene_effects(stop.effects, cx);
        // A refused scene control already ended the connection and took the session.
        if self
            .session
            .as_ref()
            .is_none_or(|current| current.generation() != session.generation())
        {
            return false;
        }
        if stop.scenes {
            return true;
        }
        message.is_none_or(|message| self.send_stop(session, message, cx))
    }

    /// The run Esc's stop would stop at `now`: the moving run, else the one whose drawing still
    /// moves.
    pub(super) fn moving_run(&self, now: std::time::Instant) -> Option<RunId> {
        escape::moving_run(&self.runs, &self.presenter, now)
    }

    /// Esc's stop: fences the moving run off, or stops what the presenter still moves.
    fn stop_moving(&mut self, cx: &mut Context<'_, Self>) {
        match escape::stop_moving(&mut self.runs, &self.presenter, std::time::Instant::now()) {
            MovingStop::Run(message) => self.stop_run(message, cx),
            MovingStop::Drawing => {
                self.place_drawings(cx.weak_entity(), cx);
                match escape::stop_drawing(&mut self.presenter, std::time::Instant::now()) {
                    Some(effects) => self.scene_effects(effects, cx),
                    None => self.draw_changed(cx),
                }
            }
            MovingStop::Idle => {}
        }
    }

    /// One Esc press, from the hotkey, the typing panel or Stop. The local effect happens here,
    /// and only then is `Stop` or `ClearDrawing` queued for the brain, which nothing waits on.
    #[cfg(test)]
    pub(super) fn escape(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.escape_with_speech(
            SpeechPress::capture(
                self.speech.get(),
                std::time::Instant::now(),
                escape::SpeechEscapeOwner::Panel,
            ),
            cx,
        )
    }

    pub(super) fn escape_with_speech(
        &mut self,
        speech: Option<SpeechPress>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.escape_after_wake(speech, None, cx)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one Esc press keeps its local decision and every effect of it in order"
    )]
    fn escape_after_wake(
        &mut self,
        speech: Option<SpeechPress>,
        mark: Option<RunId>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        if speech.is_some_and(|press| !press.on(self.open_pair())) {
            return Ok(());
        }
        self.voice.gates.forget_runs();
        // Esc is a key like any other to screen marks: they go at the first press, whatever
        // else that press stops. When they are a running answer's, the same press stops that
        // answer too, whatever else it does here.
        let press = escape::Press::new(speech, self.presenter.marks_shown().or(mark), &self.runs);
        self.marks_changed(cx);
        self.voice.state.recovery = None;
        match press.lead(self.voice_escape_owner(), self.presenter.input.selection()) {
            Lead::CancelHold => {
                self.cancel_voice(cx);
                return Ok(());
            }
            Lead::EndSelection { goes_on } => {
                self.end_selection(crate::presenter::overlay_input::ExitCause::Escape, cx);
                if !goes_on {
                    return Ok(());
                }
            }
            Lead::Decide => {}
        }
        let panel = self.escape_panel(cx);
        let action = local_decision(
            Activity {
                speaking: speech.is_some(),
                ..self.activity
            },
            panel,
            &self.presenter,
        );
        let tell = match action {
            Escape::CancelPull => {
                self.kumo
                    .update(cx, |kumo, cx| kumo.pull_event(Event::Escape, cx));
                match press.after_pull() {
                    Some(AfterPull::Speech(speech)) => {
                        self.stop_speech(speech, cx);
                        None
                    }
                    Some(AfterPull::Run) => self.runs.stop(),
                    None => None,
                }
            }
            Escape::Stop => {
                if let Some(press) = speech {
                    self.stop_speech(press, cx);
                } else {
                    self.stop_moving(cx);
                }
                None
            }
            Escape::Clear => {
                let cleared = escape::clear(&mut self.runs, &self.presenter);
                self.grant(cleared.credit);
                match cleared.then {
                    AfterClear::Presenter => {
                        let effects = self.presenter.clear(std::time::Instant::now());
                        self.scene_effects(effects, cx);
                        None
                    }
                    AfterClear::Tell(message) => message,
                }
            }
            Escape::Dismiss => {
                self.runs.dismiss_retry();
                if panel == EscapePanel::Settings {
                    if !self.dismiss_account_confirmation(cx) {
                        self.dismiss_settings(cx)?;
                    }
                } else if panel == EscapePanel::Threads {
                    self.dismiss_threads(cx)?;
                } else if panel == EscapePanel::Onboarding {
                    self.dismiss_onboarding(cx)?;
                } else {
                    self.dismiss_text(cx)?;
                }
                None
            }
            Escape::CancelEdit => {
                if panel == EscapePanel::SettingsEdit {
                    self.cancel_settings_edit(cx)?;
                } else if panel == EscapePanel::OnboardingEdit {
                    self.cancel_onboarding_edit(cx);
                } else if let Some(panel) = &self.thread_panel {
                    panel
                        .window
                        .update(cx, |_, window, cx| {
                            panel
                                .view
                                .update(cx, |view, cx| view.cancel_rename(window, cx));
                        })
                        .map_err(|error| OpenError::Gpui(error.into()))?;
                }
                None
            }
            Escape::Ignore => None,
        };
        let panel = if self.text_open() {
            EscapePanel::Open
        } else {
            EscapePanel::Closed
        };
        self.activity = Activity {
            pulling: self.kumo.read(cx).pull_live(),
            speaking: self.speaking(std::time::Instant::now()),
            ..Self::activity(&self.runs, panel, &self.presenter)
        };
        self.refresh_conversation(cx);
        // Stamped once the local stop or clear is applied, for the Esc smoke's timing.
        diagnostics::record(cx, format_args!("escape action={action:?}"));
        if let Some(message) = tell {
            self.stop_run(message, cx);
        }
        self.escape_fixture = false;
        self.publish_plain_escape(cx);
        self.sync_escape(cx)
    }

    fn escape_panel(&self, cx: &gpui_kit::App) -> EscapePanel {
        if self.settings_focused() {
            if self
                .settings_panel
                .as_ref()
                .is_some_and(|panel| panel.view.read(cx).editing().is_some())
            {
                EscapePanel::SettingsEdit
            } else {
                EscapePanel::Settings
            }
        } else if self.threads_focused() {
            if self
                .thread_panel
                .as_ref()
                .is_some_and(|panel| panel.view.read(cx).editing())
            {
                EscapePanel::Rename
            } else {
                EscapePanel::Threads
            }
        } else if self.onboarding.is_some() {
            if self.onboarding_name_edit(cx).is_some() {
                EscapePanel::OnboardingEdit
            } else {
                EscapePanel::Onboarding
            }
        } else if self.text_open() {
            EscapePanel::Open
        } else {
            EscapePanel::Closed
        }
    }
}
