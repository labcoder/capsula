//! The paint cache: a staged fill paints at its requested placement, its charge is what
//! staging leaves allocated, and it paints only for the view it was staged from.

use std::{
    any::Any,
    sync::{Arc, atomic::AtomicBool},
};

use gpui_kit::{Bounds, Path, Pixels, Point, point, px};
use kumochi_scene::{
    Admission, GlobalBudget, Handle, Record, SceneState, SceneView, WireId,
    display::{MeasureError, Paint, Prim, TextMeasure, TextStyle},
    geom::{Affine, Vec2},
    layout::{LayoutState, PrepareInput, SurfaceGeometry, prepare},
    parse_record,
    record::Rgb,
    theme::Theme,
};

use super::super::paint_paths::BuildError;
use super::{FillEntry, PaintScene, Pending, Stager, bound};
use crate::presenter::{SceneCache, Stage};

/// A self-crossing star, a triangle with an outline, and a rect.
const DRAWING: &str = r#"[{"shape":{"id":"star","kind":"polygon","points":[[800,700],[860,880],[700,770],[900,770],[740,880]],"fill":"red"}},{"shape":{"id":"tri","kind":"polygon","points":[[20,20],[50,20],[20,50]],"fill":"blue","color":"green","width":2}},{"shape":{"id":"box","kind":"rect","box":[0,0,10,10],"fill":"blue"}}]"#;

/// One filled triangle, which lyon covers with exactly one triangle.
const TRIANGLE: &str =
    r#"[{"shape":{"id":"tri","kind":"polygon","points":[[20,20],[50,20],[20,50]],"fill":"blue"}}]"#;

/// A label, which has no fill.
const LABEL: &str = r#"[{"text":{"id":"t","text":"T","at":[0,0]}}]"#;

/// A 3D view and a solid, put after the drawing's objects.
const SOLID: &str =
    r#"{"view3d":{"id":"v"}},{"solid":{"in":"v","id":"c","shape":"box","size":[2,2,2]}}"#;

/// A fill whose id and coordinates are markers no printed cache may carry.
const MARKED: &str = r#"[{"shape":{"id":"zqxprivate","kind":"polygon","points":[[13337.25,10],[13400,10],[13337.25,90]],"fill":"blue"}}]"#;

struct Fixed;

impl TextMeasure for Fixed {
    fn measure(&mut self, _: &str, _: TextStyle, _: Option<f32>) -> Result<[f32; 2], MeasureError> {
        Ok([8.0, 10.0])
    }
}

/// A cache another stage made.
struct Foreign;

impl SceneCache for Foreign {
    fn bytes(&self) -> usize {
        0
    }
}

