//! Camera, clipping, posed frames and agreement with an independent ray oracle.
#![expect(
    clippy::float_cmp,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "small exact fixtures and independent numerical oracles deliberately compare values and narrow bounded indices"
)]

use kumochi_scene::{
    geom::Vec3,
    record::View3d,
    three::{
        Transform,
        camera::{Camera, Light},
        frame::{FrameError, Solid, frame},
        raster::{BandScratch, FrameScratch, Quality, Target, raster},
        view::FaceRange,
    },
};
use std::sync::atomic::AtomicBool;

#[path = "support/allocation.rs"]
mod allocation;

#[path = "support/frame_fixture.rs"]
mod frame_fixture;
use frame_fixture::{Face, SQUARE, camera, identity, light, solid};

#[path = "support/picking.rs"]
mod picking;

const SPLIT: [[u32; 3]; 2] = [[0, 1, 2], [0, 2, 3]];
const OTHER: [[u32; 3]; 2] = [[0, 1, 3], [1, 2, 3]];

fn build(solids: &[Solid<'_>], cam: &Camera, size: [u32; 2], capacity: usize) -> FrameScratch {
    let mut out = FrameScratch::with_capacity(capacity);
    frame(
        solids,
        cam,
        &light(),
        size,
        &mut out,
        &AtomicBool::new(false),
    )
    .expect("valid fixture");
    out
}
fn image(out: &mut FrameScratch, size: [u32; 2], quality: Quality) -> (Vec<u32>, Vec<u8>) {
    let [width, height] = size;
    let (mut color, mut ids) = (
        vec![0; (width * height) as usize],
        vec![0; (width * height) as usize],
    );
    raster(
        out,
        quality,
        &mut BandScratch::new(width).expect("valid fixture"),
        Target {
            width,
            height,
            color: &mut color,
            ids: &mut ids,
        },
        &AtomicBool::new(false),
    )
    .expect("valid fixture");
    (color, ids)
}

fn assert_ulps(actual: f64, expected: f64, ulps: u64) {
    assert!(
        actual.is_finite() && actual.to_bits().abs_diff(expected.to_bits()) <= ulps,
        "{actual} differs from {expected} by more than {ulps} ulps"
    );
}

fn assert_close(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{actual} differs from {expected} by more than {tolerance}"
    );
}

fn assert_look_at(cam: Camera, target: [f64; 3], yaw: f64, pitch: f64, distance: f64) {
    let eye = cam.eye();
    let [x, y, z] = delta(eye, target);
    assert_close(x.hypot(z).hypot(y), distance, 1e-10);
    assert_close(x.atan2(z).to_degrees(), yaw, 1e-10);
    assert_close(y.atan2(x.hypot(z)).to_degrees(), pitch, 1e-10);
    let forward = cam.basis()[2];
    for i in 0..3 {
        assert_close(eye[i] + distance * forward[i], target[i], 1e-10);
    }
}

fn assert_lit_pixel(light: Light, cam: Camera, rotation: [f32; 3], expected_bgra: [u8; 4]) {
    let faces = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT,
    }];
    let mut square = solid(&SQUARE, &faces);
    square.pose = Transform::local([0.0; 3], rotation, 1.0);
    let mut out = FrameScratch::with_capacity(2);
    let size = [9, 9];
    frame(
        &[square],
        &cam,
        &light,
        size,
        &mut out,
        &AtomicBool::new(false),
    )
    .expect("lit square");
    let (color, ids) = image(&mut out, size, Quality::Motion);
    assert_eq!(
        (ids[40], color[40].to_le_bytes()),
        (1, expected_bgra),
        "the centre sample is on the square and uses the expected light"
    );
}

