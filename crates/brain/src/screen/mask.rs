//! Password rectangles one capture must never show: the element walk adds them as it reads (A3),
//! and sanitizing blacks them out before any pixel consumer (A4).

use std::sync::{Arc, Mutex};

use kumochi_protocol::geometry::PhysicalRect;
use serde::{Deserialize, Serialize};

use super::wire::CaptureId;

/// Why a capture's masks can't be vouched for. Each means "masks unknown", never "nothing to mask".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unavailable {
    /// UIPI refused the walk (an elevated window).
    Elevated,
    /// No walkable window: none in the foreground, gone, reused or off the captured display.
    NoWindow,
    /// The walk didn't finish in time, its thread died or its lock was poisoned.
    Busy,
    /// The window is Kumochi's own, so it's never walked.
    Excluded,
}

/// How far the masks are known to cover the walked window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Completeness {
    /// No walk has finished for this capture.
    Pending,
    /// The walk read the whole window and published every password field.
    Complete,
    /// The walk was cut, so fields past the cut may be unmasked.
    Partial,
    /// The masks are unknown.
    Unknown(Unavailable),
}

/// The masks for one capture, in desktop physical pixels. Each capture gets a fresh one.
#[derive(Debug)]
pub(crate) struct MaskSet {
    capture: CaptureId,
    generation: u32,
    rects: Vec<PhysicalRect>,
    completeness: Completeness,
}

/// One capture's masks, shared with the walk thread so a cut walk keeps what it reached.
pub(crate) type MaskSink = Arc<Mutex<MaskSet>>;

impl MaskSet {
    pub(crate) fn new(capture: CaptureId) -> Self {
        Self {
            capture,
            generation: 0,
            rects: Vec::new(),
            completeness: Completeness::Pending,
        }
    }

    /// Adds one rectangle and bumps the generation, so a frame sanitized earlier is superseded.
    pub(crate) fn push(&mut self, rect: PhysicalRect) {
        self.rects.push(rect);
        self.generation = self.generation.wrapping_add(1);
    }

    /// Records how the walk ended and bumps the generation. The first finish wins, so a late
    /// result can't overwrite `Unknown` or `Partial`.
    pub(crate) fn finish(&mut self, completeness: Completeness) {
        if self.completeness != Completeness::Pending {
            return;
        }
        self.completeness = completeness;
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn capture(&self) -> CaptureId {
        self.capture
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }

    pub(crate) fn rects(&self) -> &[PhysicalRect] {
        &self.rects
    }

    pub(crate) fn completeness(&self) -> Completeness {
        self.completeness
    }
}
