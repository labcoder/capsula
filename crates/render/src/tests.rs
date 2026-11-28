//! What this crate's tests pin: a charge that never moves, a copy in batches bounded by their
//! buffers and wiped after use, no scene memory on the stage side, records in main's order, and
//! reveal and dashes measured along the source as main's painter measures them.

mod fixture;

use std::{any::Any, sync::Arc};

use kumochi_protocol::message::SceneId;
use kumochi_scene::{
    GlobalBudget, Handle, Text,
    display::{DASH, Paint, PaintWork, Prim, TextStyle},
    geom::{Rect, Vec2},
    record::{Align, Rgb},
};
use kumochi_shell_core::presenter::{SceneCache, Stage};

use crate::{
    BuildInput, Cursor, Failure, FrameBuf, Mesh, Record, RenderCache, Shape, Stager,
    limits::{
        BATCH_TEXT_BYTES, INPUT_OBJECTS, INPUT_POINTS, INPUT_PRIMS, RECORDS, TESSELLATOR_BYTES,
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

fn closed_line(points: impl IntoIterator<Item = Vec2>) -> Prim {
    Prim::Line {
        points: points.into_iter().collect(),
        closed: true,
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

fn bordered(size: [f32; 2], radius: f32, width: f32) -> Prim {
    Prim::Rect {
        rect: Rect {
            origin: Vec2::new(0.0, 0.0),
            size: Vec2::new(size[0], size[1]),
        },
        radius,
        fill: None,
        border: Some((paint(), width)),
    }
}

fn filler(object: u32, first: u32, count: u32) -> Record {
    Record {
        object,
        first,
        count,
        transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        opacity: 1.0,
        draw: 1.0,
        color: None,
        flow: 0.0,
        paint_scale: 1.0,
    }
}

/// Copies `cache` batch by batch and builds each batch into `mesh`, wiping the batch on every
/// path as its owner must; returns the batches it took.
fn build(input: &mut BuildInput, mesh: &mut Mesh, cache: &RenderCache) -> Result<usize, Failure> {
    mesh.begin(cache.id());
    let mut from = Some(Cursor::START);
    let mut batches = 0;
    while let Some(at) = from {
        input.wipe();
        let built = input
            .add(cache, at)
            .and_then(|to| mesh.extend(input).map(|()| to));
        input.wipe();
        from = built?;
        batches += 1;
    }
    mesh.finish()?;
    Ok(batches)
}

/// What a rasterizer reads at `at` in every triangle of `piece` that covers it: the source
/// distance, interpolated `along` plus the first vertex's flat `skip`, and the border reveal,
/// interpolated `reveal` over interpolated `weight`.
fn sampled(mesh: &Mesh, piece: u32, at: [f32; 2]) -> Vec<[f64; 2]> {
    let vertex = |id: &u32| mesh.vertices()[usize::try_from(*id).expect("a vertex index")];
    mesh.indices()
        .chunks(3)
        .filter_map(|corners| {
            let [a, b, c] = [&corners[0], &corners[1], &corners[2]].map(vertex);
            if a.piece != piece {
                return None;
            }
            let point = |v: [f32; 2]| [f64::from(v[0]), f64::from(v[1])];
            let [pa, pb, pc, x] = [a.at, b.at, c.at, at].map(point);
            let area = (pb[0] - pa[0]) * (pc[1] - pa[1]) - (pc[0] - pa[0]) * (pb[1] - pa[1]);
            if area.abs() < 1e-12 {
                return None;
            }
            let wb = ((x[0] - pa[0]) * (pc[1] - pa[1]) - (pc[0] - pa[0]) * (x[1] - pa[1])) / area;
            let wc = ((pb[0] - pa[0]) * (x[1] - pa[1]) - (x[0] - pa[0]) * (pb[1] - pa[1])) / area;
            let wa = 1.0 - wb - wc;
            if wa.min(wb).min(wc) < -1e-9 {
                return None;
            }
            let mix = |f: fn(&crate::Vertex) -> f32| {
                wa * f64::from(f(&a)) + wb * f64::from(f(&b)) + wc * f64::from(f(&c))
            };
            Some([
                mix(|v| v.along) + f64::from(a.skip),
                mix(|v| v.reveal) / mix(|v| v.weight),
            ])
        })
        .collect()
}

#[test]
fn fixed_bytes_is_what_the_buffers_reserve_and_lyons_allowance() {
    let reserved =
        3 * FrameBuf::new().reserved() + BuildInput::new().reserved() + Mesh::new().reserved();
    assert_eq!(
        Stager::new().fixed_bytes() - TESSELLATOR_BYTES,
        reserved,
        "past lyon's allowance, the charge is what the buffers reserve"
    );
}

#[test]
fn every_part_of_a_batch_holds_its_limit_and_stops_before_one_more() {
    let emoji = |count: usize| label(&"🙂".repeat(count));
    let full_labels: Vec<Prim> = (0..BATCH_TEXT_BYTES / 1600)
        .map(|_| emoji(400))
        .chain([emoji(BATCH_TEXT_BYTES % 1600 / 4)])
        .collect();
    let at_limit = [
        vec![
            cache(0, 600, &[]),
            cache(1, 600, &[]),
            cache(2, INPUT_OBJECTS - 1200, &[]),
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
        cache(9, 1, &[]),
        cache(9, 1, &[line([Vec2::new(0.0, 0.0)])]),
        cache(9, 1, &[label("a")]),
        cache(9, 1, &[empty_rect()]),
    ];
    for (fill, over) in at_limit.iter().zip(&one_more) {
        let mut input = BuildInput::new();
        for each in fill {
            assert_eq!(
                input.add(each, Cursor::START),
                Ok(None),
                "a part at its exact limit"
            );
        }
        let full = format!("{input:?}");
        assert_eq!(
            input.add(over, Cursor::START),
            Ok(Some(Cursor::START)),
            "one more waits for the next batch, with a view slot still free"
        );
        assert_eq!(format!("{input:?}"), full, "and left nothing behind");
        assert!(input.part(over.id()).is_none());
    }
    let mut input = BuildInput::new();
    let empty = format!("{input:?}");
    let longest = cache(
        0,
        1,
        &[line((0..=INPUT_POINTS).map(|_| Vec2::new(0.0, 0.0)))],
    );
    assert_eq!(
        input.add(&longest, Cursor::START),
        Err(Failure::Build),
        "a primitive past an empty batch"
    );
    assert_eq!(format!("{input:?}"), empty);
    for id in 1..=4 {
        input
            .add(&cache(id, 0, &[]), Cursor::START)
            .expect("four views");
    }
    assert_eq!(
        input.add(&cache(5, 0, &[]), Cursor::START),
        Ok(Some(Cursor::START)),
        "a fifth view waits for the next batch, as any full part does"
    );
}

#[test]
fn a_copy_starts_only_at_a_cursor_add_returned_and_once_per_view() {
    let mut input = BuildInput::new();
    let two = cache(6, 2, &[label("a")]);
    let empty_first = RenderCache::of(
        7,
        vec![
            (handle(0), 1.0, Arc::from(Vec::<Prim>::new())),
            (handle(1), 1.0, Arc::from(vec![label("b")])),
        ],
    );
    for (view, cursor) in [
        (&two, Cursor { object: 0, prim: 1 }),
        (&two, Cursor { object: 2, prim: 0 }),
        (&two, Cursor { object: 2, prim: 1 }),
        (
            &empty_first,
            Cursor {
                object: 0,
                prim: 999,
            },
        ),
        (&cache(8, 0, &[]), Cursor { object: 0, prim: 1 }),
    ] {
        assert_eq!(
            input.add(view, cursor),
            Err(Failure::Order),
            "{cursor:?} is no cursor a copy returns"
        );
    }
    assert_eq!(input.add(&empty_first, Cursor::START), Ok(None));
    assert_eq!(
        input.add(&empty_first, Cursor { object: 1, prim: 0 }),
        Err(Failure::Order),
        "a second part of a view the batch holds"
    );
    assert_eq!(
        input.add(&cache(9, 0, &[]), Cursor::START),
        Ok(None),
        "an empty view"
    );
}

#[test]
fn a_view_past_one_batch_continues_where_it_stopped_and_builds_the_same_mesh_any_split() {
    let mut budget = GlobalBudget::default();
    let tables = fixture::view("tables", &fixture::tables(), &mut budget);
    let boxed = Stager::new().stage(&tables);
    let tables = render(&*boxed);
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    let batches = build(&mut input, &mut mesh, tables).expect("the tables build");
    assert!(
        batches >= (13 * 8 + 4000 * 399_usize).div_ceil(BATCH_TEXT_BYTES),
        "{batches} batches carry every label"
    );
    let filler = cache(99, 1, &vec![label(&"x".repeat(400)); 300]);
    let mut other = Mesh::new();
    other.begin(tables.id());
    input.wipe();
    assert_eq!(input.add(&filler, Cursor::START), Ok(None));
    let mut from = Some(Cursor::START);
    while let Some(at) = from {
        from = input.add(tables, at).expect("each batch copies");
        other.extend(&input).expect("each batch builds");
        input.wipe();
    }
    other.finish().expect("the last batch is in");
    assert!(!mesh.indices().is_empty(), "the tables have geometry");
    assert_eq!(other.vertices(), mesh.vertices());
    assert_eq!(other.indices(), mesh.indices());
    assert_eq!(other.pieces(), mesh.pieces());
    for object in 0..u32::try_from(tables.sources().len()).expect("objects") {
        let record = filler_for(tables, object);
        assert_eq!(other.range(&record), mesh.range(&record));
    }
}

fn filler_for(cache: &RenderCache, object: u32) -> Record {
    let count = cache.sources()[usize::try_from(object).expect("an object")]
        .2
        .len();
    filler(object, 0, u32::try_from(count).expect("primitives"))
}

#[test]
fn a_batch_out_of_order_or_for_another_geometry_or_refused_leaves_the_mesh_as_it_was() {
    let first = cache(0, 1, &[line([Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)])]);
    let nan = Prim::Fill {
        points: [
            Vec2::new(0.0, 0.0),
            Vec2::new(f32::NAN, 1.0),
            Vec2::new(1.0, 1.0),
        ]
        .into(),
        color: paint(),
    };
    let refused = cache(1, 1, std::slice::from_ref(&nan));
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    assert_eq!(mesh.extend(&input), Err(Failure::Order), "no build is open");
    mesh.begin(first.id());
    input.add(&refused, Cursor::START).expect("it copies");
    assert_eq!(mesh.extend(&input), Err(Failure::Order), "another geometry");
    input.wipe();
    input
        .add(&first, Cursor { object: 0, prim: 0 })
        .expect("it copies");
    mesh.extend(&input).expect("the first batch builds");
    let built = (mesh.vertices().to_vec(), mesh.indices().to_vec());
    assert_eq!(
        mesh.extend(&input),
        Err(Failure::Order),
        "the same batch twice"
    );
    assert_eq!((mesh.vertices().to_vec(), mesh.indices().to_vec()), built);
    mesh.finish().expect("the view was whole");
    assert_eq!(mesh.finish(), Err(Failure::Order), "finished once");
    let mut prims = vec![line([Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)])];
    prims.extend(vec![label(&"🙂".repeat(400)); BATCH_TEXT_BYTES / 1600]);
    prims.push(label(&"🙂".repeat(BATCH_TEXT_BYTES % 1600 / 4)));
    prims.extend([label("a"), nan]);
    let later = cache(2, 1, &prims);
    input.wipe();
    mesh.begin(later.id());
    let rest = input
        .add(&later, Cursor::START)
        .expect("the first batch copies");
    mesh.extend(&input).expect("the first batch builds");
    let built = (
        mesh.vertices().to_vec(),
        mesh.indices().to_vec(),
        mesh.pieces().to_vec(),
    );
    assert!(!built.1.is_empty(), "the line has geometry");
    input.wipe();
    let rest = rest.expect("the NaN fill is in a later batch");
    assert_eq!(input.add(&later, rest), Ok(None));
    assert_eq!(
        mesh.extend(&input),
        Err(Failure::Build),
        "lyon refuses a NaN"
    );
    assert_eq!(
        (
            mesh.vertices().to_vec(),
            mesh.indices().to_vec(),
            mesh.pieces().to_vec()
        ),
        built,
        "the refused later batch left the earlier geometry as it was"
    );
    assert_eq!(
        mesh.finish(),
        Err(Failure::Order),
        "its batch is still owed"
    );
}

#[test]
fn four_views_at_the_global_object_cap_and_the_paint_caps_fit_one_batch_and_build() {
    let mut budget = GlobalBudget::default();
    let stager = Stager::new();
    let views: Vec<_> = [500, 500, 250, 250]
        .into_iter()
        .enumerate()
        .map(|(n, count)| fixture::view(&format!("v{n}"), &fixture::squares(count), &mut budget))
        .collect();
    assert_eq!(
        budget.usage().objects,
        1500,
        "the fixture reaches the global cap"
    );
    assert!(
        views
            .iter()
            .map(|view| view.layout().paint_work())
            .fold(PaintWork::default(), |sum, work| sum + work)
            .within_limits(),
        "the four views' paint work fits the global caps together"
    );
    let boxed: Vec<_> = views.iter().map(|view| stager.stage(view)).collect();
    let mut input = BuildInput::new();
    for cache in &boxed {
        assert_eq!(input.add(render(&**cache), Cursor::START), Ok(None));
    }
    let mut mesh = Mesh::new();
    for cache in &boxed {
        mesh.begin(render(&**cache).id());
        mesh.extend(&input)
            .expect("each builds from the shared batch");
        mesh.finish().expect("each was whole");
    }
    let mut budget = GlobalBudget::default();
    let view = fixture::view("limits", &fixture::at_limits(), &mut budget);
    assert_eq!(
        budget.usage().segments,
        19_955,
        "the fixture's 19,955 of the 20,000 path segments"
    );
    let boxed = stager.stage(&view);
    assert_eq!(
        build(&mut input, &mut mesh, render(&*boxed)),
        Ok(1),
        "geometry at the paint caps fits one batch and the mesh's reservation"
    );
    let fills = fixture::view("fills", &[fixture::fills()], &mut GlobalBudget::default());
    let boxed = stager.stage(&fills);
    assert_eq!(build(&mut input, &mut mesh, render(&*boxed)), Ok(1));
    assert_eq!(
        sampled(&mesh, 1, [1500.37, 300.21]).len(),
        1,
        "the star's centre, wound three times, is filled nonzero and once"
    );
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
        build(&mut input, &mut mesh, render(&**cache)).expect("the view builds");
    }
    let tables = render(&*boxed[1]);
    let mut copied = 0;
    let mut from = Some(Cursor::START);
    while let Some(at) = from {
        input.wipe();
        from = input.add(tables, at).expect("each batch copies");
        let part = input.part(tables.id()).expect("the tables' part");
        copied += part
            .objects
            .iter()
            .flat_map(|object| input.shapes(object).expect("its shapes"))
            .filter_map(|shape| match shape {
                Shape::Text { text, .. } => input.text(*text).map(<[u8]>::len),
                _ => None,
            })
            .sum::<usize>();
    }
    assert_eq!(
        copied,
        13 * 8 + 4000 * (300 + 99),
        "every column name and generated number cell, once"
    );
}

#[test]
fn the_input_keeps_no_label_text_after_its_build_or_a_refused_copy() {
    let marker = "MARKER-7f3a-label";
    let labelled = cache(0, 1, &[label(marker)]);
    let mut input = BuildInput::new();
    let mut mesh = Mesh::new();
    assert_eq!(build(&mut input, &mut mesh, &labelled), Ok(1));
    assert!(
        !contains(input.text_storage(), marker.as_bytes()),
        "the wipe after the build overwrote the label"
    );
    let emoji = label(&"🙂".repeat(400));
    let mut prims = vec![emoji; BATCH_TEXT_BYTES / 1600];
    prims.push(label(&"🙂".repeat(BATCH_TEXT_BYTES % 1600 / 4)));
    prims.push(label(marker));
    let split = cache(1, 1, &prims);
    let rest = Cursor {
        object: 0,
        prim: u32::try_from(prims.len() - 1).expect("primitives"),
    };
    assert_eq!(input.add(&split, Cursor::START), Ok(Some(rest)));
    assert!(!contains(input.text_storage(), marker.as_bytes()));
    input.wipe();
    assert_eq!(input.add(&split, rest), Ok(None));
    assert!(
        contains(input.text_storage(), marker.as_bytes()),
        "the second batch carries the label to the stage"
    );
    input.wipe();
    assert!(!contains(input.text_storage(), marker.as_bytes()));
    let refused = cache(
        2,
        1,
        &[
            label(marker),
            line((0..=INPUT_POINTS).map(|_| Vec2::new(0.0, 0.0))),
        ],
    );
    let line_at = Cursor { object: 0, prim: 1 };
    assert_eq!(input.add(&refused, Cursor::START), Ok(Some(line_at)));
    assert!(contains(input.text_storage(), marker.as_bytes()));
    input.wipe();
    assert_eq!(
        input.add(&refused, line_at),
        Err(Failure::Build),
        "a line past an empty batch"
    );
    assert!(
        !contains(input.text_storage(), marker.as_bytes()),
        "the owner's wipe before the refused batch overwrote the label"
    );
}

#[test]
fn no_debug_output_carries_a_label() {
    let marker = "MARKER-51c9-debug";
    let view = fixture::view("s", &labelled(marker), &mut GlobalBudget::default());
    let boxed = Stager::new().stage(&view);
    let mut input = BuildInput::new();
    input
        .add(render(&*boxed), Cursor::START)
        .expect("the view fits");
    let mut mesh = Mesh::new();
    mesh.begin(render(&*boxed).id());
    mesh.extend(&input).expect("it builds");
    let mut buf = FrameBuf::new();
    let mut cache = boxed;
    record(&mut buf, SceneId(1), &fixture::sample(&view), &mut *cache).expect("it records");
    let bytes = format!("{:?}", marker.as_bytes());
    let listed = &bytes[1..bytes.len() - 1];
    for printed in [
        format!("{input:?} {input:#?}"),
        format!("{:?}", input.part(render(&*cache).id())),
        format!("{:?} {:#?}", render(&*cache), render(&*cache)),
        format!("{mesh:?} {buf:?} {:?}", buf.pictures()),
        format!("{:?}", buf.records(&buf.pictures()[0])),
        format!("{:?} {:?}", Failure::Build, Failure::Order),
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
    input
        .add(render(&*boxed), Cursor::START)
        .expect("the view fits");
    assert_eq!(
        Arc::strong_count(&held),
        alone + 1,
        "the copy holds no reference"
    );
    let mut before = Mesh::new();
    before.begin(id);
    before.extend(&input).expect("built while the cache lives");
    drop(boxed);
    drop(view);
    assert_eq!(
        Arc::strong_count(&held),
        1,
        "nothing but this test holds the list"
    );
    let mut after = Mesh::new();
    after.begin(id);
    after
        .extend(&input)
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
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, render(&*boxed)).expect("it builds");
    for record in buf.records(picture) {
        assert!(
            mesh.range(record).is_some(),
            "every record names primitives of its mesh"
        );
    }
}

#[test]
fn a_range_stays_inside_its_own_object() {
    let one = line([Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)]);
    let two = cache(0, 2, &[one]);
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, &two).expect("it builds");
    let end = u32::try_from(mesh.indices().len()).expect("indices");
    let half = end / 2;
    assert_eq!(mesh.range(&filler(0, 0, 1)), Some(0..half));
    assert_eq!(
        mesh.range(&filler(1, 0, 1)),
        Some(half..end),
        "the last primitive"
    );
    assert_eq!(
        mesh.range(&filler(0, 1, 1)),
        None,
        "past object 0 into object 1"
    );
    assert_eq!(
        mesh.range(&filler(1, 1, 1)),
        None,
        "past the last primitive"
    );
    assert_eq!(mesh.range(&filler(2, 0, 0)), None, "no third object");
}

