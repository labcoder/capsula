//! Standalone timing workloads for the renderer. Measure one at a time on the quiet host:
//! `cargo bench -p kumochi-render --bench render-workloads -- <workload>`.
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "timing samples and usage are the benchmark output"
)]

use std::{
    hint::black_box,
    process::ExitCode,
    time::{Duration, Instant},
};

use kumochi_protocol::message::SceneId;
use kumochi_render::{BuildInput, FrameBuf, Mesh, RenderCache, Stager, record};
use kumochi_scene::GlobalBudget;
use kumochi_shell_core::presenter::{SceneCache, Stage};

#[path = "../../scene/tests/support/allocation.rs"]
#[expect(
    dead_code,
    reason = "the shared test allocator includes additional instrumentation"
)]
mod allocation;
#[path = "../../scene/benches/workloads/dispatch.rs"]
mod dispatch;
#[path = "../src/tests/fixture.rs"]
#[expect(
    dead_code,
    reason = "the shared fixtures also build views only the tests draw"
)]
mod fixture;

fn main() -> ExitCode {
    dispatch::run(&[
        ("mesh_at_limits", mesh_at_limits),
        ("record_frame", record_frame),
    ])
}

fn render(cache: &dyn SceneCache) -> &RenderCache {
    let cache: &dyn std::any::Any = cache;
    cache.downcast_ref().expect("a render cache")
}

fn report(workload: &str, samples: &mut [Duration], extra: &str) {
    samples.sort_unstable();
    let at = |q: usize| samples[(samples.len() - 1) * q / 100].as_secs_f64() * 1e6;
    println!(
        "workload={workload} samples={} p50_us={:.1} p99_us={:.1} max_us={:.1} {extra}",
        samples.len(),
        at(50),
        at(99),
        at(100)
    );
}

/// The owner's copy and the stage's mesh build of one view at the global paint caps.
fn mesh_at_limits() {
    let view = fixture::view(
        "limits",
        &fixture::at_limits(),
        &mut GlobalBudget::default(),
    );
    let stager = Stager::new();
    let boxed = stager.stage(&view);
    let cache = render(&*boxed);
    let mut input = BuildInput::new();
    let mut mesh = Mesh::new();
    let (mut copies, mut builds) = (Vec::with_capacity(64), Vec::with_capacity(64));
    for run in 0..72 {
        input.wipe();
        let timer = Instant::now();
        input.add(black_box(cache)).expect("the view fits");
        let copy = timer.elapsed();
        let timer = Instant::now();
        mesh.build(black_box(&input), cache.id())
            .expect("the view builds");
        let build = timer.elapsed();
        if run >= 8 {
            copies.push(copy);
            builds.push(build);
        }
    }
    let counts = format!(
        "vertices={} indices={} fixed_bytes={} input={input:?}",
        mesh.vertices().len(),
        mesh.indices().len(),
        stager.fixed_bytes()
    );
    report("copy-at-limits", &mut copies, "");
    report("mesh-at-limits", &mut builds, &counts);
}

/// Writing one frame's records for four views at the global object cap.
fn record_frame() {
    let mut budget = GlobalBudget::default();
    let stager = Stager::new();
    let views: Vec<_> = [600, 600, 150, 150]
        .into_iter()
        .enumerate()
        .map(|(n, count)| fixture::view(&format!("v{n}"), &fixture::squares(count), &mut budget))
        .collect();
    let mut caches: Vec<_> = views.iter().map(|view| stager.stage(view)).collect();
    let frames: Vec<_> = views.iter().map(fixture::sample).collect();
    let mut buf = FrameBuf::new();
    let mut write = |buf: &mut FrameBuf| {
        buf.clear();
        for (scene, (frame, cache)) in frames.iter().zip(&mut caches).enumerate() {
            let scene = SceneId(u64::try_from(scene).expect("four scenes"));
            record(buf, scene, black_box(frame), &mut **cache).expect("the frame records");
        }
    };
    for _ in 0..64 {
        write(&mut buf);
    }
    let mut samples = Vec::with_capacity(1024);
    let ((), [allocations, frees]) = allocation::activity(|| {
        for _ in 0..1024 {
            let start = Instant::now();
            write(&mut buf);
            samples.push(start.elapsed());
        }
    });
    assert_eq!(allocations, 0, "writing records allocates nothing");
    assert_eq!(frees, 0, "writing records frees nothing");
    let records: usize = buf
        .pictures()
        .iter()
        .map(|picture| buf.records(picture).len())
        .sum();
    report(
        "record-four-views",
        &mut samples,
        &format!("allocations={allocations} frees={frees} records={records}"),
    );
}
