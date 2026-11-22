//! Tessellation, bit-identical topology and representability without per-build allocation.
use kumochi_scene::{
    geom::Vec3,
    record::{Segments, Solid, SolidShape},
    three::{
        Transform, Triangle,
        mesh::{Built, MeshScratch, build, build_local},
        size::{MeshError, triangles},
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::AtomicBool,
};

#[path = "support/allocation.rs"]
mod allocation;

fn solid(shape: &str, mut fields: Value) -> Solid {
    let fields = fields.as_object_mut().expect("fixture is an object");
    fields.insert("in".into(), json!("view"));
    fields.insert("id".into(), json!("solid"));
    fields.insert("shape".into(), json!(shape));
    serde_json::from_value(Value::Object(fields.clone())).expect("valid decoded fixture")
}

fn incidence(mesh: &[Triangle], boundaries: usize) {
    let mut edges = BTreeMap::<_, (u32, i32)>::new();
    for triangle in mesh {
        let points = triangle
            .v
            .map(|p| [p.x.to_bits(), p.y.to_bits(), p.z.to_bits()]);
        for i in 0..3 {
            let (a, b) = (points[i], points[(i + 1) % 3]);
            assert_ne!(a, b, "a retained edge is not collapsed");
            let (edge, direction) = if a < b { ((a, b), 1) } else { ((b, a), -1) };
            let (count, balance) = edges.entry(edge).or_insert((0, 0));
            *count += 1;
            *balance += direction;
        }
    }
    let mut unmatched = 0;
    for (count, balance) in edges.values() {
        if *count == 1 {
            unmatched += 1;
            assert_eq!(balance.abs(), 1, "a boundary has one directed use");
        } else {
            assert_eq!(
                (*count, *balance),
                (2, 0),
                "shared f32-bit edges have opposite winding"
            );
        }
    }
    assert_eq!(
        unmatched, boundaries,
        "only declared open boundary edges remain"
    );
}

fn mesh(shape: &Solid, world: &Transform) -> Vec<Triangle> {
    let mut scratch = MeshScratch::new();
    let count = triangles(shape).expect("valid count");
    let mut out = Vec::with_capacity(usize::try_from(count).expect("bounded count"));
    let built = build(
        shape,
        world,
        0,
        &mut out,
        &AtomicBool::new(false),
        &mut scratch,
    )
    .expect("representable mesh");
    assert_eq!(
        built.written + built.dropped,
        count,
        "every candidate is accounted for"
    );
    out
}

fn normal(vertices: [Vec3; 3]) -> [f64; 3] {
    let [a, b, c] = vertices.map(|p| [f64::from(p.x), f64::from(p.y), f64::from(p.z)]);
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    [
        ab[1] * ac[2] - ab[2] * ac[1],
        ab[2] * ac[0] - ab[0] * ac[2],
        ab[0] * ac[1] - ab[1] * ac[0],
    ]
}

fn area(mesh: &[Triangle]) -> f64 {
    mesh.iter()
        .map(|triangle| {
            normal(triangle.v)
                .into_iter()
                .map(|n| n * n)
                .sum::<f64>()
                .sqrt()
                / 2.0
        })
        .sum()
}

/// Each local point placed by `world` and narrowed as the world build narrows, as f32 bits.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "narrows exactly as the world build does"
)]
fn narrowed(world: &Transform, local: &[[[f64; 3]; 3]]) -> Vec<[[u32; 3]; 3]> {
    local
        .iter()
        .map(|triangle| {
            triangle.map(|p| {
                world.apply(p).map(|c| {
                    let v = c as f32;
                    if v == 0.0 { 0 } else { v.to_bits() }
                })
            })
        })
        .collect()
}
fn bits(world: &[Triangle]) -> Vec<[[u32; 3]; 3]> {
    world
        .iter()
        .map(|triangle| {
            triangle
                .v
                .map(|p| [p.x, p.y, p.z].map(|v| if v == 0.0 { 0 } else { v.to_bits() }))
        })
        .collect()
}

