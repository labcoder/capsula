//! Applying session-fenced link events to browsed and focused histories and runs.

#[cfg(test)]
mod dock_tests;
#[cfg(test)]
mod github_tests;

use super::{Context, OpenError, Presence, Session, diagnostics};
use crate::accounts::Receipt;
use crate::chat_notice::Source;
use crate::drawing::{Broken, Chunk, Effect};
use crate::link::policy::{self, GitHub, Models, Offered, RunStop, STOP_RETRY, Sent, Told};
use crate::link::{Batch, LinkEvent, LinkState};
use crate::settings::Update;
use kumochi_protocol::message::{
    ConnectionsView, ControlToBrain, DataToShell, HeldScreen, Phase, RunId, RunOrigin, ScreenHolds,
    ScreenOff, SettingsView, ThreadId, UserError,
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
            if event.stale(self.session.as_ref().map(Session::generation)) {
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
                        state.github.connected(session.generation());
                        state.key_paste.disconnected();
                        cx.notify();
                    });
                    self.revoke_exclusion(cx);
                    if let Some(font) = session.drawing_font_family() {
                        crate::presenter::PairFont::pin(font, cx);
                    }
                    self.session = Some(session);
                    self.link_state(LinkState::Connected, cx);
                }
                #[cfg(debug_assertions)]
                LinkEvent::FixtureEngineActive { generation } => {
                    if self
                        .session
                        .as_ref()
                        .is_some_and(|session| session.generation() == generation && session.open())
                    {
                        self.chat.update(cx, |chat, cx| {
                            chat.fixture_engine = true;
                            cx.notify();
                        });
                    }
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
                LinkEvent::SpokenRepliesDefaulted { generation, view } => {
                    self.settings_changed(generation, view, Update::SpokenRepliesDefaulted, cx);
                }
                LinkEvent::Voice { generation, view } => {
                    let current = self.session.as_ref().map(crate::link::Session::generation);
                    self.settings.update(cx, |state, cx| {
                        if state.receive_voice(current, generation, view) {
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
                        self.issue_unavailable(request, draft, kind, cx);
                    }
                }
                LinkEvent::Focus {
                    generation,
                    run,
                    thread,
                } => self.focus(generation, run, thread, cx),
                snapshot @ (LinkEvent::Account { generation, .. }
                | LinkEvent::AccountUnsupported { generation, .. }) => {
                    self.account_changed(generation, snapshot, cx);
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
        let (mut applied, notice) = self.runs.apply_run_state(run, thread, phase, error, origin);
        self.marks_run(run, origin, &applied, cx);
        self.voice_run(run, thread, origin, phase, &applied, cx);
        diagnostics::record(cx, format_args!("run phase={phase:?}"));
        if let Some(restore) = applied.restore.take()
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
        if let Some(stop) = applied.stop.take() {
            match RunStop::owed(origin, stop) {
                RunStop::Hold { run, revealed } => {
                    let effects =
                        policy::hold_run_stop(&mut self.presenter, run, revealed, Instant::now());
                    self.scene_effects(effects, cx);
                }
                RunStop::Ordinary(stop) => self.stop_run(stop, cx),
            }
        }
        self.grant(applied.credit);
        if policy::settles(&self.runs, run, phase, &applied) {
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
        let current = self.session.as_ref().map(Session::generation);
        if policy::focus(&mut self.runs, current, (generation, run, thread)) {
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

    fn account_changed(
        &mut self,
        generation: u64,
        snapshot: LinkEvent,
        cx: &mut Context<'_, Self>,
    ) {
        let current = self.session.as_ref().map(crate::link::Session::generation);
        let previous = self.settings.read(cx).accounts.generation;
        let receipt = self.settings.update(cx, |state, cx| {
            let receipt = state.accounts.receive_account(current, snapshot);
            if receipt != Receipt::Stale {
                cx.notify();
            }
            receipt
        });
        if receipt != Receipt::Stale {
            self.account_seen(generation, previous, cx);
        }
        if receipt == Receipt::Recovered {
            self.chat_notice.recovered();
            if self
                .settings
                .update(cx, |state, _| state.clear_renewal_notice())
            {
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
        let current = self.session.as_ref().map(Session::generation);
        let disposition = policy::scene_disposition(&self.runs, current, generation, &frame);
        if current == Some(generation) {
            self.place_drawings(cx.weak_entity(), cx);
        }
        // Placing can end the pair, so membership reads it again.
        let current = self.session.as_ref().map(Session::generation);
        let credit = policy::removed_scenes(&mut self.runs, current, generation, &frame);
        self.grant(credit);
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
                self.voice.state.input_ended(*run);
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
                    self.voice.state.words(generation, gesture, words);
                    bytes
                }
                DataToShell::HoldStored { gesture } => {
                    self.voice.state.stored(generation, gesture);
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
        let pair = self.session.as_ref().map(|session| (&self.link, session));
        let applied = self.settings.update(cx, |state, cx| {
            let applied = state.receive_and_replay_dock(pair, generation, view.clone(), update);
            if applied {
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
        self.restore_dock_position(self.settings.read(cx).dock_position_choice(), cx);
        if let Err(error) = self.onboarding_changed(cx) {
            tracing::error!(%error, "can't show onboarding");
        }
    }

    /// Shows the brain's error in both panels and on the connected dock.
    fn show_error(&mut self, error: &UserError, source: Source, cx: &mut Context<'_, Self>) {
        self.chatgpt_sign_in_failed(error);
        let (source, dock_restore) = self.settings.update(cx, |state, cx| {
            let outcome = state.link_error(error, source);
            cx.notify();
            outcome
        });
        self.chat_notice.present(error.clone(), source);
        self.sync_settings_notice(cx);
        self.sync_taskbar(cx);
        self.restore_dock_position(dock_restore, cx);
    }

    /// Queues a stop or scene control for the brain. A queued run's refused Stop still stops
    /// its local speech before closing the pair. A successful Stop keeps the ordered speaker
    /// retry path.
    pub(super) fn send_stop(
        &mut self,
        session: &Session,
        message: ControlToBrain,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let sent = Sent::send(
            message,
            |message| self.link.send_control(session, message),
            || session.open(),
        );
        match sent {
            Sent::Seal(playback) => {
                if let Some(run) = playback {
                    session.stop_playback(run);
                }
                let end = self.end_session(session, cx);
                let held = self.voice.gates.hold_back();
                cx.spawn(async move |_, _| {
                    end.await;
                    drop(held);
                })
                .detach();
            }
            Sent::StopSpeaker(run) => self.stop_speaker(session.clone(), run, cx),
            Sent::Told | Sent::Refused => {}
        }
        sent.told()
    }

    pub(super) fn stop_speaker(
        &mut self,
        session: Session,
        run: RunId,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending_stops.push(session, run, Told::Brain);
        if self.stop_retry.is_none() {
            self.drain_stops(cx);
        }
    }

    /// A terminal run's audio can stop without sending a new stop to its brain.
    pub(super) fn stop_speech_locally(
        &mut self,
        session: Session,
        run: RunId,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending_stops.push(session, run, Told::SpeakerOnly);
        if self.stop_retry.is_none() {
            self.drain_stops(cx);
        }
    }

    /// Offers the pending speaker stops to the owner in the order `SpeakerStops` keeps, waiting
    /// [`STOP_RETRY`] for a busy owner or for a pair's end.
    pub(super) fn drain_stops(&mut self, cx: &mut Context<'_, Self>) {
        while let Some((session, run, told)) = self.pending_stops.front() {
            let output = self.link.output();
            let stopped = match told {
                Told::SpeakerOnly => output.stop_locally(session.generation(), run),
                Told::Brain => output.stop(session.generation(), run),
            };
            let link = &self.link;
            let task = match self.pending_stops.offered(stopped, || link.audio_session()) {
                Offered::Next => continue,
                Offered::Retry { playback } => {
                    if let Some((session, run)) = playback {
                        session.stop_playback(run);
                    }
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(STOP_RETRY).await;
                        Self::after_stop(&this, cx);
                    })
                }
                Offered::End(session) => {
                    let session = session.clone();
                    let end = self.end_session(&session, cx);
                    cx.spawn(async move |this, cx| {
                        end.await;
                        Self::after_stop(&this, cx);
                    })
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
            this.pending_stops.resume();
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
        let lead = state.lead();
        if lead.clears_refusal {
            self.clear_refusal(cx);
        }
        if lead.retry {
            self.native.enable_retry();
        } else {
            self.native.disable_retry();
        }
        let models = state.models(self.session.as_ref());
        if let Models::Up(session) = models {
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
            self.voice.state.ended = None;
            self.scene_timer = None;
            self.scene_deadline = None;
            self.settings.update(cx, |model, cx| {
                model.accounts.disconnected();
                model.claude.disconnected();
                if let Models::Down(GitHub::Connecting) = models {
                    model.github.connecting();
                } else {
                    model.github.disconnected();
                }
                model.key_paste.disconnected();
                model.disconnected();
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
