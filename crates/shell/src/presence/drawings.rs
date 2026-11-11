//! UI-thread scene ownership and independent preparation watchdogs.

use std::time::Instant;

use gpui_kit::Context;
use gpui_kit::component::ActiveTheme as _;
use kumochi_protocol::message::ControlToBrain;

use super::Presence;
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
                        .as_ref()
                        .is_some_and(|session| self.link.send_control(session, message));
                    if !sent {
                        tracing::warn!("scene control queue unavailable; ending the connection");
                        self.end_scene_connection();
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
                let effects = this.presenter.completed(done, Instant::now());
                this.scene_effects(effects, cx);
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
        if let Some(session) = self.session.take() {
            self.link.end(&session);
        }
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
            let effects = self.presenter.stop(run, now);
            self.scene_effects(effects, cx);
        } else {
            self.tell(message);
        }
    }
}