fn maximum_outline() -> Value {
    let points: Vec<_> = (0..16)
        .map(|i| [i, 0])
        .chain((0..16).map(|i| [16, i]))
        .chain((0..16).map(|i| [16 - i, 16]))
        .chain((0..16).map(|i| [0, 16 - i]))
        .collect();
    json!(points)
}

#[test]
fn subdivision_seams_merged_points_and_axis_vertices_weld_bit_for_bit() {
    let world = Transform::local([12.5, -7.25, 3.0], [17.0, 31.0, 53.0], 1.3);
    for (n, sphere_faces) in [
        (3, 20),
        (5, 20),
        (6, 80),
        (11, 80),
        (12, 320),
        (23, 320),
        (24, 1280),
        (48, 1280),
    ] {
        for (kind, fields, written, boundaries) in [
            ("sphere", json!({"r":1,"segments":n}), sphere_faces, 0),
            ("cylinder", json!({"r":1,"h":2,"segments":n}), 4 * n - 4, 0),
            ("cone", json!({"r":1,"h":2,"segments":n}), 2 * n - 2, 0),
            (
                "cone",
                json!({"r":1,"r2":0.5,"h":2,"segments":n}),
                4 * n - 4,
                0,
            ),
            ("torus", json!({"r":2,"r2":0.5,"segments":n}), 2 * n * n, 0),
            (
                "lathe",
                json!({"points":[[1,0],[1,0],[0.5,1],[1,2]],"segments":n}),
                4 * n,
                2 * n,
            ),
            (
                "lathe",
                json!({"points":[[0,-1],[1,0],[0,1]],"segments":n}),
                2 * n,
                0,
            ),
            (
                "tube",
                json!({"path":[[0,0,0],[0,0,0],[0,2,0],[1,3,1],[1,4,2]],"r2":0.2,"segments":n}),
                6 * n + 2 * (n - 2),
                0,
            ),
        ] {
            let actual = mesh(&solid(kind, fields), &world);
            assert_eq!(actual.len(), written, "{kind}, {n} segments");
            incidence(&actual, boundaries);
        }
    }
    for points in [
        json!([[0, 0], [1, 0], [2, 0], [2, 2], [0, 2], [0, 0]]),
        json!([[0, 2], [2, 2], [2, 0], [1, 0], [0, 0], [0, 0]]),
    ] {
        let extrusion = mesh(
            &solid("extrude", json!({"points":points,"depth":1})),
            &world,
        );
        assert_eq!(extrusion.len(), 16);
        incidence(&extrusion, 0);
    }
    let boxed = mesh(&solid("box", json!({"size":[1,2,3]})), &world);
    assert_eq!(boxed.len(), 12);
    incidence(&boxed, 0);
}

