#![expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "manual Rez measurement with synthetic geometry"
)]
use kumochi_scene::three::raster::{BandScratch, Fragment, FrameScratch, Quality, Target, raster};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn stats(mut ms: Vec<f64>) -> String {
    ms.sort_by(f64::total_cmp);
    let rank = |p: f64| ms[((p * ms.len() as f64).ceil() as usize).clamp(1, ms.len()) - 1];
    format!(
        "p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms, n {}",
        rank(0.5),
        rank(0.95),
        ms[ms.len() - 1],
        ms.len()
    )
}

/// Synthetic projected triangles: overdraw about 2 on the target, 70% opaque, 30% translucent
/// (half of those under 0.5), each at its own constant depth.
fn synthetic(count: usize, width: i32, height: i32, opaque_only: Option<bool>) -> FrameScratch {
    let mut seed = 0x9e37_79b9_u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let area = 2.0 * f64::from(width * height) / count as f64;
    // An equilateral triangle of circumradius R covers 1.299·R², so the target is covered
    // about twice.
    let radius = (area / 1.299).sqrt() * 256.0;
    let mut frame = FrameScratch::with_capacity(count);
    while frame.len() < count {
        let centre = [
            (next() % (width as u32 * 256)) as i32,
            (next() % (height as u32 * 256)) as i32,
        ];
        let corner = |angle: f64| {
            [
                centre[0] + (radius * angle.cos()) as i32,
                centre[1] + (radius * angle.sin()) as i32,
            ]
        };
        let turn = f64::from(next() % 628) / 100.0;
        let points = [corner(turn), corner(turn + 2.1), corner(turn + 4.2)];
        let kind = next() % 10;
        let opaque = kind < 7;
        if opaque_only.is_some_and(|only| only != opaque) {
            continue;
        }
        let opacity = if opaque {
            1.0
        } else if kind < 9 {
            0.3
        } else {
            0.6
        };
        let depth = 0.1 + f32::from(u16::try_from(next() % 800).expect("small")) / 1000.0;
        let solid = u8::try_from(next() % 160 + 1).expect("solid id");
        let fragment = Fragment {
            points: &points,
            plane: [0.0, 0.0, depth],
            rgb: [200, 100, 50],
            opacity,
            solid,
            centroid_z: 1.0 / depth,
        };
        frame.push(&fragment).expect("synthetic polygon fits");
    }
    frame
}

pub(super) fn measure_raster_and_tessellation() {
    let stop = AtomicBool::new(false);
    // A full-overlay view at k = 8, then half-resolution motion of the same view.
    for (width, height, qualities) in [
        (1280_u32, 720_u32, &[Quality::Motion, Quality::Rest][..]),
        (640, 360, &[Quality::Motion][..]),
    ] {
        let mut band = BandScratch::new(width).expect("band");
        let texels = (width * height) as usize;
        let (mut color, mut ids) = (vec![0; texels], vec![0; texels]);
        for count in [2_000, 6_000, 24_000] {
            for (label, only) in [
                ("mixed", None),
                ("opaque only", Some(true)),
                ("translucent only", Some(false)),
            ] {
                let mut frame = synthetic(count, width as i32, height as i32, only);
                for &quality in qualities {
                    let mut times = Vec::new();
                    for sample in 0..23 {
                        let target = Target {
                            width,
                            height,
                            color: &mut color,
                            ids: &mut ids,
                        };
                        let start = Instant::now();
                        raster(&mut frame, quality, &mut band, target, &stop).expect("frame");
                        if sample >= 3 {
                            times.push(start.elapsed().as_secs_f64() * 1e3);
                        }
                    }
                    println!(
                        "raster {width}x{height}, {count} triangles {label} ({} kept), {quality:?}: {}",
                        frame.len(),
                        stats(times)
                    );
                }
            }
        }
    }
    let lathe: kumochi_scene::record::Solid = serde_json::from_value(serde_json::json!({
        "in": "view", "id": "lathe", "shape": "lathe", "segments": 48,
        "points": (0..251).map(|i| [1.0 + f64::from(i % 7) * 0.1, f64::from(i) * 0.05]).collect::<Vec<_>>()
    }))
    .expect("lathe fixture");
    let count = kumochi_scene::three::size::triangles(&lathe).expect("sized");
    let mut out = Vec::with_capacity(count as usize);
    let mut scratch = kumochi_scene::three::mesh::MeshScratch::new();
    let world = kumochi_scene::three::Transform::local([0.0; 3], [0.0; 3], 1.0);
    let mut times = Vec::new();
    for sample in 0..23 {
        out.clear();
        let start = Instant::now();
        kumochi_scene::three::mesh::build(&lathe, &world, 1, &mut out, &stop, &mut scratch)
            .expect("builds");
        if sample >= 3 {
            times.push(start.elapsed().as_secs_f64() * 1e3);
        }
    }
    println!(
        "mesh::build lathe of {count} candidates ({} written): {}",
        out.len(),
        stats(times)
    );
}
