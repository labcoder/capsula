use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};

use gpui_kit::{Bounds, Hsla, Rgba, TestAppContext, WeakEntity, point, px, rgb, size};
use kumochi_protocol::message::{ControlToBrain, DataToShell, SceneHeader, SceneId};
use kumochi_scene::{
    FrameItem, Record, SceneView,
    deferred::Deferred,
    display::{MeasureError, Paint, Prim, TextMeasure, TextStyle, fallback_corner_segments},
    geom::{Affine, Rect, Vec2},
    layout::SurfaceGeometry,
    parse_record,
    record::Rgb,
    text::Bounded,
    theme::Theme,
};

use super::super::Presence;
use super::super::overlay_fixture::{Mode, resting};
use super::super::paint_cache::{PaintScene, Stager};
use super::super::test_fixture;
use super::{BuildError, Gpui, Pending, Surface, assemble, color, label, open, upright};
use crate::{
    escape::Panel,
    presenter::{Effect, Frames, Presenter, SceneCache, Stage, tests::drained},
    runs::Runs,
};

#[path = "overlay_label_tests.rs"]
mod labels;

#[path = "overlay_recovery_tests.rs"]
pub(super) mod recovery;

#[test]
fn paint_notifications_coalesce_fresh_numbers_and_keep_pending_until_the_drain() {
    let notice = super::PaintNotice::default();
    let first = [Some((SceneId(1), 2, 1)), None, None, None];
    let next = [Some((SceneId(3), 8, 2)), None, None, None];
    assert!(notice.observe(false, first));
    assert!(!notice.observe(false, first));
    assert!(!notice.observe(false, [None; 4]));
    assert!(notice.pending.get());
    assert!(!notice.observe(false, next));
    assert_eq!(notice.seen.get(), next);
    assert!(notice.pending.get());
    notice.pending.set(false);
    assert!(!notice.observe(false, next));
    assert!(!notice.pending.get());
    assert!(notice.observe(true, [None; 4]));
    assert!(!notice.observe(true, [None; 4]));
    assert!(notice.failed.get());
}