#[test]
fn analytic_areas_and_outward_normals_are_preserved() {
    let identity = Transform::local([0.0; 3], [0.0; 3], 1.0);
    let box_mesh = mesh(&solid("box", json!({"size":[2,3,4]})), &identity);
    assert_eq!(area(&box_mesh).to_bits(), 52.0_f64.to_bits());
    for (kind, fields, written) in [
        ("box", json!({"size":[2,3,4]}), 12),
        ("sphere", json!({"r":1,"segments":48}), 1280),
        ("cylinder", json!({"r":1,"h":2,"segments":48}), 188),
        ("torus", json!({"r":2,"r2":0.5,"segments":48}), 4608),
    ] {
        let actual = mesh(&solid(kind, fields), &identity);
        assert_eq!(actual.len(), written, "{kind}");
        for triangle in actual {
            let n = normal(triangle.v);
            let mut centre = [0.0; 3];
            for p in triangle.v {
                centre[0] += f64::from(p.x) / 3.0;
                centre[1] += f64::from(p.y) / 3.0;
                centre[2] += f64::from(p.z) / 3.0;
            }
            if kind == "torus" {
                let radial = centre[0].hypot(centre[2]);
                centre[0] -= 2.0 * centre[0] / radial;
                centre[2] -= 2.0 * centre[2] / radial;
            }
            assert!(
                n.into_iter().zip(centre).map(|(n, p)| n * p).sum::<f64>() > 0.0,
                "{kind} faces outward"
            );
        }
    }
    let cylinder = mesh(
        &solid("cylinder", json!({"r":1,"h":2,"segments":48})),
        &identity,
    );
    let side: f64 = cylinder
        .iter()
        .filter(|triangle| normal(triangle.v)[1].abs() < 1e-12)
        .map(|triangle| area(std::slice::from_ref(triangle)))
        .sum();
    let expected = 48.0 * 2.0 * (std::f64::consts::PI / 48.0).sin() * 2.0;
    assert!(
        (side - expected).abs() < expected * 1e-6,
        "polygonal cylinder side area"
    );
    for points in [
        json!([[0, 0], [2, 0], [0, 2]]),
        json!([[0, 2], [2, 0], [0, 0]]),
    ] {
        let triangles = mesh(
            &solid("extrude", json!({"points":points,"depth":1})),
            &identity,
        );
        assert_eq!(triangles.len(), 8);
        assert!(
            (area(&triangles) - (8.0 + 8.0_f64.sqrt())).abs() < 1e-12,
            "two caps plus three rectangular sides"
        );
    }
    for scale in [0.001_f32, 1.0] {
        let world = Transform::local([0.0; 3], [0.0; 3], scale);
        let expected = 52.0 * f64::from(scale).powi(2);
        assert!(
            (area(&mesh(&solid("box", json!({"size":[2,3,4]})), &world)) - expected).abs()
                < expected * 1e-6,
            "scaled box area"
        );
        let shape = solid("cylinder", json!({"r":1,"h":2,"segments":48}));
        let expected = (192.0 * (std::f64::consts::PI / 48.0).sin()
            + 48.0 * (std::f64::consts::PI / 24.0).sin())
            * f64::from(scale).powi(2);
        assert!(
            (area(&mesh(&shape, &world)) - expected).abs() < expected * 1e-6,
            "scaled polygonal cylinder area"
        );
    }
    let shape = solid("cylinder", json!({"r":1,"h":2,"segments":4}));
    for pos in [20000.0, -20000.0] {
        let world = Transform::local([pos; 3], [0.0; 3], 1.0);
        let expected = 4.0 + 8.0 * 2.0_f64.sqrt();
        assert!(
            (area(&mesh(&shape, &world)) - expected).abs() < expected * 1e-12,
            "cardinal ring vertices remain exact at the coordinate limits"
        );
    }
}

#[test]
fn capacity_cancel_and_degenerate_failures_preserve_a_prefix() {
    let mut scratch = MeshScratch::new();
    let shape = solid("box", json!({"size":[1,1,1]}));
    let identity = Transform::local([0.0; 3], [0.0; 3], 1.0);
    let mut empty = Vec::with_capacity(11);
    assert_eq!(
        build(
            &shape,
            &identity,
            0,
            &mut empty,
            &AtomicBool::new(false),
            &mut scratch
        ),
        Err(MeshError::Full)
    );
    assert_eq!(empty.len(), 0);
    assert_eq!(
        build(
            &shape,
            &identity,
            0,
            &mut empty,
            &AtomicBool::new(true),
            &mut scratch
        ),
        Err(MeshError::Cancelled)
    );
    assert_eq!(empty.len(), 0);
    let prefix = Triangle {
        v: [Vec3::new(1.0, 2.0, 3.0); 3],
        part: 42,
    };
    let mut out = Vec::with_capacity(12);
    out.push(prefix);
    let pointer = out.as_ptr();
    assert_eq!(
        build(
            &shape,
            &identity,
            0,
            &mut out,
            &AtomicBool::new(false),
            &mut scratch
        ),
        Err(MeshError::Full)
    );
    assert_eq!(out, [prefix]);
    assert_eq!(out.as_ptr(), pointer);
    assert_eq!(
        build(
            &shape,
            &identity,
            0,
            &mut out,
            &AtomicBool::new(true),
            &mut scratch
        ),
        Err(MeshError::Cancelled)
    );
    assert_eq!(out, [prefix]);
    out.reserve_exact(12);
    assert_eq!(
        build(
            &shape,
            &identity,
            0,
            &mut out,
            &AtomicBool::new(false),
            &mut scratch
        ),
        Ok(Built {
            written: 12,
            dropped: 0
        })
    );
    assert_eq!(out[0], prefix);
    assert_eq!(out.len(), 13);
    let thin = solid("cylinder", json!({"r":0.001,"h":2000,"segments":48}));
    let mut out = Vec::with_capacity(189);
    out.push(prefix);
    assert_eq!(
        build(
            &thin,
            &identity,
            0,
            &mut out,
            &AtomicBool::new(false),
            &mut scratch
        ),
        Err(MeshError::Degenerate("r"))
    );
    out.truncate(1);
    assert_eq!(out, [prefix]);
}

