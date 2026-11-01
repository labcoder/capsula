//! Physical work-area geometry shared by the dock, its hit proxy and its panel.

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

    pub(crate) fn layout(self, area: [i32; 4], dock: [i32; 2], panel: [i32; 2]) -> Layout {
        let [left, top, width, height] = area;
        let (column, row) = self.axes();
        let dock_width = dock[0].min(width);
        let dock_height = dock[1].min(height);
        let x = left + (width - dock_width) * column / 2;
        let y = top + (height - dock_height) * row / 2;
        let panel_width = panel[0].min(width);
        let panel_height = panel[1].min(height);
        let panel_x = (x + (dock_width - panel_width) / 2).clamp(left, left + width - panel_width);
        let below = y + dock_height;
        let panel_y = if below + panel_height <= top + height {
            below
        } else {
            (y - panel_height).max(top)
        };
        Layout {
            dock: [x, y, dock_width, dock_height],
            panel: [panel_x, panel_y, panel_width, panel_height],
        }
    }
}

/// The two surfaces share an edge, so the pointer never crosses an unowned gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelVisibility {
    Hidden,
    Visible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) dock: [i32; 4],
    pub(crate) panel: [i32; 4],
}

impl Layout {
    pub(crate) fn contains(self, point: [i32; 2], panel: PanelVisibility) -> bool {
        contains(self.dock, point)
            || (panel == PanelVisibility::Visible && contains(self.panel, point))
    }
}

fn contains([x, y, width, height]: [i32; 4], [px, py]: [i32; 2]) -> bool {
    px >= x && py >= y && px < x + width && py < y + height
}

#[cfg(test)]
mod tests {
    use super::{PanelVisibility, Position};

    #[test]
    fn nine_anchors_use_work_area_edges_and_center() {
        let actual = Position::ALL.map(|position| {
            position
                .layout([-1000, 40, 900, 600], [100, 40], [300, 120])
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
            let layout = position.layout([30, -600, 900, 600], [100, 40], [300, 120]);
            let [x, y, width, height] = layout.panel;
            assert!(x >= 30 && y >= -600 && x + width <= 930 && y + height <= 0);
            assert!(y == layout.dock[1] + 40 || y + height == layout.dock[1]);
            assert!(layout.contains([layout.dock[0], layout.dock[1]], PanelVisibility::Hidden));
            assert!(layout.contains([x, y], PanelVisibility::Visible));
            assert!(!layout.contains([x - 1, y - 1], PanelVisibility::Visible));
        }
    }

    #[test]
    fn tiny_work_area_clamps_both_surfaces() {
        let layout = Position::BottomRight.layout([10, 20, 80, 30], [100, 40], [300, 120]);
        assert_eq!(layout.dock, [10, 20, 80, 30]);
        assert_eq!(layout.panel, [10, 20, 80, 30]);
    }

    #[test]
    fn panel_is_not_owned_when_hidden_and_far_edge_is_outside() {
        let layout = Position::TopCenter.layout([0, 0, 900, 600], [100, 40], [300, 120]);
        assert!(!layout.contains([300, 60], PanelVisibility::Hidden));
        assert!(layout.contains([300, 60], PanelVisibility::Visible));
        assert!(!layout.contains([600, 160], PanelVisibility::Visible));
    }
}