fn ready_canvas() -> Presenter {
    let mut presenter = begun(Box::new(Stager(size_of::<Stager>())));
    prepare_canvas_step(&mut presenter, 0, br#"{"step":{"id":"static","start":"now","put":[{"shape":{"id":"box","kind":"rect","box":[0,0,20,20],"enter":"none"}}]}}"#);
    presenter
}

fn prepare_canvas_step(presenter: &mut Presenter, index: u32, record: &[u8]) {
    prepare_canvas_step_at(presenter, index, record, Instant::now());
}

fn prepare_canvas_step_at(presenter: &mut Presenter, index: u32, record: &[u8], now: Instant) {
    prepare_scene_step_at(presenter, TWINNED, index, record, now);
}

fn prepare_scene_step_at(
    presenter: &mut Presenter,
    scene: SceneId,
    index: u32,
    record: &[u8],
    now: Instant,
) {
    let Record::Step(step) = parse_record(record).expect("fixture step") else {
        panic!("step");
    };
    let effects = presenter
        .data(
            1,
            Deferred::new(
                DataToShell::SceneStepCommit {
                    run: kumochi_protocol::message::RunId(6),
                    scene,
                    index,
                    step,
                },
                100,
            ),
            100,
            now,
        )
        .expect("admission");
    let mut jobs = effects.into_iter().filter_map(|effect| match effect {
        Effect::Admit(job) => Some(job),
        _ => None,
    });
    let job = jobs.next().expect("one admission");
    assert!(jobs.next().is_none());
    let effects = presenter.completed(job.run(), now).expect("admitted");
    let mut jobs = effects.into_iter().filter_map(|effect| match effect {
        Effect::Prepare(job) => Some(job),
        _ => None,
    });
    let job = jobs.next().expect("one preparation");
    assert!(jobs.next().is_none());
    assert!(
        presenter
            .completed(job.run(&mut Fixed), now)
            .expect("prepared")
            .is_empty()
    );
}

fn owned_static_canvas(
    cx: &mut TestAppContext,
) -> (
    gpui_kit::Entity<Presence>,
    gpui_kit::AnyWindowHandle,
    async_channel::Receiver<ControlToBrain>,
) {
    let owner = test_fixture::presence(cx);
    let presenter = ready_canvas();
    let frames = presenter.frames();
    let (notice, scratch, controls) = owner.update(cx, |this, _| {
        this.presenter = presenter;
        this.session = Some(crate::link::Session::detached(
            1,
            kumochi_protocol::message::Incarnation(1),
        ));
        (
            Rc::clone(&this.paint_notice),
            Rc::clone(&this.paint_scratch),
            this.link.record_controls(),
        )
    });
    let overlay = cx
        .update(|cx| {
            open(
                frames,
                owner.downgrade(),
                scratch,
                notice,
                cx,
                |_, _| Ok(()),
            )
        })
        .expect("owned headless host");
    let window = overlay.window;
    let painting = Rc::clone(&overlay.painting);
    owner.update(cx, |this, cx| {
        this.presenter.frames().borrow_mut().shown = true;
        this.overlay = super::super::overlay_visibility::Visibility::Shown(overlay);
        this.draw_changed(cx);
        painting.set(false);
    });
    painting.set(true);
    (owner, window, controls)
}

#[gpui::test]
fn a_static_first_paint_delivers_once_without_a_followup_frame_or_input(cx: &mut TestAppContext) {
    let (owner, window, controls) = owned_static_canvas(cx);
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::StepShown {
            run: kumochi_protocol::message::RunId(6),
            scene: TWINNED,
            index: 0,
        })
    );
    for _ in 0..3 {
        owner.update(&mut visual, |this, cx| {
            this.overlay
                .window()
                .expect("shown host")
                .drawing
                .update(cx, |_, cx| cx.notify());
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
        owner.read_with(&visual, |this, _| assert!(!this.paint_notice.pending.get()));
    }
    visual.update(|window, _| window.remove_window());
}

#[gpui::test]
fn a_hidden_epoch_cannot_acknowledge_a_success_already_waiting_in_the_defer(
    cx: &mut TestAppContext,
) {
    let (owner, window, controls) = owned_static_canvas(cx);
    let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        owner.update(cx, |this, _| {
            this.presenter.frames().borrow_mut().hide();
            this.overlay
                .window()
                .expect("current host")
                .painting
                .set(false);
            assert!(this.paint_notice.pending.get());
        });
    });
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    owner.read_with(&visual, |this, _| assert!(!this.paint_notice.pending.get()));
    visual.update(|window, _| window.remove_window());
}

#[gpui::test]
fn a_failed_surface_configuration_removes_its_hidden_window(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let frames = Rc::new(RefCell::new(Frames::default()));
    let result = cx.update(|cx| {
        open(
            frames,
            WeakEntity::new_invalid(),
            Rc::new(RefCell::new(super::Scratch::new())),
            Rc::default(),
            cx,
            |_, _| Err(crate::platform::PresenceError::NotWin32),
        )
    });
    assert!(matches!(
        result,
        Err(super::OpenError::Native(
            crate::platform::PresenceError::NotWin32
        ))
    ));
    assert_eq!(
        cx.windows().len(),
        0,
        "a refused setup leaves no hidden host"
    );
}