#[test]
fn camera_defaults_use_world_bounds_and_authored_inside_distance_survives_until_orbit() {
    let view: View3d = serde_json::from_str(r#"{"id":"view"}"#).expect("valid fixture");
    let mut cam =
        Camera::authored(&view, [[10.0, 20.0, 30.0], [12.0, 24.0, 34.0]]).expect("valid fixture");
    assert_eq!(cam.parameters(), [30.0, 20.0, 9.0, 40.0]);
    assert_look_at(cam, [11.0, 22.0, 32.0], 30.0, 20.0, 9.0);
    let view: View3d =
        serde_json::from_str(r#"{"id":"view","camera":{"dist":0.001}}"#).expect("valid fixture");
    cam = Camera::authored(&view, [[-1.0; 3], [1.0; 3]]).expect("valid fixture");
    assert_eq!(cam.parameters()[2], f64::from(0.001_f32));
    cam.orbit(25.0, 200.0, 1.0).expect("valid fixture");
    assert_eq!(cam.parameters(), [20.0, 85.0, 1.2, 40.0]);
    cam.orbit(0.0, -1000.0, 0.1).expect("valid fixture");
    assert_eq!(
        [
            cam.parameters()[0],
            cam.parameters()[1],
            cam.parameters()[3]
        ],
        [20.0, -85.0, 40.0]
    );
    assert_look_at(cam, [0.0; 3], 20.0, -85.0, 0.6);
}

#[test]
fn all_authored_fields_light_defaults_and_yaw_wrap_are_mapped() {
    let view: View3d = serde_json::from_str(r#"{"id":"v","camera":{"yaw":-30,"pitch":-10,"dist":4,"fov":120,"target":[1,2,3]},"light":{"dir":[0,0,-2],"ambient":0.25}}"#).expect("valid fixture");
    let cam = Camera::authored(&view, [[-1.0; 3], [1.0; 3]]).expect("valid bounds");
    assert_eq!(cam.parameters(), [330.0, -10.0, 4.0, 120.0]);
    assert_look_at(cam, [1.0, 2.0, 3.0], -30.0, -10.0, 4.0);
    let authored = Light::authored(&view);
    assert_lit_pixel(authored, camera(), [0.0; 3], [50, 100, 200, 255]);
    assert_lit_pixel(authored, camera(), [0.0, 45.0, 0.0], [39, 78, 156, 255]);
    let defaults: View3d = serde_json::from_str(r#"{"id":"v"}"#).expect("valid fixture");
    let default_light = Light::authored(&defaults);
    let aimed = Camera::new(30.0, 20.0, 3.0, 60.0, [0.0; 3]).expect("camera above the square");
    assert_lit_pixel(default_light, aimed, [0.0; 3], [31, 62, 123, 255]);
    assert_lit_pixel(default_light, aimed, [0.0, 90.0, 0.0], [31, 62, 123, 255]);
    for (yaw, expected) in [(-30.0, 330.0), (390.0, 30.0)] {
        assert_eq!(
            Camera::new(yaw, 0.0, 2.0, 90.0, [0.0; 3])
                .expect("valid camera")
                .parameters()[0],
            expected
        );
    }
    let mut orbit = Camera::new(30.0, 0.0, 2.0, 90.0, [0.0; 3]).expect("valid camera");
    orbit.orbit(100.0, 0.0, 1.0).expect("valid orbit");
    assert_eq!(orbit.parameters()[0], 350.0);
}

#[test]
fn rotated_camera_and_pose_shade_in_world_space() {
    let faces = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT,
    }];
    let cam = Camera::new(30.0, 20.0, 3.0, 60.0, [0.0; 3]).expect("valid camera");
    let view: View3d = serde_json::from_str(r#"{"id":"v"}"#).expect("valid fixture");
    for (rotation, expected) in [
        ([0.0; 3], [31, 62, 123, 255]),
        ([-90.0, 0.0, 0.0], [44, 88, 176, 255]),
    ] {
        let mut s = solid(&SQUARE, &faces);
        s.pose = Transform::local([0.0; 3], rotation, 1.0);
        let mut out = FrameScratch::with_capacity(2);
        frame(
            &[s],
            &cam,
            &Light::authored(&view),
            [16, 8],
            &mut out,
            &AtomicBool::new(false),
        )
        .expect("valid frame");
        let (color, ids) = image(&mut out, [16, 8], Quality::Motion);
        assert_eq!(
            ids[3 * 16 + 8],
            1,
            "the centre-adjacent sample is on the face"
        );
        for (pixel, id) in color.into_iter().zip(ids) {
            if id == 1 {
                assert_eq!(pixel.to_le_bytes(), expected);
            }
        }
    }
}

#[test]
fn non_square_renders_have_exact_vertical_fov_and_per_axis_centres() {
    let vertices = [
        Vec3::new(-1.0, -0.5, 0.0),
        Vec3::new(0.5, -0.5, 0.0),
        Vec3::new(0.5, 0.5, 0.0),
        Vec3::new(-1.0, 0.5, 0.0),
    ];
    let faces = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT,
    }];
    for (size, columns, rows) in [([16, 8], 6..9, 3..5), ([8, 16], 0..6, 6..10)] {
        let mut out = build(&[solid(&vertices, &faces)], &camera(), size, 2);
        let (_, ids) = image(&mut out, size, Quality::Motion);
        for y in 0..size[1] {
            for x in 0..size[0] {
                assert_eq!(
                    ids[(y * size[0] + x) as usize],
                    u8::from(columns.contains(&x) && rows.contains(&y)),
                    "{size:?} at {x},{y}"
                );
            }
        }
    }
}

#[test]
fn degenerate_triangle_is_counted_and_degenerate_anchor_faces_are_culled() {
    let repeated = [[0, 0, 1]];
    let faces = [Face {
        anchors: [0, 1, 2],
        triangles: &repeated,
    }];
    let out = build(&[solid(&SQUARE, &faces)], &camera(), [8, 8], 1);
    assert_eq!((out.len(), out.discarded()), (0, 1));
    for (vertices, anchors) in [
        (SQUARE, [0, 0, 1]),
        (SQUARE.map(|v| Vec3::new(v.x, 0.0, v.z)), [0, 1, 2]),
        (SQUARE.map(|v| Vec3::new(v.x, v.y, 2.0)), [0, 1, 2]),
    ] {
        let faces = [Face {
            anchors,
            triangles: &SPLIT,
        }];
        let out = build(&[solid(&vertices, &faces)], &camera(), [8, 8], 2);
        assert_eq!((out.len(), out.discarded()), (0, 0));
    }
}

#[test]
fn camera_bounds_and_nonfinite_inputs_refuse_without_changing_state() {
    for pitch in [-85.0, 85.0] {
        assert_eq!(
            Camera::new(0.0, pitch, 1.0, 10.0, [0.0; 3])
                .expect("valid fixture")
                .parameters(),
            [0.0, pitch, 1.0, 10.0]
        );
    }
    for fov in [10.0, 120.0] {
        assert_eq!(
            Camera::new(0.0, 0.0, 1.0, fov, [0.0; 3])
                .expect("valid fixture")
                .parameters()[3],
            fov
        );
    }
    for (pitch, dist, fov) in [
        (85.01, 1.0, 40.0),
        (-85.01, 1.0, 40.0),
        (0.0, 0.0, 40.0),
        (0.0, -1.0, 40.0),
        (0.0, 1.0, 9.99),
        (0.0, 1.0, 120.01),
    ] {
        assert_eq!(
            Camera::new(0.0, pitch, dist, fov, [0.0; 3]),
            Err(FrameError::Invalid)
        );
    }
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for index in 0..7 {
            let mut v = [0.0, 0.0, 1.0, 40.0, 0.0, 0.0, 0.0];
            v[index] = bad;
            assert_eq!(
                Camera::new(v[0], v[1], v[2], v[3], [v[4], v[5], v[6]]),
                Err(FrameError::Invalid)
            );
        }
        let mut cam = camera();
        let old = (cam.parameters(), cam.eye(), cam.basis());
        for (dx, dy, r) in [
            (bad, 0.0, 1.0),
            (0.0, bad, 1.0),
            (0.0, 0.0, bad),
            (0.0, 0.0, 0.0),
        ] {
            assert_eq!(cam.orbit(dx, dy, r), Err(FrameError::Invalid));
            assert_eq!((cam.parameters(), cam.eye(), cam.basis()), old);
        }
        assert_eq!(Light::new([bad, 0.0, 1.0], 0.35), Err(FrameError::Invalid));
        assert_eq!(Light::new([1.0, 0.0, 0.0], bad), Err(FrameError::Invalid));
    }
    for dist in [1.2, 6.0] {
        let mut cam = Camera::new(0.0, 0.0, dist, 40.0, [0.0; 3]).expect("valid fixture");
        cam.orbit(0.0, 0.0, 1.0).expect("valid fixture");
        assert_eq!(cam.parameters()[2], dist);
    }
    let view: View3d = serde_json::from_str(r#"{"id":"v"}"#).expect("valid fixture");
    for bounds in [
        [[0.0; 3]; 2],
        [[1.0; 3], [0.0; 3]],
        [[f64::NAN; 3], [1.0; 3]],
    ] {
        assert_eq!(Camera::authored(&view, bounds), Err(FrameError::Invalid));
    }
    assert_eq!(Light::new([0.0; 3], 0.35), Err(FrameError::Invalid));
    for ambient in [-0.001, 1.001] {
        assert_eq!(
            Light::new([1.0, 0.0, 0.0], ambient),
            Err(FrameError::Invalid)
        );
    }
    for (ambient, expected) in [(0.0, [0, 0, 0, 255]), (1.0, [50, 100, 200, 255])] {
        let back_light = Light::new([2.0, 0.0, 0.0], ambient).expect("legal ambient endpoint");
        assert_lit_pixel(back_light, camera(), [0.0, 45.0, 0.0], expected);
    }
    for length in [1.0, 2.0] {
        let side_light = Light::new([length, 0.0, 0.0], 0.25).expect("nonzero direction");
        assert_lit_pixel(side_light, camera(), [0.0, -45.0, 0.0], [39, 78, 156, 255]);
    }
}

#[test]
fn translated_tiny_camera_has_orthonormal_basis_and_precision_floor() {
    let cam = Camera::new(30.0, 20.0, 0.001, 40.0, [20000.0; 3]).expect("valid fixture");
    let basis = cam.basis();
    for i in 0..3 {
        for j in 0..3 {
            let dot: f64 = (0..3).map(|k| basis[i][k] * basis[j][k]).sum();
            assert!(
                (dot - f64::from(i == j)).abs() < 1e-10,
                "basis {i},{j}: {dot}"
            );
        }
    }
    let (sin_yaw, cos_yaw) = 30.0_f64.to_radians().sin_cos();
    let (sin_pitch, cos_pitch) = 20.0_f64.to_radians().sin_cos();
    for (actual, expected) in delta(cam.eye(), [20000.0; 3]).into_iter().zip([
        0.001 * sin_yaw * cos_pitch,
        0.001 * sin_pitch,
        0.001 * cos_yaw * cos_pitch,
    ]) {
        assert_close(actual, expected, 1e-10);
    }
    let near = cam.near(1.0, 20000.0, 1440).expect("valid fixture");
    assert_close(near, 11.25_f64 / 20.0_f64.to_radians().tan(), 1e-10);
    assert_eq!(
        Camera::new(0.0, 0.0, 0.001, 90.0, [0.0; 3])
            .expect("valid fixture")
            .near(1.0, 1.0, 2),
        Ok(0.0001)
    );
    for (r, c, h) in [
        (0.0, 1.0, 2),
        (1.0, -1.0, 2),
        (1.0, 1.0, 0),
        (f64::NAN, 1.0, 2),
        (1.0, f64::INFINITY, 2),
    ] {
        assert_eq!(cam.near(r, c, h), Err(FrameError::Invalid));
    }
    assert_eq!(cam.near(1.0, f64::MAX, 1), Err(FrameError::Numeric));
}

#[test]
fn exact_projection_culling_shading_pose_and_clipped_occluder() {
    let faces = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT,
    }];
    let mut out = build(&[solid(&SQUARE, &faces)], &camera(), [8, 8], 8);
    let (pixels, ids) = image(&mut out, [8, 8], Quality::Motion);
    for y in 0..8 {
        for x in 0..8 {
            let expected = u8::from((2..6).contains(&x) && (2..6).contains(&y));
            assert_eq!(ids[y * 8 + x], expected);
            if expected == 1 {
                assert_eq!(pixels[y * 8 + x].to_le_bytes(), [50, 100, 200, 255]);
            }
        }
    }
    let mut moved = solid(&SQUARE, &faces);
    moved.pose = Transform::local([1.0, 0.0, 0.0], [0.0; 3], 1.0);
    let mut out = build(&[moved], &camera(), [8, 8], 8);
    let (_, ids) = image(&mut out, [8, 8], Quality::Motion);
    for y in 0..8 {
        for x in 0..8 {
            assert_eq!(
                ids[y * 8 + x],
                u8::from((4..8).contains(&x) && (2..6).contains(&y))
            );
        }
    }
    let mut backwards = solid(&SQUARE, &faces);
    backwards.pose = Transform::local([0.0; 3], [0.0, 180.0, 0.0], 1.0);
    let mut out = build(&[backwards], &camera(), [8, 8], 8);
    assert_eq!(image(&mut out, [8, 8], Quality::Motion).1, vec![0; 64]);
    let mut clipped = solid(&SQUARE, &faces);
    clipped.pose = Transform::local([0.0, 0.0, 1.95], [0.0; 3], 1.0);
    let mut out = build(&[clipped, solid(&SQUARE, &faces)], &camera(), [8, 8], 8);
    assert_eq!(image(&mut out, [8, 8], Quality::Motion).1[4 * 8 + 4], 2);
    let ambient = Light::new([0.0, 0.0, 1.0], 0.25).expect("valid fixture");
    frame(
        &[solid(&SQUARE, &faces)],
        &camera(),
        &ambient,
        [8, 8],
        &mut out,
        &AtomicBool::new(false),
    )
    .expect("valid fixture");
    assert_eq!(
        image(&mut out, [8, 8], Quality::Motion).0[4 * 8 + 4].to_le_bytes(),
        [13, 25, 50, 255]
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one bound matrix checks both errors and cleared output at every limit"
)]
fn admitted_counts_capacity_sizes_invalid_indices_and_empty_frames() {
    let face = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT[..1],
    }];
    let square = solid(&SQUARE, &face);
    let prime = |out: &mut FrameScratch| {
        frame(
            &[square],
            &camera(),
            &light(),
            [8, 8],
            out,
            &AtomicBool::new(false),
        )
        .expect("prime with a visible triangle");
        assert!(!out.is_empty());
    };
    let check_clear = |out: &mut FrameScratch, solids: &[Solid<'_>], size, stop, expected| {
        prime(out);
        assert_eq!(
            frame(
                solids,
                &camera(),
                &light(),
                size,
                out,
                &AtomicBool::new(stop)
            ),
            expected
        );
        assert!(out.is_empty(), "the previous frame must not survive");
    };
    let extremal = [
        Vec3::new(11784.0, -19376.0, 16.0),
        Vec3::new(9367.0, 10804.0, 18.25),
        Vec3::new(-10216.0, 3140.0, 18.0),
    ];
    let extremal_camera = Camera::new(0.0, 0.0, 20.0, 90.0, [0.0; 3]).expect("valid fixture");
    let repeated = vec![[0, 1, 2]; 24000];
    let faces = [Face {
        anchors: [0, 1, 2],
        triangles: &repeated,
    }];
    let s = solid(&extremal, &faces);
    let mut out = FrameScratch::with_capacity(24000);
    let cancel = AtomicBool::new(false);
    for pass in ["initial", "reused"] {
        let (result, allocations) = allocation::count(|| {
            frame(&[s], &extremal_camera, &light(), [2, 2], &mut out, &cancel)
        });
        assert_eq!((result, allocations), (Ok(()), 0), "{pass}");
        assert_eq!(out.len(), 24000, "{pass}");
    }
    let mut short = FrameScratch::with_capacity(23999);
    prime(&mut short);
    assert_eq!(
        frame(
            &[s],
            &extremal_camera,
            &light(),
            [2, 2],
            &mut short,
            &cancel
        ),
        Err(FrameError::Over)
    );
    assert!(short.is_empty());
    let repeated = vec![[0, 1, 2]; 24001];
    let over = [Face {
        anchors: [0, 1, 2],
        triangles: &repeated,
    }];
    check_clear(
        &mut out,
        &[solid(&SQUARE, &over)],
        [8, 8],
        false,
        Err(FrameError::Over),
    );
    assert_eq!(
        frame(
            &vec![square; 160],
            &camera(),
            &light(),
            [4096, 1],
            &mut out,
            &cancel
        ),
        Ok(())
    );
    assert_eq!(out.len(), 160);
    check_clear(
        &mut out,
        &vec![square; 161],
        [8, 8],
        false,
        Err(FrameError::Over),
    );
    for size in [[0, 8], [8, 0], [4097, 1], [1, 4097]] {
        check_clear(&mut out, &[square], size, false, Err(FrameError::Invalid));
    }
    for opacity in [-0.01, 1.01, f32::NAN, f32::INFINITY] {
        let mut invalid = square;
        invalid.opacity = opacity;
        check_clear(
            &mut out,
            &[invalid],
            [8, 8],
            false,
            Err(FrameError::Invalid),
        );
    }
    for opacity in [0.0, 1.0] {
        let mut valid = square;
        valid.opacity = opacity;
        prime(&mut out);
        assert_eq!(
            frame(&[valid], &camera(), &light(), [8, 8], &mut out, &cancel),
            Ok(())
        );
        assert_eq!(out.len(), usize::from(opacity == 1.0));
    }
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let vertices = [Vec3::new(bad, 0.0, 0.0)];
        check_clear(
            &mut out,
            &[solid(&vertices, &[])],
            [8, 8],
            false,
            Err(FrameError::Numeric),
        );
    }
    for (anchors, triangles) in [([0, 1, 4], [[0, 1, 2]]), ([0, 1, 2], [[0, 1, 4]])] {
        let faces = [Face {
            anchors,
            triangles: &triangles,
        }];
        check_clear(
            &mut out,
            &[solid(&SQUARE, &faces)],
            [8, 8],
            false,
            Err(FrameError::Invalid),
        );
    }
    for (start, len) in [(2, 0), (0, 2), (1, 1), (u32::MAX, 1), (1, u32::MAX)] {
        let faces = [FaceRange {
            anchors: [0, 1, 2],
            start,
            len,
        }];
        let mut malformed = square;
        malformed.faces = &faces;
        check_clear(
            &mut out,
            &[malformed],
            [8, 8],
            false,
            Err(FrameError::Invalid),
        );
    }
    let mut empty = square;
    empty.faces = &[FaceRange {
        anchors: [0, 1, 2],
        start: 1,
        len: 0,
    }];
    check_clear(&mut out, &[empty], [8, 8], false, Ok(()));
    check_clear(&mut out, &[], [8, 8], false, Ok(()));
    check_clear(&mut out, &[solid(&[], &[])], [8, 8], false, Ok(()));
    check_clear(
        &mut out,
        &[square],
        [8, 8],
        true,
        Err(FrameError::Cancelled),
    );
}

