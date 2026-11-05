//! Solid capacity counts, geometry validation and affine conventions.
use kumochi_scene::{
    Budget, ErrorCode,
    geom::Vec3,
    record::{Segments, ShapeError, Solid},
    three::{
        Transform, dist_eps,
        size::{MeshError, triangles},
    },
};
use serde_json::{Value, json};

#[path = "support/allocation.rs"]
mod allocation;

fn solid(shape: &str, mut fields: Value) -> Solid {
    let fields = fields.as_object_mut().expect("fixture is an object");
    fields.insert("in".into(), json!("view"));
    fields.insert("id".into(), json!("solid"));
    fields.insert("shape".into(), json!(shape));
    serde_json::from_value(Value::Object(fields.clone())).expect("valid decoded fixture")
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
fn every_shape_has_the_analytic_candidate_count_at_both_segment_limits() {
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
        }
    }
}

#[test]
fn sphere_subdivision_transitions_and_default_are_pinned() {
    let mut sphere = solid("sphere", json!({"r":1}));
    assert_eq!(triangles(&sphere), Ok(1280));
    for (n, count) in [
        (3, 20),
        (5, 20),
        (6, 80),
        (11, 80),
        (12, 320),
        (23, 320),
        (24, 1280),
        (48, 1280),
    ] {
        sphere.segments = Some(Segments::try_from(n).expect("valid segments"));
        assert_eq!(triangles(&sphere), Ok(count), "segments {n}");
    }
    assert_eq!(triangles(&solid("cylinder", json!({"r":1,"h":1}))), Ok(92));
    assert_eq!(
        triangles(&solid("torus", json!({"r":2,"r2":0.5}))),
        Ok(1152)
    );
}

#[test]
fn merges_compare_against_the_last_retained_point_in_all_point_shapes() {
    let path = solid(
        "tube",
        json!({"segments":3,"r2":0.001,
        "path":[[0,0,0],[0,0,0],[0.000_003,0,0],[0.000_006,0,0],[10,0,0]]}),
    );
    assert_eq!(triangles(&path), Ok(14));
    let profile = solid(
        "lathe",
        json!({"segments":3,
        "points":[[1,0],[1,0],[1,0.000_003],[1,0.000_006],[1,10]]}),
    );
    assert_eq!(triangles(&profile), Ok(12));
    let outline = solid(
        "extrude",
        json!({"depth":1,
        "points":[[0,0],[0,0],[0.000_004,0],[0.000_008,0],[10,0],[0,10],[0,0]]}),
    );
    assert_eq!(triangles(&outline), Ok(12));
}

#[test]
fn point_origin_offsets_do_not_change_merge_tolerance() {
    for offset in [0.0, 10_000.0] {
        let shape = solid(
            "extrude",
            json!({"depth":1,"points":[
            [offset,offset],[offset+0.001,offset],[offset+0.002,offset],
            [offset+10000.0,offset],[offset,offset+10000.0]]}),
        );
        assert_eq!(triangles(&shape), Ok(8));
        let shape = solid(
            "tube",
            json!({"r2":0.001,"segments":3,"path":[
            [offset,0,0],[offset+0.001,0,0],[offset+0.002,0,0],[offset+10000.0,0,0]]}),
        );
        assert_eq!(triangles(&shape), Ok(8));
    }
}

#[test]
fn clockwise_concave_and_redundant_collinear_outlines_are_valid() {
    for points in [
        json!([[0, 0], [2, 0], [2, 2], [1, 1], [0, 2]]),
        json!([[0, 2], [1, 1], [2, 2], [2, 0], [0, 0]]),
        json!([[0, 0], [1, 0], [2, 0], [2, 2], [0, 2]]),
        json!([[0, 0], [1, 0], [2, 0], [2, 2], [0, 2], [0, 0]]),
    ] {
        assert_eq!(
            triangles(&solid("extrude", json!({"depth":1,"points":points}))),
            Ok(16)
        );
    }
}