#[test]
fn an_empty_frame_lists_nothing_and_a_foreign_cache_is_stale() {
    let empty = fixture::view("e", &[], &mut GlobalBudget::default());
    let stager = Stager::new();
    let mut boxed = stager.stage(&empty);
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    assert_eq!(build(&mut input, &mut mesh, render(&*boxed)), Ok(1));
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
    let mut buf = FrameBuf::new();
    buf.fill_records(filler(0, 0, 0), RECORDS - items);
    let reservation = buf.reservation();
    assert_eq!(record(&mut buf, SceneId(1), &frame, &mut *boxed), Ok(true));
    assert_eq!(
        buf.reservation(),
        reservation,
        "writing to the last record grew nothing"
    );
    let mut buf = FrameBuf::new();
    buf.fill_records(filler(0, 0, 0), RECORDS - items + 1);
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
    let listed = buf.pictures().to_vec();
    let written: Vec<Vec<Record>> = listed
        .iter()
        .map(|picture| buf.records(picture).to_vec())
        .collect();
    let reservation = buf.reservation();
    assert_eq!(
        record(&mut buf, SceneId(4), &frame, &mut *boxed),
        Err(Failure::Build)
    );
    assert_eq!(buf.pictures(), listed, "a fifth picture is refused");
    for (picture, records) in listed.iter().zip(&written) {
        assert_eq!(
            buf.records(picture),
            records,
            "and leaves the others' records"
        );
    }
    assert_eq!(buf.reservation(), reservation);
    let empty = fixture::view("e", &[], &mut GlobalBudget::default());
    let mut cache = Stager::new().stage(&empty);
    assert_eq!(
        record(&mut buf, SceneId(5), &fixture::sample(&empty), &mut *cache),
        Ok(false),
        "nothing to paint lists nothing, even past four pictures"
    );
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
    let mut boxed = Stager::new().stage(&view);
    let mut buf = FrameBuf::new();
    assert_eq!(record(&mut buf, SceneId(1), &frame, &mut *boxed), Ok(true));
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, render(&*boxed)).expect("the rect builds");
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
fn strokes_reveal_and_dash_along_their_source_length() {
    let mut zigzag = vec![Vec2::new(0.0, 0.0)];
    zigzag.extend((1..=500).map(|at| Vec2::new(if at % 2 == 1 { 0.04 } else { 0.0 }, 0.0)));
    zigzag.push(Vec2::new(20.0, 0.0));
    let prims = [
        line([Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)]),
        line([Vec2::new(0.0, 5.0), Vec2::new(40.0, 5.0)]),
        line(zigzag),
        closed_line([Vec2::new(0.0, 0.0), Vec2::new(17.0, 0.0)]),
    ];
    let cache = cache(0, 1, &prims);
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, &cache).expect("it builds");
    let wiggle = 500.0 * f64::from(0.04_f32);
    let retrace: Vec<bool> = mesh.pieces().iter().map(|piece| piece.retrace).collect();
    assert_eq!(
        retrace,
        [false, false, false, false, true],
        "the return leg is its own piece"
    );
    for (piece, length) in mesh
        .pieces()
        .iter()
        .zip([10.0, 40.0, wiggle + 20.0, 34.0, 34.0])
    {
        assert!(
            (f64::from(piece.length) - length).abs() < 1e-5,
            "each stroke reveals over its source length: {} against {length}",
            piece.length
        );
    }
    let distance = |piece: u32, at: [f32; 2]| -> Vec<f64> {
        let mut found: Vec<f64> = sampled(&mesh, piece, at)
            .into_iter()
            .map(|[distance, _]| distance)
            .collect();
        found.sort_by(f64::total_cmp);
        found.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        found
    };
    let near = |found: Vec<f64>, expected: f64| {
        assert!(
            found.len() == 1 && (found[0] - expected).abs() < 1e-4,
            "{found:?} against {expected}"
        );
    };
    near(distance(0, [7.0, 0.5]), 7.0);
    near(distance(2, [5.0, 0.5]), wiggle + 5.0);
    near(distance(3, [9.0, 0.5]), 9.0);
    near(distance(4, [9.0, 0.5]), 25.0);
    let period = f64::from(DASH[0] + DASH[1]);
    let painted = |along: f64| along.rem_euclid(period) < f64::from(DASH[0]);
    assert_eq!(
        (painted(9.0), painted(25.0)),
        (false, true),
        "at phase 0 main paints x = 9 only on the return"
    );
}