#[test]
fn opposite_shared_edges_near_and_guard_clips_cover_once_including_snap_ties() {
    for triangles in [&SPLIT, &OTHER] {
        for size in [[8, 8], [32, 32], [96, 64], [64, 96]] {
            let face = [Face {
                anchors: [0, 1, 2],
                triangles,
            }];
            for shift in [0.0, 1.0 / 2048.0, 0.0017] {
                for depth in [0.0, 1.0] {
                    let vertices = [
                        Vec3::new(-1000.0, -1000.0, depth),
                        Vec3::new(1000.0, -1000.0, depth),
                        Vec3::new(1000.0, 1000.0, 2.0),
                        Vec3::new(-1000.0, 1000.0, 2.0),
                    ];
                    let mut s = solid(&vertices, &face);
                    s.opacity = 0.5;
                    let cam =
                        Camera::new(0.0, 0.0, 2.0, 90.0, [shift, 0.0, 0.0]).expect("valid fixture");
                    let mut out = build(&[s], &cam, size, 8);
                    for q in [Quality::Motion, Quality::Rest] {
                        let (pixels, ids) = image(&mut out, size, q);
                        assert_eq!(ids, vec![1; (size[0] * size[1]) as usize]);
                        assert_eq!(
                            pixels
                                .iter()
                                .map(|v| v.to_le_bytes()[3])
                                .collect::<Vec<_>>(),
                            vec![128; (size[0] * size[1]) as usize]
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn precision_floor_clips_at_the_world_float_boundary_and_bad_depth_clears_output() {
    let faces = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT,
    }];
    let cam = Camera::new(0.0, 0.0, 1.0, 90.0, [16384.0; 3]).expect("valid fixture");
    assert_ulps(
        cam.near(2.0, 16385.0, 8).expect("finite near floor"),
        0.0625,
        2,
    );
    let boundary = f32::from_bits(0x4680_01e0);
    for (z, visible) in [(boundary.next_down(), true), (boundary.next_up(), false)] {
        let mut s = solid(&SQUARE, &faces);
        s.pose = Transform::local([16384.0, 16384.0, z], [0.0; 3], 1.0);
        let mut out = build(&[s], &cam, [8, 8], 2);
        let (_, ids) = image(&mut out, [8, 8], Quality::Rest);
        assert_eq!(ids.iter().any(|&id| id != 0), visible);
    }
    let tiny = Camera::new(0.0, 0.0, 1e-300, 90.0, [0.0; 3]).expect("valid fixture");
    let square = solid(&SQUARE, &faces);
    let mut out = build(&[square], &camera(), [8, 8], 2);
    assert!(image(&mut out, [8, 8], Quality::Rest).1.contains(&1));
    assert_eq!(
        frame(
            &[square],
            &tiny,
            &light(),
            [8, 8],
            &mut out,
            &AtomicBool::new(false)
        ),
        Err(FrameError::Numeric)
    );
    assert_eq!(
        image(&mut out, [8, 8], Quality::Rest),
        (vec![0; 64], vec![0; 64])
    );
    for size in [[1, 4096], [4096, 1]] {
        assert_eq!(
            frame(
                &[],
                &camera(),
                &light(),
                size,
                &mut out,
                &AtomicBool::new(false)
            ),
            Ok(())
        );
        assert_eq!(out.len(), 0);
    }
}

#[test]
fn one_solid_pose_does_not_move_its_neighbour_and_authored_opacity_reaches_ids() {
    let faces = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT,
    }];
    for offset in [-1.0, 1.0] {
        let fixed = solid(&SQUARE, &faces);
        let mut moving = fixed;
        moving.pose = Transform::local([offset, 0.0, 0.0], [0.0; 3], 1.0);
        let mut out = build(&[fixed, moving], &camera(), [8, 8], 4);
        let (_, ids) = image(&mut out, [8, 8], Quality::Motion);
        for y in 0..8 {
            for x in 0..8 {
                let moved = if offset < 0.0 { 0..4 } else { 4..8 };
                let expected = if !(2..6).contains(&y) {
                    0
                } else if (2..6).contains(&x) {
                    1
                } else if moved.contains(&x) {
                    2
                } else {
                    0
                };
                assert_eq!(ids[y * 8 + x], expected);
            }
        }
    }
    for (opacity, expected) in [(0.49, 1), (0.5, 2)] {
        let behind = solid(&SQUARE, &faces);
        let mut shell = behind;
        shell.pose = Transform::local([0.0, 0.0, 0.5], [0.0; 3], 1.0);
        shell.opacity = opacity;
        let mut out = build(&[behind, shell], &camera(), [8, 8], 4);
        assert_eq!(
            image(&mut out, [8, 8], Quality::Motion).1[4 * 8 + 4],
            expected
        );
    }
}

#[test]
fn coplanar_faces_share_the_source_plane_across_different_triangulations_at_bin_boundary() {
    let tilted = SQUARE.map(|v| Vec3::new(v.x, v.y, 0.125 * v.x + 0.25 * v.y));
    let hexagon = [
        (1.0, 0.1),
        (0.37, 0.91),
        (-0.62, 0.79),
        (-0.94, -0.16),
        (-0.23, -0.87),
        (0.71, -0.65),
    ]
    .map(|(x, y)| {
        Vec3::new(
            (100.0 + x) as f32,
            (200.0 + y) as f32,
            (300.0 - (x + 2.0 * y) / 3.0) as f32,
        )
    });
    let positions = hexagon.map(|v| [v.x, v.y, v.z].map(f64::from));
    let normal = vector(
        delta(positions[1], positions[0]),
        delta(positions[2], positions[0]),
    );
    assert!(
        (3..6).any(|i| scalar(normal, delta(positions[i], positions[0])).abs() > 1e-6),
        "f32 rounding must take vertices off the anchor plane"
    );
    let fans = [
        [[0, 1, 2], [0, 2, 3], [0, 3, 4], [0, 4, 5]],
        [[3, 4, 5], [3, 5, 0], [3, 0, 1], [3, 1, 2]],
    ];
    let quad_fans: [&[[u32; 3]]; 2] = [&SPLIT, &OTHER];
    for depth in [f32::from_bits(0x3f00_0007), f32::from_bits(0x3f00_0008)] {
        for (yaw, pitch, target, vertices, triangles, size) in [
            (0.0, 0.0, [0.0; 3], SQUARE.as_slice(), quad_fans, [32, 32]),
            (0.0, 20.0, [0.0; 3], &tilted, quad_fans, [32, 32]),
            (31.0, 20.0, [0.0; 3], &tilted, quad_fans, [32, 32]),
            (
                0.0,
                0.0,
                [100.0, 200.0, 300.0],
                &hexagon,
                [&fans[0], &fans[1]],
                [128, 128],
            ),
        ] {
            let cam = Camera::new(yaw, pitch, 1.0 / f64::from(depth), 90.0, target)
                .expect("valid fixture");
            assert_fan_images_match(vertices, triangles, &cam, size);
        }
    }
}

fn assert_fan_images_match(
    vertices: &[Vec3],
    fans: [&[[u32; 3]]; 2],
    cam: &Camera,
    size: [u32; 2],
) {
    let [first, second] = fans.map(|triangles| {
        solid(
            vertices,
            &[Face {
                anchors: [0, 1, 2],
                triangles,
            }],
        )
    });
    for (a, b) in [(first, second), (second, first)] {
        let mut reference = build(&[a], cam, size, a.triangles.len());
        let mut overlap = build(&[a, b], cam, size, a.triangles.len() + b.triangles.len());
        for quality in [Quality::Motion, Quality::Rest] {
            let expected = image(&mut reference, size, quality);
            let [width, height] = size.map(|v| v as usize);
            for (x, y) in [(width / 2 - 1, height / 2), (width / 2 + 1, height / 2 - 1)] {
                assert_eq!(
                    expected.1[y * width + x],
                    1,
                    "interior of the reference face"
                );
            }
            assert_eq!(
                image(&mut overlap, size, quality),
                expected,
                "{size:?}, {quality:?}, {cam:?}"
            );
        }
    }
}

const BOX_TRIANGLES: [[[u32; 3]; 2]; 6] = [
    [[4, 5, 7], [4, 7, 6]],
    [[0, 2, 3], [0, 3, 1]],
    [[0, 4, 6], [0, 6, 2]],
    [[1, 3, 7], [1, 7, 5]],
    [[0, 1, 5], [0, 5, 4]],
    [[2, 6, 7], [2, 7, 3]],
];
fn delta(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| a[i] - b[i])
}
fn scalar(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|i| a[i] * b[i]).sum()
}
fn vector(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn world_triangles(solids: &[Solid<'_>]) -> Vec<([[f64; 3]; 3], u8)> {
    let mut triangles = Vec::new();
    for (i, s) in solids.iter().enumerate() {
        for t in s.triangles {
            let p = t.map(|j| {
                s.pose
                    .apply(s.vertices[j as usize])
                    .map(|c| f64::from(c as f32))
            });
            triangles.push((p, (i + 1) as u8));
        }
    }
    triangles
}
#[expect(
    clippy::many_single_char_names,
    reason = "standard Moller-Trumbore barycentric notation in the independent oracle"
)]
fn ray_hit(eye: [f64; 3], ray: [f64; 3], triangle: [[f64; 3]; 3]) -> Option<f64> {
    let [a, b, c] = triangle;
    let e1 = delta(b, a);
    let e2 = delta(c, a);
    let h = vector(ray, e2);
    let det = scalar(e1, h);
    if det <= 0.0 {
        return None;
    }
    let s = delta(eye, a);
    let u = scalar(s, h) / det;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = vector(s, e1);
    let v = scalar(ray, q) / det;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = scalar(e2, q) / det;
    (t > 0.0).then_some(t)
}
#[expect(
    clippy::many_single_char_names,
    reason = "point, endpoints and line parameter in the independent edge-distance oracle"
)]
fn edge_distance(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let d = [b[0] - a[0], b[1] - a[1]];
    let q = [p[0] - a[0], p[1] - a[1]];
    let length = d[0] * d[0] + d[1] * d[1];
    let t = if length == 0.0 {
        0.0
    } else {
        ((q[0] * d[0] + q[1] * d[1]) / length).clamp(0.0, 1.0)
    };
    (q[0] - t * d[0]).hypot(q[1] - t * d[1])
}

