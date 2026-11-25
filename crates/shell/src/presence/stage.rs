//! Free drawing area around the dock, its status peek and the open panel.

pub(super) use kumochi_shell_core::presence::stage::machine;

pub(super) use machine::Placed;
use machine::settle;

use super::{OpenError, Position, Presence, overlay, overlay_visibility::Visibility};
use gpui_kit::{App, Context, Subscription, WeakEntity};

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
        self.show_display(cx);
        self.want_frames(cx);
        Ok(())
    }

    pub(super) fn place_drawings(&mut self, owner: WeakEntity<Self>, cx: &mut App) {
        let Ok(area) = crate::platform::work_area() else {
            tracing::warn!("can't refresh the drawing stage");
            return;
        };
        let panel = self
            .onboarding
            .as_ref()
            .map(|panel| panel.bounds)
            .or_else(|| {
                self.settings_panel
                    .as_ref()
                    .filter(|_| self.settings_open())
                    .map(|panel| panel.bounds)
            })
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
        let shown = self
            .drawing_placement
            .filter(|_| self.presenter.holds_scenes());
        let placed = settle(shown, area, self.layout, self.scale, panel);
        self.drawing_placement = Some(placed);
        if self.presenter.set_stage(placed.stage()) {
            cx.defer(move |cx| {
                if owner.update(cx, Presence::want_frames).is_err() {
                    tracing::debug!("drawing stage owner closed");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::machine::fixture::{AREA, PEEK_STAGE, TYPING_STAGE, rect, top_center};
    use super::machine::logical;
    use super::*;
    use crate::presence::geometry::Position;
    use kumochi_scene::geom::{Rect, Vec2};

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
    fn opening_typing_repositions_and_closing_it_keeps_a_resting_drawing(
        cx: &mut gpui_kit::TestAppContext,
    ) {
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
            assert_eq!(transforms(presence, 2), open);
        });
    }

    #[gpui::test]
    fn the_voice_answer_panel_moves_a_resting_drawing_before_the_next_paint(
        cx: &mut gpui_kit::TestAppContext,
    ) {
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
            let hidden = transforms(presence, 2);
            presence
                .show_voice_answer(cx)
                .expect("show the voice answer");
            let bounds = presence.text.as_ref().expect("typing").bounds;
            assert_eq!(bounds, [1040, 21, 480, 544]);
            let shown = transforms(presence, 2);
            assert_ne!(shown, hidden);
            let panel = logical(bounds, presence.scale);
            for transform in &shown {
                assert!(
                    !transform
                        .bounds(Rect {
                            origin: Vec2::new(0.0, 0.0),
                            size: Vec2::new(50.0, 20.0),
                        })
                        .intersects(panel)
                );
            }
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
            presenter.sample_at(now).expect("a sample");
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
            let open = transforms(presence, 1);
            assert_ne!(open, closed);
            let observation = crate::presenter::tests::painted(
                &presence.presenter.frames().borrow(),
                crate::presenter::tests::LIVE,
            )
            .expect("painted");
            presence.dismiss_text(cx).expect("close typing after paint");
            assert_eq!(
                crate::presenter::tests::painted(
                    &presence.presenter.frames().borrow(),
                    crate::presenter::tests::LIVE
                ),
                Some(observation)
            );
            assert_eq!(transforms(presence, 1), open);
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

    fn drawn(presence: &mut Presence) {
        presence.layout = top_center(AREA);
        presence.presenter =
            crate::presenter::tests::painter_fixture(Box::new(crate::presenter::tests::Charged(0)))
                .0;
    }

    fn stage_of(presence: &Presence) -> Option<Rect> {
        presence.drawing_placement.map(|placed| placed.stage())
    }

    fn run_state(
        presence: &mut Presence,
        phase: kumochi_protocol::message::Phase,
        cx: &mut Context<'_, Presence>,
    ) {
        use kumochi_protocol::message::{RunId, RunOrigin, ThreadId};
        presence
            .link_events(
                crate::link::Batch::Plain(vec![crate::link::LinkEvent::RunState {
                    generation: 1,
                    origin: Some(RunOrigin::Typed { turn: 1 }),
                    run: RunId(1),
                    thread: ThreadId(1),
                    phase,
                    error: None,
                }]),
                cx,
            )
            .expect("run state applies");
    }

    #[gpui::test]
    fn a_drawing_keeps_its_place_while_a_request_is_pending(cx: &mut gpui_kit::TestAppContext) {
        use kumochi_protocol::message::{Incarnation, Phase, ThreadId};
        let _area = crate::platform::work_area_fixture::set(AREA);
        let owner = crate::presence::test_fixture::presence(cx);
        owner.update(cx, |presence, cx| {
            drawn(presence);
            presence.open_text(cx).expect("open typing");
            assert_eq!(stage_of(presence), Some(TYPING_STAGE));
            let placed = transforms(presence, 2);
            // A held voice request hides the chat panel before it starts its run.
            presence.dismiss_text(cx).expect("the hold hides typing");
            assert_eq!(transforms(presence, 2), placed);
            let (link, _requests) = crate::link::LinkHandle::recording();
            presence.link = link;
            presence.session = Some(crate::link::Session::detached(1, Incarnation(1)));
            presence.runs.met(Incarnation(1));
            presence.runs.connected(1);
            presence.runs.settings(ThreadId(1));
            presence
                .runs
                .submit("a pending request", |_| true)
                .expect("submitted");
            presence.runs_changed(cx).expect("the run starts");
            run_state(presence, Phase::Thinking, cx);
            // Every scene frame re-places drawings before the presenter reads it.
            presence.place_drawings(cx.weak_entity(), cx);
            assert_eq!(transforms(presence, 2), placed);
            presence
                .open_text(cx)
                .expect("the answer shows typing again");
            assert_eq!(transforms(presence, 2), placed);
            run_state(presence, Phase::Done, cx);
            presence.place_drawings(cx.weak_entity(), cx);
            assert_eq!(stage_of(presence), Some(TYPING_STAGE));
            assert_eq!(transforms(presence, 2), placed);
        });
    }

    #[gpui::test]
    fn a_panel_opening_over_a_kept_drawing_moves_it_clear(cx: &mut gpui_kit::TestAppContext) {
        let _area = crate::platform::work_area_fixture::set(AREA);
        let owner = crate::presence::test_fixture::presence(cx);
        owner.update(cx, |presence, cx| {
            drawn(presence);
            presence.place_drawings(cx.weak_entity(), cx);
            assert_eq!(stage_of(presence), Some(PEEK_STAGE));
            presence.open_text(cx).expect("open typing");
            assert_eq!(stage_of(presence), Some(TYPING_STAGE));
            let panel = logical(presence.text.as_ref().expect("typing").bounds, 1.0);
            let canvas = rect(0.0, 0.0, 50.0, 20.0);
            for transform in transforms(presence, 2) {
                assert!(!transform.bounds(canvas).intersects(panel));
            }
        });
    }

    #[gpui::test]
    fn a_kept_stage_yields_to_a_display_change_and_a_cleared_screen(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let area = crate::platform::work_area_fixture::set(AREA);
        let owner = crate::presence::test_fixture::presence(cx);
        owner.update(cx, |presence, cx| {
            drawn(presence);
            presence.open_text(cx).expect("open typing");
            presence.dismiss_text(cx).expect("close typing");
            assert_eq!(stage_of(presence), Some(TYPING_STAGE));
            presence
                .presenter
                .clear(std::time::Instant::now())
                .expect("clear the screen");
            presence.place_drawings(cx.weak_entity(), cx);
            assert_eq!(stage_of(presence), Some(PEEK_STAGE));
            drawn(presence);
            presence.open_text(cx).expect("open typing again");
            presence.dismiss_text(cx).expect("close typing again");
            assert_eq!(stage_of(presence), Some(TYPING_STAGE));
        });
        drop(area);
        let smaller = [0, 0, 1920, 1080];
        let _smaller = crate::platform::work_area_fixture::set(smaller);
        owner.update(cx, |presence, cx| {
            presence.layout = top_center(smaller);
            presence.place_drawings(cx.weak_entity(), cx);
            assert_eq!(stage_of(presence), Some(rect(32.0, 177.0, 1856.0, 871.0)));
        });
    }

    fn scene_frame(
        presence: &mut Presence,
        message: kumochi_protocol::message::DataToShell,
        cx: &mut Context<'_, Presence>,
    ) {
        let frame = kumochi_scene::deferred::Deferred::new(message, 64);
        presence
            .link_events(
                crate::link::Batch::Plain(vec![crate::link::LinkEvent::SceneData {
                    generation: 1,
                    frame,
                    bytes: 64,
                }]),
                cx,
            )
            .expect("scene frame applies");
    }

    fn kept_below_typing(presence: &mut Presence, cx: &mut Context<'_, Presence>) {
        use kumochi_protocol::message::Incarnation;
        drawn(presence);
        presence.session = Some(crate::link::Session::detached(1, Incarnation(1)));
        presence.open_text(cx).expect("open typing");
        presence.dismiss_text(cx).expect("close typing");
        assert_eq!(stage_of(presence), Some(TYPING_STAGE));
    }

    #[gpui::test]
    fn a_new_scene_after_a_removal_takes_the_best_stage(cx: &mut gpui_kit::TestAppContext) {
        use kumochi_protocol::message::{DataToShell, SceneId};
        let _area = crate::platform::work_area_fixture::set(AREA);
        let owner = crate::presence::test_fixture::presence(cx);
        owner.update(cx, |presence, cx| {
            kept_below_typing(presence, cx);
            scene_frame(
                presence,
                DataToShell::ScenesRemoved {
                    scenes: vec![crate::presenter::tests::LIVE],
                },
                cx,
            );
            assert!(!presence.presenter.holds_scenes());
            scene_frame(
                presence,
                DataToShell::SceneBegin {
                    scene: SceneId(21),
                    header: crate::presenter::tests::header(),
                },
                cx,
            );
            assert!(presence.presenter.holds_scenes());
            assert_eq!(stage_of(presence), Some(PEEK_STAGE));
        });
    }

    #[gpui::test]
    fn a_drawing_recovered_after_a_reconnect_takes_the_best_stage(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use kumochi_protocol::message::{Incarnation, SceneDescriptorV1};
        let _area = crate::platform::work_area_fixture::set(AREA);
        let owner = crate::presence::test_fixture::presence(cx);
        owner.update(cx, |presence, cx| {
            kept_below_typing(presence, cx);
            let descriptor = SceneDescriptorV1::new(crate::presenter::tests::LIVE, 1, 1, 1)
                .expect("one recovered scene");
            presence
                .link_events(
                    crate::link::Batch::Plain(vec![
                        crate::link::LinkEvent::State(crate::link::LinkState::Reconnecting),
                        crate::link::LinkEvent::Connected(crate::link::Session::detached(
                            2,
                            Incarnation(1),
                        )),
                        crate::link::LinkEvent::ScenesV1 {
                            generation: 2,
                            scenes: kumochi_scene::text::Bounded::new(vec![descriptor])
                                .expect("one descriptor"),
                        },
                    ]),
                    cx,
                )
                .expect("the link drops, reconnects and describes its scene");
            assert!(presence.presenter.holds_scenes());
            presence.place_drawings(cx.weak_entity(), cx);
            assert_eq!(stage_of(presence), Some(PEEK_STAGE));
        });
    }
}
