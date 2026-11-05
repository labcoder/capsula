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
        frame::{Face, FrameError, Solid, frame},
        raster::{BandScratch, FrameScratch, Quality, Target, raster},
    },
};
use std::sync::atomic::AtomicBool;

#[path = "support/allocation.rs"]
mod allocation;

const SQUARE: [Vec3; 4] = [
    Vec3::new(-1.0, -1.0, 0.0),
    Vec3::new(1.0, -1.0, 0.0),
    Vec3::new(1.0, 1.0, 0.0),
    Vec3::new(-1.0, 1.0, 0.0),
];
const SPLIT: [[u32; 3]; 2] = [[0, 1, 2], [0, 2, 3]];
const OTHER: [[u32; 3]; 2] = [[0, 1, 3], [1, 2, 3]];

fn camera() -> Camera {
    Camera::new(0.0, 0.0, 2.0, 90.0, [0.0; 3]).expect("valid fixture")
}
fn light() -> Light {
    Light::new([0.0, 0.0, -1.0], 0.0).expect("valid fixture")
}
fn identity() -> Transform {
    Transform::local([0.0; 3], [0.0; 3], 1.0)
}
fn solid<'a>(vertices: &'a [Vec3], faces: &'a [Face<'a>]) -> Solid<'a> {
    Solid {
        vertices,
        faces,
        pose: identity(),
        rgb: [200, 100, 50],
        opacity: 1.0,
    }
}
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

