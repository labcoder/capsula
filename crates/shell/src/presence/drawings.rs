//! UI-thread scene ownership and independent preparation watchdogs.

use std::time::Instant;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{Context, Window};
use kumochi_protocol::message::ControlToBrain;

use super::line::on_next_frame;
use super::{EscapePanel, OpenError, Presence};
use crate::presenter::{Broken, Completion, Effect, Measure};

impl Presence {
    pub(super) fn scene_effects(
        &mut self,
        result: Result<Vec<Effect>, Broken>,
        cx: &mut Context<'_, Self>,
    ) {
        if self.scene_effects_at(result, Instant::now(), cx) {
            self.want_frames(cx);
        }
    }

    /// Arms the drawing's frames on the dock's window, unless they're armed already. Peek,
    /// typing and Settings changes keep the dock shown, but the status panel hides whenever the
    /// peek closes or the typing panel or Settings is open, and Windows paints no hidden window:
    /// frames armed there would wait, leaving a drawing at its first sample.
    pub(super) fn want_frames(&mut self, cx: &mut Context<'_, Self>) {
        if !self.frames.want(Instant::now()) {
            return;
        }
        // shortcut: a dock that fails closed (presence.md) hides with these frames armed, so
        // drawings wait for a position choice to show it; stop or clear them, or move their
        // loop, once drawings must keep moving through that failure.
        let owner = cx.weak_entity();
        let armed = self
            .dock_window
            .update(cx, |_, window, _| on_next_frame(window, owner, Self::frame));
        if let Err(error) = armed {
            self.frames.idle();
            tracing::warn!(%error, "can't animate the drawing");
        }
    }

    /// One frame callback: the pacer first, so a skipped frame does no animation work.
    fn frame(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Err(error) = self.frame_at(Instant::now(), window, cx) {
            tracing::error!(%error, "scene frame update failed");
        }
    }

    pub(super) fn frame_at(
        &mut self,
        now: Instant,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let mut updated = Ok(());
        let cap = self
            .settings
            .read(cx)
            .view()
            .map(|view| view.max_frame_rate)
            .unwrap_or_default();
        let wanted = if self
            .frames
            .frame(now, cap, window.refresh_interval(cx))
            .is_none()
        {
            true
        } else {
            let scenes = self.draw_changed_at(now, cx);
            let panel = if self.text_open() {
                EscapePanel::Open
            } else {
                EscapePanel::Closed
            };
            let activity = Self::activity_at(&self.runs, panel, &self.presenter, now);
            if self.activity.working != activity.working
                || self.activity.drawing != activity.drawing
            {
                updated = self.runs_changed_at(now, cx);
            }
            scenes && updated.is_ok()
        };
        if wanted {
            on_next_frame(window, cx.weak_entity(), Self::frame);
        } else {
            self.frames.idle();
        }
        updated
    }

    pub(super) fn scene_effects_at(
        &mut self,
        result: Result<Vec<Effect>, Broken>,
        now: Instant,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let effects = match result {
            Ok(effects) => effects,
            Err(error) => {
                tracing::warn!(%error, "invalid scene stream; ending the connection");
                self.end_scene_connection();
                return self.draw_changed_at(now, cx);
            }
        };
        for effect in effects {
            match effect {
                Effect::Credit(bytes) => self.grant(bytes),
                Effect::Control(message) => {
                    let sent = self
                        .session
                        .clone()
                        .is_some_and(|session| self.send_stop(&session, message, cx));
                    if !sent {
                        // A refused control already ended the session (`send_stop`), off this thread.
                        tracing::warn!("scene control queue unavailable; ending the connection");
                        self.forget_scene_connection();
                        return self.draw_changed_at(now, cx);
                    }
                }
                Effect::Prepare(job) => {
                    Self::scene_worker(move |measure| job.run(measure), cx);
                }
                Effect::Recover(job) => {
                    Self::scene_worker(move |measure| job.run(measure), cx);
                }
                Effect::Admit(job) => {
                    Self::scene_worker(move |_| job.run(), cx);
                }
                Effect::ConsumptionReady { scene, index } => {
                    self.place_drawings(cx.weak_entity(), cx);
                    if let Some(session) = &self.session {
                        let effects = self.presenter.consumption_ready(
                            session.generation(),
                            scene,
                            index,
                            now,
                        );
                        self.scene_effects_at(effects, now, cx);
                        if self.session.is_none() {
                            return false;
                        }
                    }
                }
            }
        }
        self.draw_changed_at(now, cx)
    }

    fn scene_worker(
        run: impl FnOnce(&mut Measure) -> Completion + Send + 'static,
        cx: &mut Context<'_, Self>,
    ) {
        let text = std::sync::Arc::clone(cx.text_system());
        let family = cx.theme().font_family.clone();
        let task = cx
            .background_executor()
            .spawn(async move { run(&mut Measure::new(text, family)) });
        cx.spawn(async move |this, cx| {
            let done = task.await;
            let result = this.update(cx, |this, cx| {
                this.place_drawings(cx.weak_entity(), cx);
                let effects = this.presenter.completed(done, Instant::now());
                let ready = this.presenter.take_ready();
                this.scene_effects(
                    effects.map(|effects| ready.into_iter().chain(effects).collect()),
                    cx,
                );
                this.runs_changed(cx)
            });
            if let Ok(Err(error)) = result {
                tracing::error!(%error, "scene completion update failed");
            }
        })
        .detach();
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "whether the scene is moving, as the frame reports it"
    )]
    pub(super) fn scene_watchdog(
        &mut self,
        now: Instant,
        moving: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let wake = (!moving).then(|| self.presenter.next_wake(now)).flatten();
        let deadline = self.presenter.deadline().into_iter().chain(wake).min();
        if self.scene_deadline == deadline {
            return;
        }
        self.scene_timer = None;
        self.scene_deadline = deadline;
        let Some(deadline) = deadline else {
            return;
        };
        let generation = self.session.as_ref().map(crate::link::Session::generation);
        self.scene_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(deadline.saturating_duration_since(Instant::now()))
                .await;
            let result = this.update(cx, |this, cx| {
                if this.scene_deadline != Some(deadline)
                    || this.session.as_ref().map(crate::link::Session::generation) != generation
                {
                    return Ok(());
                }
                this.scene_timer = None;
                this.scene_deadline = None;
                let effects = this.presenter.tick(Instant::now());
                this.scene_effects(effects, cx);
                this.runs_changed(cx)
            });
            if let Ok(Err(error)) = result {
                tracing::error!(%error, "scene watchdog update failed");
            }
        }));
    }

    pub(super) fn end_scene_connection(&mut self) {
        if let Some(session) = &self.session {
            self.link.end(session);
        }
        self.forget_scene_connection();
    }

    /// Forgets the session and its drawings at once, without ending it.
    fn forget_scene_connection(&mut self) {
        self.session = None;
        self.presenter.disconnected();
        self.scene_timer = None;
        self.scene_deadline = None;
    }

    pub(super) fn stop_run(&mut self, message: ControlToBrain, cx: &mut Context<'_, Self>) {
        let now = Instant::now();
        if let ControlToBrain::Stop { run, .. } = message
            && (self.presenter.holds_scenes() || self.presenter.live_work(now))
        {
            let run = self
                .presenter
                .stoppable_run()
                .map_or(run, |live| live.max(run));
            self.place_drawings(cx.weak_entity(), cx);
            let effects = self.presenter.stop(run, now);
            self.scene_effects(effects, cx);
        } else {
            self.tell(message, cx);
        }
    }
}
