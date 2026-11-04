//! Physical work-area geometry shared by the dock, its overlay and its panel.

/// One of the nine work-area anchors, in row-major order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Position {
    TopLeft,
    #[default]
    TopCenter,
    TopRight,
    MiddleLeft,
    Center,
    MiddleRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl Position {
    pub(crate) const ALL: [Self; 9] = [
        Self::TopLeft,
        Self::TopCenter,
        Self::TopRight,
        Self::MiddleLeft,
        Self::Center,
        Self::MiddleRight,
        Self::BottomLeft,
        Self::BottomCenter,
        Self::BottomRight,
    ];

    fn axes(self) -> (i32, i32) {
        match self {
            Self::TopLeft => (0, 0),
            Self::TopCenter => (1, 0),
            Self::TopRight => (2, 0),
            Self::MiddleLeft => (0, 1),
            Self::Center => (1, 1),
            Self::MiddleRight => (2, 1),
            Self::BottomLeft => (0, 2),
            Self::BottomCenter => (1, 2),
            Self::BottomRight => (2, 2),
        }
    }

    /// Places the dock's slot, its panel, and the overlay the dock draws in: the slot grown by
    /// `extent` on every side, inside the work area.
    pub(crate) fn layout(
        self,
        area: [i32; 4],
        dock: [i32; 2],
        panel: [i32; 2],
        extent: i32,
    ) -> Layout {
        let [left, top, width, height] = area;
        let (column, row) = self.axes();
        let dock_width = dock[0].min(width);
        let dock_height = dock[1].min(height);
        let x = left + (width - dock_width) * column / 2;
        let y = top + (height - dock_height) * row / 2;
        let panel = anchored_panel([x, y, dock_width, dock_height], area, panel);
        let overlay_x = (x - extent).max(left);
        let overlay_y = (y - extent).max(top);
        let overlay_right = (x + dock_width + extent).min(left + width);
        let overlay_bottom = (y + dock_height + extent).min(top + height);
        Layout {
            dock: [x, y, dock_width, dock_height],
            panel,
            overlay: [
                overlay_x,
                overlay_y,
                overlay_right - overlay_x,
                overlay_bottom - overlay_y,
            ],
        }
    }
}

/// The two surfaces share an edge, so the pointer never crosses an unowned gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelVisibility {
    Hidden,
    Visible,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Layout {
    /// The resting dock's slot, which holds the line and its trigger band.
    pub(crate) dock: [i32; 4],
    pub(crate) panel: [i32; 4],
    /// The click-through dock window: the slot, plus any room something drawn from it needs.
    pub(crate) overlay: [i32; 4],
}

impl Layout {
    /// Places another interactive surface against the dock, clamped to the work area.
    pub(crate) fn anchored_panel(self, area: [i32; 4], panel: [i32; 2]) -> [i32; 4] {
        anchored_panel(self.dock, area, panel)
    }

    /// Whether the pointer is over the panel. Over the dock is the line's question.
    pub(crate) fn panel_contains(self, point: [i32; 2], panel: PanelVisibility) -> bool {
        panel == PanelVisibility::Visible && contains(self.panel, point)
    }
}

fn anchored_panel(
    [x, y, dock_width, dock_height]: [i32; 4],
    [left, top, width, height]: [i32; 4],
    panel: [i32; 2],
) -> [i32; 4] {
    let panel_width = panel[0].min(width);
    let panel_height = panel[1].min(height);
    let panel_x = (x + (dock_width - panel_width) / 2).clamp(left, left + width - panel_width);
    let below = y + dock_height;
    let panel_y = if below + panel_height <= top + height {
        below
    } else {
        (y - panel_height).max(top)
    };
    [panel_x, panel_y, panel_width, panel_height]
}

fn contains([x, y, width, height]: [i32; 4], [px, py]: [i32; 2]) -> bool {
    px >= x && py >= y && px < x + width && py < y + height
}

#[cfg(test)]
mod tests {
    use super::{PanelVisibility, Position};