#[gpui::test]
fn a_retired_hosts_queued_canvas_cannot_read_the_new_hosts_frames(cx: &mut TestAppContext) {
    let adapter = test_fixture::presence(cx);
    for closed in [false, true] {
        let frames = Rc::new(RefCell::new(Frames::default()));
        frames.borrow_mut().shown = true;
        let overlay = cx
            .update(|cx| {
                open(
                    Rc::clone(&frames),
                    WeakEntity::new_invalid(),
                    Rc::new(RefCell::new(super::Scratch::new())),
                    Rc::default(),
                    cx,
                    |_, _| Ok(()),
                )
            })
            .expect("a real overlay constructor");
        let drawing = overlay.drawing.clone();
        let window = overlay.window;
        let (events, _receiver) = async_channel::unbounded();
        adapter.update(cx, |this, cx| {
            let handles = this.exclusion_handles();
            Gpui {
                input: &this.presenter.input,
                input_windows: &mut this.input_windows,
                exclusion: &mut this.exclusion,
                surfaces: super::super::exclusion::Windows {
                    native: &this.native,
                    handles,
                    cx,
                    fixture: None,
                },
                owner: WeakEntity::new_invalid(),
                frames: &frames,
                events: &events,
                scratch: &Rc::new(RefCell::new(super::Scratch::new())),
                notice: &Rc::default(),
            }
            .repaint(&overlay);
        });
        assert!(
            overlay.painting.get(),
            "the adapter first enables the real host"
        );
        let visual = gpui_kit::VisualTestContext::from_window(window, cx);
        visual.simulate_resize(size(px(100.), px(100.)));
        visual.run_until_parked();
        frames.borrow_mut().shown = false;
        adapter.update(cx, |this, cx| {
            let handles = this.exclusion_handles();
            let mut surface = Gpui {
                input: &this.presenter.input,
                input_windows: &mut this.input_windows,
                exclusion: &mut this.exclusion,
                surfaces: super::super::exclusion::Windows {
                    native: &this.native,
                    handles,
                    cx,
                    fixture: None,
                },
                owner: WeakEntity::new_invalid(),
                frames: &frames,
                events: &events,
                scratch: &Rc::new(RefCell::new(super::Scratch::new())),
                notice: &Rc::default(),
            };
            if closed {
                surface.close(overlay).expect("the old host closes");
            } else {
                surface.repaint(&overlay);
            }
        });
        let queued = if closed {
            cx.update(|cx| {
                cx.open_window(gpui_kit::WindowOptions::default(), |_, _| drawing.clone())
            })
            .expect("a queued paint still owns the retired drawing")
            .into()
        } else {
            window
        };
        let mut visual = gpui_kit::VisualTestContext::from_window(queued, cx);
        visual.simulate_resize(size(px(100.), px(100.)));
        frames.borrow_mut().shown = true;
        let held = frames.borrow_mut();
        assert!(held.shown, "another host is eligible to paint");
        drawing.update(&mut visual, |_, cx| cx.notify());
        visual.update(|window, cx| window.draw(cx).clear(cx));
        drop(held);
        visual.update(|window, _| window.remove_window());
    }
}

fn paint(hex: u32, opacity: f32) -> Paint {
    Paint {
        rgb: Rgb::try_from(hex).expect("a packed RGB value"),
        opacity,
    }
}

fn rgba(hex: u32, a: f32) -> Hsla {
    Rgba { a, ..rgb(hex) }.into()
}

const RECT: Rect = Rect {
    origin: Vec2 { x: 1.0, y: 2.0 },
    size: Vec2 { x: 30.0, y: 10.0 },
};

#[test]
fn opacity_multiplies_and_an_override_keeps_the_prim_s_own() {
    assert_eq!(
        color(paint(0x33_66_99, 0.5), None, 0.5),
        rgba(0x33_66_99, 0.25)
    );
    assert_eq!(
        color(paint(0x33_66_99, 0.5), Some(paint(0x00_ff_00, 0.8)), 0.5),
        rgba(0x00_ff_00, 0.2)
    );
    assert_eq!(
        color(paint(0x33_66_99, 1.0), None, 4.0),
        rgba(0x33_66_99, 1.0)
    );
}

#[test]
fn an_upright_whole_rect_is_a_quad_clamped_like_its_outline() {
    let transform = Affine::pose(2.0, 0.0, Vec2::new(10.0, 20.0));
    let ink = rgba(0x33_66_99, 1.0);
    let quad = upright(transform, 1.0, RECT, 8.0, Some(ink), Some((ink, 7.0)))
        .expect("an axis-aligned, fully drawn rect");
    assert_eq!(
        quad.bounds,
        Bounds::new(point(px(12.0), px(24.0)), size(px(60.0), px(20.0)))
    );
    assert_eq!(quad.corner_radii.top_left, px(10.0), "radius 8 clamps to 5");
    assert_eq!(quad.border_widths.top, px(10.0), "width 7 clamps to 5");
    assert_eq!(quad.border_color, ink);
}

#[test]
fn a_turned_mirrored_or_revealing_rect_takes_its_outline_paths() {
    let ink = Some(rgba(0x33_66_99, 1.0));
    for (transform, draw) in [
        (Affine::pose(2.0, 0.01, Vec2::new(10.0, 20.0)), 1.0),
        (Affine([0.0, 2.0, -2.0, 0.0, 100.0, 50.0]), 1.0),
        (Affine([-1.0, 0.0, 0.0, 1.0, 0.0, 0.0]), 1.0),
        (Affine::pose(2.0, 0.0, Vec2::new(10.0, 20.0)), 0.5),
    ] {
        assert!(
            upright(transform, draw, RECT, 8.0, ink, None).is_none(),
            "{transform:?} at {draw}"
        );
    }
}

