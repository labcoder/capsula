#![expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "bounded fixture triangle indices"
)]

use crate::frame_fixture::{Face, SQUARE, camera, handle, light, mapping, pointer, solid, state};
use kumochi_scene::{
    geom::Vec3,
    three::{
        Transform,
        frame::frame,
        pick::{DisplayedFrame, pick},
        raster::FrameScratch,
    },
};
use std::{sync::atomic::AtomicBool, time::Instant};

pub(super) fn measure_frame_pose_cull_clip_project_shade() {
    for count in [2000, 6000, 24000] {
        for (name, source, opacity, solid_count) in [
            ("ordinary", [SQUARE[0], SQUARE[1], SQUARE[2]], 1.0, 1),
            ("culled", [SQUARE[0], SQUARE[2], SQUARE[1]], 1.0, 1),
            ("translucent", [SQUARE[0], SQUARE[1], SQUARE[2]], 0.6, 1),
            ("four solids", [SQUARE[0], SQUARE[1], SQUARE[2]], 0.6, 4),
            (
                "near and guard",
                [
                    Vec3::new(-1000.0, -1000.0, 0.0),
                    Vec3::new(1000.0, -1000.0, 0.0),
                    Vec3::new(1000.0, 1000.0, 2.0),
                ],
                1.0,
                1,
            ),
        ] {
            let per_solid = count / solid_count;
            let vertices: Vec<_> = (0..count)
                .flat_map(|i| {
                    source.map(|v| {
                        Vec3::new(
                            v.x + f32::from(u16::try_from(i % 73).expect("bounded jitter"))
                                * 0.0001,
                            v.y + f32::from(u16::try_from(i % 37).expect("bounded jitter"))
                                * 0.0001,
                            v.z,
                        )
                    })
                })
                .collect();
            let triangles: Vec<_> = (0..count)
                .map(|i| {
                    let first = ((i % per_solid) * 3) as u32;
                    [first, first + 1, first + 2]
                })
                .collect();
            let faces: Vec<_> = triangles
                .iter()
                .map(|t| Face {
                    anchors: *t,
                    triangles: std::slice::from_ref(t),
                })
                .collect();
            let solids: Vec<_> = faces
                .chunks_exact(per_solid)
                .zip(vertices.chunks_exact(per_solid * 3))
                .enumerate()
                .map(|(i, (faces, vertices))| {
                    let mut s = solid(vertices, faces);
                    s.opacity = opacity;
                    let index = f32::from(u8::try_from(i).expect("at most four solids"));
                    s.pose =
                        Transform::local([index * 0.1, 0.0, 0.0], [0.0, index * 5.0, 0.0], 1.0);
                    s
                })
                .collect();
            let mut out = FrameScratch::with_capacity(count);
            let cam = camera();
            let lamp = light();
            let stop = AtomicBool::new(false);
            let mut times = Vec::new();
            for sample in 0..103 {
                let start = Instant::now();
                std::hint::black_box(frame(&solids, &cam, &lamp, [1280, 720], &mut out, &stop))
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

pub(super) fn measure_pick() {
    let map = mapping([3, 1]);
    let frame =
        DisplayedFrame::new(state(map, vec![Some(handle(0)), None]), vec![1, 2, 0]).expect("frame");
    let pointers = [
        pointer(map, [0.5; 2]),
        pointer(map, [1.5, 0.5]),
        pointer(map, [2.5, 0.5]),
    ];
    let mut times = Vec::new();
    for sample in 0..103 {
        let start = std::time::Instant::now();
        for i in 0..100_000 {
            std::hint::black_box(pick(
                std::hint::black_box(&frame),
                std::hint::black_box(pointers[i % 3]),
            ));
        }
        if sample >= 3 {
            times.push(start.elapsed().as_secs_f64() * 1e9 / 100_000.0);
        }
    }
    times.sort_by(f64::total_cmp);
    println!(
        "pick: p50 {:.3} ns, p99 {:.3} ns, max {:.3} ns, n {} batches of 100,000",
        times[49],
        times[98],
        times[99],
        times.len()
    );
}