#[test]
fn a_run_lyon_merges_ends_the_edge_arriving_at_it_where_main_does() {
    let wiggle = |from: f32| {
        (1..=500).map(move |at| Vec2::new(if at % 2 == 1 { from + 0.04 } else { from }, 0.0))
    };
    let step = 500.0 * (f64::from(10.04_f32) - 10.0);
    let mut interior = vec![Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)];
    interior.extend(wiggle(10.0));
    interior.push(Vec2::new(20.0, 0.0));
    let mut trailing = vec![Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0)];
    trailing.extend(wiggle(10.0));
    let cache = cache(0, 1, &[line(interior), line(trailing)]);
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, &cache).expect("it builds");
    let distance = |piece: u32, x: f32| {
        let found = sampled(&mesh, piece, [x, 0.5]);
        let [distance, _] = found.first().copied().expect("the stroke covers the point");
        assert!(
            found
                .iter()
                .all(|[other, _]| (other - distance).abs() < 1e-6)
        );
        distance
    };
    let length = f64::from(mesh.pieces()[0].length);
    assert!((length - (20.0 + step)).abs() < 1e-3, "{length}");
    let quarter = length / 4.0;
    for x in [1.0, 5.0, 9.9] {
        assert!(
            (distance(0, x) - f64::from(x)).abs() < 1e-4,
            "the arriving edge reaches x = 10 at distance 10, as main's walk does"
        );
        assert!(
            (distance(1, x) - f64::from(x)).abs() < 1e-4,
            "and before a trailing run"
        );
    }
    for x in [10.1, 15.0, 19.9] {
        assert!(
            (distance(0, x) - (f64::from(x) + step)).abs() < 1e-3,
            "the leaving edge starts past the merged run"
        );
    }
    assert!(
        distance(0, 9.9) < quarter && distance(0, 10.1) > quarter,
        "at a quarter, the first edge is whole and nothing past x = 10 shows"
    );
}

