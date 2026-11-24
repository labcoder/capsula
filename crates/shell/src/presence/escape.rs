//! Local Esc routing for the focused surface.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use super::drag::Event;
use super::{Activity, Context, Escape, EscapePanel, OpenError, Presence, Session, diagnostics};
use crate::escape::{self, SpeechEscapeOwner, SpeechPress};
use crate::settings::Field;
use kumochi_protocol::message::{
    GestureId, MAX_VISIBLE_SCENES, RunId, SceneId, ThreadId, ThreadKind,
};

pub(super) type SpeechLease = Rc<Cell<Option<(u64, RunId, Instant)>>>;
pub(super) type NameEdit = Rc<Cell<Option<Field>>>;
pub(super) type RenameEdit = Rc<Cell<Option<(ThreadId, ThreadKind)>>>;
pub(super) type PlainEscapeCell = Rc<RefCell<Option<PublishedEscape>>>;

enum PanelEdit {
    Name(NameEdit),
    Rename(RenameEdit),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edit {
    Name(Field),
    Rename(ThreadId, ThreadKind),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MarkedAnswer {
    run: RunId,
    scene: SceneId,
    other_work: Option<RunId>,
}

/// The owners the aggregate plain Esc route may affect, excluding paint and phase progress.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct PlainEscape {
    pair: Option<u64>,
    target: (Option<ThreadId>, u64),
    frontier: (u32, RunId),
    moving: (Option<RunId>, Option<RunId>),
    action: Escape,
    scenes: [[Option<SceneId>; MAX_VISIBLE_SCENES]; 2],
    hold: Option<GestureId>,
    watch: Option<super::marks::Watched>,
    marks: Option<MarkedAnswer>,
    selection: Option<GestureId>,
    pull: Option<super::drag::Generation>,
    panel: Option<(EscapePanel, gpui_kit::WindowId)>,
    edit: Option<Edit>,
}

impl PlainEscape {
    pub(super) fn still_owns(&self, live: &Self) -> bool {
        if self == live {
            return true;
        }
        let Some(MarkedAnswer {
            run,
            scene,
            other_work,
        }) = self.marks
        else {
            return false;
        };
        if self.action != Escape::Stop
            || self.hold.is_some()
            || self.selection.is_some()
            || self.moving.0 != Some(run)
            || self.moving.1.is_some_and(|drawing| drawing > run)
            || !self.scenes[1].contains(&Some(scene))
        {
            return false;
        }
        let [legacy, presented] = self.scenes;
        let mut remaining = presented.into_iter().filter(|&id| id != Some(scene));
        let scenes = [legacy, std::array::from_fn(|_| remaining.next().flatten())];
        Self {
            marks: None,
            scenes,
            moving: (Some(run), other_work),
            ..*self
        } == *live
    }
}

pub(super) struct PublishedEscape {
    scope: PlainEscape,
    edit: Option<PanelEdit>,
}

impl PublishedEscape {
    fn capture(&self) -> PlainEscape {
        let edit = match &self.edit {
            Some(PanelEdit::Name(edit)) => edit.get().map(Edit::Name),
            Some(PanelEdit::Rename(edit)) => {
                edit.get().map(|(thread, kind)| Edit::Rename(thread, kind))
            }
            None => None,
        };
        PlainEscape {
            action: if edit.is_some() {
                Escape::CancelEdit
            } else {
                self.scope.action
            },
            edit,
            ..self.scope
        }
    }
}

pub(super) struct Presses {
    pub(super) speech: SpeechLease,
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
            plain: Rc::clone(&self.plain_escape),
        }
    }

    pub(super) fn publish_plain_escape(&self, cx: &gpui_kit::App) {
        self.plain_escape.replace(self.plain_escape_snapshot(cx));
    }

    pub(super) fn plain_escape_at(&self, cx: &gpui_kit::App) -> Option<PlainEscape> {
        let scope = self.plain_escape_snapshot(cx)?.capture();
        if matches!(scope.action, Escape::Dismiss | Escape::CancelEdit)
            && matches!(scope.panel, Some((EscapePanel::Onboarding, _)))
            && scope.edit != self.onboarding_name_edit(cx).map(Edit::Name)
        {
            return None;
        }
        Some(scope)
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
        let marks = self
            .presenter
            .marks_owner()
            .map(|(marked_run, scene)| MarkedAnswer {
                run: marked_run,
                scene,
                other_work: (action == Escape::Stop
                    && run == Some(marked_run)
                    && hold.is_none()
                    && selection.is_none())
                .then(|| self.presenter.stoppable_run_excluding(Some(scene)))
                .flatten(),
            });
        let reads_edit = action == Escape::Dismiss;
        let (window, edit) = match panel {
            EscapePanel::Open => (Some(self.text.as_ref()?.window), None),
            EscapePanel::Settings => {
                let panel = self.settings_panel.as_ref()?;
                let edit = reads_edit.then(|| PanelEdit::Name(Rc::clone(&panel.editing)));
                (Some(panel.window), edit)
            }
            EscapePanel::Threads => {
                let panel = self.thread_panel.as_ref()?;
                let edit = reads_edit.then(|| PanelEdit::Rename(Rc::clone(&panel.rename)));
                (Some(panel.window), edit)
            }
            EscapePanel::Onboarding => {
                let panel = self.onboarding.as_ref()?;
                let edit = reads_edit.then(|| PanelEdit::Name(Rc::clone(&panel.escape_edit)));
                (Some(panel.window), edit)
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
                panel: window.map(|window| (panel, window.window_id())),
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
        if gesture.is_none()
            && let Some(press) = speech
        {
            if press.owner == SpeechEscapeOwner::Outside {
                return if self.stop_speech(press, cx) {
                    self.runs_changed(cx)
                } else {
                    Ok(())
                };
            }
            if self.speech_session(press).is_none() {
                return Ok(());
            }
        }
        self.voice.gates.forget_runs();
        // This press revoked the input of a hold that is already torn down: by its cancelled
        // `End`, in this delivery or at the raw Esc-down before it, or by the brain's reply or
        // the hold's own run, applied while the press waited. It is that hold's press only.
        // Marks that are up go, as at any press. The run the hold became is stopped if it is
        // what Esc would stop now. Nothing else: no local decision is left for it.
        if let Some(gesture) = gesture {
            let ended = self
                .voice
                .ended
                .take_if(|(hold, _)| *hold == gesture)
                .map(|(_, tail)| tail);
            let pending = self
                .voice
                .pending
                .as_ref()
                .filter(|hold| hold.token.gesture() == gesture)
                .map(|hold| hold.run);
            let Some(tail) = ended.or(pending) else {
                return if self.voice_working() {
                    Ok(())
                } else {
                    self.escape_with_speech(speech, cx)
                };
            };
            self.discard_voice_recovery(gesture);
            self.marks_changed(cx);
            self.cancel_voice_gesture(gesture, cx);
            self.voice.ended.take_if(|(hold, _)| *hold == gesture);
            let now = std::time::Instant::now();
            let scenes = self.presenter.holds_scenes() || self.presenter.live_work(now);
            let target = self.moving_run(now);
            if let Some(tail) = tail.filter(|tail| target == Some(tail.run)) {
                let message = self.runs.stop_owned(tail);
                let effects = self.presenter.stop_owned(tail.run, now).map(|mut effects| {
                    if !scenes {
                        effects.retain(|effect| {
                            !matches!(
                                effect,
                                crate::presenter::Effect::Control(
                                    kumochi_protocol::message::ControlToBrain::StopScenes { .. }
                                )
                            )
                        });
                    }
                    effects
                });
                let valid = effects.is_ok();
                self.scene_effects(effects, cx);
                if !scenes
                    && valid
                    && let Some(message) = message
                {
                    self.tell(message, cx);
                }
            }
            return self.runs_changed(cx);
        }
        self.voice.ended = None;
        // Marks that are up go at this press, and `escape` takes them before it ends a hold.
        // A hold is still pending here when its first mark came before its run's `Answering`.
        if self.presenter.marks_shown().is_some() {
            return self.escape_with_speech(speech, cx);
        }
        if self.voice_working() {
            return Ok(());
        }
        self.escape_with_speech(speech, cx)
    }

    pub(super) fn speech_session(&self, press: SpeechPress) -> Option<&Session> {
        self.session
            .as_ref()
            .filter(|session| session.open() && session.generation() == press.session)
    }

    /// Stops the press's exact run. Returns whether the run view needs refreshing.
    fn stop_speech(&mut self, press: SpeechPress, cx: &mut Context<'_, Self>) -> bool {
        let Some(current) = self.speech_session(press).cloned() else {
            return false;
        };
        // Done comes on control before RunEnd on data, so an ended run can still speak.
        if press.speaker_only(&self.runs, self.moving_run(std::time::Instant::now())) {
            // The lease observer follows the quiet, so no refresh here can fail the pair.
            self.stop_speech_locally(current, press.run, cx);
            return false;
        }
        self.fence_spoken(press.run, &current, cx);
        true
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
        let scenes = self.presenter.holds_scenes() || self.presenter.live_work(now);
        let message = self.runs.fence_run(run);
        let effects = self.presenter.stop_owned(run, now).map(|mut effects| {
            if !scenes {
                effects.retain(|effect| {
                    !matches!(
                        effect,
                        crate::presenter::Effect::Control(
                            kumochi_protocol::message::ControlToBrain::StopScenes { .. }
                        )
                    )
                });
            }
            effects
        });
        self.scene_effects(effects, cx);
        // A refused scene control already ended the connection and took the session.
        if self
            .session
            .as_ref()
            .is_none_or(|current| current.generation() != session.generation())
        {
            return false;
        }
        if scenes {
            return true;
        }
        message.is_none_or(|message| self.send_stop(session, message, cx))
    }

    /// The run Esc's stop would stop at `now`: the moving run, else the one whose drawing still
    /// moves.
    pub(super) fn moving_run(&self, now: std::time::Instant) -> Option<RunId> {
        self.runs
            .stoppable()
            .map(|run| {
                if self.presenter.holds_scenes() || self.presenter.live_work(now) {
                    self.presenter
                        .stoppable_run()
                        .map_or(run, |drawing| drawing.max(run))
                } else {
                    run
                }
            })
            .or_else(|| {
                let moving = self.presenter.live_work(now);
                self.presenter.stoppable_run().filter(|_| moving)
            })
    }

    /// Esc's stop: fences the moving run off, or stops what the presenter still moves.
    fn stop_moving(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(message) = self.runs.stop() {
            self.stop_run(message, cx);
        } else if self.presenter.live_work(std::time::Instant::now()) {
            self.place_drawings(cx.weak_entity(), cx);
            if let Some(run) = self.presenter.stoppable_run() {
                let effects = self.presenter.stop(run, std::time::Instant::now());
                self.scene_effects(effects, cx);
            } else {
                self.presenter.discard_replacement();
                self.draw_changed(cx);
            }
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
                SpeechEscapeOwner::Panel,
            ),
            cx,
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one Esc press keeps its local decision and every effect of it in order"
    )]
    pub(super) fn escape_with_speech(
        &mut self,
        speech: Option<SpeechPress>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        if speech.is_some_and(|press| self.speech_session(press).is_none()) {
            return Ok(());
        }
        self.voice.gates.forget_runs();
        // Esc is a key like any other to screen marks: they go at the first press, whatever
        // else that press stops. When they are a running answer's, the same press stops that
        // answer too, whatever else it does here.
        let answer = self.presenter.marks_shown().filter(|run| {
            self.runs.stoppable() == Some(*run)
                && speech.is_none_or(|press| press.run == *run && !self.runs.state_ended(*run))
        });
        self.marks_changed(cx);
        self.voice.recovery = None;
        if self.voice_working() {
            self.cancel_voice(cx);
            return Ok(());
        }
        if self.presenter.input.selection().is_some() {
            self.end_selection(crate::presenter::overlay_input::ExitCause::Escape, cx);
            // With a running answer's marks taken the press goes on, to stop that answer.
            if answer.is_none() {
                return Ok(());
            }
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
                // The answer whose marks this press took is stopped after the pull's cancel.
                if let Some(press) = speech {
                    if answer.is_some() {
                        self.stop_speech(press, cx);
                    }
                    None
                } else {
                    answer.and_then(|_| self.runs.stop())
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
                let effect = self.runs.clear();
                self.grant(effect.credit);
                if self.presenter.holds_scenes() {
                    let effects = self.presenter.clear(std::time::Instant::now());
                    self.scene_effects(effects, cx);
                    None
                } else {
                    effect.clear
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

pub(super) fn local_decision(
    mut activity: Activity,
    panel: EscapePanel,
    presenter: &crate::presenter::Presenter,
) -> Escape {
    activity.drawing |= matches!(panel, EscapePanel::Open) && presenter.holds_scenes();
    escape::decide(activity, panel)
}