/// The resting view of one step putting `put`, the first on its scene.
fn view(put: &str) -> SceneView {
    let header = br#"{"begin":{"v":1,"scene":"cache","surface":"overlay","alt":"cache test"}}"#;
    let Ok(Record::Begin(begin)) = parse_record(header) else {
        panic!("a begin record");
    };
    let source = format!(r#"{{"step":{{"id":"cache","put":{put}}}}}"#);
    let Ok(Record::Step(step)) = parse_record(source.as_bytes()) else {
        panic!("a step record");
    };
    let mut budget = GlobalBudget::default();
    let mut state = SceneState::begin(begin).expect("a scene");
    let empty = LayoutState::new(&state);
    let Admission::New(admitted) = state.admit(&step, &budget).expect("a legal step") else {
        panic!("a new step");
    };
    let committed = state.commit(admitted, &mut budget).expect("a commit");
    let Ok(input) = PrepareInput::new(
        committed.preparation,
        empty,
        SurfaceGeometry::default(),
        Arc::new(Theme::default()),
    ) else {
        panic!("the first step follows the empty layout");
    };
    let rest = prepare(input, &mut Fixed, &AtomicBool::new(false))
        .expect("a prepared step")
        .after;
    SceneView::new(&rest, state.activated())
}

/// Every fill of the view, with its object and primitive index.
fn fills(view: &SceneView) -> Vec<(Handle, usize, Prim)> {
    let mut fills = Vec::new();
    for (handle, prims) in view.paint_sources() {
        for (index, prim) in prims.iter().enumerate() {
            if matches!(prim, Prim::Fill { .. }) {
                fills.push((handle, index, prim.clone()));
            }
        }
    }
    fills
}

fn ink() -> Paint {
    Paint::from(Rgb::try_from(0x33_66_99).expect("a packed RGB value"))
}

/// A fill one point over `FILL_VERTEX_LIMIT`, which no preparation admits.
fn over() -> Prim {
    Prim::Fill {
        points: (0..=21_845_u16)
            .map(|x| Vec2::new(f32::from(x), f32::from(x % 2)))
            .collect(),
        color: ink(),
    }
}

/// Requested bytes still allocated after `make`, by the allocator: the oracle every charge is
/// checked against.
fn live<T>(make: impl FnOnce() -> T) -> (T, usize) {
    let (value, _, [allocated, released]) = crate::allocation::usage(make);
    (value, allocated - released)
}

#[test]
fn a_cache_paints_only_for_the_view_generation_it_was_staged_from() {
    let first = view(DRAWING);
    let second = view(DRAWING);
    let mut cache = Stager(size_of::<Stager>()).stage(&first);
    let any: &dyn Any = &*cache;
    let staged = std::ptr::from_ref(any.downcast_ref::<PaintScene>().expect("a paint scene"));
    let scene = bound(&mut *cache, first.generation()).expect("its own view");
    assert_eq!(std::ptr::from_ref(&*scene), staged);
    let (handle, ..) = fills(&first)[0];
    assert_eq!(
        scene.fill(handle, 99, Affine::default()).err(),
        Some(BuildError::Stale),
        "a primitive the view never had"
    );
    assert_eq!(
        bound(&mut *cache, second.generation()).err(),
        Some(BuildError::Stale),
        "an identical drawing in another view"
    );
    assert_eq!(
        bound(&mut Foreign, first.generation()).err(),
        Some(BuildError::Stale),
        "another stage's cache"
    );
}

fn covers(path: &Path<Pixels>, x: f32, y: f32) -> bool {
    path.vertices.as_chunks::<3>().0.iter().any(|triangle| {
        let sides = std::array::from_fn::<_, 3, _>(|edge| {
            let start = triangle[edge].xy_position;
            let end = triangle[(edge + 1) % 3].xy_position;
            let dx = f32::from(end.x - start.x);
            let dy = f32::from(end.y - start.y);
            dx * (y - f32::from(start.y)) - dy * (x - f32::from(start.x))
        });
        sides.iter().any(|side| *side != 0.0)
            && (sides.iter().all(|side| *side >= 0.0) || sides.iter().all(|side| *side <= 0.0))
    })
}

#[test]
fn a_staged_fill_places_its_corners_and_bounds() {
    let drawing = view(
        r#"[{"shape":{"id":"tri","kind":"polygon","points":[[0,0],[10,0],[0,10]],"fill":"blue"}}]"#,
    );
    let handle = drawing.paint_sources().next().expect("the triangle").0;
    let scene = PaintScene::staged(&drawing);
    for (transform, corners, (left, top, right, bottom), inside, outside) in [
        (
            Affine::default(),
            [(0.0, 0.0), (10.0, 0.0), (0.0, 10.0)],
            (0.0, 0.0, 10.0, 10.0),
            (2.0, 2.0),
            (8.0, 8.0),
        ),
        (
            Affine([1.5, 0.0, 0.0, 1.5, 20.0, -7.25]),
            [(20.0, -7.25), (35.0, -7.25), (20.0, 7.75)],
            (20.0, -7.25, 35.0, 7.75),
            (23.0, -4.25),
            (32.0, 4.75),
        ),
        (
            Affine([-1.0, 0.0, 0.0, 1.0, 300.0, 0.0]),
            [(300.0, 0.0), (290.0, 0.0), (300.0, 10.0)],
            (290.0, 0.0, 300.0, 10.0),
            (298.0, 2.0),
            (292.0, 8.0),
        ),
        (
            Affine([0.75, 0.5, -0.5, 0.75, 13.0, 5.5]),
            [(13.0, 5.5), (20.5, 10.5), (8.0, 13.0)],
            (8.0, 5.5, 20.5, 13.0),
            (13.5, 8.0),
            (18.0, 12.5),
        ),
    ] {
        let path = scene
            .fill(handle, 0, transform)
            .expect("the staged triangle")
            .expect("the triangle covers a region");
        for (x, y) in corners {
            assert!(
                path.vertices
                    .iter()
                    .any(|v| v.xy_position == point(px(x), px(y))),
                "{transform:?} is missing triangle corner ({x}, {y})"
            );
        }
        assert_eq!(
            path.bounds,
            Bounds::from_corners(point(px(left), px(top)), point(px(right), px(bottom)))
        );
        assert!(
            covers(&path, inside.0, inside.1),
            "{transform:?} must fill {inside:?}"
        );
        assert!(
            !covers(&path, outside.0, outside.1),
            "{transform:?} must not fill {outside:?}"
        );
    }
}

#[test]
fn a_staged_fill_skips_collinear_points_and_refuses_an_over_limit_polygon() {
    let handle = Handle::new(1, 0).expect("a handle");
    let flat = Prim::Fill {
        points: Arc::from([
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(20.0, 0.0),
        ]),
        color: ink(),
    };
    let scene = PaintScene::of(handle, &[flat, over()]);
    assert!(
        matches!(scene.fill(handle, 0, Affine::default()), Ok(None)),
        "collinear points emit no path"
    );
    assert_eq!(
        scene.fill(handle, 1, Affine::default()).err(),
        Some(BuildError::OverCharge)
    );
}

#[test]
fn a_view_with_three_dimensional_records_stages_only_its_objects_fills() {
    let mixed = view(&format!("[{},{SOLID}]", &DRAWING[1..DRAWING.len() - 1]));
    let scene = PaintScene::staged(&mixed);
    let layout = mixed.layout();
    for (name, transform, (left, top, right, bottom), inside, outside) in [
        (
            "star",
            Affine::default(),
            (700.0, 700.0, 900.0, 880.0),
            (800.0, 800.0),
            (710.0, 710.0),
        ),
        (
            "star",
            Affine([1.5, 0.0, 0.0, 1.5, 20.0, -7.25]),
            (1070.0, 1042.75, 1370.0, 1312.75),
            (1220.0, 1192.75),
            (1085.0, 1057.75),
        ),
        (
            "star",
            Affine([-1.0, 0.0, 0.0, 1.0, 300.0, 0.0]),
            (-600.0, 700.0, -400.0, 880.0),
            (-500.0, 800.0),
            (-410.0, 710.0),
        ),
        (
            "star",
            Affine([0.75, 0.5, -0.5, 0.75, 13.0, 5.5]),
            (128.0, 930.5, 303.0, 1095.5),
            (213.0, 1005.5),
            (190.5, 893.0),
        ),
        (
            "tri",
            Affine::default(),
            (20.0, 20.0, 50.0, 50.0),
            (25.0, 25.0),
            (45.0, 45.0),
        ),
    ] {
        let id = WireId::try_from(name.to_owned()).expect("a model id");
        let handle = layout.handle(&id).expect("the flat shape");
        let path = scene
            .fill(handle, 0, transform)
            .expect("a staged fill")
            .expect("the flat shape still fills in a mixed view");
        assert_eq!(
            path.bounds,
            Bounds::from_corners(point(px(left), px(top)), point(px(right), px(bottom))),
            "the {name} still fills in a mixed view at {transform:?}"
        );
        assert!(covers(&path, inside.0, inside.1), "inside the {name}");
        assert!(!covers(&path, outside.0, outside.1), "outside the {name}");
    }
    let view_id = WireId::try_from("v".to_owned()).expect("a model id");
    let volume = &layout.views()[&view_id];
    let box_id = WireId::try_from("box".to_owned()).expect("a model id");
    let rect = layout.handle(&box_id).expect("the rect");
    let triangle_id = WireId::try_from("tri".to_owned()).expect("a model id");
    let triangle = layout.handle(&triangle_id).expect("the triangle");
    for (handle, index) in [
        (volume.handle, 0),
        (volume.solids[0].handle, 0),
        (rect, 0),
        (triangle, 1),
    ] {
        assert_eq!(
            scene.fill(handle, index, Affine::default()).err(),
            Some(BuildError::Stale)
        );
    }
}

#[test]
fn a_cache_charges_exactly_what_staging_leaves_allocated_and_frees_it_on_drop() {
    let scene = size_of::<PaintScene>();
    let entry = size_of::<FillEntry>();
    let corner = size_of::<Vec2>();
    for (put, by_hand) in [
        (
            LABEL,
            Some(scene + size_of::<super::LabelEntry>() + size_of::<Pending>()),
        ),
        (
            TRIANGLE,
            Some(scene + entry + 3 * corner + 2 * size_of::<Pending>()),
        ),
        (DRAWING, None),
    ] {
        let view = view(put);
        let (cache, held) = live(|| Stager(size_of::<Stager>()).stage(&view));
        assert_eq!(
            cache.bytes(),
            held,
            "fill and pending rows against the allocator"
        );
        if let Some(by_hand) = by_hand {
            assert_eq!(held, by_hand, "the ledger's rows by hand");
        }
        let ((), _, [_, released]) = crate::allocation::usage(|| drop(cache));
        assert_eq!(released, held, "retirement frees exactly the charge");
    }
    let handle = Handle::new(1, 0).expect("a handle");
    let prims = [over()];
    let (refused, held) = live(|| Box::new(PaintScene::of(handle, &prims)));
    let generation = 2 * size_of::<usize>();
    assert_eq!(
        held,
        scene + entry + generation + size_of::<Pending>(),
        "a refused fill keeps no corners"
    );
    assert_eq!(
        refused.bytes(),
        held - generation,
        "the test-only generation is uncharged"
    );
    let (stage, counts) =
        crate::allocation::activity(|| -> Box<dyn Stage> { Box::new(Stager(size_of::<Stager>())) });
    assert_eq!(
        counts,
        [1, 0],
        "row 8: the non-ZST boxed stage allocates once"
    );
    drop(stage);
}

#[test]
fn the_fixed_charge_matches_the_scratch_and_non_zst_stage_allocations() {
    use super::super::paint_stroke::{RUN_POINTS, Scratch, VERTICES};
    use std::{cell::RefCell, rc::Rc};
    let ((scratch, stage), held) = live(|| {
        let scratch = Rc::new(RefCell::new(Scratch::new()));
        let stage = Box::new(Stager::new(&scratch));
        (scratch, stage)
    });
    let literal = 2 * size_of::<usize>()
        + size_of::<RefCell<Scratch>>()
        + size_of::<Stager>()
        + VERTICES * size_of::<lyon::math::Point>()
        + RUN_POINTS * size_of::<Point<Pixels>>();
    let literal =
        literal + (2 * size_of::<usize>() + size_of::<Vec<(String, u32)>>()).next_multiple_of(8);
    assert_eq!(
        stage.fixed_bytes(),
        literal + super::super::paint_geometry::FILL_SCRATCH_BYTES
    );
    assert!(held <= stage.fixed_bytes());
    let ((), _, [_, released]) = crate::allocation::usage(|| {
        drop(scratch);
        drop(stage);
    });
    assert_eq!(released, held);
}

#[test]
fn a_cache_prints_counts_and_no_geometry() {
    let printed = format!("{:?}", PaintScene::staged(&view(MARKED)));
    assert!(
        printed.starts_with("PaintScene { fills: 1, bytes: "),
        "{printed}"
    );
    let listed = format!("{:?}", printed.as_bytes());
    for marker in ["13337", "13400", "zqxprivate"] {
        let marker_bytes = format!("{:?}", marker.as_bytes());
        assert!(!printed.contains(marker), "{printed}");
        let marker_bytes = marker_bytes.trim_matches(['[', ']']);
        assert!(!listed.contains(marker_bytes), "{listed}");
    }
}

#[test]
fn retained_short_and_long_label_text_is_charged_once_and_freed() {
    for (text, payload) in [
        ("abcdefghijklmnopqrstuvw", 0),
        ("abcdefghijklmnopqrstuvwx", 40),
        ("                                ", 0),
    ] {
        let source = format!(r#"[{{"text":{{"id":"t","text":"{text}","at":[0,0]}}}}]"#);
        let view = view(&source);
        let expected = size_of::<PaintScene>()
            + size_of::<super::LabelEntry>()
            + size_of::<Pending>()
            + payload;
        let (cache, held) = live(|| Stager(size_of::<Stager>()).stage(&view));
        assert_eq!(held, expected);
        assert_eq!(cache.bytes(), expected);
        let ((), _, [_, freed]) = crate::allocation::usage(|| drop(cache));
        assert_eq!(freed, expected);
    }
    let label = super::LabelEntry::new(
        Handle::new(1, 0).expect("handle"),
        0,
        "private-label-sentinel",
        false,
    );
    let printed = format!("{label:?}");
    assert!(!printed.contains("private-label-sentinel"));
    assert!(!printed.contains("112, 114, 105, 118, 97, 116, 101"));
}

#[gpui::test]
fn retained_layouts_cover_hard_soft_and_long_lines(cx: &mut gpui_kit::TestAppContext) {
    cx.update(|cx| {
        let text_system = gpui_kit::WindowTextSystem::new(Arc::clone(cx.text_system()));
        let mut fonts = super::Fonts::new();
        let family = gpui_kit::SharedString::new_static(".SystemUIFont");
        for (text, wrap, lengths, widths, boundaries) in [
            ("", None, vec![0], vec![0.0], vec![vec![]]),
            ("😀", None, vec![4], vec![12.0], vec![vec![]]),
            ("ab", None, vec![2], vec![12.0], vec![vec![]]),
            (
                "ab\ncd",
                None,
                vec![2, 2],
                vec![12.0, 12.0],
                vec![vec![], vec![]],
            ),
            ("abcd", Some(12.0), vec![4], vec![24.0], vec![vec![2]]),
            ("ab cd", Some(18.0), vec![5], vec![30.0], vec![vec![3]]),
            (
                "abcdefghijklmnopqrstuvwxyz",
                None,
                vec![26],
                vec![156.0],
                vec![vec![]],
            ),
            (
                "abcdefghijklmnopqrstuvwxyz",
                Some(60.0),
                vec![26],
                vec![156.0],
                vec![vec![10, 20]],
            ),
        ] {
            let mut entry =
                super::LabelEntry::new(Handle::new(1, 0).expect("handle"), 0, text, false);
            let shaped = entry
                .shape(
                    &mut fonts,
                    &family,
                    &text_system,
                    gpui_kit::px(10.0),
                    wrap.map(gpui_kit::px),
                    1.0,
                )
                .expect("the synthetic label shapes");
            assert_eq!(
                shaped
                    .lines
                    .iter()
                    .map(gpui_kit::WrappedLine::len)
                    .collect::<Vec<_>>(),
                lengths
            );
            assert_eq!(
                shaped
                    .lines
                    .iter()
                    .map(|line| f32::from(line.unwrapped_layout.width))
                    .collect::<Vec<_>>(),
                widths
            );
            assert_eq!(
                shaped
                    .lines
                    .iter()
                    .map(|line| line
                        .wrap_boundaries
                        .iter()
                        .map(|boundary| {
                            assert_eq!(boundary.run_ix, 0);
                            boundary.glyph_ix
                        })
                        .collect::<Vec<_>>())
                    .collect::<Vec<_>>(),
                boundaries
            );
            assert_eq!(shaped.line_height, gpui_kit::px(13.0));
        }
    });
}

#[gpui::test]
fn retained_layout_keys_and_recreated_hosts_preserve_warm_storage(
    cx: &mut gpui_kit::TestAppContext,
) {
    cx.update(|cx| {
        use gpui_kit::{SharedString, WindowTextSystem, px};
        let text_system = WindowTextSystem::new(Arc::clone(cx.text_system()));
        let mut fonts = super::Fonts::new();
        assert!(Arc::ptr_eq(
            &fonts.0[0].font.features.0,
            &fonts.0[1].font.features.0
        ));
        let family = SharedString::new_static(".SystemUIFont");
        let mut entry = super::LabelEntry::new(
            Handle::new(1, 0).expect("handle"),
            0,
            "private-shaped-label",
            false,
        );
        entry
            .shape(&mut fonts, &family, &text_system, px(10.0), None, 1.0)
            .expect("cold shape");
        let mut bold = super::LabelEntry::new(Handle::new(1, 0).expect("handle"), 1, "bold", true);
        bold.shape(&mut fonts, &family, &text_system, px(10.0), None, 1.0)
            .expect("bold shape");
        assert!(fonts.0[1].id.is_some());
        let recreated = WindowTextSystem::new(Arc::clone(cx.text_system()));
        for text_system in [&text_system, &recreated] {
            let ((), counts) = crate::allocation::activity(|| {
                let shaped = entry
                    .shape(&mut fonts, &family, text_system, px(10.0), None, 1.0)
                    .expect("warm shape");
                assert_eq!(
                    (shaped.font_gen, shaped.size, shaped.wrap, shaped.scale),
                    (1, px(10.0), None, 1.0_f32.to_bits())
                );
            });
            assert_eq!(counts, [0, 0]);
        }
        for (family, size, wrap, scale, generation) in [
            (family.clone(), 11.0, None, 1.0, 1),
            (family.clone(), 11.0, Some(12.0), 1.0, 1),
            (family, 11.0, Some(12.0), 2.0, 1),
            (
                SharedString::new_static("different-family"),
                11.0,
                Some(12.0),
                2.0,
                2,
            ),
        ] {
            let ((), counts) = crate::allocation::activity(|| {
                let shaped = entry
                    .shape(
                        &mut fonts,
                        &family,
                        &text_system,
                        px(size),
                        wrap.map(px),
                        scale,
                    )
                    .expect("changed key");
                assert_eq!(
                    (shaped.font_gen, shaped.size, shaped.wrap, shaped.scale),
                    (generation, px(size), wrap.map(px), scale.to_bits())
                );
            });
            assert!(
                counts[0] > 0,
                "an invalidated key builds GPUI decoration/layout storage"
            );
        }
        assert_eq!(fonts.0[0].font.weight, gpui_kit::FontWeight::NORMAL);
        assert_eq!(fonts.0[1].font.weight, gpui_kit::FontWeight::BOLD);
        assert_eq!(fonts.0[1].id, None);
        assert_eq!((fonts.0[0].generation, fonts.0[1].generation), (2, 2));
        assert!(Arc::ptr_eq(
            &fonts.0[0].font.features.0,
            &fonts.0[1].font.features.0
        ));
        let printed = format!("{entry:?}");
        assert!(!printed.contains("private-shaped-label"));
    });
}

#[test]
fn retained_labels_reject_foreign_objects_primitives_and_view_generations() {
    let first = view(LABEL);
    let second = view(LABEL);
    let mut cache = Stager(size_of::<Stager>()).stage(&first);
    let scene = bound(&mut *cache, first.generation()).expect("its view");
    let (handle, _) = first.paint_sources().next().expect("one label object");
    assert_eq!(scene.label_index(handle, 0), Ok(0));
    assert_eq!(scene.label_index(handle, 1), Err(BuildError::Stale));
    assert_eq!(
        scene.label_index(Handle::new(2, 0).expect("foreign handle"), 0),
        Err(BuildError::Stale)
    );
    assert_eq!(
        bound(&mut *cache, second.generation()).err(),
        Some(BuildError::Stale)
    );
}