#[test]
fn a_rotated_label_stays_upright_at_its_turned_anchor_and_scales_uniformly() {
    let quarter_turn = Affine([0.0, 2.0, -2.0, 0.0, 100.0, 50.0]);
    let (anchor, font_size, wrap) = label(quarter_turn, Vec2::new(10.0, 0.0), 12.0, Some(50.0));
    assert_eq!(anchor, point(px(100.0), px(70.0)));
    assert_eq!(font_size, px(24.0));
    assert_eq!(wrap, Some(px(100.0)));
    let (_, _, unwrapped) = label(quarter_turn, Vec2::new(0.0, 0.0), 12.0, None);
    assert_eq!(unwrapped, None);
}

#[test]
fn a_shown_overlay_holds_esc_and_a_hidden_one_does_not() {
    kumochi_scene::deferred::start().expect("the dropper starts");
    let surface = SurfaceGeometry::Overlay {
        default_canvas: [1920.0, 1080.0],
    };
    let presenter = Presenter::new(
        surface,
        Arc::new(Theme::default()),
        Box::new(Stager(size_of::<Stager>())),
    );
    let runs = Runs::default();
    for shown in [false, true, false] {
        presenter.frames().borrow_mut().shown = shown;
        let activity = Presence::activity(&runs, Panel::Closed, &presenter);
        assert_eq!(activity.drawing, shown);
        assert_eq!(activity.holds_global_escape(), shown);
    }
}

struct Fixed;

impl TextMeasure for Fixed {
    fn measure(&mut self, _: &str, _: TextStyle, _: Option<f32>) -> Result<[f32; 2], MeasureError> {
        Ok([8.0, 10.0])
    }
}

/// A real resting frame item, upright, opaque and fully drawn, to carry test primitives.
fn item() -> FrameItem {
    kumochi_scene::deferred::start().expect("the dropper starts");
    let (units, _) = resting(&mut Fixed, [1920.0, 1080.0], Mode::Dry);
    let (mut item, _) = units.into_iter().next().expect("a fixture item");
    item.transform = Affine::default();
    item.opacity = 1.0;
    item.draw = 1.0;
    item.color = None;
    item
}

/// An item's pieces, its fills staged as the overlay stages them.
fn staged(item: &FrameItem, prims: &[Prim]) -> Result<Vec<Pending>, BuildError> {
    let mut scene = PaintScene::of(item.handle, prims);
    assemble(item, 0, prims, &mut scene, &mut super::Scratch::new())?;
    Ok(scene.pending)
}

fn card() -> Prim {
    Prim::Rect {
        rect: Rect {
            origin: Vec2::new(0.0, 0.0),
            size: Vec2::new(40.0, 40.0),
        },
        radius: 4.0,
        fill: Some(paint(0x1d_1b26, 1.0)),
        border: Some((paint(0x3b_82f6, 1.0), 2.0)),
    }
}

#[test]
fn an_item_builds_whole_or_not_at_all() {
    let item = item();
    let triangle = Prim::Fill {
        points: Arc::from([
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(0.0, 10.0),
        ]),
        color: paint(0xef_4444, 1.0),
    };
    let built = staged(&item, &[card(), triangle]).expect("both build");
    assert!(
        matches!(built.as_slice(), [Pending::Quad(quad), Pending::Path(_, _)] if quad.bounds.size.width == px(40.0)),
        "the upright card is a quad and the triangle one path"
    );
    let over = Prim::Fill {
        points: (0..=21_845_u16)
            .map(|x| Vec2::new(f32::from(x), f32::from(x % 2)))
            .collect(),
        color: paint(0xef_4444, 1.0),
    };
    assert_eq!(
        staged(&item, &[card(), over]).map(|built| built.len()),
        Err(BuildError::OverCharge),
        "a fill one point over its limit leaves no piece of the item"
    );
}