/// Sub-ulp distortion is accepted because the near-plane precision floor bounds its screen size.
#[test]
fn world_narrowing_refuses_collisions_and_zero_area_without_dropping_faces() {
    let mut scratch = MeshScratch::new();
    let shape = solid("box", json!({"size":[1,1,1]}));
    let mut out = Vec::with_capacity(12);
    assert_eq!(
        build(
            &shape,
            &Transform::local([19999.0; 3], [0.0; 3], 0.001),
            0,
            &mut out,
            &AtomicBool::new(false),
            &mut scratch
        ),
        Err(MeshError::Degenerate("pos"))
    );
    let rounded = mesh(&shape, &Transform::local([19999.0; 3], [0.0; 3], 0.01));
    assert_eq!(rounded.len(), 12);
    assert_eq!(
        area(&rounded).to_bits(),
        0.000_823_974_609_375_f64.to_bits()
    );
    for pos in [20000.0, -20000.0] {
        assert_eq!(
            area(&mesh(&shape, &Transform::local([pos; 3], [0.0; 3], 1.0))).to_bits(),
            6.0_f64.to_bits()
        );
    }
    let shape = solid(
        "extrude",
        json!({"depth":1,"points":[[0,0],[2,0],[2,2],[0.0005,0.0005],[0,2]]}),
    );
    let mut out = Vec::with_capacity(16);
    assert_eq!(
        build(
            &shape,
            &Transform::local([19999.0; 3], [0.0; 3], 1.0),
            0,
            &mut out,
            &AtomicBool::new(false),
            &mut scratch
        ),
        Err(MeshError::Degenerate("pos"))
    );
    out.clear();
    let shape = solid(
        "extrude",
        json!({"depth":1,"points":[[0,0],[2,0],[1,0.0005]]}),
    );
    assert_eq!(
        build(
            &shape,
            &Transform::local([19999.0; 3], [0.0; 3], 1.0),
            0,
            &mut out,
            &AtomicBool::new(false),
            &mut scratch
        ),
        Err(MeshError::Degenerate("pos"))
    );
}

#[test]
fn only_lathe_axis_candidates_drop_and_the_exact_count_is_reported() {
    let mut scratch = MeshScratch::new();
    for n in [3_u8, 48] {
        let shape = solid("lathe", json!({"points":[[0,-1],[1,0],[0,1]],"segments":n}));
        let mut out = Vec::with_capacity(usize::from(n) * 4);
        let expected = u32::from(n) * 2;
        assert_eq!(
            build(
                &shape,
                &Transform::local([0.0; 3], [0.0; 3], 1.0),
                0,
                &mut out,
                &AtomicBool::new(false),
                &mut scratch
            ),
            Ok(Built {
                written: expected,
                dropped: expected
            })
        );
        incidence(&out, 0);
    }
}

