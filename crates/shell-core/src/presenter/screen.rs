//! Screen scenes in the mirror. Their marks point at the screen as it was when a hold was
//! released, so one is shown only while the watch has seen nothing change since, and every one
//! goes as soon as something may have. The rules are the watch's (`watch.rs`); this file feeds
//! it and does the clearing.

use std::time::Instant;

use kumochi_protocol::message::{DataToShell, RunId, SceneHeader, SceneId};
use kumochi_scene::record::Surface;

use super::{Broken, Effect, Out, Presenter, watch::Watch};
use crate::input::TalkHeld;
use crate::platform::screen_watch::{Changed, Sources};

/// Whether `header` begins a screen scene.
pub(super) fn on_screen(header: &SceneHeader) -> bool {
    header.begin.surface == Surface::Screen
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

    /// The keys of a hold whose `ScreenRelease` was sent came up at `now`; `after` is the
    /// newest run named so far, and `held` the talk keys still down. Any screen scene still up
    /// goes and the watch starts: the sources start again, on a bit of this watch's own, so
    /// nothing left of the watch before can set it. If they can't start the watch has seen a
    /// change: nothing could say the screen stayed as it was.
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
    /// change counts even while the scene's frames wait to be applied. A skipped scene is
    /// refused whole: it never installs a step or paints, and a begin still held never installs.
    pub(super) fn judge(
        &mut self,
        frame: &DataToShell,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        let DataToShell::SceneStepCommit { run, scene, .. } = *frame else {
            return Ok(());
        };
        let screen = match self.scenes.get(&scene) {
            Some(mirrored) => mirrored.screen,
            None => self.screens().contains(&scene),
        };
        if !screen || self.cleared.contains(&scene) || self.watch.shows(scene) {
            return Ok(());
        }
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
            tracing::info!("screen marks skipped");
            self.retire(scene, out);
            self.forget_failed(scene, out);
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

    /// Every screen scene not cleared: mirrored, or begun by a frame still held.
    fn screens(&self) -> Vec<SceneId> {
        let held = self.held.iter().filter_map(|(frame, _)| match &**frame {
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