#[test]
fn a_fill_past_an_item_s_first_primitive_finds_its_own_staged_entry() {
    let item = item();
    let triangle = Prim::Fill {
        points: Arc::from([
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(0.0, 10.0),
        ]),
        color: paint(0xef_4444, 1.0),
    };
    let prims = [card(), triangle];
    let mut scene = PaintScene::of(item.handle, &prims);
    assemble(
        &item,
        1,
        &prims[1..],
        &mut scene,
        &mut super::Scratch::new(),
    )
    .expect("the fill at primitive 1");
    assert!(
        matches!(scene.pending.as_slice(), [Pending::Path(_, _)]),
        "one path from the staged triangle"
    );
    assert_eq!(
        assemble(
            &item,
            0,
            &prims[1..],
            &mut scene,
            &mut super::Scratch::new()
        )
        .map(|()| scene.pending.len()),
        Err(BuildError::Stale),
        "primitive 0 is the card, which has no staged fill"
    );
}

#[test]
fn the_deepest_turned_chain_builds_its_fallback_within_its_charge() {
    // The scene's depth-limit chain: fifteen turned diagrams, four scaling by 2, around a rect
    // whose radius puts the overlay's 32 just under ten pieces, so the allowance charges eleven.
    let radius = f64::from(2.534_332_f32);
    let mut put = vec![r#"{"diagram":{"id":"g0"}}"#.to_owned()];
    put.extend((1..15).map(|n| format!(r#"{{"diagram":{{"id":"g{n}","in":"g{}"}}}}"#, n - 1)));
    put.extend(
        (0..15)
            .map(|n| format!(r#"{{"anim":{{"target":"g{n}","track":"rotate","keys":[[0,7]]}}}}"#)),
    );
    put.extend(
        (0..4).map(|n| format!(r#"{{"anim":{{"target":"g{n}","track":"scale","keys":[[0,2]]}}}}"#)),
    );
    put.push(format!(r#"{{"shape":{{"id":"r","in":"g14","kind":"rect","box":[0,0,8,8],"radius":{radius},"fill":"blue"}}}}"#));
    let record = format!(
        r#"{{"step":{{"id":"chain","start":"now","put":[{}]}}}}"#,
        put.join(",")
    );
    let now = Instant::now();
    let mut presenter = begun_at(Box::new(Stager(size_of::<Stager>())), now);
    assert!(presenter.set_stage(Rect {
        origin: Vec2::new(0.0, 0.0),
        size: Vec2::new(3840.0, 2160.0),
    }));
    prepare_canvas_step_at(&mut presenter, 0, record.as_bytes(), now);
    assert!(presenter.advance(1, now).expect("activate").is_empty());
    let (sample, _) = presenter.sample(60_000);
    assert!(sample.visible);
    let failed = presenter.frames().borrow().paint(|frame, _| {
        let (item, _, prims) = frame
            .paint_items()
            .find(|(_, _, prims)| matches!(prims, [Prim::Rect { .. }]))
            .expect("the rect");
        let m = item.transform.0;
        let posed = (m[0] * m[3] - m[1] * m[2]).abs().sqrt();
        assert!((posed - 32.0).abs() < 1e-3, "placed at 2, posed at {posed}");
        let [Prim::Rect { rect, radius, .. }] = prims else {
            panic!("one rect");
        };
        assert_eq!(
            fallback_corner_segments(*rect, *radius, item.paint_scale),
            11
        );
        let mut half = item.clone();
        half.draw = 0.5;
        let built = staged(&half, prims).expect("the fallback builds within its charge");
        assert!(
            matches!(built.as_slice(), [Pending::Path(..), Pending::Path(..)]),
            "the turned, half-drawn rect's fill and border"
        );
        Ok::<(), BuildError>(())
    });
    assert!(!failed);
}

#[test]
fn an_item_that_cannot_show_builds_nothing() {
    let shown = item();
    assert_eq!(staged(&shown, &[card()]).map(|built| built.len()), Ok(1));
    for opacity in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut item = shown.clone();
        item.opacity = opacity;
        assert_eq!(
            staged(&item, &[card()]).map(|built| built.len()),
            Ok(0),
            "opacity {opacity}"
        );
    }
    for draw in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut item = shown.clone();
        item.draw = draw;
        assert_eq!(
            staged(&item, &[card()]).map(|built| built.len()),
            Ok(0),
            "draw {draw}"
        );
    }
}

#[test]
fn retained_pending_keeps_suffix_indices_and_frees_earlier_handoffs_on_refusal() {
    use super::{Pending, Scratch, assemble};
    let item = item();
    let fill = Prim::Fill {
        points: Arc::from([
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(0.0, 10.0),
        ]),
        color: paint(0xff_0000, 1.0),
    };
    let label = Prim::Text {
        origin: Vec2::default(),
        text: kumochi_scene::text::Text::try_from("T".to_owned()).expect("a label"),
        style: TextStyle {
            size: 12.0,
            bold: false,
        },
        color: paint(0, 1.0),
        max_width: None,
        align: kumochi_scene::record::Align::Left,
    };
    let prims = [card(), fill, label];
    let mut scene = PaintScene::of(item.handle, &prims);
    let mut scratch = Scratch::new();
    let capacity = scene.pending.capacity();
    assemble(&item, 1, &prims[1..], &mut scene, &mut scratch).expect("the indexed suffix");
    assert!(matches!(
        scene.pending.as_slice(),
        [Pending::Path(_, _), Pending::Text(0)]
    ));
    assert_eq!(scene.pending.capacity(), capacity);
    scene.pending.clear();
    let (result, requests) = crate::allocation::requests(100, || {
        let _app = super::paint_phase::enter(super::Phase::App);
        assemble(
            &item,
            1,
            &[prims[1].clone(), prims[1].clone()],
            &mut scene,
            &mut scratch,
        )
    });
    assert_eq!(
        result,
        Err(BuildError::Stale),
        "the second uncached fill is terminal for this item"
    );
    assert_eq!(scene.pending.len(), 0);
    assert_eq!(scene.pending.capacity(), capacity);
    let handoffs: Vec<_> = requests.iter().filter(|r| r.action == 4).collect();
    assert_eq!(
        handoffs.len(),
        1,
        "the valid first fill precedes the refusal"
    );
    assert_eq!(handoffs[0].size, 96);
    let freed: Vec<_> = requests
        .iter()
        .filter(|r| r.action == 1 && r.pointer == handoffs[0].pointer)
        .collect();
    assert_eq!(
        freed.len(),
        1,
        "refusal frees the earlier pending handoff once"
    );
    assert_eq!(freed[0].size, 96);
    assert_eq!(super::paint_phase::tests::assert_paths(&requests, 1), 1);
}

#[gpui::test]
fn a_warm_real_element_draw_has_no_boxed_application_callback(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let presenter = begun(Box::new(Stager(size_of::<Stager>())));
    let frames = presenter.frames();
    frames.borrow_mut().shown = true;
    let overlay = cx
        .update(|cx| {
            open(
                Rc::clone(&frames),
                WeakEntity::new_invalid(),
                Rc::new(RefCell::new(super::Scratch::new())),
                Rc::default(),
                cx,
                |_, _| Ok(()),
            )
        })
        .expect("a synthetic host");
    overlay.painting.set(true);
    let mut visual = gpui_kit::VisualTestContext::from_window(overlay.window, cx);
    visual.simulate_resize(size(px(100.0), px(100.0)));
    visual.run_until_parked();
    let ((), requests) = crate::allocation::requests(10_000, || {
        visual.update(|window, cx| window.draw(cx).clear(cx));
    });
    assert_eq!(
        requests.iter().filter(|r| r.action == 5).count(),
        1,
        "one actual paint before claiming zero allocations"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.action == 0 && r.tag == 1)
            .count(),
        0,
        "the empty warm draw has no handoffs or canvas box"
    );
    visual.update(|window, _| window.remove_window());
}

const TWINNED: SceneId = SceneId(1);

/// Stages each cache from a twin of the view it is given: the same layout at the same bound,
/// under another generation. It keeps each twin's generation, so its count shows where that
/// cache lives.
struct Twin(Rc<RefCell<Vec<Arc<()>>>>);

impl Stage for Twin {
    fn fixed_bytes(&self) -> usize {
        0
    }

    fn stage(&self, view: &SceneView) -> Box<dyn SceneCache> {
        let twin = SceneView::new(view.layout(), view.activated());
        self.0.borrow_mut().push(Arc::clone(twin.generation()));
        Box::new(PaintScene::staged(&twin))
    }
}

/// A connected presenter with one new scene whose frame is sampled, its caches staged by
/// `stage`.
fn begun(stage: Box<dyn Stage>) -> Presenter {
    begun_at(stage, Instant::now())
}

fn begun_at(stage: Box<dyn Stage>, now: Instant) -> Presenter {
    kumochi_scene::deferred::start().expect("the dropper starts");
    let surface = SurfaceGeometry::Overlay {
        default_canvas: [1920.0, 1080.0],
    };
    let mut presenter = Presenter::new(surface, Arc::new(Theme::default()), stage);
    presenter.connected(1);
    let effects = presenter
        .described(1, &Bounded::default(), now)
        .expect("empty membership");
    assert!(effects.is_empty());
    let record = br#"{"begin":{"v":1,"scene":"twin","surface":"overlay","alt":"test"}}"#;
    let Ok(Record::Begin(begin)) = parse_record(record) else {
        panic!("a begin record");
    };
    let header = SceneHeader {
        begin,
        layout_version: 2,
    };
    let frame = DataToShell::SceneBegin {
        scene: TWINNED,
        header,
    };
    presenter
        .data(1, Deferred::new(frame, 0), 0, now)
        .expect("a new scene");
    let (sample, effects) = presenter.sample(0);
    assert!(effects.is_empty());
    assert!(
        !sample.visible,
        "an empty scene paints no item, but its frame binds"
    );
    presenter
}

/// Opens a real overlay over `frames`, enables it as the adapter does, draws it once and runs
/// what its canvas deferred for `owner`, then removes the window.
fn draw_once(frames: &Rc<RefCell<Frames>>, owner: WeakEntity<Presence>, cx: &mut TestAppContext) {
    let adapter = owner
        .upgrade()
        .unwrap_or_else(|| test_fixture::presence(cx));
    frames.borrow_mut().shown = true;
    let overlay = cx
        .update(|cx| {
            open(
                Rc::clone(frames),
                owner.clone(),
                Rc::new(RefCell::new(super::Scratch::new())),
                Rc::default(),
                cx,
                |_, _| Ok(()),
            )
        })
        .expect("a real overlay constructor");
    let (events, _receiver) = async_channel::unbounded();
    adapter.update(cx, |this, cx| {
        let handles = this.exclusion_handles();
        Gpui {
            input: &this.presenter.input,
            input_windows: &mut this.input_windows,
            exclusion: &mut this.exclusion,
            surfaces: super::super::exclusion::Windows {
                native: &this.native,
                handles,
                cx,
                fixture: None,
            },
            owner,
            frames,
            events: &events,
            scratch: &Rc::new(RefCell::new(super::Scratch::new())),
            notice: &Rc::default(),
        }
        .repaint(&overlay);
    });
    assert!(overlay.painting.get(), "the adapter enables the real host");
    let mut visual = gpui_kit::VisualTestContext::from_window(overlay.window, cx);
    visual.simulate_resize(size(px(100.), px(100.)));
    visual.update(|window, cx| window.draw(cx).clear(cx));
    visual.run_until_parked();
    visual.update(|window, _| window.remove_window());
}

/// The scenes `paint_failed` clears, one list per clear.
fn cleared(presenter: &mut Presenter) -> Vec<Vec<SceneId>> {
    presenter
        .paint_failed()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Control(ControlToBrain::SceneFailed { scene }) => Some(vec![scene]),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn the_real_canvas_paints_a_cache_staged_from_its_frame_s_view(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (mut presenter, scene) =
        crate::presenter::tests::painter_fixture(Box::new(Stager(size_of::<Stager>())));
    let frames = presenter.frames();
    draw_once(&frames, WeakEntity::new_invalid(), cx);
    assert_eq!(
        frames.borrow().painted(scene).map(|painted| painted.bound),
        Some(1),
        "the canvas painted the sampled bound"
    );
    assert_eq!(cleared(&mut presenter), Vec::<Vec<SceneId>>::new());
    assert!(presenter.holds_scenes());
}

#[gpui::test]
fn the_real_canvas_fails_a_cache_staged_from_another_view_for_good(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let staged = Rc::new(RefCell::new(Vec::new()));
    let mut presenter = begun(Box::new(Twin(Rc::clone(&staged))));
    assert_eq!(staged.borrow().len(), 1, "one cache, for the new scene");
    assert_eq!(
        Arc::strong_count(&staged.borrow()[0]),
        2,
        "the twin's cache is held only in the scene's frame slot"
    );
    let frames = presenter.frames();
    for draw in 0..2 {
        draw_once(&frames, WeakEntity::new_invalid(), cx);
        assert_eq!(frames.borrow().painted(TWINNED), None, "draw {draw}");
        assert!(presenter.holds_scenes(), "only the owner releases it");
    }
    assert_eq!(cleared(&mut presenter), [vec![TWINNED]]);
    assert_eq!(
        cleared(&mut presenter),
        Vec::<Vec<SceneId>>::new(),
        "a failed scene is released once"
    );
    assert!(!presenter.holds_scenes());
    assert_eq!(staged.borrow().len(), 1, "nothing staged again");
    assert!(
        drained(&staged.borrow()[0], 1),
        "its cache went to the dropper with the scene"
    );
}

#[gpui::test]
fn a_canvas_refusal_reaches_the_presence_that_releases_its_scene(cx: &mut TestAppContext) {
    let presence = test_fixture::presence(cx);
    let staged = Rc::new(RefCell::new(Vec::new()));
    let presenter = begun(Box::new(Twin(Rc::clone(&staged))));
    let frames = presenter.frames();
    presence.update(cx, |this, _| this.presenter = presenter);
    draw_once(&frames, presence.downgrade(), cx);
    assert!(
        !presence.read_with(cx, |this, _| this.presenter.holds_scenes()),
        "the owner released the scene its canvas refused"
    );
    assert!(
        drained(&staged.borrow()[0], 1),
        "its cache went to the dropper with the scene"
    );
}

#[gpui::test]
fn a_real_geometry_bearing_canvas_observes_its_prepared_bound(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (presenter, scene) =
        crate::presenter::tests::painter_fixture(Box::new(Stager(size_of::<Stager>())));
    let frames = presenter.frames();
    let mut kinds = [0; 3];
    assert!(!frames.borrow().paint(|frame, _| {
        for (_, _, prims) in frame.paint_items() {
            for prim in prims {
                kinds[match prim {
                    Prim::Fill { .. } => 0,
                    Prim::Line { .. } => 1,
                    Prim::Text { .. } => 2,
                    Prim::Rect { .. } => panic!("no rect in this fixture"),
                }] += 1;
            }
        }
        Ok::<_, BuildError>(())
    }));
    assert_eq!(
        kinds,
        [1, 2, 0],
        "one static fill, two real strokes and no text"
    );
    draw_once(&frames, WeakEntity::new_invalid(), cx);
    assert_eq!(
        frames.borrow().painted(scene).map(|painted| painted.bound),
        Some(1)
    );
}

#[gpui::test]
fn a_warm_geometry_bearing_element_allocates_only_its_three_handoffs(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let scratch = Rc::new(RefCell::new(super::Scratch::new()));
    let stage = Box::new(Stager::new(&scratch));
    let (presenter, scene) = crate::presenter::tests::painter_fixture(stage);
    let frames = presenter.frames();
    frames.borrow_mut().shown = true;
    let overlay = cx
        .update(|cx| {
            open(
                Rc::clone(&frames),
                WeakEntity::new_invalid(),
                Rc::clone(&scratch),
                Rc::default(),
                cx,
                |_, _| Ok(()),
            )
        })
        .expect("a synthetic geometry host");
    overlay.painting.set(true);
    let mut visual = gpui_kit::VisualTestContext::from_window(overlay.window, cx);
    visual.simulate_resize(size(px(100.0), px(100.0)));
    visual.run_until_parked();
    let ((), events) = crate::allocation::requests(16_384, || {
        visual.update(|window, cx| window.draw(cx).clear(cx));
    });
    assert_eq!(
        events.iter().filter(|event| event.action == 5).count(),
        1,
        "one real paint, not an empty counter interval"
    );
    assert_eq!(
        super::paint_phase::tests::assert_paths(&events, 3),
        3,
        "the fill, its outline and the dashed line"
    );
    assert_eq!(
        frames.borrow().painted(scene).map(|painted| painted.bound),
        Some(1)
    );
    visual.update(|window, _| window.remove_window());
}