#[test]
fn every_shape_has_the_analytic_candidate_count_at_both_segment_limits() {
    let mut scratch = MeshScratch::new();
    for (shape, fields, expected) in [
        ("box", json!({"size":[2,3,4]}), [12, 12]),
        ("sphere", json!({"r":1}), [20, 1280]),
        ("cylinder", json!({"r":1,"h":2}), [8, 188]),
        ("cone", json!({"r":1,"h":2}), [4, 94]),
        ("cone", json!({"r":1,"r2":0.5,"h":2}), [8, 188]),
        ("torus", json!({"r":2,"r2":0.5}), [18, 4608]),
        (
            "extrude",
            json!({"points":[[0,0],[2,0],[0,2]],"depth":1}),
            [8, 8],
        ),
        ("lathe", json!({"points":[[1,0],[1,2]]}), [6, 96]),
        ("tube", json!({"path":[[0,0,0],[0,2,0]],"r2":0.5}), [8, 188]),
    ] {
        for (n, count) in [3, 48].into_iter().zip(expected) {
            let mut input = solid(shape, fields.clone());
            if !matches!(shape, "box" | "extrude") {
                input.segments = Some(Segments::try_from(n).expect("segment boundary"));
            }
            assert_eq!(triangles(&input), Ok(count), "{shape} at {n}");
            let mut out = Vec::with_capacity(usize::try_from(count).expect("bounded count"));
            let pointer = out.as_ptr();
            let capacity = out.capacity();
            let world = Transform::local([123.5, -78.25, 9.125], [17.0, 31.0, 53.0], 1.3);
            assert_eq!(
                build(
                    &input,
                    &world,
                    7,
                    &mut out,
                    &AtomicBool::new(false),
                    &mut scratch
                ),
                Ok(Built {
                    written: count,
                    dropped: 0
                }),
                "{shape} at {n}"
            );
            assert_eq!(out.as_ptr(), pointer);
            assert_eq!(out.capacity(), capacity);
            assert_eq!(out.len(), usize::try_from(count).expect("bounded count"));
            assert!(
                out.iter().all(|face| face.part == 7),
                "source part is propagated"
            );
            incidence(
                &out,
                if shape == "lathe" {
                    2 * usize::from(n)
                } else {
                    0
                },
            );
        }
    }
}

#[test]
fn merges_compare_against_the_last_retained_point_in_all_point_shapes() {
    let world = Transform::local([0.0; 3], [0.0; 3], 1.0);
    let path = solid(
        "tube",
        json!({"segments":3,"r2":0.001,
        "path":[[0,0,0],[0,0,0],[0.000_003,0,0],[0.000_006,0,0],[10,0,0]]}),
    );
    assert_eq!(triangles(&path), Ok(14));
    assert_eq!(mesh(&path, &world).len(), 14);
    let profile = solid(
        "lathe",
        json!({"segments":3,
        "points":[[1,0],[1,0],[1,0.000_003],[1,0.000_006],[1,10]]}),
    );
    assert_eq!(triangles(&profile), Ok(12));
    assert_eq!(mesh(&profile, &world).len(), 12);
    let outline = solid(
        "extrude",
        json!({"depth":1,
        "points":[[0,0],[0,0],[0.000_004,0],[0.000_008,0],[10,0],[0,10],[0,0]]}),
    );
    assert_eq!(triangles(&outline), Ok(12));
    assert_eq!(mesh(&outline, &world).len(), 12);
}

#[test]
fn maximum_point_counts_are_sized_without_allocating() {
    let mut scratch = MeshScratch::new();
    let profile: Vec<_> = (0..512).map(|i| [1, i]).collect();
    let path: Vec<_> = (0..512).map(|i| [0, i, 0]).collect();
    for (shape, expected) in [
        (
            solid("extrude", json!({"depth":1,"points":maximum_outline()})),
            252,
        ),
        (
            solid("lathe", json!({"points":profile,"segments":48})),
            49056,
        ),
        (
            solid("tube", json!({"path":path,"r2":0.5,"segments":48})),
            49148,
        ),
        (solid("torus", json!({"r":2,"r2":0.5,"segments":48})), 4608),
        (solid("box", json!({"size":[1,1,1]})), 12),
        (solid("sphere", json!({"r":1,"segments":48})), 1280),
        (solid("cylinder", json!({"r":1,"h":2,"segments":48})), 188),
        (solid("cone", json!({"r":1,"h":2,"segments":48})), 94),
    ] {
        let (count, allocations) = allocation::count(|| triangles(&shape));
        assert_eq!(count, Ok(expected));
        assert_eq!(allocations, 0);
        let mut out = Vec::with_capacity(usize::try_from(expected).expect("bounded count"));
        let world = Transform::local([0.0; 3], [0.0; 3], 1.0);
        for _ in 0..2 {
            out.clear();
            let (built, allocations) = allocation::count(|| {
                build(
                    &shape,
                    &world,
                    0,
                    &mut out,
                    &AtomicBool::new(false),
                    &mut scratch,
                )
            });
            assert_eq!(
                built,
                Ok(Built {
                    written: expected,
                    dropped: 0
                })
            );
            assert_eq!(allocations, 0);
        }
    }
    let profile: Vec<_> = (0..251).map(|i| [1, i]).collect();
    assert_eq!(
        triangles(&solid("lathe", json!({"points":profile,"segments":48}))),
        Ok(24000)
    );
}

