//! One pointer authority; adapters run returned effects in order before delivering more input.

use std::collections::BTreeMap;

use kumochi_protocol::message::GestureId;

use crate::platform::input::Button;
use crate::presence::drag::{DragOwner, End, Event, Outcome};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayInput {
    #[default]
    PassThrough,
    Regions,
    Selecting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RegionId(pub(crate) u64);

/// Physical screen [x, y, width, height], maintained by the region's owner.
pub type Regions = BTreeMap<RegionId, [i32; 4]>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DragLease(u64);

// shortcut: GestureCancel and HoldEnded have no producer in shipped code; B3a/B3b supply them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitCause {
    Release,
    Escape,
    GestureCancel,
    BrainLost,
    CaptureLost,
    WindowDestroyed,
    DisplayChanged,
    HoldEnded,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionEnd {
    Released,
    Region([i32; 4]),
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Panels,
    Dock,
    Regions,
    Overlay,
    Selection,
}

impl OverlayInput {
    /// Top first; a dormant selection surface remains below the visual overlay.
    pub fn order(self) -> [Layer; 5] {
        use Layer::{Dock, Overlay, Panels, Regions, Selection};
        match self {
            Self::PassThrough | Self::Regions => [Panels, Dock, Regions, Overlay, Selection],
            Self::Selecting => [Selection, Panels, Dock, Regions, Overlay],
        }
    }
}

/// No activation operation: macOS uses nonactivating `NSPanel`, `ignoresMouseEvents` and levels.
pub trait InputPlatform {
    type Error;
    fn configure(&mut self, mode: OverlayInput, regions: &Regions) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    InputCancelled(RegionId),
    CancelDrag(DragLease),
    Configure,
    SelectionStarted(GestureId),
    SelectionEnded(GestureId, ExitCause, SelectionEnd),
}

/// The presenter retains this state across brain connections; no platform or orbit state lives here.
#[derive(Debug, Default)]
pub struct OverlayOwner {
    mode: OverlayInput,
    regions: Regions,
    orbit: Option<RegionId>,
    drag: Option<DragLease>,
    next_lease: u64,
    selection: DragOwner,
    gesture: Option<GestureId>,
    rectangle: Option<[i32; 4]>,
}

// shortcut: slice 5 registers regions and B3a/B3b enter Selecting; no hold producer calls it yet.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "slice 5 and B3a/B3b consume this seam")
)]
impl OverlayOwner {
    pub(crate) fn mode(&self) -> OverlayInput {
        self.mode
    }

    pub fn configure<P: InputPlatform>(&self, platform: &mut P) -> Result<(), P::Error> {
        platform.configure(self.mode, &self.regions)
    }

    pub(crate) fn register(&mut self, id: RegionId, rect: [i32; 4]) -> Vec<Effect> {
        if rect[2] <= 0 || rect[3] <= 0 {
            return Vec::new();
        }
        self.regions.insert(id, rect);
        if self.mode == OverlayInput::Selecting {
            return Vec::new();
        }
        self.mode = OverlayInput::Regions;
        vec![Effect::Configure]
    }

    pub(crate) fn remove(&mut self, id: RegionId) -> Vec<Effect> {
        if self.regions.remove(&id).is_none() {
            return Vec::new();
        }
        let mut effects = Vec::new();
        if self.orbit == Some(id) {
            self.orbit = None;
            effects.push(Effect::InputCancelled(id));
        }
        if self.mode != OverlayInput::Selecting {
            if self.regions.is_empty() {
                self.mode = OverlayInput::PassThrough;
            }
            effects.push(Effect::Configure);
        }
        effects
    }

    pub(crate) fn begin_orbit(&mut self, id: RegionId) -> bool {
        if self.mode != OverlayInput::Regions
            || !self.regions.contains_key(&id)
            || self.orbit.is_some()
            || self.drag.is_some()
        {
            return false;
        }
        self.orbit = Some(id);
        true
    }

