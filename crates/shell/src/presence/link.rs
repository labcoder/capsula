//! Applying session-fenced link events to the two independent page lists and runs.

use super::{Context, OpenError, Presence, diagnostics};
use crate::link::{LinkEvent, LinkState};
use capsula_protocol::message::{ControlToBrain, DataToShell};

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
                            | DataToShell::Page { .. }) => {
                                self.threads.update(cx, |state, cx| {
                                    state.apply(message);
                                    cx.notify();
                                });
                                self.request_pages(cx);
                                bytes
                            }
                            message => self.runs.apply_data(message, bytes),
                        };
                        self.grant(credit);
                    }
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

    /// Grants the connection credit for data frames the UI has taken.
    fn grant(&self, credit: u32) {
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
        if state == LinkState::Connected {
            self.runs.connected();
            self.threads.update(cx, |state, cx| {
                state.connected();
                cx.notify();
            });
            self.request_pages(cx);
        } else {
            self.runs.disconnected();
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
