//! Where the presence tells the presenter's screen watch what became of a hold that sends the
//! screen. The watch decides what is shown and when it goes (`docs/development/scene.md`);
//! every road a hold or its run can take reports here. Every report but the release can come
//! twice and changes nothing the second time; the release is made once, immediately before its
//! `ScreenRelease` is written.

use std::time::Instant;

use kumochi_protocol::message::{GestureId, RunId, RunOrigin};
use kumochi_scene::record::ScreenDisplay;

use super::{Context, Presence};
use crate::input::TalkHeld;
use crate::presenter::{Broken, Effect};
use crate::strings::Text;

/// The hold whose release the watch was last told of, until that watch is dropped or the next
/// release takes its place. Kept apart from `voice.pending`, which is gone by `Answering`
/// while the brain's word on the screenshot can still come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Watched {
    gesture: GestureId,
    /// Later run states carry no origin, so they match by the run the first one named.
    run: Option<RunId>,
}

impl Presence {
    /// Routes what the watch answered. With nothing cleared only the watch's own limit may
    /// have moved, and the drawing is left as it is: a report from the middle of a hold must
    /// not refresh it.
    fn marks_effects(&mut self, effects: Result<Vec<Effect>, Broken>, cx: &mut Context<'_, Self>) {
        if effects.as_ref().is_ok_and(Vec::is_empty) {
            self.marks_limit(cx);
        } else {
            self.scene_effects(effects, cx);
        }
    }

    /// Has the scene watchdog keep the watch's limits as they are now.
    fn marks_limit(&mut self, cx: &mut Context<'_, Self>) {
        let now = Instant::now();
        let moving = self.presenter.animating(now);
        self.scene_watchdog(now, moving, cx);
    }

    /// The screen is about to change, or the user wants it clear: a hold was recognized, or
    /// Esc was pressed. Marks that are up go, and marks still expected will be skipped.
    pub(super) fn marks_changed(&mut self, cx: &mut Context<'_, Self>) {
        let effects = self.presenter.screen_changed(Instant::now());
        self.marks_effects(effects, cx);
    }

    /// `gesture`'s keys came up and its `ScreenRelease` is about to be written; `held` is the
    /// talk keys still down. The watch hears of it first, so its sources listen before the
    /// brain can take the picture, which it does only once it reads that message.
    pub(super) fn marks_released(
        &mut self,
        gesture: GestureId,
        held: TalkHeld,
        cx: &mut Context<'_, Self>,
    ) {
        // No shipped build installs sources, and without them no watch can run: a screen
        // scene that arrives all the same finds none and is skipped. So such a release reads
        // no display, starts no watch and sets no watchdog.
        if !self.presenter.watches_screen() {
            return;
        }
        self.show_display(cx);
        let after = self.runs.newest_run();
        let effects = self.presenter.screen_released(after, held, Instant::now());
        self.voice.watched = Some(Watched { gesture, run: None });
        self.marks_effects(effects, cx);
    }

    /// `gesture`'s `ScreenRelease` or its screenshot wasn't sent, or its hold ended with no
    /// run. Only the hold the watch was told of drops it, once.
    pub(super) fn marks_dropped(&mut self, gesture: GestureId, cx: &mut Context<'_, Self>) {
        if self
            .voice
            .watched
            .take_if(|watched| watched.gesture == gesture)
            .is_some()
        {
            self.presenter.screen_dropped();
            self.marks_limit(cx);
        }
    }

    /// A `RunState` of `run` was applied. The watched hold's first state names its run to the
    /// watch, and a state of that run that Esc or a delete has fenced off drops the watch.
    pub(super) fn marks_run(
        &mut self,
        run: RunId,
        origin: Option<RunOrigin>,
        applied: &crate::runs::Applied,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(watched) = &mut self.voice.watched {
            let first = watched.run.is_none()
                && origin
                    == Some(RunOrigin::Hold {
                        gesture: watched.gesture,
                    });
            if !applied.visible && (first || watched.run == Some(run)) {
                let gesture = watched.gesture;
                self.marks_dropped(gesture, cx);
            } else if first {
                watched.run = Some(run);
                self.presenter.screen_run(run, Instant::now());
                self.marks_limit(cx);
            }
        }
        self.marks_ended(cx);
    }

    /// Tells the watch of a run whose terminal state and `RunEnd` are now both in.
    pub(super) fn marks_ended(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(run) = self.runs.take_ended() {
            self.presenter.run_ended(run, Instant::now());
            // Shown marks now have a time limit.
            self.marks_limit(cx);
        }
    }

    /// The setting went off: marks that are up go, and none is shown for a hold already
    /// released.
    pub(super) fn marks_off(&mut self, cx: &mut Context<'_, Self>) {
        let effects = self.presenter.screen_off(Instant::now());
        self.marks_effects(effects, cx);
    }

    /// Tells the presenter which display the overlay covers, as a hold's line reads it, so a
    /// screen scene is placed only on the display its picture was taken of.
    pub(super) fn show_display(&mut self, cx: &mut Context<'_, Self>) {
        let display = self.line_display(cx).map(|(display, scale)| ScreenDisplay {
            origin: display.origin,
            size: display.size,
            dpi: super::scaled(96.0, scale).unsigned_abs(),
        });
        let effects = self.presenter.set_display(display, Instant::now());
        self.marks_effects(effects, cx);
    }

    /// The dock's line while the marks of the answer on screen were skipped.
    pub(super) fn marks_line(&self) -> Option<Text> {
        let skipped = self.presenter.marks_skipped()?;
        self.runs
            .shown()
            .is_some_and(|shown| shown.run == skipped)
            .then_some(Text::MarksSkipped)
    }
}
