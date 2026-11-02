//! Applying session-fenced link events to the two independent page lists and runs.

use super::{Context, OpenError, Presence, diagnostics};
use crate::drawing::{Broken, Chunk, Effect};
use crate::link::{LinkEvent, LinkState};
use kumochi_protocol::message::{ControlToBrain, DataToShell};

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
                LinkEvent::RunState { run, thread, phase } => {
                    let applied = self.runs.apply_state(run, thread, phase);
                    diagnostics::record(cx, format_args!("run phase={phase:?}"));
                    if let Some(stop) = applied.stop {
                        self.tell(stop);
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
                } => {
                    if self
                        .session
                        .as_ref()
                        .is_some_and(|session| session.generation() == generation)
                    {
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
                            DataToShell::ScenesRemoved { scenes } => {
                                bytes.saturating_add(self.runs.remove_scenes(&scenes))
                            }
                            message => self.runs.apply_data(message, bytes),
                        };
                        self.grant(credit);
                    }
                }
                LinkEvent::Scenes(scenes) => match self.runs.apply_scenes(&scenes) {
                    Ok(effect) => {
                        self.effect(effect);
                        self.send_deletes(cx);
                    }
                    Err(error) => self.end(&error),
                },
                LinkEvent::ThreadDeleted {
                    thread,
                    outcome,
                    last_run,
                } => self.thread_deleted(thread, outcome, last_run, cx),
                LinkEvent::TurnRefused => {
                    self.runs.refused();
                    self.sync_deletes(cx);
                }
            }
        }
        self.runs_changed(cx)
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
            self.threads.update(cx, |state, cx| {
                state.connected();
                cx.notify();
            });
            self.request_pages(cx);
        } else {
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
