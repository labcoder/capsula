//! Content-free panic evidence from child processes. Its own binary, because a child started here
//! while another test spawned a worker could inherit that worker's pipe ends.
#![cfg(all(windows, debug_assertions))]

#[path = "../../../tests/support/scratch.rs"]
mod scratch;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kumochi_brain::platform::Job;
use kumochi_brain::screen::{Generation, HoldClient, WorkerLost};

const WORKER: &str = env!("CARGO_BIN_EXE_kumochid");
const PATIENCE: Duration = Duration::from_secs(10);
/// One fixture process, start to exit [guess: each takes a few seconds].
const FIXTURE_PATIENCE: Duration = Duration::from_secs(60);

/// A request's deadline under [`PATIENCE`].
fn until() -> tokio::time::Instant {
    tokio::time::Instant::now() + PATIENCE
}

/// Waits for `next` under [`PATIENCE`] of real time, which a paused tokio clock can't measure.
async fn within_patience<T>(next: impl std::future::Future<Output = T>) -> T {
    let (expire, expired) = tokio::sync::oneshot::channel::<()>();
    drop(std::thread::spawn(move || {
        std::thread::sleep(PATIENCE);
        drop(expire);
    }));
    tokio::select! {
        value = next => value,
        _ = expired => panic!("nothing within {PATIENCE:?}"),
    }
}

/// Everything `pipe` carries, read on its own thread so a full pipe can't stall the child.
fn drain(mut pipe: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).expect("fixture output");
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the only test this binary runs in-process, so no spawn overlaps it"
)]
fn panic_hook_and_environment_fault_are_exercised_in_child_processes() {
    let dir = scratch::dir("screen-logs");
    for (test, fault) in [
        ("panic_fixture", "panic"),
        ("environment_crash_fixture", "crash"),
        ("exit_at_launch_fixture", "exit"),
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", test, "--ignored", "--nocapture"])
            .env(kumochi_protocol::constants::SCREEN_FAULT_ENV, fault)
            .env("LOCALAPPDATA", dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("fixture process");
        let stdout = drain(child.stdout.take().expect("piped stdout"));
        let stderr = drain(child.stderr.take().expect("piped stderr"));
        let until = Instant::now() + FIXTURE_PATIENCE;
        let status = loop {
            if let Some(status) = child.try_wait().expect("fixture status") {
                break status;
            }
            if Instant::now() >= until {
                child.kill().expect("end the late fixture");
                let reap_by = Instant::now() + PATIENCE;
                while child.try_wait().expect("late fixture status").is_none() {
                    assert!(Instant::now() < reap_by, "{test} did not exit after kill");
                    std::thread::sleep(Duration::from_millis(20));
                }
                panic!("{test} outlived {FIXTURE_PATIENCE:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        while !stdout.is_finished() || !stderr.is_finished() {
            assert!(Instant::now() < until, "{test} output did not close");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (stdout, stderr) = (
            stdout.join().expect("stdout"),
            stderr.join().expect("stderr"),
        );
        assert_eq!(status.code(), Some(0), "{test}: {stdout}{stderr}");
    }
    let logs = std::fs::read_dir(dir.join(kumochi_protocol::constants::DATA_DIR).join("logs"))
        .expect("logs")
        .map(|entry| std::fs::read_to_string(entry.expect("entry").path()).expect("log"))
        .collect::<String>();
    assert!(logs.contains("actor.rs:"), "{logs}");
    assert!(!logs.contains("private screen fault payload"), "{logs}");
    assert!(!logs.contains("ScreenRequest"), "{logs}");
    let path = dir.path().to_path_buf();
    drop(dir);
    assert!(!path.try_exists().expect("check cleanup"), "remove logs");
}

#[tokio::test]
#[ignore = "subprocess fixture with isolated environment"]
async fn panic_fixture() {
    let job = Arc::new(Job::new().expect("job"));
    let screen = HoldClient::start(PathBuf::from(WORKER), Arc::clone(&job)).expect("worker");
    assert_eq!(screen.ping(until()).await, Err(WorkerLost::Lost));
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

#[tokio::test]
#[ignore = "subprocess fixture with isolated environment"]
async fn environment_crash_fixture() {
    let job = Arc::new(Job::new().expect("job"));
    let screen = HoldClient::start(PathBuf::from(WORKER), Arc::clone(&job)).expect("worker");
    let mut losses = screen.lost();
    tokio::time::pause();
    let keep_clock_manual = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    for dead in 0..3 {
        assert_eq!(screen.ping(until()).await, Err(WorkerLost::Lost));
        assert_eq!(
            within_patience(losses.recv()).await.expect("worker loss"),
            Generation(dead)
        );
    }
    assert_eq!(screen.ping(until()).await, Err(WorkerLost::Lost));
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
    let screen = HoldClient::start(PathBuf::from(WORKER), Arc::clone(&job)).expect("worker");
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
    assert_eq!(screen.ping(until()).await, Err(WorkerLost::Lost));
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}