#[test]
fn camera_defaults_use_world_bounds_and_authored_inside_distance_survives_until_orbit() {
    let view: View3d = serde_json::from_str(r#"{"id":"view"}"#).expect("valid fixture");
    let mut cam =
        Camera::authored(&view, [[10.0, 20.0, 30.0], [12.0, 24.0, 34.0]]).expect("valid fixture");
    assert_eq!(cam.parameters(), [30.0, 20.0, 9.0, 40.0]);
    let basis = cam.basis();
    assert_eq!(
        cam.eye(),
        std::array::from_fn(|i| [11.0, 22.0, 32.0][i] - 9.0 * basis[2][i])
    );
    let view: View3d =
        serde_json::from_str(r#"{"id":"view","camera":{"dist":0.001}}"#).expect("valid fixture");
    cam = Camera::authored(&view, [[-1.0; 3], [1.0; 3]]).expect("valid fixture");
    assert_eq!(cam.parameters()[2], f64::from(0.001_f32));
    cam.orbit(25.0, 200.0, 1.0).expect("valid fixture");
    assert_eq!(cam.parameters(), [20.0, 85.0, 1.2, 40.0]);
    cam.orbit(0.0, -1000.0, 0.1).expect("valid fixture");
    assert_eq!(
        cam.parameters(),
        [20.0, -85.0, 0.600_000_000_000_000_1, 40.0]
    );
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
        let old = cam;
        for (dx, dy, r) in [
            (bad, 0.0, 1.0),
            (0.0, bad, 1.0),
            (0.0, 0.0, bad),
            (0.0, 0.0, 0.0),
        ] {
            assert_eq!(cam.orbit(dx, dy, r), Err(FrameError::Invalid));
            assert_eq!(cam, old);
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
    for ambient in [0.0, 1.0] {
        assert_eq!(
            Light::new([1.0, 0.0, 0.0], ambient).expect("valid fixture"),
            Light::new([2.0, 0.0, 0.0], ambient).expect("valid fixture")
        );
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
                (dot - f64::from(i == j)).abs() < 2e-16,
                "basis {i},{j}: {dot}"
            );
        }
    }
    assert_eq!(
        cam.eye(),
        [
            20_000.000_469_846_31,
            20_000.000_342_020_143,
            20_000.000_813_797_68
        ]
    );
    let near = cam.near(1.0, 20000.0, 1440).expect("valid fixture");
    assert_eq!(
        near.to_bits(),
        (11.25_f64 / 20.0_f64.to_radians().tan()).to_bits()
    );
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
    let mut out = build(&[solid(&SQUARE, &faces)], &camera(), [8, 8], 2);
    assert_eq!((out.len(), out.discarded()), (2, 0));
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
    let mut out = build(&[moved], &camera(), [8, 8], 2);
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
    assert_eq!(build(&[backwards], &camera(), [8, 8], 2).len(), 0);
    let mut clipped = solid(&SQUARE, &faces);
    clipped.pose = Transform::local([0.0, 0.0, 1.95], [0.0; 3], 1.0);
    let mut out = build(&[clipped, solid(&SQUARE, &faces)], &camera(), [8, 8], 4);
    assert_eq!(out.len(), 2);
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
    let (result, allocations) =
        allocation::count(|| frame(&[s], &extremal_camera, &light(), [2, 2], &mut out, &cancel));
    assert_eq!((result, allocations), (Ok(()), 0));
    assert_eq!(out.len(), 24000);
    let mut short = FrameScratch::with_capacity(23999);
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
    assert_eq!(short.len(), 0);
    let repeated = vec![[0, 1, 2]; 24001];
    let over = [Face {
        anchors: [0, 1, 2],
        triangles: &repeated,
    }];
    assert_eq!(
        frame(
            &[solid(&SQUARE, &over)],
            &camera(),
            &light(),
            [8, 8],
            &mut out,
            &cancel
        ),
        Err(FrameError::Over)
    );
    assert_eq!(out.len(), 0);
    let face = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT[..1],
    }];
    let s = solid(&SQUARE, &face);
    assert_eq!(
        frame(
            &vec![s; 160],
            &camera(),
            &light(),
            [4096, 1],
            &mut out,
            &cancel
        ),
        Ok(())
    );
    assert_eq!(out.len(), 160);
    assert_eq!(
        frame(
            &vec![s; 161],
            &camera(),
            &light(),
            [8, 8],
            &mut out,
            &cancel
        ),
        Err(FrameError::Over)
    );
    assert_eq!(out.len(), 0);
    for size in [[0, 8], [8, 0], [4097, 1], [1, 4097]] {
        assert_eq!(
            frame(&[s], &camera(), &light(), size, &mut out, &cancel),
            Err(FrameError::Invalid)
        );
        assert_eq!(out.len(), 0);
    }
    for opacity in [-0.01, 1.01, f32::NAN, f32::INFINITY] {
        let mut invalid = s;
        invalid.opacity = opacity;
        assert_eq!(
            frame(&[invalid], &camera(), &light(), [8, 8], &mut out, &cancel),
            Err(FrameError::Invalid)
        );
    }
    for opacity in [0.0, 1.0] {
        let mut valid = s;
        valid.opacity = opacity;
        assert_eq!(
            frame(&[valid], &camera(), &light(), [8, 8], &mut out, &cancel),
            Ok(())
        );
        assert_eq!(out.len(), usize::from(opacity == 1.0));
    }
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let vertices = [Vec3::new(bad, 0.0, 0.0)];
        assert_eq!(
            frame(
                &[solid(&vertices, &[])],
                &camera(),
                &light(),
                [8, 8],
                &mut out,
                &cancel
            ),
            Err(FrameError::Numeric)
        );
    }
    for (anchors, triangles) in [([0, 1, 4], [[0, 1, 2]]), ([0, 1, 2], [[0, 1, 4]])] {
        let faces = [Face {
            anchors,
            triangles: &triangles,
        }];
        assert_eq!(
            frame(
                &[solid(&SQUARE, &faces)],
                &camera(),
                &light(),
                [8, 8],
                &mut out,
                &cancel
            ),
            Err(FrameError::Invalid)
        );
    }
    assert_eq!(
        frame(&[], &camera(), &light(), [8, 8], &mut out, &cancel),
        Ok(())
    );
    assert_eq!(out.len(), 0);
    assert_eq!(
        frame(
            &[solid(&[], &[])],
            &camera(),
            &light(),
            [8, 8],
            &mut out,
            &cancel
        ),
        Ok(())
    );
    assert_eq!(out.len(), 0);
    assert_eq!(
        frame(
            &[s],
            &camera(),
            &light(),
            [8, 8],
            &mut out,
            &AtomicBool::new(true)
        ),
        Err(FrameError::Cancelled)
    );
    assert_eq!(out.len(), 0);
}

