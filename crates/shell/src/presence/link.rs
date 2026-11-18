//! Applying session-fenced link events to browsed and focused histories and runs.

#[cfg(test)]
mod github_tests;

use super::{Context, OpenError, Presence, Session, diagnostics};
use crate::chat_notice::Source;
use crate::drawing::{Broken, Chunk, Effect};
use crate::link::{Batch, LinkEvent, LinkState};
use crate::settings::Update;
use kumochi_protocol::message::{
    AccountView, ConnectionsView, ControlToBrain, DataToShell, HeldScreen, Phase, RunId, RunOrigin,
    ScreenHolds, ScreenOff, SettingsView, ThreadId, UserError,
};
use kumochi_scene::deferred::Deferred;
use std::time::Instant;

impl Presence {
    /// Applies what the link reported, in order, then follows the runs once. Each event leaves
    /// the batch only to be applied; a scene batch's buffer still drops off this thread.
    #[expect(
        clippy::too_many_lines,
        reason = "one ordered batch dispatcher keeps session fences and event application together"
    )]
    pub(super) fn link_events(
        &mut self,
        mut events: Batch,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let mut applied = false;
        for event in events.drain(..) {
            if let LinkEvent::RunState { generation, .. }
            | LinkEvent::TurnRefused { generation, .. }
            | LinkEvent::VoiceHold { generation, .. }
            | LinkEvent::CaptureDone { generation, .. }
            | LinkEvent::ScreenHolds { generation, .. }
            | LinkEvent::HoldScreen { generation, .. }
            | LinkEvent::DrawingNotice { generation, .. }
            | LinkEvent::CaptureFailed { generation, .. } = &event
                && self
                    .session
                    .as_ref()
                    .is_none_or(|session| session.generation() != *generation)
            {
                continue;
            }
            match event {
                LinkEvent::DrawingNotice {
                    key, thread, run, ..
                } => self.show_error(&key.into(), Source::Drawing { thread, run }, cx),
                LinkEvent::State(state) => {
                    self.recover_hold(cx);
                    self.cancel_voice(cx);
                    self.retire_issue_review(cx);
                    self.revoke_exclusion(cx);
                    self.session = None;
                    self.link_state(state, cx);
                }
                LinkEvent::Connected(session) => {
                    self.cancel_voice(cx);
                    self.retire_issue_review(cx);
                    self.settings.update(cx, |state, cx| {
                        state.github.disconnected();
                        state.key_paste.disconnected();
                        cx.notify();
                    });
                    self.revoke_exclusion(cx);
                    self.session = Some(session);
                    self.link_state(LinkState::Connected, cx);
                }
                LinkEvent::Settings {
                    generation,
                    view,
                    answer,
                } => {
                    self.settings_changed(
                        generation,
                        view,
                        if answer {
                            Update::Answer
                        } else {
                            Update::Unsolicited
                        },
                        cx,
                    );
                }
                LinkEvent::VoiceHold {
                    generation,
                    gesture,
                    status,
                } => self.voice_status(generation, gesture, status, cx),
                LinkEvent::Voice {
                    generation,
                    speech_input,
                } => {
                    let current = self.session.as_ref().map(crate::link::Session::generation);
                    self.settings.update(cx, |state, cx| {
                        if state.receive_speech_input(current, generation, speech_input) {
                            cx.notify();
                        }
                    });
                }
                LinkEvent::CaptureDone {
                    generation,
                    capture_for,
                } => {
                    self.capture_finished(generation, capture_for, cx);
                }
                LinkEvent::CaptureFailed {
                    generation,
                    capture_for,
                    reason: _reason,
                } => {
                    self.capture_finished(generation, capture_for, cx);
                }
                LinkEvent::ScreenHolds { state, .. } => {
                    let off = matches!(state, ScreenHolds::Off(ScreenOff::TurnedOff));
                    self.runs.set_screen_holds(state);
                    if off {
                        self.marks_off(cx);
                    }
                }
                LinkEvent::HoldScreen {
                    gesture, outcome, ..
                } => {
                    self.runs.hold_screen(gesture, outcome);
                    // An answer that didn't get the screenshot has nothing to mark.
                    if outcome != HeldScreen::Screen {
                        self.marks_dropped(gesture, cx);
                    }
                }
                LinkEvent::RunState {
                    run,
                    thread,
                    phase,
                    error,
                    origin,
                    ..
                } => self.run_state(run, thread, phase, error, origin, cx),
                LinkEvent::Data {
                    generation,
                    message,
                    bytes,
                } => self.data_received(generation, message, bytes, cx),
                LinkEvent::SceneData {
                    generation,
                    frame,
                    bytes,
                } => self.scene_data(generation, frame, bytes, cx),
                LinkEvent::Scenes(scenes) => match self.runs.apply_scenes(&scenes) {
                    Ok(effect) => {
                        self.effect(effect);
                        if let Some(session) = &self.session {
                            let effects = self.presenter.described(
                                session.generation(),
                                &kumochi_scene::text::Bounded::default(),
                                Instant::now(),
                            );
                            self.scene_effects(effects, cx);
                        }
                        self.send_deletes(cx);
                    }
                    Err(error) => self.end(&error),
                },
                LinkEvent::ScenesV1 { generation, scenes } => {
                    let effects = self
                        .presenter
                        .described(generation, &scenes, Instant::now());
                    self.scene_effects(effects, cx);
                    self.send_deletes(cx);
                }
                LinkEvent::ThreadDeleted {
                    thread,
                    outcome,
                    last_run,
                } => self.thread_deleted(thread, outcome, last_run, cx),
                LinkEvent::TurnRefused { turn, thread, .. } => {
                    self.runs.refused(turn, thread);
                    self.sync_deletes(cx);
                }
                LinkEvent::ThreadsChanged => self.threads_changed(cx),
                LinkEvent::Connections { generation, view } => {
                    self.connections_changed(generation, view, cx);
                }
                LinkEvent::ActionState {
                    generation,
                    request,
                    draft,
                    version,
                    state,
                    approval,
                } => {
                    if self
                        .session
                        .as_ref()
                        .is_some_and(|session| session.generation() == generation)
                    {
                        self.threads.update(cx, |model, cx| {
                            model.action_state(draft, version, state, approval, request);
                            cx.notify();
                        });
                        self.chat.update(cx, |_, cx| cx.notify());
                        self.request_pages(cx);
                    }
                }
                LinkEvent::IssueRefused {
                    generation,
                    request,
                    draft,
                    version,
                    approval,
                    reason,
                } => {
                    if self
                        .session
                        .as_ref()
                        .is_some_and(|session| session.generation() == generation)
                    {
                        self.threads.update(cx, |model, cx| {
                            model.issue_refused(request, draft, version, approval, reason);
                            cx.notify();
                        });
                        self.chat.update(cx, |_, cx| cx.notify());
                        self.fail_issue_review(request, draft, cx);
                    }
                }
                LinkEvent::IssueUnavailable {
                    generation,
                    request,
                    draft,
                    kind,
                } => {
                    if self
                        .session
                        .as_ref()
                        .is_some_and(|session| session.generation() == generation)
                    {
                        self.threads.update(cx, |model, cx| {
                            model.issue_unavailable(request, draft, kind);
                            cx.notify();
                        });
                        self.chat.update(cx, |_, cx| cx.notify());
                        self.fail_issue_review(request, draft, cx);
                    }
                }
                LinkEvent::Focus {
                    generation,
                    run,
                    thread,
                } => self.focus(generation, run, thread, cx),
                LinkEvent::Account { generation, view } => {
                    self.account_changed(generation, view, cx);
                }
                LinkEvent::GitHub { generation, view } => {
                    let current = self.session.as_ref().map(crate::link::Session::generation);
                    let accepted = self.settings.update(cx, |state, cx| {
                        let accepted =
                            state
                                .github
                                .receive(current, generation, view, Instant::now());
                        if accepted {
                            cx.notify();
                        }
                        accepted
                    });
                    if accepted {
                        self.settle_github_interactive(cx);
                    }
                }
                LinkEvent::GitHubError { generation, error } => {
                    let current = self.session.as_ref().map(crate::link::Session::generation);
                    let accepted = self.settings.update(cx, |state, cx| {
                        let accepted = state.github.receive_error(current, generation, error);
                        if accepted {
                            cx.notify();
                        }
                        accepted
                    });
                    if accepted {
                        self.settle_github_interactive(cx);
                    }
                }
                LinkEvent::KeyStage { generation, reply } => self.key_reply(generation, &reply, cx),
                LinkEvent::Error { generation, error } => {
                    if self
                        .session
                        .as_ref()
                        .is_some_and(|session| session.generation() == generation)
                    {
                        self.show_error(&error, Source::Account, cx);
                    }
                }
            }
            applied = true;
        }
        events.attribute(events.capacity() * size_of::<LinkEvent>());
        if applied {
            self.screen_changed(cx);
            self.runs_changed(cx)
        } else {
            Ok(())
        }
    }

    /// A run's phase changed; a failure the brain explained shows its reason, unless Esc or a
    /// delete fenced the run off first.
    fn run_state(
        &mut self,
        run: RunId,
        thread: ThreadId,
        phase: Phase,
        error: Option<UserError>,
        origin: Option<RunOrigin>,
        cx: &mut Context<'_, Self>,
    ) {
        let (applied, notice) = self.runs.apply_run_state(run, thread, phase, error, origin);
        self.marks_run(run, origin, &applied, cx);
        self.voice_run(run, thread, origin, phase, &applied, cx);
        diagnostics::record(cx, format_args!("run phase={phase:?}"));
        if let Some(restore) = applied.restore
            && let Some(text) = &self.text
            && text
                .window
                .update(cx, |_, window, cx| {
                    text.view
                        .update(cx, |view, cx| view.restore(restore, window, cx));
                })
                .is_err()
        {
            tracing::warn!("can't restore the submitted draft");
        }
        if let Some(error) = notice {
            self.show_error(&error, Source::Run(run), cx);
        }
        if let Some(stop) = applied.stop {
            match (origin, stop) {
                (Some(RunOrigin::Hold { .. }), ControlToBrain::Stop { run, revealed }) => {
                    let effects =
                        self.presenter
                            .stop_owned(run, Instant::now())
                            .map(|mut effects| {
                                let cut = effects.iter().any(|effect| match effect {
                                    crate::presenter::Effect::Control(
                                        ControlToBrain::StopScenes { cuts, .. },
                                    ) => !cuts.is_empty(),
                                    _ => false,
                                });
                                if !cut {
                                    effects.retain(|effect| {
                                        !matches!(
                                            effect,
                                            crate::presenter::Effect::Control(
                                                ControlToBrain::StopScenes { .. }
                                            )
                                        )
                                    });
                                    effects.push(crate::presenter::Effect::Control(
                                        ControlToBrain::Stop { run, revealed },
                                    ));
                                }
                                effects
                            });
                    self.scene_effects(effects, cx);
                }
                (_, stop) => self.stop_run(stop, cx),
            }
        }
        self.grant(applied.credit);
        if phase == Phase::Done
            && applied.visible
            && self
                .runs
                .shown()
                .is_some_and(|shown| shown.run == run && shown.done)
        {
            self.chat.update(cx, |state, _| state.settled(thread, run));
        }
        for model in [&self.threads, &self.chat] {
            model.update(cx, |state, cx| {
                state.run_state(thread);
                cx.notify();
            });
        }
        self.sync_deletes(cx);
        self.request_pages(cx);
    }

    /// A run asked to move the typed input: it moves, and the task shows, only when the current
    /// connection sent it and the runs still agree.
    fn focus(&mut self, generation: u64, run: RunId, thread: ThreadId, cx: &mut Context<'_, Self>) {
        let current = self
            .session
            .as_ref()
            .is_some_and(|session| session.generation() == generation);
        if current && self.runs.focus(run, thread) {
            self.select_thread(Some(thread), cx);
            self.refresh_conversation(cx);
        }
    }

    /// The brain changed a listed thread: the list is read again.
    fn threads_changed(&mut self, cx: &mut Context<'_, Self>) {
        for model in [&self.threads, &self.chat] {
            model.update(cx, |state, cx| {
                state.changed();
                cx.notify();
            });
        }
        self.request_pages(cx);
    }

    fn account_changed(&mut self, generation: u64, view: AccountView, cx: &mut Context<'_, Self>) {
        let current = self.session.as_ref().map(crate::link::Session::generation);
        let previous = self.settings.read(cx).accounts.generation;
        let (accepted, recovered) = self.settings.update(cx, |state, cx| {
            if state.accounts.receive(current, generation, view) {
                cx.notify();
                return (
                    true,
                    matches!(
                        state.accounts.view,
                        Some(AccountView::SignedIn { recovery: None, .. })
                    ) && state.accounts.error.is_none(),
                );
            }
            (false, false)
        });
        if accepted {
            self.account_seen(generation, previous, cx);
        }
        if recovered {
            self.chat_notice.recovered();
            if self.settings.read(cx).notice().is_some_and(|error| {
                error.key() == kumochi_protocol::message::ErrorKey::RenewalUnconfirmed
            }) {
                self.settings.update(cx, |state, _| state.clear_notice());
                self.sync_settings_notice(cx);
            }
        }
    }

    fn connections_changed(
        &mut self,
        generation: u64,
        view: ConnectionsView,
        cx: &mut Context<'_, Self>,
    ) {
        let current = self.session.as_ref().map(crate::link::Session::generation);
        let previous = self.settings.read(cx).claude.generation;
        let accepted = self.settings.update(cx, |state, cx| {
            let accepted = state.claude.receive(current, generation, view.claude_code);
            let key_changed = current == Some(generation)
                && state
                    .key_paste
                    .receive(view.anthropic, view.anthropic_revision);
            if accepted || key_changed {
                cx.notify();
            }
            accepted
        });
        if accepted {
            self.claude_seen(generation, previous, cx);
        }
    }

    /// Scene frames go to the presenter even when stale, so it drops them off the UI thread.
    fn scene_data(
        &mut self,
        generation: u64,
        frame: Deferred<DataToShell>,
        bytes: u32,
        cx: &mut Context<'_, Self>,
    ) {
        let disposition = if self
            .session
            .as_ref()
            .is_some_and(|session| session.generation() == generation)
            && let DataToShell::SceneStepCommit { run, .. } = &*frame
        {
            self.runs.owned_disposition(*run)
        } else {
            crate::runs::RunDisposition::Live
        };
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.generation() == generation)
        {
            self.place_drawings(cx.weak_entity(), cx);
        }
        if let DataToShell::ScenesRemoved { scenes } | DataToShell::ScenesReplaced { scenes, .. } =
            &*frame
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.generation() == generation)
        {
            let credit = self.runs.remove_scenes(scenes);
            self.grant(credit);
        }
        let effects = self.presenter.data_with_disposition(
            generation,
            frame,
            bytes,
            disposition,
            Instant::now(),
        );
        self.scene_effects(effects, cx);
    }

    fn data_received(
        &mut self,
        generation: u64,
        message: DataToShell,
        bytes: u32,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.generation() == generation)
        {
            if let DataToShell::RunEnd { run } = &message {
                self.voice_input_ended(*run);
            }
            let credit = match message {
                DataToShell::IssueReview { request, card } => {
                    if self.deliver_issue_review(request, card, bytes, cx) {
                        0
                    } else {
                        bytes
                    }
                }
                message @ (DataToShell::ThreadList { request, .. }
                | DataToShell::Page { request, .. }
                | DataToShell::PageFailed { request }
                | DataToShell::IssueCardRead { request, .. }
                | DataToShell::IssueCardsDone { request, .. }) => {
                    let chat = self.chat.read(cx).owns_request(request);
                    let cards = matches!(
                        &message,
                        DataToShell::IssueCardRead { .. } | DataToShell::IssueCardsDone { .. }
                    );
                    let model = if chat { &self.chat } else { &self.threads };
                    let credit = model.update(cx, |state, cx| {
                        cx.notify();
                        state.receive(message, bytes)
                    });
                    if cards {
                        let peer = if chat { &self.threads } else { &self.chat };
                        peer.update(cx, |_, cx| cx.notify());
                    }
                    self.request_pages(cx);
                    credit
                }
                DataToShell::HoldWords { gesture, words } => {
                    self.hold_words(generation, gesture, words);
                    bytes
                }
                DataToShell::HoldStored { gesture } => {
                    self.hold_stored(generation, gesture);
                    bytes
                }
                DataToShell::Models { connection, models } => {
                    self.settings.update(cx, |state, cx| {
                        state.set_models(connection, models);
                        cx.notify();
                    });
                    bytes
                }
                DataToShell::SceneSnapshot {
                    scene,
                    revision,
                    chunk,
                    last,
                } => {
                    let chunk = Chunk {
                        scene,
                        revision,
                        steps: chunk,
                        last,
                        bytes,
                    };
                    match self.runs.apply_snapshot(generation, chunk) {
                        Ok(credit) => credit,
                        Err(error) => {
                            self.end(&error);
                            0
                        }
                    }
                }
                message => self.runs.apply_data(message, bytes),
            };
            self.grant(credit);
            self.marks_ended(cx);
        }
    }

    fn settings_changed(
        &mut self,
        generation: u64,
        view: SettingsView,
        update: Update,
        cx: &mut Context<'_, Self>,
    ) {
        let current = self.session.as_ref().map(crate::link::Session::generation);
        let (link, session) = (&self.link, &self.session);
        let applied = self.settings.update(cx, |state, cx| {
            let applied = state.receive(current, generation, view.clone(), update);
            if applied {
                state.flush_offline_position(|op| {
                    session.as_ref().is_some_and(|session| {
                        link.send_control(session, ControlToBrain::Settings(op))
                    })
                });
                cx.notify();
            }
            applied
        });
        if !applied {
            return;
        }
        self.runs.settings(view.main_thread);
        if !view.hold_to_talk {
            self.cancel_voice(cx);
        }
        self.clear_refusal(cx);
        self.sync_settings_notice(cx);
        if matches!(
            self.chat_notice.shown().map(|notice| notice.source),
            Some(Source::Settings(_))
        ) && self.settings.read(cx).notice().is_none()
        {
            self.chat_notice.dismiss();
        }
        self.threads.update(cx, |state, cx| {
            state.set_settings(view.clone());
            cx.notify();
        });
        self.chat.update(cx, |state, cx| {
            state.set_settings(view);
            cx.notify();
        });
        self.sync_taskbar(cx);
        if let Err(error) =
            self.restore_dock_position(self.settings.read(cx).dock_position_choice(), cx)
        {
            tracing::warn!(%error, "can't restore the saved dock position");
        }
        if let Err(error) = self.onboarding_changed(cx) {
            tracing::error!(%error, "can't show onboarding");
        }
    }

    /// Shows the brain's error in both panels and on the connected dock.
    fn show_error(&mut self, error: &UserError, source: Source, cx: &mut Context<'_, Self>) {
        self.chatgpt_sign_in_failed(error);
        let field = self.settings.update(cx, |state, cx| {
            state.accounts.error(error);
            let field = state.error(error.clone());
            cx.notify();
            field
        });
        let source = if matches!(
            error.key(),
            kumochi_protocol::message::ErrorKey::SettingsRefused
                | kumochi_protocol::message::ErrorKey::ConfigUnreadable
                | kumochi_protocol::message::ErrorKey::HoldToTalkOff
        ) {
            Source::Settings(field)
        } else {
            source
        };
        self.chat_notice.present(error.clone(), source);
        self.sync_settings_notice(cx);
        self.sync_taskbar(cx);
    }

    /// Queues `message` for the brain. A run's `Stop` or `StopScenes` then stops its speech, and
    /// only once the brain's is queued, so speech never stops for a stop the brain wasn't told.
    pub(super) fn send_stop(&self, session: &Session, message: ControlToBrain) -> bool {
        let run = match &message {
            ControlToBrain::Stop { run, .. } | ControlToBrain::StopScenes { run, .. } => Some(*run),
            _ => None,
        };
        let sent = self.link.send_control(session, message);
        // A speaker that can't take the `Stop` already ended the session through the owner.
        if sent
            && let Some(run) = run
            && !self.link.output().stop(session.generation(), run)
        {
            tracing::debug!("no speech stopped for this session");
        }
        sent
    }

    /// Queues a `Stop` or `ClearDrawing` for the brain; its local effect has already happened.
    pub(super) fn tell(&self, message: ControlToBrain) {
        let sent = self
            .session
            .as_ref()
            .is_some_and(|session| self.send_stop(session, message));
        if !sent {
            tracing::warn!("the brain isn't connected or isn't reading; nothing to tell");
        }
    }

    /// Grants a drawing change's credit, then queues its `ClearDrawing`.
    pub(super) fn effect(&self, effect: Effect) {
        self.grant(effect.credit);
        if let Some(clear) = effect.clear {
            self.tell(clear);
        }
    }

    /// Ends a connection whose brain broke the drawing's protocol; the link reconnects.
    fn end(&self, error: &Broken) {
        tracing::warn!(%error, "the brain broke the drawing's protocol; ending the connection");
        if let Some(session) = &self.session {
            self.link.end(session);
        }
    }

    /// Grants the connection credit for data frames the UI has taken.
    pub(super) fn grant(&self, credit: u32) {
        if let Some(session) = self.session.as_ref().filter(|_| credit > 0) {
            session.grant(credit);
        }
    }

    fn link_state(&mut self, state: LinkState, cx: &mut Context<'_, Self>) {
        self.chat_notice.clear();
        self.end_selection(crate::presenter::overlay_input::ExitCause::BrainLost, cx);
        if state == LinkState::Connected {
            self.clear_refusal(cx);
        }
        if state == LinkState::Failed {
            self.native.enable_retry();
        } else {
            self.native.disable_retry();
        }
        if let Some(session) = self
            .session
            .as_ref()
            .filter(|_| state == LinkState::Connected)
        {
            self.runs.met(session.incarnation());
            self.runs.connected(session.generation());
            self.presenter.connected(session.generation());
            // Recovered drawings start on a cleared screen, so none keeps the old stage.
            self.drawing_placement = None;
            for model in [&self.threads, &self.chat] {
                model.update(cx, |state, cx| {
                    state.connected();
                    cx.notify();
                });
            }
            self.request_pages(cx);
        } else {
            self.presenter.disconnected();
            // The watch went with the session, so no hold is watched.
            self.voice.watched = None;
            self.voice.ended = None;
            self.scene_timer = None;
            self.scene_deadline = None;
            self.settings.update(cx, |state, cx| {
                state.accounts.disconnected();
                state.claude.disconnected();
                state.github.disconnected();
                state.key_paste.disconnected();
                state.disconnected();
                cx.notify();
            });
            self.sync_settings_notice(cx);
            self.runs.disconnected();
            self.sync_deletes(cx);
            for model in [&self.threads, &self.chat] {
                model.update(cx, |state, cx| {
                    state.disconnected();
                    cx.notify();
                });
            }
        }
        self.settle_github_interactive(cx);
        self.draw_changed(cx);
        diagnostics::record(cx, format_args!("link state={state:?}"));
        self.state = state;
        if let Err(error) = self.onboarding_changed(cx) {
            tracing::error!(%error, "can't show onboarding");
        }
        self.refresh_dock(cx);
        self.panel.update(cx, |panel, cx| {
            panel.state = state;
            cx.notify();
        });
    }
}
