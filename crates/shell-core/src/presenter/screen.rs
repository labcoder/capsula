//! Screen scenes in the mirror. Their marks point at the screen as it was when a hold was
//! released, so one is shown only while the watch has seen nothing change since, and every one
//! goes as soon as something may have. The rules are the watch's (`watch.rs`); this file feeds
//! it and does the clearing.

use std::time::Instant;

use kumochi_protocol::message::{ControlToBrain, DataToShell, RunId, SceneHeader, SceneId};
use kumochi_scene::{
    Begin,
    record::{ScreenDisplay, Surface},
    screen,
    text::Finite,
};

use super::{Broken, Effect, Out, Presenter, watch::Watch};
use crate::input::TalkHeld;
use crate::platform::screen_watch::{Changed, Sources};

/// Whether `header` begins a screen scene.
pub(super) fn on_screen(header: &SceneHeader) -> bool {
    header.begin.surface == Surface::Screen
}

/// Whether an overlay covering `overlay` covers the display `begin`'s picture was taken of, so
/// the scene's view can be placed on it. `SceneView::set_screen` places it by the same rule.
fn placeable(begin: &Begin, overlay: Option<ScreenDisplay>) -> bool {
    begin
        .display
        .zip(begin.size)
        .zip(overlay)
        .is_some_and(|((display, size), overlay)| {
            screen::placement(display, size.map(Finite::get), overlay).is_some()
        })
}

/// The line for a shown screen scene that goes by a road with no word of its own: a successor
/// takes its place, the user clears it, the brain removes it, it fails or the session is lost.
pub(super) fn left() {
    tracing::info!(reason = "left", "screen marks cleared");
}

impl Presenter {
    /// Installs the platform's sources, once, before the first release. Without them no watch
    /// can run, so no screen scene is ever shown.
    pub fn watch_sources(&mut self, sources: Box<dyn Sources>) {
        debug_assert!(!self.listening, "sources are installed before a watch runs");
        self.sources = Some(sources);
    }

    /// Whether the platform's sources are installed. Without them no watch can run, so a
    /// release has nothing to start and nothing to keep.
    pub fn watches_screen(&self) -> bool {
        self.sources.is_some()
    }