#[test]
fn opposite_shared_edges_near_and_guard_clips_cover_once_including_snap_ties() {
    let face = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT,
    }];
    for shift in [0.0, 1.0 / 2048.0] {
        for depth in [0.0, 1.0] {
            let vertices = [
                Vec3::new(-1000.0, -1000.0, depth),
                Vec3::new(1000.0, -1000.0, depth),
                Vec3::new(1000.0, 1000.0, 2.0),
                Vec3::new(-1000.0, 1000.0, 2.0),
            ];
            let mut s = solid(&vertices, &face);
            s.opacity = 0.5;
            let cam = Camera::new(0.0, 0.0, 2.0, 90.0, [shift, 0.0, 0.0]).expect("valid fixture");
            let mut out = build(&[s], &cam, [8, 8], 2);
            assert_eq!((out.len(), out.discarded()), (2, 0));
            for q in [Quality::Motion, Quality::Rest] {
                let (pixels, ids) = image(&mut out, [8, 8], q);
                assert_eq!(ids, vec![1; 64]);
                assert_eq!(
                    pixels
                        .iter()
                        .map(|v| v.to_le_bytes()[3])
                        .collect::<Vec<_>>(),
                    vec![128; 64]
                );
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
    assert_eq!(cam.near(2.0, 16385.0, 8), Ok(0.062_500_000_000_000_01));
    let boundary = f32::from_bits(0x4680_01e0);
    for (z, expected) in [(boundary.next_down(), 2), (boundary, 0)] {
        let mut s = solid(&SQUARE, &faces);
        s.pose = Transform::local([16384.0, 16384.0, z], [0.0; 3], 1.0);
        let out = build(&[s], &cam, [8, 8], 2);
        assert_eq!(out.len(), expected);
    }
    let mut out = FrameScratch::with_capacity(2);
    let tiny = Camera::new(0.0, 0.0, 1e-300, 90.0, [0.0; 3]).expect("valid fixture");
    assert_eq!(
        frame(
            &[solid(&SQUARE, &faces)],
            &tiny,
            &light(),
            [8, 8],
            &mut out,
            &AtomicBool::new(false)
        ),
        Err(FrameError::Numeric)
    );
    assert_eq!(out.len(), 0);
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
    let first = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT,
    }];
    let second = [Face {
        anchors: [0, 1, 2],
        triangles: &OTHER,
    }];
    let tilted = SQUARE.map(|v| Vec3::new(v.x, v.y, 0.125 * v.x + 0.25 * v.y));
    for depth in [f32::from_bits(0x3f00_0007), f32::from_bits(0x3f00_0008)] {
        for (yaw, pitch, vertices) in [
            (0.0, 0.0, &SQUARE),
            (0.0, 20.0, &tilted),
            (31.0, 20.0, &tilted),
        ] {
            let cam = Camera::new(yaw, pitch, 1.0 / f64::from(depth), 90.0, [0.0; 3])
                .expect("valid fixture");
            for (a, b) in [(&first, &second), (&second, &first)] {
                let mut reference = build(&[solid(vertices, a)], &cam, [32, 32], 2);
                let mut overlap =
                    build(&[solid(vertices, a), solid(vertices, b)], &cam, [32, 32], 4);
                for q in [Quality::Motion, Quality::Rest] {
                    assert_eq!(
                        image(&mut overlap, [32, 32], q),
                        image(&mut reference, [32, 32], q)
                    );
                }
            }
        }
    }
}

#[test]
fn reused_frame_has_zero_allocations() {
    let faces = [Face {
        anchors: [0, 1, 2],
        triangles: &SPLIT,
    }];
    let s = solid(&SQUARE, &faces);
    let mut out = build(&[s], &camera(), [8, 8], 2);
    let cam = camera();
    let lamp = light();
    let stop = AtomicBool::new(false);
    let (result, allocations) =
        allocation::count(|| frame(&[s], &cam, &lamp, [8, 8], &mut out, &stop));
    assert_eq!((result, allocations), (Ok(()), 0));
    assert_eq!(out.len(), 2);
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
        for f in s.faces {
            for t in f.triangles {
                let p = t.map(|j| {
                    let v = s.pose.point(s.vertices[j as usize]).expect("valid fixture");
                    [v.x, v.y, v.z].map(f64::from)
                });
                triangles.push((p, (i + 1) as u8));
            }
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
            let mut checked = 0;
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
                    checked += 1;
                }
            }
            assert!(
                checked > 2000,
                "the edge/depth exclusions must not hide the view: {checked}"
            );
        }
    }
}

mod measure {
    use super::*;
    use std::time::Instant;
    #[test]
    #[ignore = "run release under the Rez quiet lock, never during a crew build"]
    fn measure_frame_pose_cull_clip_project_shade() {
        for count in [2000, 6000, 24000] {
            let triangles = [[0, 1, 2]];
            let faces = vec![
                Face {
                    anchors: [0, 1, 2],
                    triangles: &triangles,
                };
                count
            ];
            for (name, vertices) in [
                ("ordinary", SQUARE),
                ("culled", [SQUARE[0], SQUARE[2], SQUARE[1], SQUARE[3]]),
                (
                    "near and guard",
                    [
                        Vec3::new(-1000.0, -1000.0, 0.0),
                        Vec3::new(1000.0, -1000.0, 0.0),
                        Vec3::new(1000.0, 1000.0, 2.0),
                        Vec3::new(-1000.0, 1000.0, 2.0),
                    ],
                ),
            ] {
                let s = solid(&vertices, &faces);
                let mut out = FrameScratch::with_capacity(count);
                let cam = camera();
                let lamp = light();
                let stop = AtomicBool::new(false);
                let mut times = Vec::new();
                for sample in 0..103 {
                    let start = Instant::now();
                    std::hint::black_box(frame(&[s], &cam, &lamp, [1280, 720], &mut out, &stop))
                        .expect("valid fixture");
                    if sample >= 3 {
                        times.push(start.elapsed().as_secs_f64() * 1000.0);
                    }
                }
                times.sort_by(f64::total_cmp);
                println!(
                    "frame {name} {count}: p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms, n {}",
                    times[49],
                    times[98],
                    times[99],
                    times.len()
                );
            }
        }
    }
}
