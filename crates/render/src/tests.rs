//! What this crate's tests pin: a charge that never moves, a copy bounded by its
//! buffer and wiped after use, no scene memory on the stage side, and records in main's order.

mod fixture;

use std::{any::Any, sync::Arc};

use kumochi_protocol::message::SceneId;
use kumochi_scene::{
    GlobalBudget, Handle, Text,
    display::{Paint, Prim, TextStyle, corner_segments, fallback_corner_segments},
    geom::{Rect, Vec2},
    record::{Align, Rgb},
};
use kumochi_shell_core::presenter::{SceneCache, Stage};

use crate::{
    BuildInput, Failure, FrameBuf, Mesh, Record, RenderCache, Shape, Stager,
    limits::{
        INPUT_OBJECTS, INPUT_POINTS, INPUT_PRIMS, INPUT_TEXT_BYTES, RECORDS, TESSELLATOR_BYTES,
    },
    record,
};

fn render(cache: &dyn SceneCache) -> &RenderCache {
    let cache: &dyn Any = cache;
    cache.downcast_ref().expect("a render cache")
}

fn handle(slot: usize) -> Handle {
    Handle::new(1, slot).expect("fixture handle")
}

fn paint() -> Paint {
    Paint::from(Rgb::try_from(0x33_66_99).expect("fixture colour"))
}

fn label(text: &str) -> Prim {
    Prim::Text {
        origin: Vec2::new(0.0, 0.0),
        text: Text::try_from(text.to_owned()).expect("fixture label"),
        style: TextStyle {
            size: 14.0,
            bold: false,
        },
        color: paint(),
        max_width: None,
        align: Align::Left,
    }
}

fn line(points: impl IntoIterator<Item = Vec2>) -> Prim {
    Prim::Line {
        points: points.into_iter().collect(),
        closed: false,
        color: paint(),
        width: 2.0,
        dashed: false,
    }
}

fn empty_rect() -> Prim {
    Prim::Rect {
        rect: Rect {
            origin: Vec2::new(0.0, 0.0),
            size: Vec2::new(4.0, 4.0),
        },
        radius: 0.0,
        fill: None,
        border: None,
    }
}

