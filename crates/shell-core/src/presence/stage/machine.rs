use crate::presence::geometry::Layout;
use kumochi_scene::geom::{Rect, Vec2};

#[cfg(any(test, feature = "test-support"))]
pub mod fixture;

/// Clearance from the work-area edges and the occupied dock/panel footprint.
const MARGIN: f32 = 32.0;
/// Smallest usable drawing band after clearance, in logical pixels.
const MIN_STAGE_SIZE: Vec2 = Vec2::new(240.0, 160.0);
/// Retain a usable band within 1% of the largest raw area.
const NEAR_TIE_RATIO: f32 = 0.99;

/// The stage drawings were given and the physical geometry it was chosen from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    area: [i32; 4],
    layout: Layout,
    scale: f32,
    stage: Rect,
    band: Option<usize>,
}

impl Placed {
    pub fn stage(&self) -> Rect {
        self.stage
    }
}

/// Keep a usable side through near-ties. At unchanged display and dock geometry, also keep an
/// already placed stage that still fits, so a panel closing needn't move the drawing.
pub fn settle(
    shown: Option<Placed>,
    area: [i32; 4],
    layout: Layout,
    scale: f32,
    panel: Option<[i32; 4]>,
) -> Placed {
    let shown =
        shown.filter(|kept| kept.scale.to_bits() == scale.to_bits() && kept.area[..2] == area[..2]);
    let (stage, band) = pick_stage(area, layout, scale, panel, shown.and_then(|kept| kept.band));
    Placed {
        area,
        layout,
        scale,
        stage: shown
            .filter(|kept| {
                kept.area == area && kept.layout == layout && contains(stage, kept.stage)
            })
            .map_or(stage, |kept| kept.stage),
        band,
    }
}

fn contains(outer: Rect, inner: Rect) -> bool {
    outer.origin.x <= inner.origin.x
        && outer.origin.y <= inner.origin.y
        && inner.max().x <= outer.max().x
        && inner.max().y <= outer.max().y
}

#[expect(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "desktop pixel coordinates fit f32 precision"
)]
pub fn logical([x, y, w, h]: [i32; 4], scale: f32) -> Rect {
    Rect {
        origin: Vec2::new(x as f32 / scale, y as f32 / scale),
        size: Vec2::new(w as f32 / scale, h as f32 / scale),
    }
}

// shortcut: work-area-only changes wait for the next placement; add a work-area notification through the presence adapter.
fn pick_stage(
    area: [i32; 4],
    layout: Layout,
    scale: f32,
    panel: Option<[i32; 4]>,
    preferred: Option<usize>,
) -> (Rect, Option<usize>) {
    let area = logical(area, scale);
    let peek = logical(layout.dock, scale).union(logical(layout.panel, scale));
    let occupied = panel.map_or(peek, |panel| peek.union(logical(panel, scale)));
    if let Some((stage, band)) = free_stage(area, occupied, preferred) {
        return (stage, Some(band));
    }
    // shortcut: on a display too small for both, an open panel can cover a drawing and its entrance can play unseen; hold the entrance until a usable stage exists.
    if panel.is_some()
        && let Some((stage, band)) = free_stage(area, peek, preferred)
    {
        return (stage, Some(band));
    }
    (inset(area), None)
}

fn free_stage(area: Rect, occupied: Rect, preferred: Option<usize>) -> Option<(Rect, usize)> {
    let mut best: Option<(f32, Rect, usize)> = None;
    let mut kept = None;
    for (index, band) in free_bands(area, occupied).into_iter().enumerate() {
        let stage = inset(band);
        if !(stage.size.x >= MIN_STAGE_SIZE.x && stage.size.y >= MIN_STAGE_SIZE.y) {
            continue;
        }
        let band_area = band.size.x * band.size.y;
        let candidate = (band_area, stage, index);
        if preferred == Some(index) {
            kept = Some(candidate);
        }
        if best.is_none_or(|(best_area, _, _)| band_area > best_area) {
            best = Some(candidate);
        }
    }
    let best = best?;
    let (_, stage, band) = kept
        .filter(|(area, _, _)| *area >= NEAR_TIE_RATIO * best.0)
        .unwrap_or(best);
    Some((stage, band))
}

