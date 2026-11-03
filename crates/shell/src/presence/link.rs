//! Applying session-fenced link events to the two independent page lists and runs.

use super::{Context, OpenError, Presence, diagnostics};
use crate::drawing::{Broken, Chunk, Effect};
use crate::link::{LinkEvent, LinkState};
use crate::settings::Update;
use kumochi_protocol::message::{ControlToBrain, DataToShell, SettingsView, UserError};
use std::time::Instant;

impl Presence {
    /// Applies what the link reported, in order, then follows the runs once.
    pub(super) fn link_events(
        &mut self,
        events: Vec<LinkEvent>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        for event in events {
            match event {
                LinkEvent::State(state) => {
                    self.session = None;
                    self.link_state(state, cx);
                }
                LinkEvent::Connected(session) => {
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
                LinkEvent::RunState { run, thread, phase } => {
                    let applied = self.runs.apply_state(run, thread, phase);
                    diagnostics::record(cx, format_args!("run phase={phase:?}"));
                    if let Some(stop) = applied.stop {
                        self.stop_run(stop, cx);
                    }
                    self.grant(applied.credit);
                    self.threads.update(cx, |state, cx| {
                        state.run_state(thread);
                        cx.notify();
                    });
                    self.sync_deletes(cx);
                    self.request_pages(cx);
                }
                LinkEvent::Data {
                    generation,
                    message,
                    bytes,
                } => self.data_received(generation, message, bytes, cx),
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
                LinkEvent::TurnRefused => {
                    self.runs.refused();
                    self.sync_deletes(cx);
                }
                LinkEvent::Error(error) => self.show_error(&error, cx),
            }
        }
        self.runs_changed(cx)
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
            if matches!(
                &message,
                DataToShell::SceneSnapshotV1 { .. }
                    | DataToShell::SceneBegin { .. }
                    | DataToShell::SceneStepCommit { .. }
                    | DataToShell::SceneCutApplied { .. }
                    | DataToShell::SceneEnd { .. }
                    | DataToShell::ScenesRemoved { .. }
            ) {
                if let DataToShell::ScenesRemoved { scenes } = &message {
                    let credit = self.runs.remove_scenes(scenes);
                    self.grant(credit);
                }
                let effects = self
                    .presenter
                    .data(generation, message, bytes, Instant::now());
                self.scene_effects(effects, cx);
                return;
            }
            let credit = match message {
                message @ (DataToShell::ThreadList { .. }
                | DataToShell::Page { .. }
                | DataToShell::PageFailed { .. }) => {
                    let credit = self.threads.update(cx, |state, cx| {
                        cx.notify();
                        state.receive(message, bytes)
                    });
                    self.request_pages(cx);
                    credit
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
        let applied = self.settings.update(cx, |state, cx| {
            let applied = state.receive(current, generation, view.clone(), update);
            if applied {
                cx.notify();
            }
            applied
        });
        if !applied {
            return;
        }
        self.runs.settings(view.main_thread);
        self.clear_refusal(cx);
        self.sync_settings_notice(cx);
        self.threads.update(cx, |state, cx| {
            state.set_settings(view);
            cx.notify();
        });
    }

    /// Shows the brain's error in both panels and on the connected dock.
    fn show_error(&mut self, error: &UserError, cx: &mut Context<'_, Self>) {
        self.settings.update(cx, |state, cx| {
            let _field = state.error(error.clone());
            cx.notify();
        });
        self.sync_settings_notice(cx);
    }

    /// Queues a `Stop` or `ClearDrawing` for the brain; its local effect has already happened.
    pub(super) fn tell(&self, message: ControlToBrain) {
        let sent = self
            .session
            .as_ref()
            .is_some_and(|session| self.link.send_control(session, message));
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
            self.threads.update(cx, |state, cx| {
                state.connected();
                cx.notify();
            });
            self.request_pages(cx);
        } else {
            self.presenter.disconnected();
            self.scene_timer = None;
            self.scene_deadline = None;
            self.settings.update(cx, |state, cx| {
                state.disconnected();
                cx.notify();
            });
            self.sync_settings_notice(cx);
            self.runs.disconnected();
            self.sync_deletes(cx);
            self.threads.update(cx, |state, cx| {
                state.disconnected();
                cx.notify();
            });
        }
        diagnostics::record(cx, format_args!("link state={state:?}"));
        self.state = state;
        // Listening keeps its label until the gesture ends.
        self.refresh_dock(cx);
        self.panel.update(cx, |panel, cx| {
            panel.state = state;
            cx.notify();
        });
    }
}