/// One cache of `objects` objects, each holding `prims`.
fn cache(id: u64, objects: usize, prims: &[Prim]) -> RenderCache {
    let prims: Arc<[Prim]> = prims.into();
    let sources = (0..objects)
        .map(|slot| (handle(slot), 1.0, Arc::clone(&prims)))
        .collect();
    RenderCache::of(id, sources)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Two nodes and two labelled edges: the edges paint their chips and text in the second pass.
fn labelled(edge_label: &str) -> Vec<String> {
    vec![format!(
        r#"{{"id":"d","put":[{{"diagram":{{"id":"g","box":[0,0,400,300]}}}},{{"node":{{"id":"a","in":"g","label":"A"}}}},{{"node":{{"id":"b","in":"g","label":"B"}}}},{{"edge":{{"id":"e1","in":"g","from":"a","to":"b","label":"{edge_label}"}}}},{{"edge":{{"id":"e2","in":"g","from":"b","to":"a","label":"back"}}}}]}}"#
    )]
}

#[test]
fn fixed_bytes_is_what_the_buffers_reserve() {
    let reserved = 3 * FrameBuf::new().reserved()
        + BuildInput::new().reserved()
        + Mesh::new().reserved()
        + TESSELLATOR_BYTES;
    assert_eq!(
        Stager::new().fixed_bytes(),
        reserved,
        "the charge is what the buffers reserve"
    );
}

#[test]
fn every_part_of_the_input_holds_its_limit_and_refuses_one_more() {
    let emoji = |count: usize| label(&"🙂".repeat(count));
    let full_labels: Vec<Prim> = (0..INPUT_TEXT_BYTES / 1600)
        .map(|_| emoji(400))
        .chain([emoji(INPUT_TEXT_BYTES % 1600 / 4)])
        .collect();
    let at_limit = [
        vec![
            cache(0, 600, &[]),
            cache(1, 600, &[]),
            cache(2, INPUT_OBJECTS - 1200, &[]),
            cache(3, 0, &[]),
        ],
        vec![cache(
            0,
            1,
            &[line((0..INPUT_POINTS).map(|_| Vec2::new(1.0, 1.0)))],
        )],
        vec![cache(0, 1, &full_labels)],
        vec![cache(0, 1, &vec![empty_rect(); INPUT_PRIMS])],
    ];
    let one_more = [
        cache(9, 0, &[]),
        cache(9, 1, &[line([Vec2::new(0.0, 0.0)])]),
        cache(9, 1, &[label("a")]),
        cache(9, 1, &[empty_rect()]),
    ];
    for (fill, over) in at_limit.iter().zip(&one_more) {
        let mut input = BuildInput::new();
        for each in fill {
            assert_eq!(input.add(each), Ok(()), "a part at its exact limit");
        }
        let full = format!("{input:?}");
        assert_eq!(
            input.add(over),
            Err(Failure::Build),
            "one more than its limit"
        );
        assert_eq!(
            format!("{input:?}"),
            full,
            "the refused view left nothing behind"
        );
        assert_eq!(input.objects(over.id()), None);
    }
    let objects = cache(0, 1, &[]);
    let mut input = BuildInput::new();
    input.add(&cache(1, 0, &[])).expect("four views");
    input.add(&cache(2, 0, &[])).expect("four views");
    input.add(&cache(3, 0, &[])).expect("four views");
    input.add(&objects).expect("four views");
    assert_eq!(
        input.add(&cache(4, 0, &[])),
        Err(Failure::Build),
        "a fifth view"
    );
}

#[test]
fn four_views_at_the_global_object_cap_and_the_paint_caps_fit_and_build() {
    let mut budget = GlobalBudget::default();
    let stager = Stager::new();
    let views: Vec<_> = [600, 600, 150, 150]
        .into_iter()
        .enumerate()
        .map(|(n, count)| fixture::view(&format!("v{n}"), &fixture::squares(count), &mut budget))
        .collect();
    assert_eq!(
        budget.usage().objects,
        1500,
        "the fixture reaches the global cap"
    );
    let boxed: Vec<_> = views.iter().map(|view| stager.stage(view)).collect();
    let mut input = BuildInput::new();
    let mut mesh = Mesh::new();
    for cache in &boxed {
        input
            .add(render(&**cache))
            .expect("every view at the cap fits");
    }
    for cache in &boxed {
        mesh.build(&input, render(&**cache).id())
            .expect("each builds");
    }
    let mut budget = GlobalBudget::default();
    let view = fixture::view("limits", &fixture::at_limits(), &mut budget);
    assert_eq!(
        budget.usage().segments,
        19_955,
        "the fixture is at the segment cap"
    );
    let boxed = stager.stage(&view);
    let mut input = BuildInput::new();
    input
        .add(render(&*boxed))
        .expect("geometry at the paint caps fits");
    mesh.build(&input, render(&*boxed).id())
        .expect("and builds within the mesh's reservation");
}

#[test]
fn views_at_the_segment_cap_and_the_cell_cap_copy_and_build() {
    let stager = Stager::new();
    let mut budget = GlobalBudget::default();
    let charts = fixture::view("charts", &fixture::charts(), &mut budget);
    assert_eq!(
        (budget.usage().segments, budget.usage().marks),
        (20_000, 39 * 500 + 220),
        "the charts reach the segment cap with their most marks"
    );
    let mut budget = GlobalBudget::default();
    let tables = fixture::view("tables", &fixture::tables(), &mut budget);
    assert_eq!(budget.usage().cells, 4000, "the tables reach the cell cap");
    let boxed = [&charts, &tables].map(|view| stager.stage(view));
    let mut mesh = Mesh::new();
    let mut input = BuildInput::new();
    for cache in &boxed {
        input.wipe();
        input.add(render(&**cache)).expect("the view fits");
        mesh.build(&input, render(&**cache).id())
            .expect("and builds");
    }
    let copied: usize = input
        .objects(render(&*boxed[1]).id())
        .expect("the tables' copy")
        .iter()
        .flat_map(|object| input.shapes(object).expect("its shapes"))
        .filter_map(|shape| match shape {
            Shape::Text { text, .. } => input.text(*text).map(<[u8]>::len),
            _ => None,
        })
        .sum();
    assert_eq!(
        copied,
        13 * 8 + 4000 * (300 + 99),
        "every column name and generated number cell"
    );
}

#[test]
fn the_input_keeps_no_label_text_after_its_build_or_a_refused_copy() {
    let marker = "MARKER-7f3a-label";
    let labelled = cache(0, 1, &[label(marker)]);
    let mut input = BuildInput::new();
    input.add(&labelled).expect("one label fits");
    assert!(
        contains(input.text_storage(), marker.as_bytes()),
        "the copy carries the label to the stage"
    );
    let mut mesh = Mesh::new();
    mesh.build(&input, labelled.id())
        .expect("labels build no geometry");
    input.wipe();
    assert!(
        !contains(input.text_storage(), marker.as_bytes()),
        "the wipe overwrote the label"
    );
    let refused = cache(
        1,
        1,
        &[
            label(marker),
            line((0..=INPUT_POINTS).map(|_| Vec2::new(0.0, 0.0))),
        ],
    );
    assert_eq!(input.add(&refused), Err(Failure::Build));
    assert!(
        !contains(input.text_storage(), marker.as_bytes()),
        "a refused copy overwrote the label it had copied"
    );
}

#[test]
fn no_debug_output_carries_a_label() {
    let marker = "MARKER-51c9-debug";
    let view = fixture::view("s", &labelled(marker), &mut GlobalBudget::default());
    let boxed = Stager::new().stage(&view);
    let mut input = BuildInput::new();
    input.add(render(&*boxed)).expect("the view fits");
    let mut mesh = Mesh::new();
    mesh.build(&input, render(&*boxed).id()).expect("it builds");
    let mut buf = FrameBuf::new();
    let mut cache = boxed;
    record(&mut buf, SceneId(1), &fixture::sample(&view), &mut *cache).expect("it records");
    let bytes = format!("{:?}", marker.as_bytes());
    let listed = &bytes[1..bytes.len() - 1];
    for printed in [
        format!("{input:?} {input:#?}"),
        format!("{:?} {:#?}", render(&*cache), render(&*cache)),
        format!("{mesh:?} {buf:?} {:?}", buf.pictures()),
        format!("{:?}", buf.records(&buf.pictures()[0])),
        format!("{:?}", Failure::Build),
    ] {
        assert!(!printed.contains(marker), "no label as text: {printed}");
        assert!(!printed.contains(listed), "no label as bytes: {printed}");
    }
}

#[test]
fn a_cache_retired_after_its_copy_leaves_nothing_on_the_stage_side() {
    let view = fixture::view("s", &labelled("go"), &mut GlobalBudget::default());
    let (_, _, prims) = view.shared_sources().next().expect("an object");
    let held = Arc::clone(prims);
    let alone = Arc::strong_count(&held);
    let boxed = Stager::new().stage(&view);
    assert_eq!(
        Arc::strong_count(&held),
        alone + 1,
        "the cache shares the view's list"
    );
    let id = render(&*boxed).id();
    let mut input = BuildInput::new();
    input.add(render(&*boxed)).expect("the view fits");
    assert_eq!(
        Arc::strong_count(&held),
        alone + 1,
        "the copy holds no reference"
    );
    let mut before = Mesh::new();
    before
        .build(&input, id)
        .expect("built while the cache lives");
    drop(boxed);
    drop(view);
    assert_eq!(
        Arc::strong_count(&held),
        1,
        "nothing but this test holds the list"
    );
    let mut after = Mesh::new();
    after
        .build(&input, id)
        .expect("built after the cache was retired");
    assert!(!after.indices().is_empty(), "the view has geometry");
    assert_eq!(after.vertices(), before.vertices());
    assert_eq!(after.indices(), before.indices());
}

#[test]
fn records_follow_paint_items_range_for_range() {
    let view = fixture::view("s", &labelled("go"), &mut GlobalBudget::default());
    let mut boxed = Stager::new().stage(&view);
    let id = render(&*boxed).id();
    let frame = fixture::sample(&view);
    let mut buf = FrameBuf::new();
    assert_eq!(record(&mut buf, SceneId(7), &frame, &mut *boxed), Ok(true));
    let [picture] = buf.pictures() else {
        panic!("one picture");
    };
    assert_eq!((picture.scene, picture.geometry), (SceneId(7), id));
    let mut handles: Vec<Handle> = view.shared_sources().map(|(handle, ..)| handle).collect();
    handles.sort_unstable();
    let written: Vec<_> = buf
        .records(picture)
        .iter()
        .map(|record| {
            let object = usize::try_from(record.object).expect("an object index");
            let [first, count] =
                [record.first, record.count].map(|n| usize::try_from(n).expect("a count"));
            (handles[object], first, count)
        })
        .collect();
    let items: Vec<_> = frame
        .paint_items()
        .map(|(item, first, prims)| (item.handle, first, prims.len()))
        .collect();
    assert_eq!(
        written, items,
        "one record per paint item, in paint_items order"
    );
    assert_eq!(
        items.iter().filter(|(_, first, _)| *first > 0).count(),
        2,
        "both edge labels paint in the second pass"
    );
    let mut input = BuildInput::new();
    input.add(render(&*boxed)).expect("the view fits");
    let mut mesh = Mesh::new();
    mesh.build(&input, id).expect("it builds");
    for record in buf.records(picture) {
        assert!(
            mesh.range(record).is_some(),
            "every record names primitives of its mesh"
        );
    }
}

#[test]
fn an_empty_frame_lists_nothing_and_a_foreign_cache_is_stale() {
    let empty = fixture::view("e", &[], &mut GlobalBudget::default());
    let stager = Stager::new();
    let mut boxed = stager.stage(&empty);
    let mut input = BuildInput::new();
    input.add(render(&*boxed)).expect("an empty view fits");
    let mut mesh = Mesh::new();
    mesh.build(&input, render(&*boxed).id())
        .expect("and builds");
    assert_eq!((mesh.vertices().len(), mesh.indices().len()), (0, 0));
    let mut buf = FrameBuf::new();
    assert_eq!(
        record(&mut buf, SceneId(1), &fixture::sample(&empty), &mut *boxed),
        Ok(false)
    );
    let view = fixture::view("s", &labelled("go"), &mut GlobalBudget::default());
    let frame = fixture::sample(&view);
    assert_eq!(
        record(&mut buf, SceneId(1), &frame, &mut *boxed),
        Err(Failure::Stale)
    );
    let mut other = cache(0, 1, &[]);
    assert_eq!(
        record(&mut buf, SceneId(1), &frame, &mut other),
        Err(Failure::Stale)
    );
    assert!(buf.pictures().is_empty(), "nothing was listed");
}

#[test]
fn records_fill_their_reservation_exactly_and_refuse_past_it() {
    let view = fixture::view("s", &labelled("go"), &mut GlobalBudget::default());
    let mut boxed = Stager::new().stage(&view);
    let frame = fixture::sample(&view);
    let items = frame.paint_items().count();
    let filler = Record {
        object: 0,
        first: 0,
        count: 0,
        transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        opacity: 1.0,
        draw: 1.0,
        color: None,
        flow: 0.0,
        paint_scale: 1.0,
    };
    let mut buf = FrameBuf::new();
    buf.fill_records(filler, RECORDS - items);
    let reservation = buf.reservation();
    assert_eq!(record(&mut buf, SceneId(1), &frame, &mut *boxed), Ok(true));
    assert_eq!(
        buf.reservation(),
        reservation,
        "writing to the last record grew nothing"
    );
    let mut buf = FrameBuf::new();
    buf.fill_records(filler, RECORDS - items + 1);
    assert_eq!(
        record(&mut buf, SceneId(1), &frame, &mut *boxed),
        Err(Failure::Build)
    );
    assert!(
        buf.pictures().is_empty(),
        "the refused picture isn't listed"
    );
    let mut buf = FrameBuf::new();
    for scene in 0..4 {
        assert_eq!(
            record(&mut buf, SceneId(scene), &frame, &mut *boxed),
            Ok(true)
        );
    }
    let reservation = buf.reservation();
    assert_eq!(
        record(&mut buf, SceneId(4), &frame, &mut *boxed),
        Err(Failure::Build)
    );
    assert_eq!(buf.pictures().len(), 4, "a fifth picture is refused");
    assert_eq!(buf.reservation(), reservation);
}

#[test]
fn a_rect_builds_its_corners_at_the_scale_it_was_charged() {
    let steps = [
        r#"{"id":"r","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"radius":4,"fill":"blue"}}]}"#,
        r#"{"id":"z","put":[{"anim":{"target":"r","track":"scale","keys":[[0,64]]}}]}"#,
    ]
    .map(str::to_owned);
    let mut view = fixture::view("s", &steps, &mut GlobalBudget::default());
    view.set_stage(Rect {
        origin: Vec2::new(0.0, 0.0),
        size: Vec2::new(20_000.0, 20_000.0),
    });
    let frame = fixture::sample(&view);
    let [item] = frame.items() else {
        panic!("one rect");
    };
    let [a, b, c, d, ..] = item.transform.0;
    let pose = (a * d - b * c).abs().sqrt();
    assert_eq!(
        (item.paint_scale, pose),
        (64.0, 64.0),
        "layout caps the pose at the composed limit it charged"
    );
    assert_eq!(
        (
            fallback_corner_segments(4.0, 64.0),
            corner_segments(4.0 * 128.0)
        ),
        (18, 26),
        "main's painter refuses this pose as over its charge"
    );
    let mut boxed = Stager::new().stage(&view);
    let mut buf = FrameBuf::new();
    assert_eq!(record(&mut buf, SceneId(1), &frame, &mut *boxed), Ok(true));
    let mut input = BuildInput::new();
    input.add(render(&*boxed)).expect("the rect fits");
    let mut mesh = Mesh::new();
    mesh.build(&input, render(&*boxed).id())
        .expect("the rect builds");
    assert_eq!(
        mesh.vertices()
            .iter()
            .filter(|vertex| vertex.piece == 0)
            .count(),
        4 * (18 + 1),
        "the convex body is four corners of 18 pieces"
    );
}

#[test]
fn strokes_and_borders_reveal_along_their_own_length() {
    let square = Prim::Rect {
        rect: Rect {
            origin: Vec2::new(0.0, 0.0),
            size: Vec2::new(10.0, 10.0),
        },
        radius: 0.0,
        fill: None,
        border: Some((paint(), 1.0)),
    };
    let prims = [
        line([Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)]),
        line([Vec2::new(0.0, 5.0), Vec2::new(40.0, 5.0)]),
        square,
    ];
    let cache = cache(0, 1, &prims);
    let mut input = BuildInput::new();
    input.add(&cache).expect("it fits");
    let mut mesh = Mesh::new();
    mesh.build(&input, cache.id()).expect("it builds");
    let of = |piece: u32| {
        mesh.vertices()
            .iter()
            .filter(move |vertex| vertex.piece == piece)
    };
    for (piece, length) in [(0, 10.0), (1, 40.0)] {
        let along = of(piece).map(|vertex| vertex.along).fold(0.0, f32::max);
        let reveal = of(piece).map(|vertex| vertex.reveal).fold(0.0, f32::max);
        assert_eq!(
            [along, reveal].map(f32::to_bits),
            [length, 1.0].map(f32::to_bits),
            "dashes walk the stroke in primitive units and each reveals over its own length"
        );
    }
    let ring: Vec<([f32; 2], f32)> = of(2).map(|vertex| (vertex.at, vertex.reveal)).collect();
    assert_eq!(
        ring,
        [
            ([10.0, 0.0], 0.0),
            ([9.0, 1.0], 0.0),
            ([10.0, 10.0], 0.25),
            ([9.0, 9.0], 0.25),
            ([0.0, 10.0], 0.5),
            ([1.0, 9.0], 0.5),
            ([0.0, 0.0], 0.75),
            ([1.0, 1.0], 0.75),
            ([10.0, 0.0], 1.0),
            ([9.0, 1.0], 1.0),
        ],
        "a border reveals clockwise from the right end of its top edge"
    );
}