    #[test]
    fn a_larger_threads_surface_uses_the_same_anchor_and_stays_in_the_work_area() {
        let area = [30, -600, 900, 600];
        let threads =
            [Position::TopLeft, Position::Center, Position::BottomRight].map(|position| {
                position
                    .layout(area, [100, 40], [300, 120], 0)
                    .anchored_panel(area, [760, 540])
            });
        assert_eq!(
            threads,
            [
                [30, -560, 760, 540],
                [100, -600, 760, 540],
                [170, -580, 760, 540],
            ]
        );
        let tiny = Position::BottomRight.layout([10, 20, 80, 30], [100, 40], [300, 120], 0);
        assert_eq!(
            tiny.anchored_panel([10, 20, 80, 30], [760, 540]),
            [10, 20, 80, 30]
        );
    }

    #[test]
    fn nine_anchors_use_work_area_edges_and_center() {
        let actual = Position::ALL.map(|position| {
            position
                .layout([-1000, 40, 900, 600], [100, 40], [300, 120], 0)
                .dock
        });
        assert_eq!(
            actual,
            [
                [-1000, 40, 100, 40],
                [-600, 40, 100, 40],
                [-200, 40, 100, 40],
                [-1000, 320, 100, 40],
                [-600, 320, 100, 40],
                [-200, 320, 100, 40],
                [-1000, 600, 100, 40],
                [-600, 600, 100, 40],
                [-200, 600, 100, 40],
            ]
        );
    }

    #[test]
    fn every_panel_fits_and_touches_its_dock() {
        for position in Position::ALL {
            let layout = position.layout([30, -600, 900, 600], [100, 40], [300, 120], 0);
            let [x, y, width, height] = layout.panel;
            assert!(x >= 30 && y >= -600 && x + width <= 930 && y + height <= 0);
            assert!(y == layout.dock[1] + 40 || y + height == layout.dock[1]);
            assert!(!layout.panel_contains([x, y], PanelVisibility::Hidden));
            assert!(layout.panel_contains([x, y], PanelVisibility::Visible));
            assert!(!layout.panel_contains([x - 1, y - 1], PanelVisibility::Visible));
        }
    }

    #[test]
    fn tiny_work_area_clamps_both_surfaces() {
        let layout = Position::BottomRight.layout([10, 20, 80, 30], [100, 40], [300, 120], 50);
        assert_eq!(layout.dock, [10, 20, 80, 30]);
        assert_eq!(layout.panel, [10, 20, 80, 30]);
        assert_eq!(layout.overlay, [10, 20, 80, 30]);
    }

    #[test]
    fn panel_is_not_owned_when_hidden_and_far_edge_is_outside() {
        let layout = Position::TopCenter.layout([0, 0, 900, 600], [100, 40], [300, 120], 0);
        assert!(!layout.panel_contains([300, 60], PanelVisibility::Hidden));
        assert!(layout.panel_contains([300, 60], PanelVisibility::Visible));
        assert!(!layout.panel_contains([600, 160], PanelVisibility::Visible));
    }

    #[test]
    fn the_overlay_grows_the_slot_inside_the_work_area() {
        let top = Position::TopCenter.layout([0, 0, 900, 600], [100, 40], [300, 120], 110);
        assert_eq!(top.dock, [400, 0, 100, 40]);
        assert_eq!(top.overlay, [290, 0, 320, 150]);
        let middle = Position::Center.layout([0, 0, 900, 600], [100, 40], [300, 120], 110);
        assert_eq!(middle.overlay, [290, 170, 320, 260]);
        for position in Position::ALL {
            let layout = position.layout([-1000, 40, 900, 600], [100, 40], [300, 120], 110);
            let [x, y, width, height] = layout.overlay;
            let [dock_x, dock_y, dock_width, dock_height] = layout.dock;
            assert!(x >= -1000 && y >= 40 && x + width <= -100 && y + height <= 640);
            assert!(x <= dock_x && y <= dock_y);
            assert!(x + width >= dock_x + dock_width && y + height >= dock_y + dock_height);
        }
    }
}
