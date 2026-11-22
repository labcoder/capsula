//! Stable, standalone timing workloads. Measure one at a time on the quiet host.
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "timing samples and usage are the benchmark output"
)]

use std::process::ExitCode;

#[path = "../tests/support/allocation.rs"]
#[expect(
    dead_code,
    reason = "the shared test allocator includes additional instrumentation"
)]
mod allocation;
#[path = "workloads/dispatch.rs"]
mod dispatch;
#[path = "../tests/support/frame_fixture.rs"]
mod frame_fixture;
#[path = "workloads/motion.rs"]
mod motion;
#[path = "../tests/support/motion.rs"]
mod motion_fixture;
#[path = "workloads/raster.rs"]
mod raster;
#[path = "../tests/support/scene.rs"]
#[expect(
    dead_code,
    reason = "the shared scene fixtures include helpers unused by this benchmark"
)]
mod scene_support;
#[path = "workloads/state_three.rs"]
mod state_three;
#[path = "../tests/support/state_three.rs"]
mod state_three_fixture;
#[path = "workloads/three_frame.rs"]
mod three_frame;
#[path = "workloads/tokenize.rs"]
mod tokenize;
#[path = "../tests/support/tokenizer_fixture.rs"]
mod tokenizer_fixture;
use scene_support::{new_scene, prepared};
fn main() -> ExitCode {
    dispatch::run(&[
        ("frame_four_views_max", motion::frame_four_views_max),
        ("activation_at_caps", motion::activation_at_caps),
        (
            "measure_raster_and_tessellation",
            raster::measure_raster_and_tessellation,
        ),
        ("measure_admit", state_three::measure_admit),
        ("measure_pick", three_frame::measure_pick),
        (
            "measure_frame_pose_cull_clip_project_shade",
            three_frame::measure_frame_pose_cull_clip_project_shade,
        ),
        ("measure_tokenizer", tokenize::measure_tokenizer),
    ])
}
