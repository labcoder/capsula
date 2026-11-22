//! Applying session-fenced link events to browsed and focused histories and runs.

#[cfg(test)]
mod github_tests;

use super::{Context, OpenError, Presence, Session, diagnostics};
use crate::chat_notice::Source;
use crate::drawing::{Broken, Chunk, Effect};
use crate::link::{Batch, LinkEvent, LinkState};
use crate::settings::Update;
use kumochi_protocol::message::{
    AccountView, AudioSession, ConnectionsView, ControlToBrain, DataToShell, HeldScreen, Phase,
    RunId, RunOrigin, ScreenHolds, ScreenOff, SettingsView, ThreadId, UserError,
};
use kumochi_scene::deferred::Deferred;
use kumochi_shell_core::link::output::Denied;
use std::time::{Duration, Instant};

/// How many times a speaker `Stop` is offered to a busy owner before its session ends: a
/// policy, not a measured bound.
const STOP_TRIES: u8 = 10;
/// The wait between those offers.
const STOP_RETRY: Duration = Duration::from_millis(1);

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
                LinkEvent::NarrationReady(wake) => {
                    if !self
                        .session
                        .as_ref()
                        .is_some_and(|session| session.accepts_narration(wake))
                    {
                        continue;
                    }
                    self.scene_effects(Ok(Vec::new()), cx);
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
                    // A refused capture has no screenshot to mark: its own hold's watch ends.
                    if let kumochi_protocol::message::CaptureFor::Gesture(gesture) = capture_for
                        && self
                            .session
                            .as_ref()
                            .is_some_and(|session| session.generation() == generation)
                    {
                        self.marks_dropped(gesture, cx);
                    }
                }
                LinkEvent::ScreenHolds { state, .. } => {
                    let off = matches!(state, ScreenHolds::Off(ScreenOff::TurnedOff));
                    self.runs.set_screen_holds(state);
                    // The Privacy tab's help follows the offer's route.
                    self.settings.update(cx, |_, cx| cx.notify());
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
                        self.effect(effect, cx);
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
        self.follow_voice_input(cx);
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
                DataToShell::EventsChanged { thread } => {
                    for model in [&self.threads, &self.chat] {
                        model.update(cx, |state, cx| {
                            state.events_changed(thread);
                            cx.notify();
                        });
                    }
                    self.request_pages(cx);
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
        #[cfg(any(debug_assertions, all(windows, feature = "gate-draw-replay")))]
        if let Some(session) = &self.session
            && let Some(draft) = super::Observer::take_draw_fixture(cx)
            && let Err(refusal) = self
                .runs
                .submit(draft, |turn| self.link.send_data(session, turn))
        {
            tracing::warn!(?refusal, "drawing timing fixture submission refused");
        }
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

    /// Queues a stop or scene control for the brain. A run's `Stop` or `StopScenes` then stops its
    /// speech, and only once the brain's is queued, so speech never stops for a stop the brain
    /// wasn't told. Any refused one ends the session instead ([`Presence::end_session`]), once.
    /// The speaker's `Stop` waits behind any earlier one a busy owner hasn't taken.
    pub(super) fn send_stop(
        &mut self,
        session: &Session,
        message: ControlToBrain,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let run = match &message {
            ControlToBrain::Stop { run, .. } | ControlToBrain::StopScenes { run, .. } => Some(*run),
            _ => None,
        };
        let sent = self.link.send_control(session, message);
        // A control the brain wasn't told can't be undone locally, so that session ends; one
        // already ending isn't ended again.
        if !sent && session.open() {
            let end = self.end_session(session, cx);
            // Its speaker got no stop, so no resume may reach it before that end has run.
            let held = self.voice.gates.hold_back();
            cx.spawn(async move |_, _| {
                end.await;
                drop(held);
            })
            .detach();
        }
        if sent && let Some(run) = run {
            self.stop_speaker(session.clone(), run, cx);
        }
        sent
    }

    /// Offers `run`'s speaker `Stop` on `session` behind any earlier one not taken yet.
    pub(super) fn stop_speaker(
        &mut self,
        session: Session,
        run: kumochi_protocol::message::RunId,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending_stops
            .push_back((session, run, STOP_TRIES, false));
        if self.stop_retry.is_none() {
            self.drain_stops(cx);
        }
    }

    /// Offers `run`'s speaker a local `Stop` behind any earlier one; no outcome of it ends the session.
    pub(super) fn stop_speech_locally(
        &mut self,
        session: Session,
        run: kumochi_protocol::message::RunId,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending_stops
            .push_back((session, run, STOP_TRIES, true));
        if self.stop_retry.is_none() {
            self.drain_stops(cx);
        }
    }

    /// Offers the pending speaker stops to the owner in order. A busy owner is offered the front
    /// one again after [`STOP_RETRY`], with later ones behind it. Once a brain-told one's tries run
    /// out, its session ends ([`Presence::end_session`]), and the queue waits for that end.
    pub(super) fn drain_stops(&mut self, cx: &mut Context<'_, Self>) {
        while let Some((session, run, tries, local)) = self.pending_stops.front_mut() {
            let output = self.link.output();
            let stopped = if *local {
                output.stop_locally(session.generation(), *run)
            } else {
                output.stop(session.generation(), *run)
            };
            let task = match stopped {
                Err(Denied::Busy) if *tries > 1 => {
                    *tries -= 1;
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(STOP_RETRY).await;
                        Self::after_stop(&this, cx);
                    })
                }
                // Audio the brain ended or replaced: no outcome of its local stop ends the pair.
                Err(_) if *local => {
                    self.pending_stops.pop_front();
                    continue;
                }
                Err(reason)
                    if reason == Denied::Busy
                        || reason == Denied::Lost
                            && self.link.audio_session() == AudioSession::QueuedRequired =>
                {
                    *tries = 0;
                    let session = session.clone();
                    let end = self.end_session(&session, cx);
                    cx.spawn(async move |this, cx| {
                        end.await;
                        Self::after_stop(&this, cx);
                    })
                }
                // Queued, or its session is gone or ending already.
                Ok(()) | Err(_) => {
                    self.pending_stops.pop_front();
                    continue;
                }
            };
            self.stop_retry = Some(task);
            return;
        }
        // A retirement waiting on these stops may resume now.
        self.voice.gates.poke();
    }

    /// Ends `session` without waiting: nothing more is sent on it from now, and the pipe abort and
    /// the owner's barrier, which takes the owner's lock, finish on the background executor.
    pub(super) fn end_session(
        &self,
        session: &Session,
        cx: &mut Context<'_, Self>,
    ) -> gpui_kit::Task<()> {
        session.close();
        let (link, session) = (self.link.clone(), session.clone());
        cx.background_executor()
            .spawn(async move { link.end(&session) })
    }

    /// Goes on with the pending stops, past a front one with no tries left, whose session has
    /// ended.
    fn after_stop(this: &gpui_kit::WeakEntity<Self>, cx: &mut gpui_kit::AsyncApp) {
        let updated = this.update(cx, |this, cx| {
            this.stop_retry = None;
            if this
                .pending_stops
                .front()
                .is_some_and(|(_, _, tries, _)| *tries == 0)
            {
                this.pending_stops.pop_front();
            }
            this.drain_stops(cx);
        });
        if updated.is_err() {
            tracing::debug!("presence closed with a speaker stop pending");
        }
    }

    /// Queues a `Stop` or `ClearDrawing` for the brain; its local effect has already happened.
    pub(super) fn tell(&mut self, message: ControlToBrain, cx: &mut Context<'_, Self>) {
        let sent = self
            .session
            .clone()
            .is_some_and(|session| self.send_stop(&session, message, cx));
        if !sent {
            tracing::warn!("the brain isn't connected or isn't reading; nothing to tell");
        }
    }

    /// Grants a drawing change's credit, then queues its `ClearDrawing`.
    pub(super) fn effect(&mut self, effect: Effect, cx: &mut Context<'_, Self>) {
        self.grant(effect.credit);
        if let Some(clear) = effect.clear {
            self.tell(clear, cx);
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
            session.attach_narration(&mut self.presenter);
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
