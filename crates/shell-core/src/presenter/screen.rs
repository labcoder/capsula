//! Screen scenes in the mirror. Their marks point at the screen as it was when a hold was
//! released, so one is shown only while the watch has seen nothing change since, and every one
//! goes as soon as something may have. The rules are the watch's (`watch.rs`); this file feeds
//! it and does the clearing.

use std::time::Instant;

use kumochi_protocol::message::{DataToShell, RunId, SceneHeader, SceneId};
use kumochi_scene::record::Surface;

use super::{Effect, Out, Presenter, watch::Watch};
use crate::platform::screen_watch::Sources;

/// Whether `header` begins a screen scene.
pub(super) fn on_screen(header: &SceneHeader) -> bool {
    header.begin.surface == Surface::Screen
}

impl Presenter {
    /// Installs the platform's sources, once, before the first release. Without them no watch
    /// can run, so no screen scene is ever shown.
    pub fn watch_sources(&mut self, sources: Box<dyn Sources>) {
        debug_assert!(!self.listening, "sources are installed before a watch runs");
        self.sources = Some(sources);
    }

    /// The keys of a hold whose `ScreenRelease` was sent came up at `now`; `after` is the
    /// newest run named so far. Any screen scene still up goes and the watch starts, from a clear bit. If
    /// its sources can't start it has seen a change: nothing could say the screen stayed as it
    /// was.
    pub fn screen_released(&mut self, after: RunId, now: Instant) -> Vec<Effect> {
        let mut out = Out::default();
        self.clear_screens("release", &mut out);
        self.watch.released(after, now);
        self.changed.take();
        if !self.listening {
            let changed = self.changed.clone();
            self.listening = self
                .sources
                .as_mut()
                .is_some_and(|sources| sources.start(changed));
        }
        if !self.listening {
            self.watch.changed();
        }
        out.finish()
    }

    /// The released hold's run was named. Until it is, the watch ends 30 seconds after the
    /// release.
    pub fn screen_run(&mut self, run: RunId) {
        self.watch.named(run);
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

    /// Something may have changed the screen: a source set the bit and woke the UI, or the next
    /// hold was recognized.
    pub fn screen_changed(&mut self) -> Vec<Effect> {
        let mut out = Out::default();
        self.changed.set();
        self.heed(&mut out);
        out.finish()
    }

    /// The setting went off: every screen scene goes, and one that arrives later isn't shown.
    pub fn screen_off(&mut self) -> Vec<Effect> {
        let mut out = Out::default();
        self.clear_screens("off", &mut out);
        self.watch = Watch::default();
        self.hush();
        out.finish()
    }

    /// The run whose marks were skipped because the screen changed first, until the next
    /// release.
    pub fn marks_skipped(&self) -> Option<RunId> {
        self.watch.skipped()
    }

    /// Applies the bit, if it is set.
    pub(super) fn heed(&mut self, out: &mut Out) {
        if self.changed.take() {
            if self.watch.changed() {
                self.clear_screens("changed", out);
            }
            self.hush();
        }
    }

    /// The watch's two limits, checked by the watchdog.
    pub(super) fn expire_marks(&mut self, now: Instant, out: &mut Out) {
        if self.watch.expired(now) {
            self.clear_screens("limit", out);
        }
        self.hush();
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
    /// refused whole and never installs.
    pub(super) fn judge(&mut self, frame: &DataToShell, out: &mut Out) {
        let DataToShell::SceneStepCommit { run, scene, .. } = *frame else {
            return;
        };
        let screen = match self.scenes.get(&scene) {
            Some(mirrored) => mirrored.screen,
            None => self.screens().contains(&scene),
        };
        if !screen || self.cleared.contains(&scene) || self.watch.shows(scene) {
            return;
        }
        if self.watch.marked(scene, run) {
            for other in self.screens() {
                if other != scene {
                    self.release(other);
                    self.forget(other, out);
                }
            }
        } else {
            tracing::info!("screen marks skipped");
            self.release(scene);
            self.forget_failed(scene, out);
        }
    }

    /// Clears every screen scene as the shell's own clear.
    fn clear_screens(&mut self, reason: &'static str, out: &mut Out) {
        let screens = self.screens();
        if !screens.is_empty() {
            tracing::info!(reason, "screen marks cleared");
        }
        for scene in screens {
            self.release(scene);
            self.forget(scene, out);
        }
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
