//! Solid capacity counts, geometry validation and affine conventions.
use kumochi_scene::{
    Budget, ErrorCode,
    record::{Segments, ShapeError, Solid},
    three::{
        dist_eps,
        size::{MeshError, triangles},
    },
};
use serde_json::{Value, json};

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
