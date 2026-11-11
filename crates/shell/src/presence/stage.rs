//! Free drawing area around the dock, its status peek and the open panel.
use kumochi_scene::geom::{Rect, Vec2};

use super::geometry::Layout;
use super::{OpenError, Position, Presence, overlay, overlay_visibility::Visibility};
use gpui_kit::{App, Context, Subscription, WeakEntity};

/// Clearance from the work-area edges and the occupied dock/panel footprint.
const MARGIN: f32 = 32.0;
/// Smallest usable drawing band after clearance, in logical pixels.
const MIN_STAGE_SIZE: Vec2 = Vec2::new(240.0, 160.0);

pub(super) fn watch_displays(owner: WeakEntity<Presence>, cx: &App) -> Subscription {
    cx.observe_displays(move |_, cx| {
        if !matches!(owner.update(cx, Presence::display_changed), Ok(Ok(()))) {
            tracing::warn!("can't update drawing geometry after the display changed");
        }
    })
}

impl Presence {
    fn display_changed(&mut self, cx: &mut Context<'_, Self>) -> Result<(), OpenError> {
        self.scale = self
            .dock_window
            .update(cx, |_, window, _| window.scale_factor())
            .map_err(|error| OpenError::Gpui(error.into()))?;
        if let Some(position) = self
            .line
            .position_index()
            .and_then(|index| Position::ALL.get(index))
            .copied()
        {
            self.place_position(position, cx)?;
        }
        if let Visibility::Shown(surface) = &self.overlay {
            overlay::show(surface.window, cx)?;
        }
        self.want_frames(cx);
        Ok(())
    }

    pub(super) fn place_drawings(&mut self, owner: WeakEntity<Self>, cx: &mut App) {
        let Ok(area) = crate::platform::work_area() else {
            tracing::warn!("can't refresh the drawing stage");
            return;
        };
        let panel = self
            .settings_panel
            .as_ref()
            .filter(|_| self.settings_open())
            .map(|panel| panel.bounds)
            .or_else(|| {
                self.thread_panel
                    .as_ref()
                    .filter(|panel| panel.typing.visible)
                    .map(|panel| panel.bounds)
            })
            .or_else(|| {
                self.text
                    .as_ref()
                    .filter(|panel| panel.typing.visible)
                    .map(|panel| panel.bounds)
            });
        if self
            .presenter
            .set_stage(stage(area, self.layout, self.scale, panel))
        {
            cx.defer(move |cx| {
                if owner.update(cx, Presence::want_frames).is_err() {
                    tracing::debug!("drawing stage owner closed");
                }
            });
        }
    }
}

#[expect(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "desktop pixel coordinates fit f32 precision"
)]
fn logical([x, y, w, h]: [i32; 4], scale: f32) -> Rect {
    Rect {
        origin: Vec2::new(x as f32 / scale, y as f32 / scale),
        size: Vec2::new(w as f32 / scale, h as f32 / scale),
    }
}

// shortcut: work-area-only changes wait for the next placement; add a work-area notification through the presence adapter.
fn stage(area: [i32; 4], layout: Layout, scale: f32, panel: Option<[i32; 4]>) -> Rect {
    let area = logical(area, scale);
    let peek = logical(layout.dock, scale).union(logical(layout.panel, scale));
    let occupied = panel.map_or(peek, |panel| peek.union(logical(panel, scale)));
    if let Some(stage) = free_stage(area, occupied) {
        return stage;
    }
    // shortcut: on a display too small for both, an open panel can cover a drawing and its entrance can play unseen; hold the entrance until a usable stage exists.
    if panel.is_some()
        && let Some(stage) = free_stage(area, peek)
    {
        return stage;
    }
    inset(area)
}