    /// The keys of a hold came up at `now`, and its `ScreenRelease` is about to be written;
    /// `after` is the newest run named so far, and `held` the talk keys still down. The
    /// caller reports the release here first, so the sources listen before the brain can take
    /// the hold's picture, which it does only after it reads that message. A `ScreenRelease`
    /// that isn't sent after all is reported with [`Presenter::screen_dropped`].
    ///
    /// Any screen scene still up goes and the watch starts: the sources start again, on a bit
    /// of this watch's own, so nothing left of the watch before can set it. The hold's
    /// recognition has already stopped them, through [`Presenter::screen_changed`]; any still
    /// listening are stopped here first. If they can't start the watch has seen a change:
    /// nothing could say the screen stayed as it was.
    /// # Errors
    /// [`Broken`] when a frame held behind a cleared scene's admission can't be mirrored.
    pub fn screen_released(
        &mut self,
        after: RunId,
        held: TalkHeld,
        now: Instant,
    ) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        self.clear_screens("release", now, &mut out)?;
        self.watch.released(after, now);
        self.changed = Changed::default();
        let changed = self.changed.clone();
        let listening = std::mem::take(&mut self.listening);
        self.listening = self.sources.as_mut().is_some_and(|sources| {
            if listening {
                sources.stop();
            }
            sources.start(changed, held)
        });
        if !self.listening {
            self.watch.changed();
        }
        Ok(out.finish())
    }

    /// The released hold's run was named, at `now`. Until it is, the watch ends 30 seconds
    /// after the release, and a name that comes later than that names nothing.
    pub fn screen_run(&mut self, run: RunId, now: Instant) {
        self.watch.named(run, now);
        self.hush();
    }

    /// The released hold's screenshot wasn't sent, or the hold ended with no run.
    pub fn screen_dropped(&mut self) {
        self.watch.dropped();
        self.hush();
    }

    /// A run ended: its terminal `RunState` and its `RunEnd` are both in. A watch still
    /// expecting that run's scene ends, and a shown scene of that run goes 60 seconds from
    /// `now`.
    pub fn run_ended(&mut self, run: RunId, now: Instant) {
        self.watch.ended(run, now);
        self.hush();
    }

    /// A source woke the UI after setting the bit. The wake itself says nothing: it only has
    /// the bit read, so one that arrives after a newer release finds the bit clear and changes
    /// nothing.
    /// # Errors
    /// [`Broken`] when a frame held behind a cleared scene's admission can't be mirrored.
    pub fn source_woke(&mut self, now: Instant) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        self.heed(now, &mut out)?;
        Ok(out.finish())
    }

    /// The shell itself knows the screen is about to change: the next hold was recognized.
    /// Never a source's wake, which is [`Presenter::source_woke`].
    /// # Errors
    /// [`Broken`] when a frame held behind a cleared scene's admission can't be mirrored.
    pub fn screen_changed(&mut self, now: Instant) -> Result<Vec<Effect>, Broken> {
        self.changed.set();
        self.source_woke(now)
    }

    /// The display the overlay covers, in physical pixels, or none while it isn't known. Every
    /// screen scene's view is placed on it again. One that can't be, because its picture is of
    /// another display or of this one before it changed, counts as a changed screen.
    /// # Errors
    /// [`Broken`] when a frame held behind a cleared scene's admission can't be mirrored.
    pub fn set_display(
        &mut self,
        display: Option<ScreenDisplay>,
        now: Instant,
    ) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        if self.display == display {
            return Ok(Vec::new());
        }
        self.display = display;
        let mut placed = true;
        {
            let mut frames = self.frames.borrow_mut();
            for (id, scene) in self.scenes.iter_mut().filter(|(_, scene)| scene.screen) {
                placed &= scene.view.set_screen(scene.state.view().header(), display);
                frames.reposition(*id, &scene.view);
            }
        }
        if !placed {
            self.unplaced(now, &mut out)?;
        }
        Ok(out.finish())
    }

    /// The setting went off: every screen scene goes, and one that arrives later isn't shown.
    /// # Errors
    /// [`Broken`] when a frame held behind a cleared scene's admission can't be mirrored.
    pub fn screen_off(&mut self, now: Instant) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        self.clear_screens("off", now, &mut out)?;
        self.watch = Watch::default();
        self.hush();
        Ok(out.finish())
    }

    /// The run whose marks were skipped because the screen changed first, until the next
    /// release.
    pub fn marks_skipped(&self) -> Option<RunId> {
        self.watch.skipped()
    }

    /// The run whose screen marks are up.
    pub fn marks_shown(&self) -> Option<RunId> {
        self.watch.shown_run()
    }

    /// The run and exact scene owned by the visible screen marks.
    pub fn marks_owner(&self) -> Option<(RunId, SceneId)> {
        self.watch.shown_run().zip(self.watch.shown())
    }

    /// The bit and the limits, before a frame is activated or sampled and before a job's
    /// answer is used, so neither shows marks that a change or a limit has already taken. One
    /// read while no sources listen.
    pub(super) fn check_marks(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        if self.listening {
            self.heed(now, out)?;
            self.expire_marks(now, out)?;
        }
        Ok(())
    }

    /// Applies the bit, if it is set.
    pub(super) fn heed(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        if self.changed.take() {
            if self.watch.changed() {
                self.clear_screens("changed", now, out)?;
            }
            self.hush();
        }
        Ok(())
    }

    /// The watch's two limits as of `now`. The watchdog checks them, and so does every data
    /// frame before it is judged, so a first mark that arrives at a limit finds it applied.
    pub(super) fn expire_marks(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        if self.watch.expired(now) {
            self.clear_screens("limit", now, out)?;
        }
        self.hush();
        Ok(())
    }

    /// A screen scene can't be placed on the overlay's display, which counts as a changed
    /// screen: shown marks go, under the word `display`, and expected ones will be skipped.
    fn unplaced(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        if self.watch.changed() {
            self.clear_screens("display", now, out)?;
        }
        self.hush();
        Ok(())
    }

    /// Stops the sources once the watch has no more use for them.
    pub(super) fn hush(&mut self) {
        if self.listening && !self.watch.listening() {
            self.listening = false;
            if let Some(sources) = &mut self.sources {
                sources.stop();
            }
        }
    }

    /// Judges a screen scene's first mark as its frame arrives, in the pipe's order, so a
    /// change counts even while the scene's frames wait to be applied. A skipped scene goes
    /// whole: it never installs a step or paints, a begin still held never installs, and the
    /// brain hears `SceneSkipped`. A scene whose view can't be placed is skipped as well, and
    /// the marks that were up are cleared under the word `display`.
    pub(super) fn judge(
        &mut self,
        frame: &DataToShell,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        let DataToShell::SceneStepCommit { run, scene, .. } = *frame else {
            return Ok(());
        };
        // The scene's header, if it is a screen scene: mirrored, or begun by a frame still held.
        let begin = match self.scenes.get(&scene) {
            Some(mirrored) => mirrored.screen.then(|| mirrored.state.view().header()),
            None => self.held.iter().find_map(|(frame, _, _)| match &**frame {
                DataToShell::SceneBegin {
                    scene: begun,
                    header,
                } if *begun == scene && on_screen(header) => Some(&header.begin),
                _ => None,
            }),
        };
        let Some(begin) = begin else {
            return Ok(());
        };
        let placed = placeable(begin, self.display);
        if self.cleared.contains(&scene) || self.watch.shows(scene) {
            return Ok(());
        }
        // A scene that can't be placed counts as a changed screen. The marks that were up go
        // only once its own skip is settled: cleared first, it would be remembered as cleared
        // and the brain would hear no `SceneSkipped` for it.
        let unplaced = !placed && self.watch.changed();
        let displaced = self.watch.shown().is_some();
        if self.watch.marked(scene, run) {
            if displaced {
                left();
            }
            for other in self.screens() {
                if other != scene {
                    self.retire(other, out);
                    self.forget(other, out);
                }
            }
        } else {
            if placed {
                tracing::info!("screen marks skipped");
            } else {
                tracing::info!(reason = "display", "screen marks skipped");
            }
            self.retire(scene, out);
            self.forget_skipped(scene, out);
        }
        if unplaced {
            self.clear_screens("display", now, out)?;
        }
        self.drain(now, out)
    }

    /// Clears every screen scene as the shell's own clear, and lets the frames held behind one
    /// go on.
    fn clear_screens(
        &mut self,
        reason: &'static str,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        let screens = self.screens();
        if screens.is_empty() {
            return Ok(());
        }
        tracing::info!(reason, "screen marks cleared");
        for scene in screens {
            // It goes under `reason`, not as a scene that left by another road.
            self.watch.gone(scene);
            self.retire(scene, out);
            self.forget(scene, out);
        }
        self.drain(now, out)
    }

    /// Drops a whole screen scene, its own admission with it: the admission is cancelled, its
    /// credit goes back once, and its late answer finds no lane and drops.
    fn retire(&mut self, scene: SceneId, out: &mut Out) {
        self.cancel_admission(Some(scene), out);
        self.release(scene);
    }

    /// A screen scene skipped whole. The brain hears that the screen may have changed: not a
    /// failure the model could retry, and not a clear.
    fn forget_skipped(&mut self, scene: SceneId, out: &mut Out) {
        if !self.cleared.contains(&scene) {
            self.remember(scene);
            out.effects
                .push(Effect::Control(ControlToBrain::SceneSkipped { scene }));
        }
    }

    /// Every screen scene not cleared: mirrored, or begun by a frame still held.
    fn screens(&self) -> Vec<SceneId> {
        let held = self.held.iter().filter_map(|(frame, _, _)| match &**frame {
            DataToShell::SceneBegin { scene, header }
                if on_screen(header) && !self.cleared.contains(scene) =>
            {
                Some(*scene)
            }
            _ => None,
        });
        self.scenes
            .iter()
            .filter(|(_, scene)| scene.screen)
            .map(|(scene, _)| *scene)
            .chain(held)
            .collect()
    }
}