pub(crate) fn free_bands(area: Rect, occupied: Rect) -> [Rect; 4] {
    let a = area.origin;
    let b = area.max();
    let left = occupied.origin.x.clamp(a.x, b.x);
    let top = occupied.origin.y.clamp(a.y, b.y);
    let right = occupied.max().x.clamp(a.x, b.x);
    let bottom = occupied.max().y.clamp(a.y, b.y);
    [
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
    ]
}

fn inset(band: Rect) -> Rect {
    let inset = Vec2::new(
        MARGIN.min((band.size.x - 1.0).max(0.0) * 0.5),
        MARGIN.min((band.size.y - 1.0).max(0.0) * 0.5),
    );
    Rect {
        origin: band.origin + inset,
        size: band.size - inset * 2.0,
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{AREA, PEEK_STAGE, TYPING_STAGE, rect, top_center};
    use super::*;
    use crate::presence::geometry::Position;

    fn stage(area: [i32; 4], layout: Layout, scale: f32, panel: Option<[i32; 4]>) -> Rect {
        settle(None, area, layout, scale, panel).stage()
    }

    #[test]
    fn open_panels_fit_the_canvas_below_or_beside_the_occupied_footprint() {
        let area = [0, 0, 2560, 1400];
        let canvas = Rect {
            origin: Vec2::new(0.0, 0.0),
            size: Vec2::new(960.0, 600.0),
        };
        for (position, panel, expected_stage, expected_transform) in [
            (
                Position::TopCenter,
                None,
                Rect {
                    origin: Vec2::new(32.0, 177.0),
                    size: Vec2::new(2496.0, 1191.0),
                },
                [1.985, 0.0, 0.0, 1.985, 327.2, 177.0],
            ),
            (
                Position::TopCenter,
                Some([1040, 21, 480, 544]),
                Rect {
                    origin: Vec2::new(32.0, 597.0),
                    size: Vec2::new(2496.0, 771.0),
                },
                [1.285, 0.0, 0.0, 1.285, 663.2, 597.0],
            ),
            (
                Position::TopCenter,
                Some([896, 21, 768, 544]),
                Rect {
                    origin: Vec2::new(32.0, 597.0),
                    size: Vec2::new(2496.0, 771.0),
                },
                [1.285, 0.0, 0.0, 1.285, 663.2, 597.0],
            ),
            (
                Position::TopRight,
                Some([1792, 21, 768, 544]),
                Rect {
                    origin: Vec2::new(32.0, 32.0),
                    size: Vec2::new(1728.0, 1336.0),
                },
                [1.8, 0.0, 0.0, 1.8, 32.0, 160.0],
            ),
        ] {
            let layout = position.layout(area, [58, 21], [380, 124], 0);
            let fitted_stage = stage(area, layout, 1.0, panel);
            assert_eq!(fitted_stage, expected_stage);
            let placed = kumochi_scene::layout::place(canvas, fitted_stage, 2.0);
            for (actual, expected) in placed.0.into_iter().zip(expected_transform) {
                assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
            }
            assert!(!placed.bounds(canvas).intersects(logical(layout.dock, 1.0)));
            assert!(!placed.bounds(canvas).intersects(logical(layout.panel, 1.0)));
            if let Some(panel) = panel {
                assert!(!placed.bounds(canvas).intersects(logical(panel, 1.0)));
            }
        }

        let wider_dock = Position::TopCenter.layout(area, [100, 24], [380, 124], 0);
        assert_eq!(
            stage(area, wider_dock, 1.0, None),
            Rect {
                origin: Vec2::new(32.0, 180.0),
                size: Vec2::new(2496.0, 1188.0),
            }
        );
    }

    #[test]
    fn stage_is_in_overlay_logical_pixels_at_different_dpi() {
        let layout = Position::BottomRight.layout([80, 40, 1800, 1200], [150, 60], [570, 186], 0);
        assert_eq!(
            stage([80, 40, 1800, 1200], layout, 1.5, None),
            Rect {
                origin: Vec2::new(80.0 / 1.5 + 32.0, 40.0 / 1.5 + 32.0),
                size: Vec2::new(1200.0 - 64.0, 636.0 - 64.0),
            }
        );
    }

    #[test]
    fn a_full_panel_falls_back_to_the_dock_and_peek_stage() {
        let area = [0, 0, 640, 480];
        let layout = Position::TopCenter.layout(area, [58, 21], [380, 124], 0);
        let panel = layout.anchored_panel(area, [768, 544]);
        assert_eq!(panel, [0, 0, 640, 480]);
        assert_eq!(
            stage(area, layout, 1.0, Some(panel)),
            Rect {
                origin: Vec2::new(32.0, 177.0),
                size: Vec2::new(576.0, 271.0),
            }
        );
    }

    #[test]
    fn a_larger_raw_sliver_yields_to_a_usable_side_band() {
        let area = [0, 0, 1376, 788];
        let layout = Position::TopCenter.layout(area, [58, 21], [380, 124], 0);
        let panel = layout.anchored_panel(area, [768, 544]);
        assert_eq!(panel, [304, 21, 768, 544]);
        assert_eq!(
            stage(area, layout, 1.0, Some(panel)),
            Rect {
                origin: Vec2::new(32.0, 32.0),
                size: Vec2::new(240.0, 724.0),
            }
        );
    }

    #[test]
    fn the_inset_minimum_is_inclusive_in_both_dimensions() {
        for (position, area, origin, size) in [
            (
                Position::TopCenter,
                [0, 0, 800, 789],
                [32.0, 597.0],
                [736.0, 160.0],
            ),
            (
                Position::TopCenter,
                [0, 0, 800, 788],
                [32.0, 177.0],
                [736.0, 579.0],
            ),
            (
                Position::TopRight,
                [0, 0, 1072, 640],
                [32.0, 32.0],
                [240.0, 576.0],
            ),
            (
                Position::TopRight,
                [0, 0, 1071, 640],
                [32.0, 177.0],
                [1007.0, 431.0],
            ),
        ] {
            let layout = position.layout(area, [58, 21], [380, 124], 0);
            let panel = layout.anchored_panel(area, [768, 544]);
            assert_eq!(
                stage(area, layout, 1.0, Some(panel)),
                Rect {
                    origin: Vec2::new(origin[0], origin[1]),
                    size: Vec2::new(size[0], size[1])
                },
                "{area:?} at {position:?}",
            );
        }
    }

    #[test]
    fn every_anchor_keeps_a_nonempty_stage_clear_of_the_footprint() {
        let area = [0, 0, 1200, 800];
        for position in Position::ALL {
            let layout = position.layout(area, [100, 24], [380, 124], 0);
            let stage = stage(area, layout, 1.0, None);
            assert!(!stage.intersects(logical(layout.dock, 1.0)));
            assert!(!stage.intersects(logical(layout.panel, 1.0)));
            assert!(stage.size.x > 0.0 && stage.size.y > 0.0);
            assert!(stage.origin.x >= 32.0 && stage.origin.y >= 32.0);
            assert!(stage.max().x <= 1168.0 && stage.max().y <= 768.0);
        }
    }

    #[test]
    fn tiny_desktops_use_the_documented_last_resort() {
        let area = [0, 0, 20, 10];
        let layout = Position::Center.layout(area, [100, 24], [380, 124], 0);
        let occupied = logical(layout.dock, 1.0).union(logical(layout.panel, 1.0));
        assert_eq!(free_stage(logical(area, 1.0), occupied, None), None);
        assert_eq!(
            stage(area, layout, 1.0, None),
            Rect {
                origin: Vec2::new(9.5, 4.5),
                size: Vec2::new(1.0, 1.0)
            }
        );
    }

    fn right_of_work_area(shown: Option<Placed>, height: i32, panel_size: [i32; 2]) -> Placed {
        let area = [0, 0, 1536, height];
        let layout = Position::TopRight.layout(area, [58, 21], [380, 124], 0);
        let panel = layout.anchored_panel(area, panel_size);
        settle(shown, area, layout, 1.0, Some(panel))
    }

    #[test]
    fn a_one_pixel_work_area_change_keeps_the_current_band_on_either_side_of_a_tie() {
        let place = |shown, height| right_of_work_area(shown, height, [768, 544]);
        let mut below = place(None, 1130);
        assert_eq!(below.stage(), rect(32.0, 597.0, 1472.0, 501.0));
        let mut left = place(None, 1129);
        assert_eq!(left.stage(), rect(32.0, 32.0, 704.0, 1065.0));
        assert_eq!(place(None, 1131).stage(), rect(32.0, 597.0, 1472.0, 502.0));

        for (offset, height) in [(-1.0, 1129), (0.0, 1130), (1.0, 1131)] {
            below = place(Some(below), height);
            left = place(Some(left), height);
            assert_eq!(below.stage(), rect(32.0, 597.0, 1472.0, 501.0 + offset),);
            assert_eq!(left.stage(), rect(32.0, 32.0, 704.0, 1066.0 + offset),);
        }
    }

    #[test]
    fn a_later_dock_reanchor_keeps_the_side_chosen_during_the_resize() {
        let original = [0, 0, 1536, 1130];
        let resized = [0, 0, 1536, 1129];
        let layout = |area| Position::BottomRight.layout(area, [58, 21], [380, 124], 0);
        let panel = |area, layout: Layout| Some(layout.anchored_panel(area, [768, 544]));
        let old = layout(original);
        let shown = settle(None, original, old, 1.0, panel(original, old));
        let above = rect(32.0, 32.0, 1472.0, 501.0);
        assert_eq!(shown.stage(), above);
        let waiting = settle(Some(shown), resized, old, 1.0, panel(original, old));
        assert_eq!(waiting.stage(), above);

        let accepted = layout(resized);
        assert_eq!(
            settle(
                Some(waiting),
                resized,
                accepted,
                1.0,
                panel(resized, accepted)
            )
            .stage(),
            rect(32.0, 32.0, 1472.0, 500.0)
        );
    }

    #[test]
    fn a_meaningfully_larger_band_or_an_unusable_current_band_switches_immediately() {
        let place = |shown, height| right_of_work_area(shown, height, [768, 544]);
        let below = place(None, 1130);
        let below = place(Some(below), 1119);
        assert_eq!(below.stage(), rect(32.0, 597.0, 1472.0, 490.0));
        let left = place(Some(below), 1118);
        assert_eq!(left.stage(), rect(32.0, 32.0, 704.0, 1054.0));
        assert_eq!(
            place(Some(left), 1160).stage(),
            rect(32.0, 597.0, 1472.0, 531.0)
        );

        let place = |shown, height| right_of_work_area(shown, height, [1232, 887]);
        let below = place(None, 1133);
        assert_eq!(below.stage(), rect(32.0, 940.0, 1472.0, 161.0));
        assert_eq!(
            place(Some(below), 1131).stage(),
            rect(32.0, 32.0, 240.0, 1067.0)
        );
    }

    #[test]
    fn only_a_panel_closing_keeps_the_stage_of_a_shown_drawing() {
        let layout = top_center(AREA);
        let panel = Some(layout.anchored_panel(AREA, [480, 544]));
        let open = settle(None, AREA, layout, 1.0, panel);
        assert_eq!(open.stage(), TYPING_STAGE);
        assert_eq!(
            settle(Some(open), AREA, layout, 1.0, None).stage(),
            TYPING_STAGE
        );
        let closed = settle(None, AREA, layout, 1.0, None);
        assert_eq!(closed.stage(), PEEK_STAGE);
        assert_eq!(
            settle(Some(closed), AREA, layout, 1.0, panel).stage(),
            TYPING_STAGE
        );
        let right = Position::TopRight.layout(AREA, [58, 21], [380, 124], 0);
        assert_eq!(
            settle(Some(open), AREA, right, 1.0, None).stage(),
            PEEK_STAGE
        );
        assert_eq!(
            settle(Some(open), AREA, layout, 1.25, None),
            settle(None, AREA, layout, 1.25, None)
        );
        let wider = [0, 0, 3840, 1400];
        assert_eq!(
            settle(Some(open), wider, top_center(wider), 1.0, None).stage(),
            rect(32.0, 177.0, 3776.0, 1191.0)
        );
    }
}
