//! Free drawing area around the dock and its largest status panel.
use kumochi_scene::geom::{Rect, Vec2};

use super::geometry::Layout;

const MARGIN: f32 = 32.0;

#[expect(
    clippy::cast_precision_loss,
    reason = "desktop pixel coordinates fit f32 precision"
)]
fn logical([x, y, w, h]: [i32; 4], scale: f32) -> Rect {
    Rect {
        origin: Vec2::new(x as f32 / scale, y as f32 / scale),
        size: Vec2::new(w as f32 / scale, h as f32 / scale),
    }
}

pub(super) fn stage(area: [i32; 4], layout: Layout, scale: f32) -> Rect {
    let area = logical(area, scale);
    let occupied = logical(layout.dock, scale).union(logical(layout.panel, scale));
    let a = area.origin;
    let b = area.max();
    let left = occupied.origin.x.clamp(a.x, b.x);
    let top = occupied.origin.y.clamp(a.y, b.y);
    let right = occupied.max().x.clamp(a.x, b.x);
    let bottom = occupied.max().y.clamp(a.y, b.y);
    let bands = [
        Rect {
            origin: Vec2::new(a.x, bottom),
            size: Vec2::new(area.size.x, b.y - bottom),
        },
        Rect {
            origin: a,
            size: Vec2::new(area.size.x, top - a.y),
        },
        Rect {
            origin: a,
            size: Vec2::new(left - a.x, area.size.y),
        },
        Rect {
            origin: Vec2::new(right, a.y),
            size: Vec2::new(b.x - right, area.size.y),
        },
    ];
    let mut best = Rect::default();
    for band in bands {
        if band.size.x * band.size.y > best.size.x * best.size.y {
            best = band;
        }
    }
    if best.size.x <= 0.0 || best.size.y <= 0.0 {
        best = area;
    }
    let inset = Vec2::new(
        MARGIN.min((best.size.x - 1.0).max(0.0) * 0.5),
        MARGIN.min((best.size.y - 1.0).max(0.0) * 0.5),
    );
    Rect {
        origin: best.origin + inset,
        size: best.size - inset * 2.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::geometry::Position;

    #[test]
    fn top_center_reserves_the_panel_and_uses_the_band_below_it() {
        let layout = Position::TopCenter.layout([0, 0, 2560, 1400], [100, 24], [380, 124], 0);
        assert_eq!(
            stage([0, 0, 2560, 1400], layout, 1.0),
            Rect {
                origin: Vec2::new(32.0, 180.0),
                size: Vec2::new(2496.0, 1220.0 - 32.0),
            }
        );
    }

    #[test]
    fn stage_is_in_overlay_logical_pixels_at_different_dpi() {
        let layout = Position::BottomRight.layout([80, 40, 1800, 1200], [150, 60], [570, 186], 0);
        assert_eq!(
            stage([80, 40, 1800, 1200], layout, 1.5),
            Rect {
                origin: Vec2::new(80.0 / 1.5 + 32.0, 40.0 / 1.5 + 32.0),
                size: Vec2::new(1200.0 - 64.0, 636.0 - 64.0),
            }
        );
    }

    #[test]
    fn every_anchor_keeps_a_nonempty_stage_clear_of_the_footprint() {
        let area = [0, 0, 1200, 800];
        for position in Position::ALL {
            let layout = position.layout(area, [100, 24], [380, 124], 0);
            let stage = stage(area, layout, 1.0);
            assert!(!stage.intersects(logical(layout.dock, 1.0)));
            assert!(!stage.intersects(logical(layout.panel, 1.0)));
            assert!(stage.size.x > 0.0 && stage.size.y > 0.0);
            assert!(stage.origin.x >= 32.0 && stage.origin.y >= 32.0);
            assert!(stage.max().x <= 1168.0 && stage.max().y <= 768.0);
        }
        let layout = Position::Center.layout([0, 0, 20, 10], [100, 24], [380, 124], 0);
        assert_eq!(
            stage([0, 0, 20, 10], layout, 1.0),
            Rect {
                origin: Vec2::new(9.5, 4.5),
                size: Vec2::new(1.0, 1.0)
            }
        );
    }
}