    pub(crate) fn end_orbit(&mut self, id: RegionId) {
        if self.orbit == Some(id) {
            self.orbit = None;
        }
    }

    pub fn lease_drag(&mut self) -> Option<DragLease> {
        if self.mode == OverlayInput::Selecting || self.orbit.is_some() || self.drag.is_some() {
            return None;
        }
        self.next_lease += 1;
        let lease = DragLease(self.next_lease);
        self.drag = Some(lease);
        Some(lease)
    }

    pub fn return_drag(&mut self, lease: DragLease) {
        if self.drag == Some(lease) {
            self.drag = None;
        }
    }

    /// B3a supplies its live hold; B3b acknowledges entry only after preemption and native setup.
    pub(crate) fn select(&mut self, gesture: GestureId) -> Vec<Effect> {
        if self.gesture.is_some() {
            return Vec::new();
        }
        let mut effects = Vec::new();
        if let Some(id) = self.orbit.take() {
            effects.push(Effect::InputCancelled(id));
        }
        if let Some(lease) = self.drag.take() {
            effects.push(Effect::CancelDrag(lease));
        }
        self.gesture = Some(gesture);
        self.mode = OverlayInput::Selecting;
        effects.push(Effect::Configure);
        effects
    }

    /// Stamp `seq` after native capture returns, so preempted capture losses precede this grab.
    pub(crate) fn selected(
        &mut self,
        gesture: GestureId,
        button: Button,
        seq: u64,
        ready: Result<(), ()>,
    ) -> Vec<Effect> {
        if ready.is_err() {
            return self.end_selection(gesture, ExitCause::Failed);
        }
        if self.gesture != Some(gesture) || self.selection.press(button, seq).is_none() {
            return Vec::new();
        }
        vec![Effect::SelectionStarted(gesture)]
    }

    pub fn selection(&self) -> Option<GestureId> {
        self.gesture
    }

    pub(crate) fn rectangle(&mut self, gesture: GestureId, rect: [i32; 4]) {
        if self.gesture == Some(gesture) {
            self.rectangle = (rect[2] >= 4 && rect[3] >= 4).then_some(rect);
        }
    }

    pub fn selection_event(&mut self, event: Event) -> Vec<Effect> {
        let outcome = self.selection.handle(event);
        self.ending(outcome)
    }

    pub fn reconcile(&mut self) -> Vec<Effect> {
        let outcome = self.selection.reconcile();
        self.ending(outcome)
    }

    pub fn end_selection(&mut self, gesture: GestureId, cause: ExitCause) -> Vec<Effect> {
        if self.gesture != Some(gesture) {
            return Vec::new();
        }
        self.selection.handle(Event::SourceClosed);
        self.restore(cause)
    }

    fn ending(&mut self, outcome: Outcome) -> Vec<Effect> {
        match outcome {
            Outcome::Ended(_, end) => self.restore(match end {
                End::Released(_) => ExitCause::Release,
                End::Cancelled | End::Preempted => ExitCause::Escape,
                End::Lost => ExitCause::CaptureLost,
            }),
            _ => Vec::new(),
        }
    }

    fn restore(&mut self, cause: ExitCause) -> Vec<Effect> {
        let gesture = self
            .gesture
            .take()
            .expect("only a pending or live selection ends");
        // Regions registered or removed during the selection decide the restored mode.
        self.mode = if self.regions.is_empty() {
            OverlayInput::PassThrough
        } else {
            OverlayInput::Regions
        };
        let rectangle = self.rectangle.take();
        let end = match (cause, rectangle) {
            (ExitCause::Release | ExitCause::HoldEnded, Some(rect)) => SelectionEnd::Region(rect),
            (ExitCause::Release, None) => SelectionEnd::Released,
            _ => SelectionEnd::Cancelled,
        };
        vec![
            Effect::Configure,
            Effect::SelectionEnded(gesture, cause, end),
        ]
    }
}

#[cfg(test)]
#[path = "overlay_input_tests.rs"]
mod tests;
