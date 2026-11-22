//! Local Esc routing for the focused surface.

use super::drag::Event;
use super::{Activity, Context, Escape, EscapePanel, OpenError, Presence, Session, diagnostics};
use crate::escape;
use kumochi_protocol::message::{GestureId, RunId};

impl Presence {
    pub(super) fn native_escape(
        &mut self,
        gesture: Option<GestureId>,
        speech: Option<(u64, RunId)>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
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
                    self.escape_for(speech, cx)
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
            return self.escape_for(speech, cx);
        }
        if self.voice_working() {
            return Ok(());
        }
        self.escape_for(speech, cx)
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

    /// One Esc press from the typing panel or Stop, targeting the speech lease that holds now.
    pub(super) fn escape(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.escape_for(self.live_speech(std::time::Instant::now()), cx)
    }

    /// One Esc press. The local effect happens here, and only then is `Stop` or `ClearDrawing`
    /// queued for the brain, which nothing waits on. `speech` is the lease the press saw: once
    /// its run is no longer the moving one, the press stops only that run's speaker, locally.
    #[expect(
        clippy::too_many_lines,
        reason = "one Esc press keeps its local decision and every effect of it in order"
    )]
    fn escape_for(
        &mut self,
        speech: Option<(u64, RunId)>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.voice.gates.forget_runs();
        // Esc is a key like any other to screen marks: they go at the first press, whatever
        // else that press stops. When they are a running answer's, the same press stops that
        // answer too, whatever else it does here.
        let answer = self
            .presenter
            .marks_shown()
            .filter(|run| self.runs.stoppable() == Some(*run));
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
        let panel = if self.settings_focused() {
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
            if self.onboarding_editing(cx) {
                EscapePanel::OnboardingEdit
            } else {
                EscapePanel::Onboarding
            }
        } else if self.text_open() {
            EscapePanel::Open
        } else {
            EscapePanel::Closed
        };
        let activity = Activity {
            speaking: speech.is_some(),
            ..self.activity
        };
        let action = local_decision(activity, panel, &self.presenter);
        let tell = match action {
            Escape::CancelPull => {
                self.kumo
                    .update(cx, |kumo, cx| kumo.pull_event(Event::Escape, cx));
                // The answer whose marks this press took is stopped after the pull's cancel.
                answer.and_then(|_| self.runs.stop())
            }
            Escape::Stop => {
                if let Some((session, run)) =
                    speech.filter(|(_, run)| self.runs.stoppable() != Some(*run))
                {
                    if let Some(current) = self
                        .session
                        .clone()
                        .filter(|current| current.generation() == session)
                    {
                        self.stop_speaker(current, run, cx);
                    }
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
        self.sync_escape(cx)
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