fn assert_box_face_groups(groups: &[u8], triangles: &[[[f64; 3]; 3]]) {
    assert_eq!(groups, [2; 6], "two triangles share each box face");
    let (faces, remainder) = triangles.as_chunks::<2>();
    assert!(remainder.is_empty(), "every box triangle belongs to a pair");
    let planes: BTreeSet<_> = faces
        .iter()
        .map(|face| {
            let axis = (0..3)
                .find(|&axis| {
                    let coordinate = face[0][0][axis].to_bits();
                    face.iter()
                        .flatten()
                        .all(|p| p[axis].to_bits() == coordinate)
                })
                .expect("one box plane per group");
            (axis, face[0][0][axis].to_bits())
        })
        .collect();
    let expected = [0.5_f64, 1.0, 1.5]
        .into_iter()
        .enumerate()
        .flat_map(|(axis, half)| [-half, half].map(|v| (axis, v.to_bits())))
        .collect();
    assert_eq!(
        planes, expected,
        "all six distinct box planes must be present"
    );
}

#[test]
fn local_and_world_builds_agree_at_the_composed_rest() {
    let parent = Transform::local([10.0, 0.0, 0.0], [0.0; 3], 2.0);
    let rest = Transform::local([1.0, 0.0, 0.0], [30.0, 0.0, 0.0], 1.5).then(&parent);
    let far = Transform::local([0.0; 3], [0.0; 3], 0.001).then(&Transform::local(
        [19998.0, 0.0, 0.0],
        [0.0; 3],
        1.0,
    ));
    let ok = |written, dropped| Ok(Built { written, dropped });
    // Counts from each shape's topology: 6 quads; a 320-face icosphere; 4n − 4 and 2n − 2 for
    // cylinder and cone; 2n² for the torus; one axis triangle dropped per lathe quad; 4 sides
    // and 2 caps for the square; 4 bands and 2 two-triangle caps for the tube.
    let cases = [
        (solid("box", json!({"size":[1,2,3]})), &rest, ok(12, 0)),
        (
            solid("sphere", json!({"r":1,"segments":12})),
            &rest,
            ok(320, 0),
        ),
        (
            solid("cylinder", json!({"r":1,"h":2,"segments":12})),
            &rest,
            ok(44, 0),
        ),
        (
            solid("cone", json!({"r":1,"h":2,"segments":12})),
            &rest,
            ok(22, 0),
        ),
        (
            solid("torus", json!({"r":2,"r2":0.5,"segments":12})),
            &rest,
            ok(288, 0),
        ),
        (
            solid("lathe", json!({"points":[[0,-1],[1,0],[0,1]],"segments":8})),
            &rest,
            ok(16, 16),
        ),
        (
            solid(
                "extrude",
                json!({"points":[[0,0],[1,0],[1,1],[0,1]],"depth":1}),
            ),
            &rest,
            ok(12, 0),
        ),
        (
            solid(
                "tube",
                json!({"path":[[0,0,0],[0,0,1]],"r2":0.25,"segments":4}),
            ),
            &rest,
            ok(12, 0),
        ),
        (
            solid("box", json!({"size":[1,1,1]})),
            &far,
            Err(MeshError::Degenerate("pos")),
        ),
    ];
    let mut scratch = MeshScratch::new();
    for (shape, world, expected) in cases {
        let count = usize::try_from(triangles(&shape).expect("valid count")).expect("bounded");
        let (mut a, mut b, mut groups) = (
            Vec::with_capacity(count),
            Vec::with_capacity(count),
            Vec::new(),
        );
        let cancel = AtomicBool::new(false);
        let local = build_local(&shape, world, &mut b, &mut groups, &cancel, &mut scratch);
        assert_eq!(local, expected, "{:?}", shape.shape);
        assert_eq!(
            build(&shape, world, 0, &mut a, &cancel, &mut scratch),
            expected,
            "{:?}",
            shape.shape
        );
        if let Ok(built) = expected {
            assert_eq!(
                groups.iter().map(|&n| u32::from(n)).sum::<u32>(),
                built.written
            );
            assert_eq!(narrowed(world, &b), bits(&a), "{:?}", shape.shape);
            if shape.shape == SolidShape::Box {
                assert_box_face_groups(&groups, &b);
            }
        }
    }
    let mut out = Vec::with_capacity(12);
    let alone = build_local(
        &solid("box", json!({"size":[1,1,1]})),
        &Transform::local([0.0; 3], [0.0; 3], 0.001),
        &mut out,
        &mut Vec::new(),
        &AtomicBool::new(false),
        &mut scratch,
    );
    assert_eq!(
        alone.map(|built| built.written),
        Ok(12),
        "only the parent collapses it"
    );
}

