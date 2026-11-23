//! Stable, standalone timing workloads. Measure one at a time on the quiet host.
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "timing samples and usage are the benchmark output"
)]

use std::process::ExitCode;

#[path = "workloads/dispatch.rs"]
mod dispatch;
#[path = "workloads/mermaid.rs"]
mod mermaid;
#[path = "../tests/support/mermaid.rs"]
mod mermaid_fixture;
#[path = "../tests/support/scene.rs"]
#[expect(
    dead_code,
    reason = "the shared scene fixtures include helpers unused by this benchmark"
)]
mod scene_support;
#[path = "workloads/state_followups.rs"]
mod state_followups;
#[path = "../tests/support/state_followups.rs"]
mod state_followups_fixture;
use scene_support::new_scene;
fn main() -> ExitCode {
    dispatch::run(&[
        (
            "measure_mermaid_admission_and_prepare",
            mermaid::measure_mermaid_admission_and_prepare,
        ),
        (
            "measure_whole_scene_removal_admit",
            state_followups::measure_whole_scene_removal_admit,
        ),
    ])
}