#[test]
fn a_closed_line_keeps_its_seam_however_its_last_point_is_written() {
    let plain = [
        Vec2::new(0.0, 0.0),
        Vec2::new(20.0, 0.0),
        Vec2::new(0.0, 20.0),
    ];
    let mut repeated = plain.to_vec();
    repeated.push(Vec2::new(0.0, 0.0));
    let mut near = plain.to_vec();
    near.push(Vec2::new(0.0, 0.02));
    let cache = cache(
        0,
        1,
        &[closed_line(plain), closed_line(repeated), closed_line(near)],
    );
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, &cache).expect("it builds");
    let along = |piece: u32, at: [f32; 2]| {
        let found = sampled(&mesh, piece, at);
        let [distance, _] = found.first().copied().expect("the stroke covers the point");
        distance
    };
    let read = |piece: u32, at: [f32; 2]| {
        along(piece, at) / f64::from(mesh.pieces()[usize::try_from(piece).expect("a piece")].length)
    };
    for at in [[5.0, 0.25], [15.0, 0.25], [0.25, 15.0], [0.25, 5.0]] {
        let expected = read(0, at);
        for piece in [1, 2] {
            assert!(
                (read(piece, at) - expected).abs() < 1e-3,
                "piece {piece} at {at:?}: {} against {expected}",
                read(piece, at)
            );
        }
    }
    for at in [[5.0, 0.25], [15.0, 0.25], [0.25, 15.0], [0.25, 5.0]] {
        for piece in [1, 2] {
            assert!(
                (along(piece, at) - along(0, at)).abs() < 1e-3,
                "dashes walk piece {piece} at {at:?} as the plain spelling: {} against {}",
                along(piece, at),
                along(0, at)
            );
        }
    }
    assert!(read(0, [5.0, 0.25]) < 0.1, "the first edge reveals first");
    assert!(read(0, [0.25, 5.0]) > 0.9, "the last edge reveals last");
}