#[test]
fn adversarial_extrusions_keep_their_authored_points_locally() {
    // CAPS-191's three-point and CAPS-194's seven-point outlines: a first-vertex f32 origin made
    // both caps or B, C and D collinear. Local f64 points are the authored ones, exactly.
    let identity = Transform::local([0.0; 3], [0.0; 3], 1.0);
    let cases: [(&[[f32; 2]], u32); 2] = [
        (&[[-6000.0, -5999.0], [1.001, 2.001], [0.0, 1.0]], 8),
        (
            &[
                [-8500.0, 1.0],
                [9000.0, 0.0],
                [9000.001, 1.0],
                [9000.0, 2.0],
                [8999.0, 0.5],
                [-9500.0, 2.0],
                [-9500.0, 0.0],
            ],
            24,
        ),
    ];
    let mut scratch = MeshScratch::new();
    let cancel = AtomicBool::new(false);
    for (outline, written) in cases {
        let shape = solid("extrude", json!({"points": outline, "depth": 1}));
        let count = usize::try_from(written).expect("bounded");
        let (mut world, mut local, mut groups) = (
            Vec::with_capacity(count),
            Vec::with_capacity(count),
            Vec::new(),
        );
        let expected = Ok(Built {
            written,
            dropped: 0,
        });
        assert_eq!(
            build(&shape, &identity, 0, &mut world, &cancel, &mut scratch),
            expected
        );
        assert_eq!(
            build_local(
                &shape,
                &identity,
                &mut local,
                &mut groups,
                &cancel,
                &mut scratch
            ),
            expected
        );
        assert_eq!(narrowed(&identity, &local), bits(&world));
        let authored: BTreeSet<_> = outline
            .iter()
            .map(|p| p.map(|v| f64::from(v).to_bits()))
            .collect();
        let depths = [-0.5_f64, 0.5].map(f64::to_bits);
        for p in local.iter().flatten() {
            assert!(
                authored.contains(&[p[0].to_bits(), p[1].to_bits()]),
                "{p:?} is not an authored point"
            );
            assert!(depths.contains(&p[2].to_bits()), "{p:?}");
        }
        for depth in depths {
            let cap = local
                .iter()
                .filter(|face| face.iter().all(|p| p[2].to_bits() == depth))
                .flatten()
                .map(|p| [p[0].to_bits(), p[1].to_bits()])
                .collect();
            assert_eq!(authored, cap, "every authored point appears on both caps");
        }
    }
}