#[test]
fn outlines_refuse_collapses_crossings_touches_and_overlapping_edges() {
    for points in [
        json!([[0, 0], [0, 0], [0, 0]]),
        json!([[0, 0], [0, 0], [1, 1]]),
        json!([[0, 0], [1, 1], [2, 2]]),
        json!([[0, 0], [2, 2], [0, 2], [2, 0]]),
        json!([[0, 0], [3, 3], [0, 3], [2, 0]]),
        json!([[0, 0], [2, 0], [2, 2], [1, 0], [0, 2]]),
        json!([[0, 0], [2, 0], [1, 0], [2, 2], [0, 2]]),
        json!([
            [0, 0],
            [3, 0],
            [3, 2],
            [1, 2],
            [1, 0],
            [2, 0],
            [2, 3],
            [0, 3]
        ]),
    ] {
        assert_eq!(
            triangles(&solid("extrude", json!({"depth":1,"points":points}))),
            Err(MeshError::Degenerate("points"))
        );
    }
}

#[test]
fn profiles_paths_and_antiparallel_threshold_have_typed_refusals() {
    for points in [json!([[1, 0], [1, 0]]), json!([[0, 0], [0, 2]])] {
        assert_eq!(
            triangles(&solid("lathe", json!({"points":points}))),
            Err(MeshError::Degenerate("points"))
        );
    }
    assert_eq!(
        triangles(&solid("tube", json!({"r2":1,"path":[[1,2,3],[1,2,3]]}))),
        Err(MeshError::Degenerate("path"))
    );
    for y in [0.0, 0.001, 0.001_414_214_6] {
        assert_eq!(
            triangles(&solid(
                "tube",
                json!({"r2":0.001,"segments":3,
            "path":[[-1,0,0],[0,0,0],[-1,y,0]]})
            )),
            Err(MeshError::Degenerate("path"))
        );
    }
    for y in [0.001_414_214_6_f32.next_up(), 0.002, 1.0] {
        assert_eq!(
            triangles(&solid(
                "tube",
                json!({"r2":0.001,"segments":3,
            "path":[[-1,0,0],[0,0,0],[-1,y,0]]})
            )),
            Ok(14)
        );
    }
    assert_eq!(
        triangles(&solid(
            "lathe",
            json!({"segments":3,"points":[[0,0],[1,1],[0,2]]})
        )),
        Ok(12)
    );
}

#[test]
fn maximum_point_counts_are_sized_without_allocating() {
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
    }
    let profile: Vec<_> = (0..251).map(|i| [1, i]).collect();
    assert_eq!(
        triangles(&solid("lathe", json!({"points":profile,"segments":48}))),
        Ok(24000)
    );
}

#[test]
fn sizing_keeps_shape_errors_and_decoder_limits() {
    let mut outline = maximum_outline();
    outline
        .as_array_mut()
        .expect("outline array")
        .push(json!([0, 0]));
    assert_eq!(
        triangles(&solid("extrude", json!({"depth":1,"points":outline}))),
        Err(MeshError::Shape(ShapeError {
            field: "points",
            code: ErrorCode::OverBudget(Budget::PathPoints)
        }))
    );
    for (shape, fields, field, code) in [
        ("box", json!({}), "size", ErrorCode::MissingField),
        (
            "box",
            json!({"size":[1,1,1],"segments":3}),
            "segments",
            ErrorCode::UnknownField,
        ),
        (
            "extrude",
            json!({"points":[[0,0],[1,1]],"depth":1}),
            "points",
            ErrorCode::OutOfRange,
        ),
        (
            "extrude",
            json!({"points":[[0,0],[1,0],[0,1]],"depth":1,"segments":3}),
            "segments",
            ErrorCode::UnknownField,
        ),
        (
            "lathe",
            json!({"points":[[1,0]]}),
            "points",
            ErrorCode::OutOfRange,
        ),
        (
            "lathe",
            json!({"points":[]}),
            "points",
            ErrorCode::MissingField,
        ),
        (
            "lathe",
            json!({"points":[[-1,0],[1,1]]}),
            "points",
            ErrorCode::OutOfRange,
        ),
        (
            "tube",
            json!({"path":[[0,0,0]],"r2":1}),
            "path",
            ErrorCode::OutOfRange,
        ),
        ("torus", json!({"r":1,"r2":1}), "r2", ErrorCode::OutOfRange),
    ] {
        assert_eq!(
            triangles(&solid(shape, fields)),
            Err(MeshError::Shape(ShapeError { field, code }))
        );
    }
    assert_eq!(Segments::try_from(2), Err(ErrorCode::OutOfRange));
    assert_eq!(Segments::try_from(49), Err(ErrorCode::OutOfRange));
    let points: Vec<_> = (0..513).map(|i| [1, i]).collect();
    let decoded =
        serde_json::from_value::<Solid>(json!({"in":"v","id":"s","shape":"lathe","points":points}));
    assert_eq!(
        decoded.expect_err("one over decoder cap").to_string(),
        "scene.TooLong"
    );
    let path: Vec<_> = (0..513).map(|i| [0, i, 0]).collect();
    let decoded = serde_json::from_value::<Solid>(
        json!({"in":"v","id":"s","shape":"tube","path":path,"r2":1}),
    );
    assert_eq!(
        decoded.expect_err("one over path decoder cap").to_string(),
        "scene.TooLong"
    );
}