#[test]
fn a_closed_line_lyon_keeps_two_points_of_goes_out_and_back() {
    let three = [
        Vec2::new(0.0, 0.0),
        Vec2::new(17.0, 0.0),
        Vec2::new(17.01, 0.0),
    ];
    let cache = cache(0, 1, &[closed_line(three)]);
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, &cache).expect("it builds");
    let legs: Vec<bool> = mesh.pieces().iter().map(|piece| piece.retrace).collect();
    assert_eq!(legs, [false, true], "out, then back");
    let total = 2.0 * f64::from(17.01_f32);
    assert!((f64::from(mesh.pieces()[0].length) - total).abs() < 1e-5);
    let [[out, _]] = sampled(&mesh, 0, [9.0, 0.5])[..] else {
        panic!("one outward triangle");
    };
    let [[back, _]] = sampled(&mesh, 1, [9.0, 0.5])[..] else {
        panic!("one return triangle");
    };
    assert!((out - 9.0).abs() < 1e-4, "{out}");
    assert!(
        (back - (f64::from(17.01_f32) + 8.01)).abs() < 0.01,
        "{back}"
    );
}

#[test]
fn every_stroke_triangle_starts_at_its_earliest_vertex() {
    let mut run = vec![Vec2::new(0.0, 0.0)];
    run.extend((1..=500).map(|at| Vec2::new(if at % 2 == 1 { 0.04 } else { 0.0 }, 0.0)));
    run.extend([
        Vec2::new(10.0, 0.0),
        Vec2::new(10.04, 0.0),
        Vec2::new(10.0, 0.0),
    ]);
    run.extend([
        Vec2::new(20.0, 0.0),
        Vec2::new(20.0, 10.0),
        Vec2::new(20.04, 10.0),
    ]);
    let prims = [
        line(run),
        closed_line([
            Vec2::new(0.0, 0.0),
            Vec2::new(20.0, 0.0),
            Vec2::new(0.0, 20.0),
            Vec2::new(0.0, 0.02),
        ]),
        closed_line([
            Vec2::new(0.0, 0.0),
            Vec2::new(17.0, 0.0),
            Vec2::new(17.01, 0.0),
        ]),
    ];
    let cache = cache(0, 1, &prims);
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, &cache).expect("it builds");
    let along = |index: u32| mesh.vertices()[usize::try_from(index).expect("an index")].along;
    let (triangles, rest) = mesh.indices().as_chunks::<3>();
    assert!(!triangles.is_empty() && rest.is_empty());
    for corners in triangles {
        let [first, second, third] = corners.map(along);
        assert!(
            first <= second && first <= third,
            "a triangle's skip is read from its earliest point: {first}, {second}, {third}"
        );
    }
}

