//! Presentation state survives activation independently of the animation clock.
use super::SceneView;
use crate::{
    Begin,
    geom::{Affine, Rect},
    layout::{self, LayoutState},
    record::{ScreenDisplay, Surface},
    screen,
    text::Finite,
};

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Placement {
    fit: Fit,
    pub(super) transform: Affine,
}

/// What places the drawing; one tag, so a view is no larger than with a stage alone.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
enum Fit {
    /// At its own scale until the host gives a stage.
    #[default]
    Free,
    /// Fitted into the host's stage box.
    Stage(Rect),
    /// A screen scene, which no stage moves: whether it is placed on its overlay.
    Screen { placed: bool },
}

impl Placement {
    pub(super) fn for_layout(self, layout: &LayoutState) -> Self {
        let Fit::Stage(stage) = self.fit else {
            return self;
        };
        Self {
            fit: self.fit,
            transform: layout::place(layout.fit_extent(), stage, layout.presentation_scale()),
        }
    }
}

impl SceneView {
    /// Snaps the drawing into a host-selected box without preparing it again.
    pub fn set_stage(&mut self, stage: Rect) -> bool {
        if matches!(self.placement.fit, Fit::Screen { .. })
            || self.placement.fit == Fit::Stage(stage)
        {
            return false;
        }
        self.placement.fit = Fit::Stage(stage);
        self.placement = self.placement.for_layout(&self.layout);
        true
    }

    /// Places a screen scene on an overlay covering `overlay`, through
    /// [`screen::placement`] with the header's stamped display and size. From then on the view
    /// ignores `set_stage`. Returns whether it is placed. When it isn't (no overlay display
    /// known, another display, an unstamped header), the view paints nothing until a later call
    /// places it. A header that isn't a screen scene's changes nothing.
    pub fn set_screen(&mut self, header: &Begin, overlay: Option<ScreenDisplay>) -> bool {
        if header.surface != Surface::Screen {
            return false;
        }
        let placed =
            header
                .display
                .zip(header.size)
                .zip(overlay)
                .and_then(|((display, size), overlay)| {
                    screen::placement(display, size.map(Finite::get), overlay)
                });
        self.placement.fit = Fit::Screen {
            placed: placed.is_some(),
        };
        self.placement.transform = placed.unwrap_or_default();
        placed.is_some()
    }

    /// Whether the view paints: not while its screen scene can't be placed.
    pub(super) fn placed(&self) -> bool {
        self.placement.fit != Fit::Screen { placed: false }
    }
}
