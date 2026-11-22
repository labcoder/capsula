//! Re-account settled canonical drawings when a new pair uses a different text measure.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kumochi_protocol::message::SceneId;
use kumochi_scene::display::{PaintWork, ProfileMeasure, TextMetricsV1};
use kumochi_scene::layout::recover_all;
use kumochi_scene::theme::Theme;
use kumochi_scene::{Begin, GlobalBudget, SceneEvent, SceneState};

use super::Drawing;
use super::prepare::{MAX_RETAINED_BYTES, Paint, SURFACE};

pub(crate) struct ReaccountJob {
    epoch: u64,
    metrics: Option<TextMetricsV1>,
    scenes: Vec<Source>,
    theme: Arc<Theme>,
}

struct Source {
    id: SceneId,
    revision: u32,
    activated: u32,
    header: Begin,
    log: Vec<SceneEvent>,
}

pub(crate) struct Reaccounted {
    scenes: Vec<(SceneState, Paint)>,
    global: GlobalBudget,
}

impl Drawing {
    pub(crate) fn reaccount(
        &mut self,
        epoch: u64,
        metrics: Option<TextMetricsV1>,
    ) -> Option<ReaccountJob> {
        debug_assert!(
            self.settled(),
            "no older admission may overlap re-accounting"
        );
        if self.paint_metrics == metrics {
            return None;
        }
        if self.scenes.is_empty() {
            self.paint_metrics = metrics;
            return None;
        }
        Some(ReaccountJob {
            epoch,
            metrics,
            scenes: self
                .scenes
                .iter()
                .filter(|held| !held.gone)
                .map(|held| Source {
                    id: held.scene,
                    revision: held.state.revision(),
                    activated: held.state.activated(),
                    header: held.state.header().clone(),
                    log: held.state.recovery_log().to_vec(),
                })
                .collect(),
            theme: Arc::clone(&self.theme),
        })
    }

    pub(crate) fn install_reaccounted(
        &mut self,
        epoch: u64,
        source: &ReaccountJob,
        replay: Option<Reaccounted>,
    ) -> bool {
        if epoch != source.epoch
            || !self.settled()
            || self.scenes.len() != source.scenes.len()
            || self.scenes.iter().zip(&source.scenes).any(|(held, old)| {
                held.gone
                    || held.scene != old.id
                    || held.state.revision() != old.revision
                    || held.state.activated() != old.activated
                    || held.state.recovery_log() != old.log.as_slice()
            })
        {
            return false;
        }
        if let Some(replay) = replay {
            for (held, (state, paint)) in self.scenes.iter_mut().zip(replay.scenes) {
                held.state = state;
                held.paint = Some(paint);
            }
            self.global = replay.global;
        } else {
            for held in &mut self.scenes {
                held.paint = None;
            }
        }
        self.paint_metrics = source.metrics;
        true
    }
}

impl ReaccountJob {
    pub(crate) fn run(&self, cancel: &AtomicBool) -> Option<Reaccounted> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let sources: Vec<_> = self
            .scenes
            .iter()
            .map(|scene| (&scene.header, scene.log.as_slice()))
            .collect();
        let (laid, global) = recover_all(
            &sources,
            SURFACE,
            &self.theme,
            &mut ProfileMeasure::new(self.metrics),
            cancel,
            Some(PaintWork::LIMITS),
        );
        let laid = laid.into_iter().collect::<Result<Vec<_>, _>>().ok()?;
        let mut bytes = 0;
        let mut work = PaintWork::default();
        let scenes = laid
            .into_iter()
            .map(|(state, layout)| {
                let paint = Paint::at_rest(layout);
                bytes += paint.resident_bytes();
                work = work + paint.charge();
                (state, paint)
            })
            .collect();
        (bytes <= MAX_RETAINED_BYTES && work.within_limits())
            .then_some(Reaccounted { scenes, global })
    }
}

impl fmt::Debug for ReaccountJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReaccountJob")
            .field("epoch", &self.epoch)
            .field("scenes", &self.scenes.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Reaccounted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reaccounted")
            .field("scenes", &self.scenes.len())
            .finish_non_exhaustive()
    }
}