#[test]
fn a_triangle_starts_at_an_earliest_corner_when_two_tie() {
    for along in [
        [20.0, 10.0, 10.0],
        [10.0, 20.0, 10.0],
        [10.0, 10.0, 20.0],
        [5.0, 5.0, 5.0],
        [20.0, 30.0, 10.0],
    ] {
        let first = along[crate::stage::earliest(along)];
        assert!(
            along.iter().all(|other| first <= *other),
            "{along:?} starts at {first}"
        );
    }
}

#[test]
fn a_line_of_no_points_or_one_builds_to_nothing() {
    let one = [Vec2::new(3.0, 4.0)];
    let cache = cache(
        0,
        1,
        &[
            line(Vec::<Vec2>::new()),
            line(one),
            closed_line(Vec::<Vec2>::new()),
            closed_line(one),
        ],
    );
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, &cache).expect("it builds");
    assert_eq!(
        (
            mesh.vertices().len(),
            mesh.indices().len(),
            mesh.pieces().len()
        ),
        (0, 0, 0)
    );
}

#[test]
fn a_join_folded_at_the_first_point_leaves_the_first_edge_rising_from_it() {
    let cache = cache(
        0,
        1,
        &[closed_line([
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            Vec2::new(10.0, 1.0),
        ])],
    );
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, &cache).expect("it builds");
    let perimeter = 11.0 + f64::from(10.0_f32).hypot(1.0);
    let length = f64::from(mesh.pieces()[0].length);
    assert!((length - perimeter).abs() < 1e-4, "{length}");
    let slack = 1.0;
    for at in [[3.0, 0.5], [6.0, 0.8]] {
        let [first, y] = at.map(f64::from);
        let last = 11.0 + ((10.0 - first) * 10.0 + (1.0 - y)) / f64::from(10.0_f32).hypot(1.0);
        let found = sampled(&mesh, 0, at);
        assert!(
            found
                .iter()
                .any(|[distance, _]| (distance - first).abs() < slack),
            "the first edge covers {at:?}: {found:?}"
        );
        for [distance, _] in &found {
            assert!(
                (distance - first).abs() < slack || (distance - last).abs() < slack,
                "every triangle at {at:?} reads one edge's distance: {distance} against {first} \
                 or {last}"
            );
            assert!(
                (distance / length - first / perimeter).abs() < slack / perimeter
                    || (distance / length - last / perimeter).abs() < slack / perimeter,
                "and its reveal: {}",
                distance / length
            );
        }
    }
}

