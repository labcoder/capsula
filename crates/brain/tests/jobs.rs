//! The Job Objects end exactly the processes they should.
#![cfg(windows)]

use std::path::Path;
use std::time::Duration;

use kumochi_brain::SCREEN_WORKER;
use kumochi_brain::platform::Job;

const WORKER: &str = env!("CARGO_BIN_EXE_kumochid");
const SETTLE: Duration = Duration::from_millis(300);

#[test]
fn a_run_job_ends_its_child_and_leaves_the_screen_worker() {
    let brain = Job::new().expect("brain job");
    let (screen, _screen_pipes) =
        Job::spawn_piped(Path::new(WORKER), SCREEN_WORKER, &[&brain]).expect("screen worker");
    let run = Job::new().expect("run job");
    let (child, _child_pipes) =
        Job::spawn_piped(Path::new(WORKER), SCREEN_WORKER, &[&brain, &run]).expect("run child");
    assert_eq!(
        brain.active_processes().expect("count"),
        2,
        "the run job nests in the brain job"
    );
    assert_eq!(run.active_processes().expect("count"), 1);

    run.terminate().expect("run job ends");
    assert!(
        child.exits_within(Duration::from_secs(1)).expect("wait"),
        "the run child ended"
    );
    assert!(
        !screen.exits_within(SETTLE).expect("wait"),
        "the screen worker survived"
    );
    assert_eq!(brain.active_processes().expect("count"), 1);

    drop(run);
    drop(brain);
    assert!(
        screen.exits_within(Duration::from_secs(1)).expect("wait"),
        "closing the brain job ended it"
    );
}
