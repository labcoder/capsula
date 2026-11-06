//! Content-free panic evidence from child processes. Its own binary, because a child started here
//! while another test spawned a worker could inherit that worker's pipe ends.
#![cfg(all(windows, debug_assertions))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kumochi_brain::platform::Job;
use kumochi_brain::screen::{Generation, RequestBody, ScreenError, ScreenSupervisor};

const WORKER: &str = env!("CARGO_BIN_EXE_kumochid");
const PATIENCE: Duration = Duration::from_secs(10);

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the only test this binary runs in-process, so no spawn overlaps it"
)]
fn panic_hook_and_environment_fault_are_exercised_in_child_processes() {
    let dir = std::env::temp_dir().join(format!("screen-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("logs directory");
    for (test, fault) in [
        ("panic_fixture", "panic"),
        ("environment_crash_fixture", "crash"),
        ("exit_at_launch_fixture", "exit"),
    ] {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", test, "--ignored", "--nocapture"])
            .env(kumochi_protocol::constants::SCREEN_FAULT_ENV, fault)
            .env("LOCALAPPDATA", &dir)
            .output()
            .expect("fixture process");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let logs = std::fs::read_dir(dir.join(kumochi_protocol::constants::DATA_DIR).join("logs"))
        .expect("logs")
        .map(|entry| std::fs::read_to_string(entry.expect("entry").path()).expect("log"))
        .collect::<String>();
    assert!(logs.contains("worker.rs:"), "{logs}");
    assert!(!logs.contains("private screen fault payload"), "{logs}");
    assert!(!logs.contains("RequestBody"), "{logs}");
    std::fs::remove_dir_all(dir).expect("remove logs");
}

#[tokio::test]
#[ignore = "subprocess fixture with isolated environment"]
async fn panic_fixture() {
    let job = Arc::new(Job::new().expect("job"));
    let screen = ScreenSupervisor::with_deadline(PathBuf::from(WORKER), Arc::clone(&job), PATIENCE)
        .expect("supervisor");
    assert_eq!(
        screen.request(RequestBody::Ping).await,
        Err(ScreenError::WorkerLost)
    );
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

#[tokio::test]
#[ignore = "subprocess fixture with isolated environment"]
async fn environment_crash_fixture() {
    let job = Arc::new(Job::new().expect("job"));
    let screen = ScreenSupervisor::with_deadline(PathBuf::from(WORKER), Arc::clone(&job), PATIENCE)
        .expect("supervisor");
    let mut losses = screen.lost();
    tokio::time::pause();
    let keep_clock_manual = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    for dead in 0..3 {
        assert_eq!(
            screen.request(RequestBody::Ping).await,
            Err(ScreenError::WorkerLost)
        );
        assert_eq!(losses.recv().await.expect("worker loss"), Generation(dead));
    }
    assert_eq!(
        screen.request(RequestBody::Ping).await,
        Err(ScreenError::WorkerLost)
    );
    assert_eq!(
        losses.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    );
    keep_clock_manual.abort();
    assert!(
        keep_clock_manual
            .await
            .expect_err("manual clock task cancelled")
            .is_cancelled()
    );
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

#[tokio::test]
#[ignore = "subprocess fixture with isolated environment"]
async fn exit_at_launch_fixture() {
    let job = Arc::new(Job::new().expect("job"));
    let screen = ScreenSupervisor::with_deadline(PathBuf::from(WORKER), Arc::clone(&job), PATIENCE)
        .expect("supervisor");
    let mut losses = screen.lost();
    let mut at = Vec::new();
    for dead in 0..5 {
        let loss = tokio::time::timeout(PATIENCE, losses.recv()).await;
        assert_eq!(
            loss.expect("lost with nothing pending").expect("loss"),
            Generation(dead)
        );
        at.push(std::time::Instant::now());
    }
    // A child that exits at every launch respawns in milliseconds; only backoff spaces these.
    assert!(
        at[3] - at[2] >= Duration::from_millis(200),
        "third loss waited"
    );
    assert!(
        at[4] - at[3] >= Duration::from_secs(1),
        "fourth loss waited"
    );
    assert_eq!(
        screen.request(RequestBody::Ping).await,
        Err(ScreenError::WorkerLost)
    );
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}