#[test]
fn a_border_cuts_straight_across_its_band_as_main_does() {
    let prims = [
        bordered([10.0, 10.0], 0.0, 1.0),
        bordered([20.0, 10.0], 0.0, 4.0),
        bordered([20.0, 20.0], 2.0, 3.0),
    ];
    let cache = cache(0, 1, &prims);
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, &cache).expect("it builds");
    let reveal = |piece: u32, at: [f32; 2]| {
        let found = sampled(&mesh, piece, at);
        let [_, reveal] = found.first().copied().expect("the band covers the point");
        assert!(found.iter().all(|[_, other]| (other - reveal).abs() < 1e-6));
        reveal
    };
    for (piece, cut, points, total) in [
        (0, 5.0, [[10.0, 5.0], [9.5, 5.0], [9.1, 5.0]], 40.0),
        (1, 5.0, [[20.0, 5.0], [18.0, 5.0], [16.5, 5.0]], 60.0),
    ] {
        for at in points {
            assert!(
                (reveal(piece, at) - cut / total).abs() < 1e-6,
                "main's cut joins the same fraction of both edges: {at:?}"
            );
            assert!(reveal(piece, [at[0], at[1] - 0.1]) < cut / total);
            assert!(reveal(piece, [at[0], at[1] + 0.1]) > cut / total);
        }
    }
    assert!(
        of_piece(&mesh, 2).all(|v| v.reveal.is_finite() && v.weight.is_finite()),
        "a corner the inset collapses stays finite"
    );
    let corner = reveal(2, [1.0, 1.5]);
    assert!((0.0..=1.0).contains(&corner), "{corner}");
}

#[test]
fn the_fill_reservation_is_not_a_bound_lyon_can_emit_past_the_charge() {
    let step = r#"{"id":"s","put":[{"shape":{"id":"f","kind":"polygon","fill":"blue","width":0,"points":[[0.07,0.93],[0.38,0.59999996],[0.22999999,0.37],[0.22999999,0.66999996],[0.32999998,0.089999996],[0.48999998,0.45],[0.84,0.14]]}}]}"#;
    let view = fixture::view("g", &[step.to_owned()], &mut GlobalBudget::default());
    let (_, scale, prims) = view.shared_sources().next().expect("the fill");
    let charged = PaintWork::of(prims, scale).fill_vertices;
    let boxed = Stager::new().stage(&view);
    let (mut input, mut mesh) = (BuildInput::new(), Mesh::new());
    build(&mut input, &mut mesh, render(&*boxed)).expect("it builds");
    assert!(
        mesh.vertices().len() > charged,
        "{} vertices against a charge of {charged}",
        mesh.vertices().len()
    );
}

fn of_piece(mesh: &Mesh, piece: u32) -> impl Iterator<Item = &crate::Vertex> {
    mesh.vertices()
        .iter()
        .filter(move |vertex| vertex.piece == piece)
}