#[test]
fn partial_near_and_guard_clips_on_both_diagonals_match_rays_at_non_square_sizes() {
    for distance in [0.8, 1.0, 1.2] {
        let vertices = [(-200.0, -1.0), (200.0, -1.0), (200.0, 1.0), (-200.0, 1.0)]
            .map(|(x, y)| Vec3::new(x, y, (distance * 0.95 - 0.4 * f64::from(y)) as f32));
        for size in [[96, 64], [64, 96]] {
            let focal = f64::from(size[1]) / (2.0 * (60.0_f64.to_radians()).tan());
            let cam = Camera::new(0.0, 0.0, distance, 120.0, [0.0; 3]).expect("valid camera");
            let z0 = distance - f64::from(vertices[0].z);
            let z1 = distance - f64::from(vertices[2].z);
            let radius = (200.0_f64.powi(2) + 1.0 + ((z1 - z0) / 2.0).powi(2)).sqrt();
            let near = (distance * 0.05).max(radius * 1e-4);
            let near_y = -1.0 + 2.0 * (near - z0) / (z1 - z0);
            let project = |x: f64, y: f64, z: f64| {
                [
                    f64::from(size[0]) / 2.0 + focal * x / z,
                    f64::from(size[1]) / 2.0 - focal * y / z,
                ]
            };
            let boundary = [
                project(-200.0, near_y, near),
                project(200.0, near_y, near),
                project(200.0, 1.0, z1),
                project(-200.0, 1.0, z1),
            ];
            for diagonal in [&SPLIT, &OTHER] {
                let faces = [Face {
                    anchors: [0, 1, 2],
                    triangles: diagonal,
                }];
                let solids = [solid(&vertices, &faces)];
                let triangles = world_triangles(&solids);
                let mut out = build(&solids, &cam, size, 8);
                for quality in [Quality::Motion, Quality::Rest] {
                    let (color, ids) = image(&mut out, size, quality);
                    let mut counts = [0; 2];
                    for y in 0..size[1] {
                        for x in 0..size[0] {
                            let sample = [f64::from(x) + 0.5, f64::from(y) + 0.5];
                            if (0..4).any(|i| {
                                edge_distance(sample, boundary[i], boundary[(i + 1) % 4]) <= 1.0
                            }) {
                                continue;
                            }
                            let ray = [
                                (sample[0] - f64::from(size[0]) / 2.0) / focal,
                                -(sample[1] - f64::from(size[1]) / 2.0) / focal,
                                -1.0,
                            ];
                            let expected = u8::from(triangles.iter().any(|(t, _)| {
                                ray_hit(cam.eye(), ray, *t).is_some_and(|depth| depth >= near)
                            }));
                            let index = (y * size[0] + x) as usize;
                            assert_eq!(
                                ids[index], expected,
                                "distance {distance}, {size:?}, {quality:?}, sample {x},{y}"
                            );
                            assert_eq!(
                                color[index].to_le_bytes()[3],
                                expected * 255,
                                "clipped polygon coverage at {x},{y}"
                            );
                            counts[usize::from(expected)] += 1;
                        }
                    }
                    assert!(
                        counts[0] > 1000 && counts[1] > 1000,
                        "both sides of the visible near edge are tested: {counts:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn interpenetrating_boxes_and_three_bar_cycle_match_rays_from_26_cameras() {
    let box_vertices: [Vec3; 8] = std::array::from_fn(|i| {
        Vec3::new(
            if i & 1 == 0 { -0.5 } else { 0.5 },
            if i & 2 == 0 { -0.5 } else { 0.5 },
            if i & 4 == 0 { -0.5 } else { 0.5 },
        )
    });
    let faces = BOX_TRIANGLES.each_ref().map(|t| Face {
        anchors: t[0],
        triangles: t,
    });
    let bars: [[Vec3; 8]; 3] = std::array::from_fn(|axis| {
        box_vertices.map(|v| {
            let mut p = [v.x, v.y, v.z];
            p[axis] *= 3.0;
            Vec3::new(p[0], p[1], p[2])
        })
    });
    for cycle in [false, true] {
        let solids = if cycle {
            vec![
                solid(&bars[0], &faces),
                solid(&bars[1], &faces),
                solid(&bars[2], &faces),
            ]
        } else {
            let mut a = solid(&box_vertices, &faces);
            a.pose = Transform::local([-0.3, 0.0, 0.0], [0.0, 30.0, 0.0], 1.0);
            let mut b = solid(&box_vertices, &faces);
            b.pose = Transform::local([0.3, 0.2, 0.2], [20.0, 0.0, 0.0], 1.0);
            vec![a, b]
        };
        let triangles = world_triangles(&solids);
        let mut seen = [0; 4];
        for view in 0..26 {
            let yaw = f64::from(view % 9) * 40.0;
            let pitch = f64::from(view / 9 - 1) * 40.0;
            let fov = if view == 25 { 120.0 } else { 60.0 };
            let cam = Camera::new(yaw, pitch, 4.0, fov, [0.0; 3]).expect("valid fixture");
            let basis = cam.basis();
            let eye = cam.eye();
            let focal = 32.0 / (fov.to_radians() / 2.0).tan();
            let project = |p: [f64; 3]| {
                let p = delta(p, eye);
                let z = scalar(p, basis[2]);
                [
                    32.0 + focal * scalar(p, basis[0]) / z,
                    32.0 - focal * scalar(p, basis[1]) / z,
                ]
            };
            let projected: Vec<_> = triangles.iter().map(|(t, _)| t.map(project)).collect();
            let mut out = build(&solids, &cam, [64, 64], 36);
            let (_, ids) = image(&mut out, [64, 64], Quality::Motion);
            let mut samples = [0; 4];
            for y in 0..64 {
                for x in 0..64 {
                    let p = [f64::from(x) + 0.5, f64::from(y) + 0.5];
                    if projected
                        .iter()
                        .any(|t| (0..3).any(|i| edge_distance(p, t[i], t[(i + 1) % 3]) <= 1.0))
                    {
                        continue;
                    }
                    let ray = std::array::from_fn(|i| {
                        basis[2][i] + (p[0] - 32.0) / focal * basis[0][i]
                            - (p[1] - 32.0) / focal * basis[1][i]
                    });
                    let mut hits: Vec<_> = triangles
                        .iter()
                        .filter_map(|(t, id)| ray_hit(eye, ray, *t).map(|z| (z, *id)))
                        .collect();
                    hits.sort_by(|a, b| a.0.total_cmp(&b.0));
                    if hits.len() > 1 && (hits[0].0 - hits[1].0).abs() <= hits[0].0 * 1e-6 {
                        continue;
                    }
                    let expected = hits.first().map_or(0, |v| v.1);
                    assert_eq!(
                        ids[(y * 64 + x) as usize],
                        expected,
                        "cycle {cycle}, camera {view}, texel {x},{y}"
                    );
                    samples[usize::from(expected)] += 1;
                }
            }
            assert!(
                samples.iter().sum::<usize>() > 2000,
                "cycle {cycle}, camera {view}: edge/depth exclusions leave {samples:?}"
            );
            for (total, count) in seen.iter_mut().zip(samples) {
                *total += count;
            }
        }
        assert!(
            seen[..=solids.len()].iter().all(|&count| count > 0),
            "cycle {cycle}: every solid and the background win rays across the sweep: {seen:?}"
        );
    }
}
