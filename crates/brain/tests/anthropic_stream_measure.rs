//! Opt-in launcher for the local, separately built parsing measurement runner.

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
#[ignore = "manual quiet-Rez release measurement; requires the scratch runner"]
#[expect(clippy::disallowed_methods, reason = "spawns while holding spawn_lock")]
fn measure_anthropic_stream() {
    let runner = std::env::var_os("A2B_MEASURE_RUNNER")
        .expect("set A2B_MEASURE_RUNNER to the prebuilt scratch executable");
    let mut child = {
        let _spawning = kumochi_brain::platform::spawn_lock();
        Command::new(runner)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the prebuilt measurement runner")
    };
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        if let Some(status) = child.try_wait().expect("check measurement runner") {
            assert!(status.success(), "measurement runner failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().expect("stop the overdue measurement runner");
            child.wait().expect("reap the measurement runner");
            panic!("measurement runner exceeded its launcher deadline");
        }
        thread::sleep(Duration::from_millis(20));
    }
}