fn free_stage(area: Rect, occupied: Rect) -> Option<Rect> {
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
    let mut best: Option<(f32, Rect)> = None;
    for band in bands {
        let stage = inset(band);
        let band_area = band.size.x * band.size.y;
        if stage.size.x >= MIN_STAGE_SIZE.x
            && stage.size.y >= MIN_STAGE_SIZE.y
            && best.is_none_or(|(best_area, _)| band_area > best_area)
        {
            best = Some((band_area, stage));
        }
    }
    best.map(|(_, stage)| stage)
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
    use super::*;
    use crate::presence::geometry::Position;

    fn transforms(presence: &Presence, count: usize) -> Vec<kumochi_scene::geom::Affine> {
        let mut transforms = Vec::new();
        assert!(
            !presence.presenter.frames().borrow().paint(|frame, _| {
                transforms.extend(frame.items().iter().map(|item| item.transform));
                Ok::<_, ()>(())
            }),
            "retained fixture frames paint successfully"
        );
        assert_eq!(transforms.len(), count, "all fixture items remain present");
        transforms
    }

    #[gpui::test]
    fn opening_and_closing_typing_repositions_a_resting_drawing(cx: &mut gpui_kit::TestAppContext) {
        let area = [0, 0, 2560, 1400];
        let _area = crate::platform::work_area_fixture::set(area);
        let owner = crate::presence::test_fixture::presence(cx);
        owner.update(cx, |presence, cx| {
            presence.layout = Position::TopCenter.layout(area, [58, 21], [380, 124], 0);
            presence.presenter = crate::presenter::tests::painter_fixture(Box::new(
                crate::presenter::tests::Charged(0),
            ))
            .0;
            presence.place_drawings(cx.weak_entity(), cx);
            let closed = transforms(presence, 2);
            presence.open_text(cx).expect("open typing");
            let open = transforms(presence, 2);
            assert_ne!(open, closed);
            let bounds = presence.text.as_ref().expect("typing").bounds;
            assert_eq!(bounds, [1040, 21, 480, 544]);
            let panel = logical(bounds, presence.scale);
            for transform in &open {
                assert!(
                    !transform
                        .bounds(Rect {
                            origin: Vec2::new(0.0, 0.0),
                            size: Vec2::new(50.0, 20.0),
                        })
                        .intersects(panel)
                );
            }
            presence.layout = Position::TopRight.layout(area, [58, 21], [380, 124], 0);
            presence.reanchor_panels(cx).expect("re-anchor typing");
            presence.place_drawings(cx.weak_entity(), cx);
            let moved = presence.text.as_ref().expect("typing").bounds;
            assert_eq!(&moved[2..], &bounds[2..]);
            assert_ne!(transforms(presence, 2), open);
            presence.layout = Position::TopCenter.layout(area, [58, 21], [380, 124], 0);
            presence.reanchor_panels(cx).expect("restore anchor");
            presence.place_drawings(cx.weak_entity(), cx);
            assert_eq!(transforms(presence, 2), open);
            presence.dismiss_text(cx).expect("close typing");
            assert_eq!(transforms(presence, 2), closed);
        });
    }

    #[gpui::test]
    fn panel_transitions_preserve_a_pending_first_paint(cx: &mut gpui_kit::TestAppContext) {
        let area = [0, 0, 2560, 1400];
        let _area = crate::platform::work_area_fixture::set(area);
        let owner = crate::presence::test_fixture::presence(cx);
        owner.update(cx, |presence, cx| {
            presence.layout = Position::TopCenter.layout(area, [58, 21], [380, 124], 0);
            let (mut presenter, now, job) = crate::presenter::tests::preparing();
            presenter
                .completed(job.run(&mut crate::presenter::tests::Shaper), now)
                .expect("prepare");
            presenter.advance(1, now).expect("activate");
            presenter.sample_at(now);
            presence.presenter = presenter;
            presence.place_drawings(cx.weak_entity(), cx);
            let closed = transforms(presence, 1);
            assert!(!presence.presenter.begin_painted(now));
            presence.open_text(cx).expect("open typing before paint");
            assert_eq!(
                presence.text.as_ref().expect("typing").bounds,
                [1040, 21, 480, 544]
            );
            presence.presenter.frames().borrow_mut().shown = true;
            assert_ne!(transforms(presence, 1), closed);
            let observation = presence
                .presenter
                .frames()
                .borrow()
                .painted(crate::presenter::tests::LIVE)
                .expect("painted");
            presence.dismiss_text(cx).expect("close typing after paint");
            assert_eq!(
                presence
                    .presenter
                    .frames()
                    .borrow()
                    .painted(crate::presenter::tests::LIVE),
                Some(observation)
            );
            assert_eq!(transforms(presence, 1), closed);
            assert!(presence.presenter.begin_painted(now));
            assert!(!presence.presenter.begin_painted(now));
        });
    }

    #[gpui::test]
    fn opening_typing_on_a_small_display_keeps_the_fallback_placement(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let area = [0, 0, 640, 480];
        let _area = crate::platform::work_area_fixture::set(area);
        let owner = crate::presence::test_fixture::presence(cx);
        owner.update(cx, |presence, cx| {
            presence.layout = Position::TopCenter.layout(area, [58, 21], [380, 124], 0);
            presence.presenter = crate::presenter::tests::painter_fixture(Box::new(
                crate::presenter::tests::Charged(0),
            ))
            .0;
            presence.place_drawings(cx.weak_entity(), cx);
            let closed = transforms(presence, 2);
            presence.open_text(cx).expect("open typing");
            assert_eq!(
                presence.text.as_ref().expect("typing").bounds,
                [80, 0, 480, 480]
            );
            assert_eq!(transforms(presence, 2), closed);
            presence.dismiss_text(cx).expect("close typing");
            assert_eq!(transforms(presence, 2), closed);
        });
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
    }

    #[test]
    fn top_center_reserves_the_panel_and_uses_the_band_below_it() {
        let layout = Position::TopCenter.layout([0, 0, 2560, 1400], [100, 24], [380, 124], 0);
        assert_eq!(
            stage([0, 0, 2560, 1400], layout, 1.0, None),
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
        assert_eq!(free_stage(logical(area, 1.0), occupied), None);
        assert_eq!(
            stage(area, layout, 1.0, None),
            Rect {
                origin: Vec2::new(9.5, 4.5),
                size: Vec2::new(1.0, 1.0)
            }
        );
    }
}
