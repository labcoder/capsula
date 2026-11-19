//! Source observations for an opt-in drawing timing consumer.

use kumochi_protocol::message::{RunId, SceneId};

/// The live step identity shared with the brain's drawing receipts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrawKey {
    /// The run that supplied the step.
    pub run: RunId,
    /// The scene that owns the step.
    pub scene: SceneId,
    /// The zero-based lifetime step index in the scene.
    pub index: u32,
}

/// Presenter observations for drawing receipts, without any scene content or clock dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrawTiming {
    /// Preparation passed the presenter's checks and entered the ready window.
    Ready(DrawKey),
    /// The presenter installed the step's activated view and paint cache.
    Active {
        /// The installed step.
        key: DrawKey,
        /// The synthetic narration length, before taking the maximum with the entrance span.
        segment_ms: u64,
        /// Time from activation to the last entrance's end, or zero if there are no entrances.
        entrance_ms: u64,
    },
    /// The presenter accepted a successful shown sample for the current step.
    Shown {
        /// The step that the sample showed.
        key: DrawKey,
        /// The frame epoch that owns the accepted sample.
        epoch: u64,
    },
}