#[test]
fn distance_tolerance_uses_radius_and_a_fixed_floor() {
    assert_eq!(dist_eps(0.0).to_bits(), 1e-9_f64.to_bits());
    assert_eq!(
        dist_eps(0.001_f64.next_down()).to_bits(),
        1e-9_f64.to_bits()
    );
    assert_eq!(dist_eps(0.001).to_bits(), 1e-9_f64.to_bits());
    assert_eq!(
        dist_eps(0.001_f64.next_up()).to_bits(),
        1.000_000_000_000_000_3e-9_f64.to_bits()
    );
    assert_eq!(dist_eps(1.0).to_bits(), 1e-6_f64.to_bits());
    assert_eq!(dist_eps(1000.0).to_bits(), 0.001_f64.to_bits());
}

#[test]
fn transform_rotation_order_and_child_then_parent_are_analytic() {
    let quarter = Transform::local([0.0; 3], [90.0, 0.0, 0.0], 1.0);
    let p = quarter
        .point(Vec3::new(0.0, 1.0, 0.0))
        .expect("finite rotation");
    assert_eq!(p.x.to_bits(), 0.0_f32.to_bits());
    assert!(
        p.y.abs() < 1e-15,
        "cosine of ninety degrees rounds near zero"
    );
    assert_eq!(p.z.to_bits(), 1.0_f32.to_bits());
    let child = Transform::local([10.0, 20.0, 30.0], [90.0, 90.0, 90.0], 2.0);
    assert_eq!(
        child.point(Vec3::new(1.0, 2.0, 3.0)),
        Ok(Vec3::new(16.0, 24.0, 28.0))
    );
    let parent = Transform::local([1.0, 2.0, 3.0], [0.0, 0.0, 90.0], 3.0);
    assert_eq!(
        child.then(&parent).point(Vec3::new(1.0, 2.0, 3.0)),
        Ok(Vec3::new(-71.0, 50.0, 87.0))
    );
    assert_eq!(
        Transform::local([0.0; 3], [0.0; 3], 1.0).then(&child),
        child
    );
    let asymmetric = Transform::local([5.0, -7.0, 11.0], [30.0, 60.0, 45.0], 2.0);
    let p = asymmetric
        .point(Vec3::new(1.0, 2.0, 3.0))
        .expect("finite asymmetric rotation");
    let x = 5.0 + 2.0_f64.sqrt() * (17.0 / 4.0 - 3.0_f64.sqrt() / 2.0);
    let y = -7.0 + 2.0_f64.sqrt() * (5.0 / 4.0 + 3.0 * 3.0_f64.sqrt() / 2.0);
    let z = 12.0 + 3.0_f64.sqrt() / 2.0;
    assert!((f64::from(p.x) - x).abs() < 1e-6, "analytic Rz Ry Rx x");
    assert!((f64::from(p.y) - y).abs() < 1e-6, "analytic Rz Ry Rx y");
    assert!((f64::from(p.z) - z).abs() < 1e-6, "analytic Rz Ry Rx z");
}

#[test]
fn transformed_point_finiteness_is_checked_after_narrowing() {
    let identity = Transform::local([0.0; 3], [0.0; 3], 1.0);
    assert_eq!(
        identity.point(Vec3::new(f32::MAX, 0.0, 0.0)),
        Ok(Vec3::new(f32::MAX, 0.0, 0.0))
    );
    let double = Transform::local([0.0; 3], [0.0; 3], 2.0);
    assert_eq!(
        double.point(Vec3::new(f32::MAX, 0.0, 0.0)),
        Err(MeshError::Degenerate("pos"))
    );
    assert_eq!(
        identity.point(Vec3::new(f32::NAN, 0.0, 0.0)),
        Err(MeshError::Degenerate("pos"))
    );
    assert_eq!(
        Transform::local([f32::INFINITY, 0.0, 0.0], [0.0; 3], 1.0).point(Vec3::default()),
        Err(MeshError::Degenerate("pos"))
    );
}
